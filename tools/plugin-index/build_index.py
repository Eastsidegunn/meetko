"""Build the meetko plugin gallery index.

Finds GitHub repos with the topic `meetko-plugin`, reads `meetko.toml` at the latest release
tag (or the default branch's latest commit), checks it, and writes `index.json`.

    python3.12 -I tools/plugin-index/build_index.py --out index.json [--previous index.json]
                                                    [--blocklist blocklist.txt] [--allow-shrink]

Stdlib only. Requires Python 3.11 or newer (tomllib); older versions stop with a message.
`meetko.toml` is read from the tarball of the resolved commit, the same bytes `meetko plugin install`
gets (codeload tarballs are `git archive` output and honour `.gitattributes export-subst`, so the
Contents API could show something else). The tarball is streamed and capped, never written to disk.
A token is optional and only raises the API rate limit:
set GITHUB_TOKEN in the environment (it is read from the environment, never from a file, and
never printed). This is a convenience filter, NOT a review: nothing here runs plugin code or
judges whether it is safe. See docs/plugin-publishing.md.
"""

from __future__ import annotations

import sys

if sys.version_info < (3, 11):  # tomllib arrived in 3.11
    raise SystemExit(f"build_index.py needs Python 3.11 or newer (this is {sys.version_info.major}.{sys.version_info.minor})")

import argparse
import gzip
import io
import json
import os
import re
import tarfile
import tomllib
import unicodedata
import urllib.error
import urllib.parse
import urllib.request
import zlib
from datetime import datetime, timezone
from typing import Callable

TOPIC = "meetko-plugin"
SUPPORTED_CONTRACT = "0.1"
SUPPORTED_PROTOCOL = "stdio-jsonrpc"
MAX_MANIFEST_BYTES = 64 * 1024
API = "https://api.github.com"
CODELOAD = "https://codeload.github.com"
# the same ceilings the app applies when it unpacks (crates/meetko-core/src/plugin_source.rs)
MAX_TARBALL = 25 * 1024 * 1024
MAX_UNPACKED = 100 * 1024 * 1024 + 16 * 1024 * 1024
MAX_MEMBERS = 5000
MAX_EXTENSION_ENTRY = 1024 * 1024

ID_RE = re.compile(r"^([a-z0-9][a-z0-9_.-]{0,63})/([a-z0-9][a-z0-9_.-]{0,63})$")
VERSION_RE = re.compile(r"^\d+\.\d+\.\d+([-+][0-9A-Za-z.+-]+)?$")
SHA_RE = re.compile(r"^[0-9a-f]{40}$")
REPO_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,99}/[A-Za-z0-9._-]{1,100}$")


class Rejected(Exception):
    """A repo that does not make it into the index; the message says why."""


class FetchFailed(Exception):
    """GitHub did not give a usable answer to something the whole run depends on. The run fails
    (nothing is written) instead of publishing a smaller index."""


# Must agree with crates/meetko-core/src/text.rs: categories Cc, Cf, Zl, Zp, plus characters that render
# blank though they are letters or marks, and a cap on runs of combining marks.
_FILLERS = {0x34F, 0x115F, 0x1160, 0x3164, 0xFFA0}
MAX_MARK_RUN = 8


def _hidden_chars(v: str) -> bool:
    """Control, format, line/paragraph separator and invisible filler characters; long combining-mark runs."""
    run = 0
    for c in v:
        cp = ord(c)
        if (
            unicodedata.category(c) in ("Cc", "Cf", "Zl", "Zp")
            or cp in _FILLERS
            or 0xFE00 <= cp <= 0xFE0F
            or 0xE0000 <= cp <= 0xE007F
            or 0xE0100 <= cp <= 0xE01EF
        ):
            return True
        run = run + 1 if unicodedata.category(c) in ("Mn", "Me") else 0
        if run > MAX_MARK_RUN:
            return True
    return False


def _text(table: dict, key: str, limit: int) -> str:
    v = table.get(key, "")
    if not isinstance(v, str):
        raise Rejected(f"{key} must be a string")
    if len(v) > limit:
        raise Rejected(f"{key} is longer than {limit} characters")
    v = v.strip()
    if _hidden_chars(v):
        raise Rejected(f"{key} contains control or invisible formatting characters")
    return v


