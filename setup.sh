#!/usr/bin/env bash
# XaeroTools one-liner setup: installs the Rust toolchain if missing (user-local,
# no root), builds the release binary, and offers to start the viewer.
#
#   ./setup.sh            build everything
#   ./setup.sh --serve    build, then start the viewer immediately
#
# Node.js is NOT required: the web UI ships prebuilt in webui/dist and is
# embedded into the binary. Install Node only if you want to modify the UI.
set -euo pipefail
cd "$(dirname "$0")"

say() { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m==>\033[0m %s\n' "$*" >&2; exit 1; }

# 1. Rust toolchain (user-local via rustup; no sudo needed).
if ! command -v cargo >/dev/null 2>&1; then
  if [ -x "$HOME/.cargo/bin/cargo" ]; then
    export PATH="$HOME/.cargo/bin:$PATH"
  else
    say "Rust not found — installing via rustup (user-local, ~1 min)"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
    # shellcheck disable=SC1091
    . "$HOME/.cargo/env"
  fi
fi
say "using $(cargo --version)"

# macOS needs the Xcode command line tools for the C compiler (SQLite build).
if [ "$(uname)" = "Darwin" ] && ! xcode-select -p >/dev/null 2>&1; then
  say "installing Xcode command line tools (accept the dialog, then re-run ./setup.sh)"
  xcode-select --install || true
  exit 1
fi

# rustup installs Rust, not a C toolchain: rustc links through cc, and SQLite
# is compiled from C source (rusqlite "bundled"). A fresh Debian or Fedora box
# has neither, and the failure there is an opaque "linker `cc` not found".
command -v cc >/dev/null 2>&1 \
  || die "no C compiler — install one and re-run: apt install build-essential | dnf groupinstall 'Development Tools' | pacman -S base-devel"

# 2. Build.
say "building XaeroTools (release)…"
cargo build --release -p xaerotools

# 3. Self-check against the sample corpus when it's next to the repo.
if [ -d "../sample data" ]; then
  say "sample data found — running the format round-trip self-check"
  cargo test -p xaero-core --release --test corpus 2>/dev/null | grep -E "test result" || true
fi

BIN="$(pwd)/target/release/xaerotools"
say "done! binary: $BIN"
echo
echo "  Start the viewer (auto-detects .minecraft):   $BIN"
echo "  Point at a folder:                            $BIN serve --root /path/to/xaero --open"
echo "  All commands:                                 $BIN help"
echo

if [ "${1:-}" = "--serve" ]; then
  exec "$BIN" serve --open
fi
