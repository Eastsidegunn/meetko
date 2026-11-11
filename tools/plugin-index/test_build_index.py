"""Offline tests for the gallery index builder (fixture data, no network).

    python -I tools/plugin-index/test_build_index.py     # stdlib unittest
    python -I -m pytest tools/plugin-index               # also works
"""

import gzip
import http.server
import io
import json
import socketserver
import sys
import tarfile
import threading
import unittest
from datetime import datetime, timezone
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))  # -I drops the script dir from sys.path

import build_index as bi  # noqa: E402

GOOD = """
[package]
id = "acme/summary"
version = "1.2.0"
kind = "plugin"
contract = "0.1"
license = "MIT"
description = "Short summary"
homepage = "https://example.com/summary"

[run]
command = ["python3", "-I", "plugin.py"]
protocol = "stdio-jsonrpc"

[input]
audio = "full"

[permissions]
network = true
"""

SHA = "a" * 40
SHA2 = "b" * 40


def repo(full="acme/summary-plugin", stars=3, **kw):
    return {"full_name": full, "stargazers_count": stars, "pushed_at": "2026-10-30T01:02:03Z", "default_branch": "main", **kw}


def swap(old, new):
    assert old in GOOD
    return GOOD.replace(old, new)


class Validate(unittest.TestCase):
    def test_good_manifest(self):
        f = bi.validate_manifest(GOOD, "Acme")  # owner compare is case-insensitive
        self.assertEqual(f["id"], "acme/summary")
        self.assertEqual(f["name"], "summary")
        self.assertEqual(f["permissions"], {"network": True, "audio": True})
        self.assertEqual(f["runtime"], "python")

    def test_defaults_are_no_permissions(self):
        m = "\n".join(l for l in GOOD.splitlines() if not l.startswith(("audio", "network")))
        self.assertEqual(bi.validate_manifest(m, "acme")["permissions"], {"network": False, "audio": False})

    def test_rejections(self):
        bad = {
            "kind": swap('kind = "plugin"', 'kind = "engine"'),
            "contract": swap('contract = "0.1"', 'contract = "9.0"'),
            "protocol": swap("stdio-jsonrpc", "ws"),
            "no description": swap('description = "Short summary"', ""),
            "bad version": swap('version = "1.2.0"', 'version = "latest"'),
            "id shape": swap("acme/summary", "Acme Summary"),
            "other publisher": swap("acme/summary", "someone-else/summary"),
            "audio value": swap('audio = "full"', 'audio = "yes"'),
            "network type": swap("network = true", 'network = "yes"'),
            "http homepage": swap("https://example.com", "http://example.com"),
            "absolute command": swap('"python3", "-I", "plugin.py"', '"/usr/bin/evil"'),
            "dotdot command": swap('"python3", "-I", "plugin.py"', '"../evil"'),
            "empty command": swap('["python3", "-I", "plugin.py"]', "[]"),
            "no run": GOOD.split("[run]")[0],
            "not toml": "this is = = not toml",
            "too big": GOOD + "#" * 70000,
            "long description": swap("Short summary", "x" * 301),
        }
        for why, text in bad.items():
            with self.assertRaises(bi.Rejected, msg=why):
                bi.validate_manifest(text, "acme")

    def test_secrets_and_settings_are_carried_and_checked(self):
        with_keys = swap("network = true", 'network = true\nsecrets = ["ANTHROPIC_API_KEY", "OPENAI_API_KEY"]\nsettings = ["ANTHROPIC_MODEL"]')
        f = bi.validate_manifest(with_keys, "acme")
        self.assertEqual(f["permissions"], {"network": True, "audio": True, "secrets": ["ANTHROPIC_API_KEY", "OPENAI_API_KEY"], "settings": ["ANTHROPIC_MODEL"]})
        none = bi.validate_manifest(swap("network = true", "network = true\nsecrets = []\nsettings = []"), "acme")["permissions"]
        self.assertNotIn("secrets", none)
        self.assertNotIn("settings", none)
        bad = {
            "no network": swap("network = true", 'network = false\nsecrets = ["API_KEY"]'),
            "setting without network": swap("network = true", 'network = false\nsettings = ["A_MODEL"]'),
            "lowercase": swap("network = true", 'network = true\nsecrets = ["api_key"]'),
            "no key ending": swap("network = true", 'network = true\nsecrets = ["ANTHROPIC"]'),
            "key as setting": swap("network = true", 'network = true\nsettings = ["A_API_KEY"]'),
            "setting as key": swap("network = true", 'network = true\nsecrets = ["A_MODEL"]'),
            "reserved": swap("network = true", 'network = true\nsecrets = ["PATH"]'),
            "loader": swap("network = true", 'network = true\nsecrets = ["DYLD_INSERT_KEY"]'),
            "openssl": swap("network = true", 'network = true\nsecrets = ["OPENSSL_KEY"]'),
            "go": swap("network = true", 'network = true\nsecrets = ["GOPROXY_TOKEN"]'),
            "npm": swap("network = true", 'network = true\nsecrets = ["NPM_CONFIG_TOKEN"]'),
            "proxy": swap("network = true", 'network = true\nsecrets = ["HTTPS_PROXY"]'),
            "twice": swap("network = true", 'network = true\nsecrets = ["A_KEY", "A_KEY"]'),
            "too many": swap("network = true", 'network = true\nsecrets = ["A1_KEY", "A2_KEY", "A3_KEY", "A4_KEY", "A5_KEY"]'),
            "not a list": swap("network = true", 'network = true\nsecrets = "API_KEY"'),
            "not strings": swap("network = true", "network = true\nsecrets = [1]"),
        }
        for why, text in bad.items():
            with self.assertRaises(bi.Rejected, msg=why):
                bi.validate_manifest(text, "acme")
        self.assertTrue(bi.valid_secret_name("GOOGLE_API_KEY"))
        e = bi.make_entry(repo(), "v1", SHA, f)
        self.assertEqual(bi.validate_index({"generated": "x", "plugins": [e]}), [])
        ok = lambda perm: bi.validate_index({"generated": "x", "plugins": [{**e, "permissions": perm}]})
        self.assertTrue(ok({"network": True, "audio": False, "secrets": []}))
        self.assertTrue(ok({"network": True, "audio": False, "settings": []}))
        self.assertTrue(ok({"network": False, "audio": False, "secrets": ["API_KEY"]}))
        self.assertTrue(ok({"network": True, "audio": False, "secrets": ["path"]}))
        self.assertTrue(ok({"network": True, "audio": False, "settings": ["A_API_KEY"]}))
        self.assertEqual(ok({"network": True, "audio": False, "settings": ["A_MODEL"]}), [])

    def test_runtime_labels(self):
        self.assertEqual(bi._runtime(["target/release/x"]), "binary")
        self.assertEqual(bi._runtime(["node", "a.js"]), "node")
        self.assertEqual(bi._runtime(["python3.12"]), "python")
        self.assertEqual(bi._runtime(["bash", "x.sh"]), "shell")