def _table(m: dict, key: str) -> dict:
    """An optional table: absent is empty, anything but a table is a rejection (not a crash)."""
    v = m.get(key, {})
    if not isinstance(v, dict):
        raise Rejected(f"[{key}] must be a table")
    return v


def _runtime(command: list[str]) -> str:
    """A coarse label from the program: shown to users, not used to run anything."""
    prog = command[0].replace("\\", "/")
    if "/" in prog:
        return "binary"
    name = prog.lower()
    if name.startswith("python"):
        return "python"
    if name in ("node", "deno", "bun"):
        return "node"
    if name in ("sh", "bash", "zsh"):
        return "shell"
    return name[:32]


def validate_manifest(text: str, owner: str) -> dict:
    """Return the manifest facts the index needs, or raise Rejected.

    Mirrors what the meetko core refuses at install (kind, contract, protocol, command, audio)
    plus gallery-only rules: a description, a semver version, and an id whose publisher part is
    the repo owner (so `acme/x` can only be listed from github.com/acme/...).
    """
    if len(text.encode("utf-8")) > MAX_MANIFEST_BYTES:
        raise Rejected("meetko.toml is larger than 64 KB")
    try:
        m = tomllib.loads(text)
    except tomllib.TOMLDecodeError as e:
        raise Rejected(f"meetko.toml does not parse: {e}") from None
    except (RecursionError, ValueError, OverflowError):
        raise Rejected("meetko.toml is nested too deeply or malformed") from None
    pkg, run = m.get("package"), m.get("run")
    if not isinstance(pkg, dict) or not isinstance(run, dict):
        raise Rejected("[package] and [run] are required")
    pid = pkg.get("id")
    if not isinstance(pid, str) or not ID_RE.match(pid):
        raise Rejected("package.id must look like publisher/name (lowercase letters, digits, . _ -)")
    if pid.split("/")[0] != owner.lower():
        raise Rejected(f"package.id publisher {pid.split('/')[0]!r} must be the repo owner {owner.lower()!r}")
    version = pkg.get("version")
    if not isinstance(version, str) or not VERSION_RE.match(version):
        raise Rejected("package.version must be semver (1.2.3)")
    if pkg.get("kind") != "plugin":
        raise Rejected("package.kind must be \"plugin\"")
    if pkg.get("contract") != SUPPORTED_CONTRACT:
        raise Rejected(f"package.contract {pkg.get('contract')!r} is not {SUPPORTED_CONTRACT!r}")
    description = _text(pkg, "description", 300)
    if not description:
        raise Rejected("package.description is required for the gallery")
    license_ = _text(pkg, "license", 64)
    homepage = _text(pkg, "homepage", 300)
    if homepage and not homepage.startswith("https://"):
        raise Rejected("package.homepage must be an https:// URL")
    command = run.get("command")
    if isinstance(command, str):
        command = [command]
    if not isinstance(command, list) or not command or not all(isinstance(c, str) for c in command) or not command[0]:
        raise Rejected("run.command must be a non-empty array of strings")
    if run.get("protocol") != SUPPORTED_PROTOCOL:
        raise Rejected(f"run.protocol must be {SUPPORTED_PROTOCOL!r}")
    if any(_hidden_chars(c) for c in command):
        raise Rejected("run.command contains control or invisible formatting characters")
    prog = command[0].replace("\\", "/")
    if prog.startswith("/") or ".." in prog.split("/") or re.match(r"^[A-Za-z]:", prog):
        raise Rejected("run.command must stay inside the plugin folder")
    audio = _table(m, "input").get("audio", "none")
    if not isinstance(audio, str) or audio not in ("none", "full"):
        raise Rejected("input.audio must be none or full")
    network = _table(m, "permissions").get("network", False)
    if not isinstance(network, bool):
        raise Rejected("permissions.network must be true or false")
    return {
        "id": pid,
        "name": pid.split("/", 1)[1],
        "version": version,
        "description": description,
        "license": license_,
        "homepage": homepage,
        "permissions": {"network": network, "audio": audio == "full"},
        "runtime": _runtime(command),
    }


