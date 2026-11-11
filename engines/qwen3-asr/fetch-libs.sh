#!/bin/sh
# Fetch the official CrispASR prebuilt shared libraries (MIT, with ggml, MIT) into vendor/.
# The archive is pinned by sha256: a download that does not match is never unpacked. The libraries
# kept from it, the headers the hand-written FFI follows and the license texts are pinned too and
# re-checked on every run (including the "already installed" path).
# usage: engines/qwen3-asr/fetch-libs.sh   (macOS arm64 only for now)
#
# Windows: the same release has libcrispasr-windows-x86_64.tar.gz
# (sha256 fd0ed79492c999f399779060add6abacbaabd3c0981ada94f3eaed6e1ce7f312). It is not unpacked here
# because the engine has not been built or run on Windows yet.
set -eu

VERSION=0.8.53
case "$(uname -s)-$(uname -m)" in
  Darwin-arm64)
    ARCHIVE=libcrispasr-macos-arm64.tar.gz
    ARCHIVE_SHA256=b00fc140fc475bdcaa32903e1b264aa471e5df68466cab1f122e161ce53299d5
    ;;
  *) echo "no pinned CrispASR $VERSION libraries for $(uname -s) $(uname -m) yet" >&2; exit 1 ;;
esac
URL="https://github.com/CrispStrobe/CrispASR/releases/download/v${VERSION}/${ARCHIVE}"
# <file in the archive's lib/>:<name in vendor/lib, the install name the libraries load each other by>:<sha256>
LIBS="libcrispasr.0.8.53.dylib:libcrispasr.1.dylib:6105f82b6d80b9465666b8b6e73d6dd752588e9cb1d1b72a8e35879e280544a5
libggml.0.26.0.dylib:libggml.0.dylib:ea3464aa0086b96ba49b79b52f1315763fa976eef66c0998cd1f762ece117bd3
libggml-base.0.26.0.dylib:libggml-base.0.dylib:3f86673cf9f9045e403cc358e14237bc4cc95a16d4f4072fc9ffdcffb95df7eb
libggml-cpu.0.26.0.dylib:libggml-cpu.0.dylib:5b67999c55912f49fd43faa4247fc7c845e57825978beeea85f1c77b5303380d
libggml-blas.0.26.0.dylib:libggml-blas.0.dylib:7784c386f09e59ebc0d812d6166f39c590bd07d4b5691587422f67d2625acba1
libggml-metal.0.26.0.dylib:libggml-metal.0.dylib:fd714f4299af1a508ddca912cc4873bca435ffa4a28a44ad0a29fbbb67efeb50"
# the C API the FFI in src/ffi.rs follows, and the license texts that must ship with the libraries
FILES="include/crispasr_session.h:crispasr_session.h:2756c8c122ac562c37eb9cfc1b6e6fe0de9f36819495d2073c2a4d69f4f33ee0
include/crispasr.h:crispasr.h:330814a2d4b64df0ffb7257950a7c2d273710db2bba142ec3bc72eab64ab5156
LICENSE:licenses/crispasr-LICENSE:aa918fb53ff1948e12cdae8136e045210fb6db222a1f6f0c14bad48e6de2100e
THIRD_PARTY_NOTICES.txt:licenses/crispasr-THIRD_PARTY_NOTICES.txt:57ce6c4c433489e76ed9e85b425a1f085a2e3d126d628dc60f9e4d4303b18ae0"

here=$(cd "$(dirname "$0")" && pwd)
vendor="$here/vendor"

sha() { shasum -a 256 "$1" | cut -d' ' -f1; }
fetch() { curl -fsS --proto '=https' --proto-redir '=https' -L -o "$2" "$1"; }

verify() {
  for e in $LIBS; do
    rest=${e#*:}; f="$vendor/lib/${rest%%:*}"
    [ -f "$f" ] && [ ! -L "$f" ] && [ "$(sha "$f")" = "${rest#*:}" ] || return 1
  done
  for e in $FILES; do
    rest=${e#*:}; f="$vendor/${rest%%:*}"
    [ -f "$f" ] && [ ! -L "$f" ] && [ "$(sha "$f")" = "${rest#*:}" ] || return 1
  done
}

if [ -f "$vendor/.version" ] && [ "$(cat "$vendor/.version")" = "$VERSION" ] \
   && [ "$(cat "$vendor/.archive-sha256" 2>/dev/null)" = "$ARCHIVE_SHA256" ] && verify; then
  echo "CrispASR $VERSION is already in vendor/ and matches its pinned hashes"
  exit 0
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
fetch "$URL" "$tmp/$ARCHIVE"
if [ "$(sha "$tmp/$ARCHIVE")" != "$ARCHIVE_SHA256" ]; then
  echo "sha256 mismatch for $ARCHIVE" >&2
  exit 1
fi
# refuse absolute paths and parent references before anything is written
if tar -tzf "$tmp/$ARCHIVE" | grep -Eq '^/|(^|/)\.\.(/|$)'; then
  echo "the archive lists an absolute or parent path; refusing to unpack" >&2
  exit 1
fi
mkdir "$tmp/x"
tar -xzf "$tmp/$ARCHIVE" -C "$tmp/x"
src="$tmp/x/libcrispasr-macos-arm64"
# only regular files are copied (the archive's symlinks are not followed): each one under the name the
# libraries load it by
copy() {
  from="$src/$1"
  if [ ! -f "$from" ] || [ -L "$from" ]; then
    echo "$1 is missing in the archive or is not a regular file; refusing" >&2
    rm -rf "$vendor"
    exit 1
  fi
  mkdir -p "$(dirname "$vendor/$2")"
  cp -X "$from" "$vendor/$2"
}
rm -rf "$vendor"
mkdir -p "$vendor/lib" "$vendor/licenses"
for e in $LIBS; do rest=${e#*:}; copy "lib/${e%%:*}" "lib/${rest%%:*}"; done
for e in $FILES; do rest=${e#*:}; copy "${e%%:*}" "${rest%%:*}"; done
if ! verify; then
  echo "a file in the archive does not match its pinned sha256" >&2
  rm -rf "$vendor"
  exit 1
fi
echo "$ARCHIVE_SHA256" > "$vendor/.archive-sha256"
echo "$VERSION" > "$vendor/.version"
echo "CrispASR $VERSION installed in vendor/ (archive, libraries, headers and license texts verified)"