class Index(unittest.TestCase):
    def entry(self, full, stars, pid="acme/summary"):
        f = bi.validate_manifest(GOOD.replace("acme/summary", pid), pid.split("/")[0])
        return bi.make_entry(repo(full, stars), "v1.2.0", SHA, f)

    def test_entry_shape_and_validity(self):
        e = self.entry("acme/summary-plugin", 3)
        self.assertEqual((e["repo"], e["ref"], e["sha"], e["stars"], e["updated"]), ("acme/summary-plugin", "v1.2.0", SHA, 3, "2026-10-30T01:02:03Z"))
        doc = {"generated": "2026-10-31T00:00:00Z", "plugins": [e]}
        self.assertEqual(bi.validate_index(doc), [])

    def test_dedupe_by_id_keeps_most_stars(self):
        a, b = self.entry("acme/one", 1), self.entry("acme/two", 9)
        out, notes = bi.dedupe([a, b])
        self.assertEqual([e["repo"] for e in out], ["acme/two"])
        self.assertEqual(len(notes), 1)

    def test_validate_index_catches_bad_documents(self):
        e = self.entry("acme/one", 1)
        ok = lambda p: bi.validate_index({"generated": "x", "plugins": [p]})
        self.assertEqual(ok(e), [])
        self.assertTrue(ok({**e, "sha": "abc"}))
        self.assertTrue(ok({**e, "permissions": {"network": 1, "audio": False}}))
        self.assertTrue(ok({**e, "extra": 1}))
        self.assertTrue(ok({k: v for k, v in e.items() if k != "ref"}))
        self.assertTrue(bi.validate_index({"generated": "x", "plugins": [e, e]}))
        self.assertTrue(bi.validate_index([]))

    def test_schema_file_agrees(self):
        schema = json.loads((HERE / "index.schema.json").read_text(encoding="utf-8"))
        plugin = schema["$defs"]["plugin"]
        self.assertEqual(set(plugin["required"]) | {"homepage", "path"}, set(plugin["properties"]))
        self.assertEqual(set(schema["required"]), {"generated", "plugins"})
        try:
            import jsonschema  # optional; not needed for the stdlib checks
        except ImportError:
            return
        e = self.entry("acme/one", 1)
        jsonschema.validate({"generated": "2026-10-31T00:00:00Z", "plugins": [e]}, schema)
        keyed = bi.make_entry(repo("acme/one", 1), "v1", SHA, bi.validate_manifest(swap("network = true", 'network = true\nsecrets = ["API_KEY"]\nsettings = ["A_MODEL"]'), "acme"))
        jsonschema.validate({"generated": "2026-10-31T00:00:00Z", "plugins": [keyed]}, schema)
        with self.assertRaises(jsonschema.ValidationError):
            jsonschema.validate({"generated": "2026-10-31T00:00:00Z", "plugins": [{**e, "sha": "zz"}]}, schema)