def make_entry(repo: dict, ref: str, sha: str, facts: dict) -> dict:
    full = repo["full_name"]
    if not REPO_RE.match(full) or not SHA_RE.match(sha):
        raise Rejected("repo name or sha is malformed")
    entry = {
        "id": facts["id"],
        "name": facts["name"],
        "version": facts["version"],
        "description": facts["description"],
        "repo": full,
        "ref": ref,
        "sha": sha,
        "permissions": facts["permissions"],
        "runtime": facts["runtime"],
        "license": facts["license"],
        "stars": int(repo.get("stargazers_count", 0)),
        "updated": repo.get("pushed_at") or repo.get("updated_at") or "",
    }
    if facts["homepage"]:
        entry["homepage"] = facts["homepage"]
    return entry


def dedupe(entries: list[dict]) -> tuple[list[dict], list[str]]:
    """One entry per id: most stars wins, ties go to the repo name that sorts first."""
    best: dict[str, dict] = {}
    notes: list[str] = []
    for e in sorted(entries, key=lambda e: (-e["stars"], e["repo"].lower())):
        if e["id"] in best:
            notes.append(f"{e['repo']}: id {e['id']} is already listed from {best[e['id']]['repo']}")
        else:
            best[e["id"]] = e
    return sorted(best.values(), key=lambda e: (-e["stars"], e["id"])), notes


def validate_index(doc: object) -> list[str]:
    """Stdlib check of index.json against index.schema.json's rules; returns error strings."""
    errs: list[str] = []
    if not isinstance(doc, dict) or set(doc) != {"generated", "plugins"}:
        return ["top level must be exactly {generated, plugins}"]
    if not isinstance(doc["generated"], str) or not doc["generated"]:
        errs.append("generated must be a timestamp string")
    if not isinstance(doc["plugins"], list):
        return errs + ["plugins must be a list"]
    required = {"id", "name", "version", "description", "repo", "ref", "sha", "permissions", "runtime", "license", "stars", "updated"}
    optional = {"homepage"}
    seen = set()
    for i, p in enumerate(doc["plugins"]):
        where = f"plugins[{i}]"
        if not isinstance(p, dict) or not required <= set(p) <= required | optional:
            errs.append(f"{where}: wrong keys")
            continue
        if not (isinstance(p["id"], str) and ID_RE.match(p["id"])):
            errs.append(f"{where}.id")
        elif p["id"] in seen:
            errs.append(f"{where}.id duplicate")
        else:
            seen.add(p["id"])
        if not (isinstance(p["sha"], str) and SHA_RE.match(p["sha"])):
            errs.append(f"{where}.sha")
        if not (isinstance(p["repo"], str) and REPO_RE.match(p["repo"])):
            errs.append(f"{where}.repo")
        for k in ("name", "version", "description", "ref", "runtime", "license", "updated"):
            if not isinstance(p[k], str):
                errs.append(f"{where}.{k}")
        if not isinstance(p["stars"], int) or isinstance(p["stars"], bool) or p["stars"] < 0:
            errs.append(f"{where}.stars")
        perm = p["permissions"]
        if not (isinstance(perm, dict) and set(perm) == {"network", "audio"} and all(isinstance(v, bool) for v in perm.values())):
            errs.append(f"{where}.permissions")
    return errs


# ------------------------------------------------------------------ the source tarball


class _Cap(io.RawIOBase):
    """Reads from `raw` and raises Rejected once more than `limit` bytes have been asked for."""

    def __init__(self, raw, limit: int, what: str):
        self.raw, self.left, self.what, self.limit = raw, limit, what, limit

    def readable(self) -> bool:
        return True

    def readinto(self, b) -> int:
        data = self.raw.read(min(len(b), self.left + 1))
        if len(data) > self.left:
            raise Rejected(f"{self.what} is larger than {self.limit // (1024 * 1024)} MB")
        self.left -= len(data)
        b[: len(data)] = data
        return len(data)


