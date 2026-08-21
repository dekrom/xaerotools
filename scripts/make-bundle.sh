#!/usr/bin/env bash
# Repackages the portable dev bundle (repo + sample data + prebuilt binary)
# after you've made changes. Run from anywhere:
#   ./scripts/make-bundle.sh [output.zip]
set -euo pipefail
REPO="$(cd "$(dirname "$0")/.." && pwd)"
OUT="${1:-$REPO/../xaerotools-bundle.zip}"
STAGE="$(mktemp -d)/xaerotools-bundle"
mkdir -p "$STAGE/bin"

echo "==> building release binary"
(cd "$REPO" && cargo build --release -p xaerotools)

echo "==> staging"
rsync -a --exclude='target/' --exclude='node_modules/' "$REPO/" "$STAGE/xaerotools/"
if [ -d "$REPO/../sample data" ]; then
  rsync -a "$REPO/../sample data/" "$STAGE/sample data/"
fi
cp "$REPO/target/release/xaerotools" "$STAGE/bin/xaerotools-linux-x86_64" 2>/dev/null ||
  cp "$REPO/target/release/xaerotools.exe" "$STAGE/bin/xaerotools-windows-x86_64.exe"
[ -f "$STAGE/xaerotools/START-HERE.md" ] || true
# START-HERE lives at the bundle root; source of truth is docs/START-HERE.md.
cp "$REPO/docs/START-HERE.md" "$STAGE/START-HERE.md"

echo "==> zipping to $OUT"
rm -f "$OUT"
cd "$(dirname "$STAGE")"
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
rm -rf "$(dirname "$STAGE")"
echo "==> done: $OUT"
