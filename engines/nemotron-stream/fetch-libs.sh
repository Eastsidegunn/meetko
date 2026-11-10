#!/bin/sh
# Fetch the official NeMo-Speech.cpp prebuilt libraries (Apache-2.0, with llama.cpp/ggml, MIT) into vendor/.
# The archive is pinned by sha256: a download that does not match is never unpacked. On macOS the
# unpacked libraries are pinned too and re-checked on every run (including the "already installed"
# path); on Linux/Windows only the archive pin applies (those platforms are not exercised yet).
# usage: engines/nemotron-stream/fetch-libs.sh
#   MEETKO_NEMO_BACKEND=vulkan selects the Windows Vulkan build (default: cpu)
set -eu

VERSION=0.2.0
LIBS=""
case "$(uname -s)-$(uname -m)" in
  Darwin-arm64)
    PLATFORM=macos-aarch64-metal; EXT=tar.gz
    ARCHIVE_SHA256=5cb02ba7c04f0b5585ce5cde9c830be5f0b7dfb4c83083c500109c381b4f2da9
    # soname-form names (the libraries load each other as @rpath/<soname>); asr_c is also kept under its link name
    LIBS="libnemo_speech_asr_c.1.dylib:9616b27281458327b38cbfef2bf04faf631aac6eb136ff2f75bcdc54d909d859 libnemo_speech_asr_c.dylib:9616b27281458327b38cbfef2bf04faf631aac6eb136ff2f75bcdc54d909d859 libnemo_speech_asr.dylib:19cd54d0c86b5ee65af8b6a3cfa84ef9f77579a330e650798ed7d63898c33ace libnemo_speech_text_normalization.dylib:cdf3b15b94283aabff1ec8c1f68debaff6c5f939c7d19ff2cab454c58e0eb3ba libggml.0.dylib:7204eb88974620d3c5ca4b89b4c7d63dc22319ddb7c28a5fcc90a65a0d7d55f6 libggml-base.0.dylib:38c4380d3ddc9faca32b15b91c878b5da89af63270ed2ce2e33fd73ef601cf67 libggml-cpu.0.dylib:84dce88e2cee84c8cec546fb508f536438fc01f73ee83bc423227b2555e8cf63 libggml-blas.0.dylib:8b66e983da6952342e7e4e70062897dafe3e06696545a0910cf775417d6d3384 libggml-metal.0.dylib:abf8143d5d986d0263225d5883a0ad00b724bd73cc5ca988b5d49db1f4d0a786"
    ;;
  Linux-x86_64)
    PLATFORM=linux-x86_64-cpu; EXT=tar.gz
    ARCHIVE_SHA256=f396057150f1b774935c7414fd32ebc04195a795a34120d78d8c9ecefa1b7507
    ;;
  MINGW*-x86_64|MSYS*-x86_64)
    EXT=zip
    if [ "${MEETKO_NEMO_BACKEND:-cpu}" = vulkan ]; then
      PLATFORM=windows-x86_64-vulkan; ARCHIVE_SHA256=edf15a04ba98740aa0ab75ae4681ac7ad0a488b22b35ce2cbebd10590bb1c1e1
    else
      PLATFORM=windows-x86_64-cpu; ARCHIVE_SHA256=82c80451086e86194aba9af19f800da77cd66d1c28d993d0efceffd6cdb49bb8
    fi
    ;;
  *) echo "no NeMo-Speech.cpp ${VERSION} prebuilt for $(uname -s) $(uname -m)" >&2; exit 1 ;;
esac
ARCHIVE="nemo-speech-${VERSION}-${PLATFORM}.${EXT}"
URL="https://github.com/NVIDIA/NeMo-Speech.cpp/releases/download/v${VERSION}/${ARCHIVE}"

here=$(cd "$(dirname "$0")" && pwd)
vendor="$here/vendor"

sha() { shasum -a 256 "$1" | cut -d' ' -f1; }
fetch() { curl -fsS --proto '=https' --proto-redir '=https' -L -o "$2" "$1"; }

verify_libs() {
  [ -n "$LIBS" ] || return 0
  for e in $LIBS; do
    f="$vendor/lib/${e%%:*}"
    [ -f "$f" ] && [ ! -L "$f" ] && [ "$(sha "$f")" = "${e#*:}" ] || return 1
  done
}

if [ -f "$vendor/.version" ] && [ "$(cat "$vendor/.version")" = "$VERSION" ] \
   && [ "$(cat "$vendor/.archive-sha256" 2>/dev/null)" = "$ARCHIVE_SHA256" ] && [ -f "$vendor/licenses/nemo-speech/NOTICE" ] && [ -f "$vendor/diar.h" ] && verify_libs; then
  echo "NeMo-Speech.cpp $VERSION is already in vendor/ and matches its pinned hashes"
  exit 0
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
fetch "$URL" "$tmp/$ARCHIVE"
if [ "$(sha "$tmp/$ARCHIVE")" != "$ARCHIVE_SHA256" ]; then
  echo "sha256 mismatch for $ARCHIVE" >&2
  exit 1
fi
mkdir "$tmp/x"
if [ "$EXT" = zip ]; then
  # refuse absolute paths and parent references before anything is written
  if unzip -Z1 "$tmp/$ARCHIVE" | grep -Eq '^/|(^|/)\.\.(/|$)'; then echo "the archive lists an absolute or parent path; refusing to unpack" >&2; exit 1; fi
  unzip -q "$tmp/$ARCHIVE" -d "$tmp/x"
else
  if tar -tzf "$tmp/$ARCHIVE" | grep -Eq '^/|(^|/)\.\.(/|$)'; then echo "the archive lists an absolute or parent path; refusing to unpack" >&2; exit 1; fi
  tar -xzf "$tmp/$ARCHIVE" -C "$tmp/x"
fi
src=$(find "$tmp/x" -maxdepth 1 -mindepth 1 -type d | head -n 1)
# the libraries link to each other through short names that are symlinks in the archive; only
# bare file names in the same directory are followed (cp below follows them)
for l in $(find "$src" -type l); do
  case "$(readlink "$l")" in */*) echo "the archive holds a symlink that leaves its directory; refusing" >&2; exit 1 ;; esac
done
rm -rf "$vendor"
mkdir -p "$vendor/lib" "$vendor/licenses"
case "$PLATFORM" in
  macos-*) for e in $LIBS; do cp -X "$src/lib/${e%%:*}" "$vendor/lib/"; done ;;
  linux-*) for f in "$src"/lib/*.so*; do cp "$f" "$vendor/lib/"; done ;;
  windows-*) cp "$src"/bin/*.dll "$src"/lib/nemo_speech_asr_c.lib "$vendor/lib/" ;;
esac
if ! verify_libs; then
  echo "a library in the archive does not match its pinned sha256" >&2
  rm -rf "$vendor"
  exit 1
fi
# the whole license tree ships inside the (pinned) archive: the libraries have SentencePiece, OpenFst,
# re2, protobuf and more linked in, each with its text under third_party/
cp -R "$src/share/licenses/nemo-speech" "$vendor/licenses/nemo-speech"
# the Rust bindings in src/ffi.rs and src/diar.rs are written against these headers
cp "$src/include/nemo_speech/asr.h" "$src/include/nemo_speech/diar.h" "$vendor/"
echo "$ARCHIVE_SHA256" > "$vendor/.archive-sha256"
echo "$VERSION" > "$vendor/.version"
echo "NeMo-Speech.cpp $VERSION ($PLATFORM) installed in vendor/ (archive verified)"