def _pax_has_size(data: bytes) -> bool:
    """True if a PAX extended header has a `size=` record (it changes how the next entry is framed,
    so the watcher would lose sync); a malformed header raises Rejected."""
    rest = data
    while rest:
        sp = rest.find(b" ")
        try:
            n = int(rest[:sp])
        except ValueError:
            n = 0
        if sp < 0 or n <= sp + 1 or n > len(rest):
            raise Rejected("a tar extended header is malformed")
        if rest[sp + 1 : n].startswith(b"size="):
            return True
        rest = rest[n:]
    return False


class _TarWatch(_Cap):
    """The decompressed stream, capped, that also follows the 512-byte tar header blocks and
    refuses a PAX / GNU long-name extension entry claiming more than MAX_EXTENSION_ENTRY bytes
    (tarfile reads one fully into memory), a base-256 size field, and a PAX `size=` record
    (both would make tarfile frame entries differently from this watcher)."""

    def __init__(self, raw, limit: int, what: str):
        super().__init__(raw, limit, what)
        self.block, self.skip = bytearray(), 0
        self.collecting, self.data_left, self.collected = False, 0, bytearray()

    def readinto(self, b) -> int:
        n = super().readinto(b)
        data = memoryview(b)[:n]
        while len(data):
            if self.skip:
                k = min(self.skip, len(data))
                if self.collecting:
                    c = min(k, self.data_left)
                    self.collected += data[:c]
                    self.data_left -= c
                self.skip, data = self.skip - k, data[k:]
                if not self.skip:
                    self._done()
                continue
            k = min(512 - len(self.block), len(data))
            self.block += data[:k]
            data = data[k:]
            if len(self.block) == 512:
                self._header(bytes(self.block))
                self.block = bytearray()
                if not self.skip:
                    self._done()
        return n

    def _done(self) -> None:
        if self.collecting:
            self.collecting = False
            if _pax_has_size(bytes(self.collected)):
                raise Rejected("a tar extended header sets the entry size")

    def _header(self, h: bytes) -> None:
        field = h[124:136]
        if field[0] & 0x80:
            raise Rejected("a tar entry uses a base-256 size field")
        digits = field.split(b"\0")[0].strip()
        try:
            size = int(digits, 8) if digits else 0
        except ValueError:
            raise Rejected("a tar entry has an unreadable size") from None
        kind = h[156:157]
        if kind in (b"x", b"g", b"L", b"K") and size > MAX_EXTENSION_ENTRY:
            raise Rejected(f"a tar extension entry claims {size} bytes")
        self.skip = (size + 511) // 512 * 512
        self.data_left = size
        self.collecting = kind in (b"x", b"g")
        self.collected = bytearray()


def manifest_from_tarball(fileobj, *, max_unpacked: int = MAX_UNPACKED, max_download: int = MAX_TARBALL) -> bytes | None:
    """`<top>/meetko.toml` from a .tar.gz stream, or None when there is none. This is what a user
    who installs that commit gets, so it is the file the index must describe. Streamed through
    size caps and never extracted: only that one regular file is read, up to 64 KB."""
    gz = gzip.GzipFile(fileobj=io.BufferedReader(_Cap(fileobj, max_download, "the download")), mode="rb")
    stream = io.BufferedReader(_TarWatch(gz, max_unpacked, "the unpacked archive"))
    found: bytes | None = None
    try:
        with tarfile.open(fileobj=stream, mode="r|") as tf:
            for count, member in enumerate(tf, 1):
                if count > MAX_MEMBERS:
                    raise Rejected(f"the archive has more than {MAX_MEMBERS} entries")
                parts = [p for p in member.name.split("/") if p not in ("", ".")]
                if len(parts) != 2 or parts[1] != "meetko.toml":
                    continue
                if not member.isreg():
                    raise Rejected("meetko.toml is not a regular file")
                if found is not None:
                    raise Rejected("meetko.toml appears twice in the archive")
                if member.size > MAX_MANIFEST_BYTES:
                    raise Rejected("meetko.toml is larger than 64 KB")
                f = tf.extractfile(member)
                found = f.read(MAX_MANIFEST_BYTES + 1) if f else None
    except (tarfile.TarError, OSError, EOFError, zlib.error, ValueError) as e:
        raise Rejected(f"cannot read the source archive: {e}") from None
    return found