class Hostile(unittest.TestCase):
    """Manifests that used to crash the whole run, and other malformed input."""

    def test_wrong_types_are_rejections_not_crashes(self):
        bad = {
            "input is a string": 'input = "x"\n' + GOOD,
            "permissions is an int": 'permissions = 1\n' + GOOD.replace("[permissions]\nnetwork = true", ""),
            "audio is a list": swap('audio = "full"', 'audio = ["full"]'),
            "package is an int": "package = 1\n[run]\ncommand = ['a']\nprotocol = 'stdio-jsonrpc'\n",
            "run is a list": GOOD.replace("[run]", "[[run]]"),
            "command has ints": swap('["python3", "-I", "plugin.py"]', "[1, 2]"),
            "deep nesting": "z = " + "[" * 20000 + "]" * 20000 + "\n" + GOOD,
            "bidi in description": swap("Short summary", "Short \u202esummary"),
            "newline in description": swap('"Short summary"', '"Short\\nsummary"'),
            "control in command": swap('"plugin.py"', '"plug\\u0007in.py"'),
        }
        for why, text in bad.items():
            with self.assertRaises(bi.Rejected, msg=why):
                bi.validate_manifest(text, "acme")

    def test_invisible_separator_filler_selector_tag_and_format_characters_are_rejections(self):
        # same classes as crates/meetko-core/src/text.rs
        for cp in (0x2028, 0x2029, 0x115F, 0x1160, 0x3164, 0xFFA0, 0x34F, 0xFE0F, 0xE0101, 0xE0041, 0x600, 0x6DD, 0x70F, 0x8E2, 0x110BD, 0x13430, 0x1D173):
            with self.assertRaises(bi.Rejected, msg=hex(cp)):
                bi.validate_manifest(swap("Short summary", "Short " + chr(cp) + "summary"), "acme")
        with self.assertRaises(bi.Rejected, msg="mark run"):
            bi.validate_manifest(swap("Short summary", "Short e" + "\u0301" * 9), "acme")
        bi.validate_manifest(swap("Short summary", "Short e" + "\u0301" * 8 + " ok"), "acme")

    def test_a_trailing_newline_in_a_multiline_description_is_fine(self):
        f = bi.validate_manifest(swap('"Short summary"', '"""Short summary\n"""'), "acme")
        self.assertEqual(f["description"], "Short summary")

    def test_build_skips_a_repo_that_blows_up_and_keeps_the_rest(self):
        class Exploding(FakeGitHub):
            def manifest(self, full, sha):
                if full == "acme/boom":
                    raise ValueError("surprise")
                return super().manifest(full, sha)

        gh = Exploding([repo("acme/boom", 9), repo("acme/fine", 1)], tags={"acme/boom": "v1.2.0", "acme/fine": "v1.2.0"}, manifests={("acme/fine", SHA): GOOD})
        doc, notes = bi.build(gh)
        self.assertEqual([p["repo"] for p in doc["plugins"]], ["acme/fine"])
        self.assertTrue(any("acme/boom" in n and "ValueError" in n for n in notes), notes)


