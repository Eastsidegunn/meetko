#!/bin/sh
# SPDX-License-Identifier: AGPL-3.0-or-later
# swift test on a full Xcode (plain `swift test`) or with Command Line Tools only, where Swift
# Testing lives outside the default search paths. The developer dir comes from xcode-select
# (or DEVELOPER_DIR), so nothing machine-specific is hardcoded.
set -eu
dev=${DEVELOPER_DIR:-$(xcode-select -p)}
case "$dev" in
  *CommandLineTools)
    D=$dev/Library/Developer
    exec swift test -Xswiftc -F"$D/Frameworks" -Xlinker -F"$D/Frameworks" \
      -Xlinker -rpath -Xlinker "$D/Frameworks" -Xlinker -rpath -Xlinker "$D/usr/lib" "$@"
    ;;
  *)
    exec swift test "$@"
    ;;
esac