# ------------------------------------------------------------------ GitHub (the only network code)


class GitHub:
    def __init__(self, token: str | None = None, base: str = API, codeload: str = CODELOAD):
        self.token, self.base, self.codeload = token, base.rstrip("/"), codeload.rstrip("/")

    def request(self, path: str, accept: str) -> urllib.request.Request:
        req = urllib.request.Request(self.base + path, headers={"Accept": accept, "User-Agent": "meetko-plugin-index", "X-GitHub-Api-Version": "2022-11-28"})
        if self.token:
            # unredirected: a redirect to another host must not carry the token along
            req.add_unredirected_header("Authorization", f"Bearer {self.token}")
        return req

    def get(self, path: str, accept: str = "application/vnd.github+json", *, limit: int = 2_000_000) -> tuple[int, bytes]:
        try:
            with urllib.request.urlopen(self.request(path, accept), timeout=30) as r:
                return r.status, r.read(limit + 1)[:limit]
        except urllib.error.HTTPError as e:
            if e.code in (403, 429) and e.headers.get("X-RateLimit-Remaining") == "0":
                raise SystemExit("GitHub rate limit reached; set GITHUB_TOKEN or try later") from None
            return e.code, b""

    def json(self, path: str) -> object | None:
        status, body = self.get(path)
        return json.loads(body) if status == 200 else None

    def search_repos(self) -> list[dict]:
        out: list[dict] = []
        for page in range(1, 11):  # the search API stops at 1000 results
            q = urllib.parse.urlencode({"q": f"topic:{TOPIC}", "per_page": 100, "page": page})
            status, body = self.get(f"/search/repositories?{q}")
            if status != 200:
                # a failed search must not look like "no plugins": the caller would publish a shrunken index
                raise FetchFailed(f"GitHub search answered {status} (page {page})")
            try:
                data = json.loads(body)
            except ValueError:
                raise FetchFailed(f"GitHub search page {page} is not JSON") from None
            items = data.get("items") if isinstance(data, dict) else None
            if not isinstance(items, list):
                raise FetchFailed(f"GitHub search page {page} has no items")
            out += [i for i in items if isinstance(i, dict) and isinstance(i.get("full_name"), str)]
            if len(items) < 100:
                break
        return out

    def latest_release_tag(self, full: str) -> str | None:
        data = self.json(f"/repos/{full}/releases/latest")
        tag = data.get("tag_name") if isinstance(data, dict) else None
        return tag if isinstance(tag, str) and tag else None

    def resolve(self, full: str, ref: str) -> str | None:
        status, body = self.get(f"/repos/{full}/commits/{urllib.parse.quote(ref, safe='')}", accept="application/vnd.github.sha")
        sha = body.decode("ascii", "replace").strip().lower()
        return sha if status == 200 and SHA_RE.match(sha) else None

    def manifest(self, full: str, sha: str) -> str | None:
        """meetko.toml as it is INSIDE the tarball of `sha` (not the Contents API's raw file: the
        archive applies export-subst). No token is sent to the download host."""
        url = f"{self.codeload}/{full}/tar.gz/{sha}"
        req = urllib.request.Request(url, headers={"User-Agent": "meetko-plugin-index"})
        try:
            with urllib.request.urlopen(req, timeout=30) as r:
                data = manifest_from_tarball(r)
        except urllib.error.HTTPError as e:
            raise Rejected(f"cannot download the source ({e.code})") from None
        except (urllib.error.URLError, TimeoutError, OSError) as e:
            raise Rejected(f"cannot download the source ({e})") from None
        if data is None:
            return None
        try:
            return data.decode("utf-8")
        except UnicodeDecodeError:
            raise Rejected("meetko.toml is not UTF-8") from None