def make_tar(members, *, compress=True):
    """members: (name, bytes) or (name, bytes, TarInfo tweaks dict). Returns .tar.gz bytes."""
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w:gz" if compress else "w", format=tarfile.PAX_FORMAT) as tf:
        for name, data, *rest in members:
            info = tarfile.TarInfo(name)
            info.size = len(data)
            for k, v in (rest[0] if rest else {}).items():
                setattr(info, k, v)
            tf.addfile(info, io.BytesIO(data))
    return buf.getvalue()


class Tarball(unittest.TestCase):
    def test_reads_the_manifest_at_the_top_level_only(self):
        data = make_tar([("acme-x-abc/", b"", {"type": tarfile.DIRTYPE}), ("acme-x-abc/sub/meetko.toml", b"nested"), ("acme-x-abc/meetko.toml", GOOD.encode()), ("acme-x-abc/plugin.py", b"x")])
        self.assertEqual(bi.manifest_from_tarball(io.BytesIO(data)), GOOD.encode())
        self.assertIsNone(bi.manifest_from_tarball(io.BytesIO(make_tar([("t/readme", b"hi")]))))

    def test_odd_manifest_entries_are_rejected(self):
        with self.assertRaises(bi.Rejected):  # twice
            bi.manifest_from_tarball(io.BytesIO(make_tar([("t/meetko.toml", b"a"), ("t/meetko.toml", b"b")])))
        with self.assertRaises(bi.Rejected):  # a symlink
            bi.manifest_from_tarball(io.BytesIO(make_tar([("t/meetko.toml", b"", {"type": tarfile.SYMTYPE, "linkname": "/etc/passwd"})])))
        with self.assertRaises(bi.Rejected):  # too big
            bi.manifest_from_tarball(io.BytesIO(make_tar([("t/meetko.toml", b"#" * 70000)])))
        with self.assertRaises(bi.Rejected):  # not a gzip
            bi.manifest_from_tarball(io.BytesIO(b"definitely not gzip"))

    def test_decompression_bombs_are_refused_without_reading_them(self):
        # a PAX header claiming 2 GiB, then zeros: tarfile would try to read it all into memory
        header = tarfile.TarInfo("PaxHeaders/x")
        header.type, header.size = tarfile.XHDTYPE, 2 << 30
        out = io.BytesIO()
        with gzip.GzipFile(fileobj=out, mode="wb", compresslevel=9) as z:
            z.write(header.tobuf(tarfile.USTAR_FORMAT))
            for _ in range(32):
                z.write(bytes(1 << 20))
        self.assertLess(len(out.getvalue()), 1 << 20)
        with self.assertRaisesRegex(bi.Rejected, "extension entry claims"):
            bi.manifest_from_tarball(io.BytesIO(out.getvalue()), max_unpacked=8 << 20)
        # plain data far past the unpacked cap, and a compressed stream past the download cap
        big = io.BytesIO()
        with gzip.GzipFile(fileobj=big, mode="wb", compresslevel=9) as z:
            filler = tarfile.TarInfo("t/f")
            filler.size = 32 << 20
            z.write(filler.tobuf(tarfile.USTAR_FORMAT))
            for _ in range(32):
                z.write(bytes(1 << 20))
        with self.assertRaisesRegex(bi.Rejected, "unpacked archive is larger"):
            bi.manifest_from_tarball(io.BytesIO(big.getvalue()), max_unpacked=4 << 20)
        import os
        with self.assertRaisesRegex(bi.Rejected, "download is larger"):
            bi.manifest_from_tarball(io.BytesIO(make_tar([("t/a", os.urandom(50000))])), max_download=2000)
        # many entries
        old = bi.MAX_MEMBERS
        bi.MAX_MEMBERS = 3
        try:
            with self.assertRaisesRegex(bi.Rejected, "more than 3 entries"):
                bi.manifest_from_tarball(io.BytesIO(make_tar([(f"t/f{i}", b"") for i in range(6)])))
        finally:
            bi.MAX_MEMBERS = old


