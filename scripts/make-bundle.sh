#!/usr/bin/env bash
# Repackages the portable dev bundle (repo + sample data + prebuilt binary)
# after you've made changes. Run from anywhere:
#   ./scripts/make-bundle.sh [output.zip]
set -euo pipefail
REPO="$(cd "$(dirname "$0")/.." && pwd)"
# Resolved before any cd: a relative output path must mean "relative to where
# you ran this", not to the staging tempdir that is deleted at the end.
OUT="$(realpath -m "${1:-$REPO/../xaerotools-bundle.zip}")"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
STAGE="$TMP/xaerotools-bundle"
mkdir -p "$STAGE/bin"

echo "==> building release binary"
(cd "$REPO" && cargo build --release -p xaerotools)

echo "==> staging"
rsync -a --exclude='target/' --exclude='node_modules/' "$REPO/" "$STAGE/xaerotools/"
if [ -d "$REPO/../sample data" ]; then
  rsync -a "$REPO/../sample data/" "$STAGE/sample data/"
fi
if [ -f "$REPO/target/release/xaerotools" ]; then
  BIN_NAME=xaerotools-linux-x86_64
  cp "$REPO/target/release/xaerotools" "$STAGE/bin/$BIN_NAME"
else
  BIN_NAME=xaerotools-windows-x86_64.exe
  cp "$REPO/target/release/xaerotools.exe" "$STAGE/bin/$BIN_NAME"
fi
# When this repo lives inside an unpacked bundle, refresh that bundle's own
# prebuilt binary too — staging into a tempdir never touched it before, so
# the copy beside the repo went stale after every build.
if [ -d "$REPO/../bin" ]; then
  cp "$STAGE/bin/$BIN_NAME" "$REPO/../bin/$BIN_NAME"
  echo "==> refreshed $(realpath "$REPO/../bin/$BIN_NAME")"
fi
# START-HERE lives at the bundle root; source of truth is docs/START-HERE.md.
cp "$REPO/docs/START-HERE.md" "$STAGE/START-HERE.md"

echo "==> zipping to $OUT"
rm -f "$OUT"
cd "$TMP"
if command -v zip >/dev/null 2>&1; then
  zip -qr "$OUT" "$(basename "$STAGE")"
elif command -v bsdtar >/dev/null 2>&1; then
  bsdtar -a -cf "$OUT" "$(basename "$STAGE")"
elif command -v 7z >/dev/null 2>&1; then
  7z a -bd "$OUT" "$(basename "$STAGE")" >/dev/null
else
  echo "need zip, bsdtar or 7z" >&2
  exit 1
fi
echo "==> done: $OUT"