def index_repo(gh: GitHub, repo: dict) -> dict:
    full = repo["full_name"]
    if repo.get("archived") or repo.get("disabled") or repo.get("fork") or repo.get("private"):
        raise Rejected("archived, disabled, private or a fork")
    if not isinstance(full, str) or not REPO_RE.match(full):
        raise Rejected("repo name is malformed")
    owner = full.split("/")[0]
    tag = gh.latest_release_tag(full)
    ref = tag or repo.get("default_branch") or "main"
    sha = gh.resolve(full, ref)
    if not sha:
        raise Rejected(f"cannot resolve {ref!r}")
    text = gh.manifest(full, sha)
    if text is None:
        raise Rejected("no meetko.toml at the repository root")
    return make_entry(repo, ref, sha, validate_manifest(text, owner))


def build(gh: GitHub, now: Callable[[], datetime] = lambda: datetime.now(timezone.utc)) -> tuple[dict, list[str]]:
    entries, notes = [], []
    for repo in gh.search_repos():
        try:
            entries.append(index_repo(gh, repo))
        except Rejected as e:
            notes.append(f"skipped {repo.get('full_name')}: {e}")
        except Exception as e:  # one hostile or broken repo must not take the whole run down
            notes.append(f"skipped {repo.get('full_name')}: unexpected {type(e).__name__}")
    plugins, dup = dedupe(entries)
    doc = {"generated": now().strftime("%Y-%m-%dT%H:%M:%SZ"), "plugins": plugins}
    return doc, notes + dup


def apply_blocklist(doc: dict, lines: list[str]) -> dict:
    """Drop entries named by the blocklist. Lines (comments after #, case-insensitive):
    `OWNER/REPO`, `OWNER/*` (every repo of an owner), or a 40-hex commit sha."""
    repos, owners, shas = set(), set(), set()
    for line in lines:
        item = line.split("#")[0].strip().lower()
        if not item:
            continue
        if SHA_RE.match(item):
            shas.add(item)
        elif item.endswith("/*"):
            owners.add(item[:-2])
        else:
            repos.add(item)

    def blocked(p: dict) -> bool:
        repo = p["repo"].lower()
        return repo in repos or repo.split("/")[0] in owners or p["sha"] in shas

    return {**doc, "plugins": [p for p in doc["plugins"] if not blocked(p)]}


def check_not_shrunk(previous: int, now: int, allow: bool) -> str | None:
    """An error text when the new index has less than half of the previous plugins (a broken
    run, not a real change), unless `allow`."""
    if allow or previous == 0 or now * 2 >= previous:
        return None
    return f"the new index has {now} plugins, the previous one {previous}: refusing to publish a much smaller index (use --allow-shrink if this is intended)"


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--out", default="index.json")
    ap.add_argument("--previous", help="the index published before; a run that loses more than half of it fails")
    ap.add_argument("--blocklist", help="file with OWNER/REPO, OWNER/* or commit sha per line")
    ap.add_argument("--allow-shrink", action="store_true", help="accept an index with less than half of the previous plugins")
    args = ap.parse_args(argv)
    try:
        doc, notes = build(GitHub(os.environ.get("GITHUB_TOKEN") or None))
    except FetchFailed as e:
        print(f"failed, nothing written: {e}", file=sys.stderr)
        return 1
    if args.blocklist and os.path.exists(args.blocklist):
        with open(args.blocklist, encoding="utf-8") as f:
            doc = apply_blocklist(doc, f.read().splitlines())
    errs = validate_index(doc)
    if errs:
        print("refusing to write an invalid index:", *errs, sep="\n  ", file=sys.stderr)
        return 1
    if args.previous and os.path.exists(args.previous):
        try:
            with open(args.previous, encoding="utf-8") as f:
                before = len(json.load(f)["plugins"])
        except (ValueError, KeyError, TypeError, OSError):
            before = 0
        why = check_not_shrunk(before, len(doc["plugins"]), args.allow_shrink)
        if why:
            print(why, file=sys.stderr)
            return 1
    with open(args.out, "w", encoding="utf-8") as f:
        json.dump(doc, f, ensure_ascii=False, indent=2)
        f.write("\n")
    for n in notes:
        print(n, file=sys.stderr)
    print(f"{len(doc['plugins'])} plugins -> {args.out}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