class _Quiet(http.server.HTTPServer):
    def server_bind(self):  # HTTPServer's own bind does a reverse-DNS lookup that can take 30 s offline
        socketserver.TCPServer.server_bind(self)
        self.server_name, self.server_port = "localhost", self.socket.getsockname()[1]


class Desync(unittest.TestCase):
    """Framing the tarfile module and the watcher must agree on."""

    def test_base256_sizes_and_pax_size_records_are_refused(self):
        h = bytearray(tarfile.TarInfo("t/f").tobuf(tarfile.USTAR_FORMAT))
        h[124:136] = bytes([0x80]) + bytes(10) + bytes([5])
        h[148:156] = b"        "
        h[148:156] = b"%06o\0 " % sum(h)
        with self.assertRaisesRegex(bi.Rejected, "base-256"):
            bi.manifest_from_tarball(io.BytesIO(gzip.compress(bytes(h) + bytes(1024))))
        pax = tarfile.TarInfo("t/meetko.toml")
        pax.pax_headers = {"size": "0"}
        pax.size = 3
        buf = io.BytesIO()
        with tarfile.open(fileobj=buf, mode="w:gz", format=tarfile.PAX_FORMAT) as tf:
            tf.addfile(pax, io.BytesIO(b"abc"))
        with self.assertRaisesRegex(bi.Rejected, "sets the entry size"):
            bi.manifest_from_tarball(io.BytesIO(buf.getvalue()))
        ok = tarfile.TarInfo("t/meetko.toml")
        ok.pax_headers = {"comment": "hi"}
        ok.size = 3
        buf = io.BytesIO()
        with tarfile.open(fileobj=buf, mode="w:gz", format=tarfile.PAX_FORMAT) as tf:
            tf.addfile(ok, io.BytesIO(b"abc"))
        self.assertEqual(bi.manifest_from_tarball(io.BytesIO(buf.getvalue())), b"abc")


