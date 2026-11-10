#!/bin/sh
# Fetch the official sherpa-onnx prebuilt shared libraries (Apache-2.0) into vendor/.
# Everything is pinned by sha256: a download that does not match is never unpacked, and the
# unpacked libraries are re-checked on every run (including the "already installed" path).
# usage: engines/sherpa-stream/fetch-libs.sh   (macOS universal2 only for now)
set -eu

VERSION=2.0.1
ORT_VERSION=1.28.2
ARCHIVE="sherpa-onnx-v${VERSION}-osx-universal2-shared-no-tts-lib.tar.bz2"
ARCHIVE_SHA256=242b0593a033456e9344a76a11713d1c55da794d0e37eabf6acabaf7beeb2b36
URL="https://github.com/k2-fsa/sherpa-onnx/releases/download/v${VERSION}/${ARCHIVE}"
# the archive carries no license files: the upstream texts are fetched by tag and pinned too
SHERPA_LICENSE_SHA256=cfc7749b96f63bd31c3c42b5c471bf756814053e847c10f3eb003417bc523d30
ORT_LICENSE_SHA256=2f07c72751aed99790b8a4869cf2311df85a860b22ded05fa22803587a48922c
ORT_NOTICES_SHA256=0e07b95f3a8d6230037707c5c4a2b554d12c4cb67369669ac255635528ffcee2
LIBS="libonnxruntime.dylib:0181c2458590a90540a64ec218a78c9a4879f55747349253e54fa97d0d8de7eb libsherpa-onnx-c-api.dylib:c260f78e7ae485738bc4fe826a92a02f711c72534b2147a6fc62b814d7cee5ce libsherpa-onnx-cxx-api.dylib:57bbfdebb02af5611932058c71d7accc1396c7368956649dad85727d3f2dd71d"

here=$(cd "$(dirname "$0")" && pwd)
vendor="$here/vendor"

sha() { shasum -a 256 "$1" | cut -d' ' -f1; }
fetch() { curl -fsS --proto '=https' --proto-redir '=https' -L -o "$2" "$1"; }

verify_libs() {
  for e in $LIBS; do
    f="$vendor/lib/${e%%:*}"
    [ -f "$f" ] && [ ! -L "$f" ] && [ "$(sha "$f")" = "${e#*:}" ] || return 1
  done
}

if [ -f "$vendor/.version" ] && [ "$(cat "$vendor/.version")" = "$VERSION" ] \
   && [ "$(cat "$vendor/.archive-sha256" 2>/dev/null)" = "$ARCHIVE_SHA256" ] && verify_libs; then
  echo "sherpa-onnx $VERSION is already in vendor/ and matches its pinned hashes"
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
if tar -tjf "$tmp/$ARCHIVE" | grep -Eq '^/|(^|/)\.\.(/|$)'; then
  echo "the archive lists an absolute or parent path; refusing to unpack" >&2
  exit 1
fi
mkdir "$tmp/x"
tar -xjf "$tmp/$ARCHIVE" -C "$tmp/x"
src=$(find "$tmp/x" -maxdepth 1 -mindepth 1 -type d | head -n 1)
# regular files only: a symlink in the archive would be followed by cp
if [ -n "$(find "$src/lib" ! -type f ! -type d)" ]; then
  echo "the archive's lib/ holds something other than regular files; refusing" >&2
  exit 1
fi
rm -rf "$vendor"
mkdir -p "$vendor/lib" "$vendor/licenses"
for e in $LIBS; do cp -X "$src/lib/${e%%:*}" "$vendor/lib/"; done
if ! verify_libs; then
  echo "a library in the archive does not match its pinned sha256" >&2
  rm -rf "$vendor"
  exit 1
fi
fetch "https://raw.githubusercontent.com/k2-fsa/sherpa-onnx/v${VERSION}/LICENSE" "$vendor/licenses/sherpa-onnx-LICENSE"
fetch "https://raw.githubusercontent.com/microsoft/onnxruntime/v${ORT_VERSION}/LICENSE" "$vendor/licenses/onnxruntime-LICENSE"
fetch "https://raw.githubusercontent.com/microsoft/onnxruntime/v${ORT_VERSION}/ThirdPartyNotices.txt" "$vendor/licenses/onnxruntime-ThirdPartyNotices.txt"
for pair in "sherpa-onnx-LICENSE:$SHERPA_LICENSE_SHA256" "onnxruntime-LICENSE:$ORT_LICENSE_SHA256" "onnxruntime-ThirdPartyNotices.txt:$ORT_NOTICES_SHA256"; do
  if [ "$(sha "$vendor/licenses/${pair%%:*}")" != "${pair#*:}" ]; then
    echo "license text ${pair%%:*} does not match its pinned sha256" >&2
    rm -rf "$vendor"
    exit 1
  fi
done
echo "$ARCHIVE_SHA256" > "$vendor/.archive-sha256"
echo "$VERSION" > "$vendor/.version"
echo "sherpa-onnx $VERSION installed in vendor/ (libraries and license texts verified)"