class Serve:
    """A local stand-in for codeload that records what it was asked."""

    def __init__(self, tarball):
        outer = self
        self.requests = []

        class H(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                outer.requests.append((self.path, dict(self.headers)))
                if self.path == f"/acme/x/tar.gz/{SHA}":
                    self.send_response(200)
                    self.end_headers()
                    self.wfile.write(tarball)
                else:
                    self.send_response(404)
                    self.end_headers()

            def log_message(self, *a):
                pass

        self.srv = _Quiet(("127.0.0.1", 0), H)
        self.base = f"http://127.0.0.1:{self.srv.server_port}"
        threading.Thread(target=self.srv.serve_forever, daemon=True).start()

    def close(self):
        self.srv.shutdown()


class ExportSubst(unittest.TestCase):
    def test_the_manifest_is_the_one_in_the_tarball_users_install(self):
        # the Contents API would return the raw blob ("version = 1.0.0"); the archive has the
        # export-subst-expanded text. The index must describe the archive's.
        expanded = GOOD.replace('version = "1.2.0"', 'version = "6.6.6"').replace("network = true", "network = false")
        s = Serve(make_tar([("acme-x-abc/meetko.toml", expanded.encode())]))
        try:
            gh = bi.GitHub(token="secret-token", codeload=s.base)
            text = gh.manifest("acme/x", SHA)
            facts = bi.validate_manifest(text, "acme")
            self.assertEqual((facts["version"], facts["permissions"]["network"]), ("6.6.6", False))
            self.assertEqual([p for p, _ in s.requests], [f"/acme/x/tar.gz/{SHA}"], "no Contents API read")
            self.assertTrue(all("Authorization" not in h for _, h in s.requests), "the token stays off the download host")
            with self.assertRaises(bi.Rejected):
                gh.manifest("acme/x", SHA2)  # 404
        finally:
            s.close()

    def test_the_token_is_unredirected(self):
        req = bi.GitHub(token="t").request("/x", "application/json")
        self.assertEqual(req.unredirected_hdrs.get("Authorization"), "Bearer t")
        self.assertNotIn("Authorization", req.headers)


class Run(unittest.TestCase):
    def test_a_failed_search_fails_the_run(self):
        class Down(bi.GitHub):
            def get(self, path, accept="", *, limit=0):
                return 500, b""

        with self.assertRaises(bi.FetchFailed):
            Down().search_repos()

        class NotJson(bi.GitHub):
            def get(self, path, accept="", *, limit=0):
                return 200, b"<html>"

        with self.assertRaises(bi.FetchFailed):
            NotJson().search_repos()

    def test_shrink_guard(self):
        self.assertIsNone(bi.check_not_shrunk(10, 5, False))
        self.assertIsNone(bi.check_not_shrunk(0, 0, False))
        self.assertIn("much smaller", bi.check_not_shrunk(10, 4, False))
        self.assertIn("much smaller", bi.check_not_shrunk(1, 0, False))
        self.assertIsNone(bi.check_not_shrunk(10, 0, True))

    def test_blocklist_entries(self):
        def p(repo, sha):
            return {"repo": repo, "sha": sha}

        doc = {"generated": "x", "plugins": [p("acme/a", SHA), p("Evil/b", SHA2), p("evil/c", "c" * 40), p("fine/d", "d" * 40)]}
        out = bi.apply_blocklist(doc, ["ACME/A  # one repo", "evil/*", "", "# comment", "d" * 40])
        self.assertEqual(out["plugins"], [])
        out = bi.apply_blocklist(doc, ["evil/*"])
        self.assertEqual([x["repo"] for x in out["plugins"]], ["acme/a", "fine/d"])
        self.assertEqual([x["repo"] for x in bi.apply_blocklist(doc, [SHA2])["plugins"]], ["acme/a", "evil/c", "fine/d"])


class FakeGitHub(bi.GitHub):
    """Stands in for the network: repos, release tags, commits and manifests are dicts."""

    def __init__(self, repos, tags=None, manifests=None):
        super().__init__()
        self._repos, self._tags, self._manifests = repos, tags or {}, manifests or {}

    def search_repos(self):
        return self._repos

    def latest_release_tag(self, full):
        return self._tags.get(full)

    def resolve(self, full, ref):
        return {"v1.2.0": SHA, "main": SHA2}.get(ref)

    def manifest(self, full, sha, path=""):
        return self._manifests.get((full, sha, path) if path else (full, sha))

    def json(self, path):
        full = path.removeprefix("/repos/")
        return next((r for r in self._repos + getattr(self, "_extra", []) if r["full_name"].lower() == full.lower()), None)


class Build(unittest.TestCase):
    def test_build_end_to_end_with_fixtures(self):
        gh = FakeGitHub(
            [
                repo("acme/released", 5),
                repo("acme/branch-only", 1),
                repo("acme/no-manifest", 7),
                repo("acme/archived", 9, archived=True),
                repo("acme/forked", 9, fork=True),
                repo("evil/impostor", 50),
            ],
            tags={"acme/released": "v1.2.0"},
            manifests={
                ("acme/released", SHA): GOOD,
                ("acme/branch-only", SHA2): GOOD.replace("acme/summary", "acme/other"),
                ("evil/impostor", SHA2): GOOD,  # claims acme's id from another owner
            },
        )
        doc, notes = bi.build(gh, now=lambda: datetime(2026, 10, 31, tzinfo=timezone.utc))
        self.assertEqual(doc["generated"], "2026-10-31T00:00:00Z")
        self.assertEqual([(p["repo"], p["ref"], p["sha"]) for p in doc["plugins"]], [("acme/released", "v1.2.0", SHA), ("acme/branch-only", "main", SHA2)])
        self.assertEqual(bi.validate_index(doc), [])
        text = "\n".join(notes)
        for why in ("no-manifest: no meetko.toml", "archived", "forked", "impostor: package.id publisher"):
            self.assertIn(why, text)

    def test_main_refuses_to_write_nothing_invalid(self):
        self.assertEqual(bi.validate_index({"generated": "x", "plugins": [{"id": "nope"}]})[0], "plugins[0]: wrong keys")


class Subfolders(unittest.TestCase):
    def test_valid_subpath_mirrors_the_app(self):
        for ok in ("a", "summary-py", "plugins/x_y.z", "/".join(["x"] * 8), "y" * 99 + "/" + "y" * 100, "_x", "-x"):
            self.assertTrue(bi.valid_subpath(ok), ok)
        for bad in ("", "/a", "a/", "a//b", "../a", "a/..", "./a", ".github", "a\\b", "a b", "a:b", "a./b", "con", "x/NUL.txt", "lpt1",
                    "é", "/".join(["x"] * 9), "z" * 101, "y" * 100 + "/" + "y" * 100, 5, None):
            self.assertFalse(bi.valid_subpath(bad), bad)

    def test_the_manifest_is_read_from_the_folder(self):
        data = make_tar([("t/meetko.toml", b"root"), ("t/a/meetko.toml", b"folder a"), ("t/a/b/meetko.toml", b"deeper"), ("t/ab/meetko.toml", b"other")])
        self.assertEqual(bi.manifest_from_tarball(io.BytesIO(data), "a"), b"folder a")
        self.assertEqual(bi.manifest_from_tarball(io.BytesIO(data), "a/b"), b"deeper")
        self.assertEqual(bi.manifest_from_tarball(io.BytesIO(data)), b"root")
        self.assertIsNone(bi.manifest_from_tarball(io.BytesIO(data), "c"))
        with self.assertRaises(bi.Rejected):
            bi.manifest_from_tarball(io.BytesIO(data), "../a")
        with self.assertRaises(bi.Rejected):  # a symlinked manifest in the folder
            bi.manifest_from_tarball(io.BytesIO(make_tar([("t/a/meetko.toml", b"", {"type": tarfile.SYMTYPE, "linkname": "/etc/passwd"})])), "a")

    def test_sources_file_shape(self):
        rows = bi.load_sources(json.dumps({"plugins": [{"repo": "acme/cat", "path": "a"}, {"repo": "acme/x", "ref": "v1"}], "unlisted": {"b": "needs a build"}}), catalog="Acme/Cat")
        self.assertEqual(rows, [{"repo": "acme/cat", "path": "a", "ref": None}, {"repo": "acme/x", "path": "", "ref": "v1"}])
        for bad in ("[]", "{}", '{"plugins": [{"repo": "acme"}]}', '{"plugins": [{"repo": "acme/x", "path": "../y"}]}', '{"plugins": [{"repo": "acme/x", "ref": "pull/1/head"}]}',
                    '{"plugins": [{"repo": "acme/x", "extra": 1}]}', '{"plugins": [{"repo": "acme/x"}, {"repo": "ACME/x"}]}', "not json"):
            with self.assertRaises(bi.FetchFailed, msg=bad):
                bi.load_sources(bad)

    def test_third_party_rows_must_pin_a_ref(self):
        unpinned = json.dumps({"plugins": [{"repo": "alice/x"}]})
        with self.assertRaises(bi.FetchFailed) as e:
            bi.load_sources(unpinned)
        self.assertIn("needs a ref", str(e.exception))
        self.assertEqual(bi.load_sources(json.dumps({"plugins": [{"repo": "alice/x", "ref": "v1.0.0"}]}))[0]["ref"], "v1.0.0")
        # the catalog's own rows may follow its default branch
        own = bi.load_sources(json.dumps({"plugins": [{"repo": "eastsidegunn/MEETKO-plugins", "path": "a"}]}))
        self.assertIsNone(own[0]["ref"])
        self.assertIsNone(bi.load_sources(unpinned, catalog="alice/x")[0]["ref"])

    def test_build_from_sources_with_folders(self):
        gh = FakeGitHub(
            [repo("acme/catalog", 2), repo("acme/solo", 1), repo("acme/searched", 99)],
            tags={"acme/solo": "v1.2.0"},
            manifests={
                ("acme/catalog", SHA2, "a"): GOOD.replace("acme/summary", "acme/a"),
                ("acme/catalog", SHA2, "b/c"): GOOD.replace("acme/summary", "acme/c"),
                ("acme/solo", SHA): GOOD.replace("acme/summary", "acme/solo"),
                ("acme/searched", SHA2): GOOD.replace("acme/summary", "acme/a"),  # same id as a listed one
                ("evil/catalog", SHA2, "a"): GOOD.replace("acme/summary", "acme/z"),
            },
        )
        gh._extra = [repo("evil/catalog", 50)]
        sources = bi.load_sources(catalog="acme/catalog", text=json.dumps({"plugins": [
            {"repo": "acme/catalog", "path": "a"}, {"repo": "acme/catalog", "path": "b/c"}, {"repo": "acme/catalog", "path": "missing"},
            {"repo": "acme/solo", "ref": "v1.2.0"}, {"repo": "acme/gone", "ref": "v1"}, {"repo": "evil/catalog", "path": "a", "ref": "main"},
        ]}))
        doc, notes = bi.build(gh, sources=sources)
        rows = [(p["id"], p["repo"], p.get("path"), p["ref"], p["sha"]) for p in doc["plugins"]]
        self.assertEqual(sorted(rows), sorted([("acme/a", "acme/catalog", "a", "main", SHA2), ("acme/c", "acme/catalog", "b/c", "main", SHA2), ("acme/solo", "acme/solo", None, "v1.2.0", SHA)]))
        self.assertEqual(bi.validate_index(doc), [])
        text = "\n".join(notes)
        for why in ("acme/catalog/missing: no meetko.toml in missing/", "acme/gone: repository not found", "evil/catalog/a: package.id publisher", "acme/searched: id acme/a is already listed from acme/catalog/a"):
            self.assertIn(why, text)
        only, _ = bi.build(gh, sources=sources, search=False)
        self.assertEqual(len(only["plugins"]), 3)
        # an entry's path is checked; a folder can be blocked on its own
        e = doc["plugins"][0]
        self.assertTrue(bi.validate_index({"generated": "x", "plugins": [{**e, "path": "../x"}]}))
        blocked = bi.apply_blocklist(doc, ["acme/catalog/a"])
        self.assertEqual(sorted(p["id"] for p in blocked["plugins"]), ["acme/c", "acme/solo"])
        self.assertEqual([p["id"] for p in bi.apply_blocklist(doc, ["acme/catalog"])["plugins"]], ["acme/solo"])


if __name__ == "__main__":
    unittest.main()
