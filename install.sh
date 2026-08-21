#!/usr/bin/env bash
# XaeroTools installer — turns the repo into a desktop app (Linux).
#
# Why: `setup.sh` builds the binary, but using it still meant a terminal and
# a path. This puts `xaerotools` on PATH and in the application menu, so the
# map viewer is one click away and every remaining knob lives in its web UI
# (roots, merge tools, live-share tokens) or in the Meteor addon's GUI.
#
#   ./install.sh                  build if needed, install binary + launcher
#   ./install.sh --dry-run        print what would happen, touch nothing
#   ./install.sh --addon          also install the companion addon jar into
#                                 every detected .minecraft/mods folder
#   ./install.sh --mods-dir PATH  install the jar into exactly this folder
#   ./install.sh --uninstall      remove binary/launcher/icon (never map data,
#                                 never the config, vault or ingest dirs)
#
# Windows/macOS: use setup.ps1 / setup.sh and run the binary directly.
set -euo pipefail
cd "$(dirname "$0")"
REPO="$PWD"

say() { printf '\033[1;36m==>\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m==>\033[0m %s\n' "$*" >&2; exit 1; }

DRY=0 ADDON=0 UNINSTALL=0 MODS_DIR=""
while [ $# -gt 0 ]; do
  case "$1" in
    --dry-run) DRY=1 ;;
    --addon) ADDON=1 ;;
    --mods-dir) shift; MODS_DIR="${1:?--mods-dir needs a path}"; ADDON=1 ;;
    --uninstall) UNINSTALL=1 ;;
    *) die "unknown arg: $1 (see the header of this script)" ;;
  esac
  shift
done

run() { if [ "$DRY" = 1 ]; then echo "DRY: $*"; else "$@"; fi; }

BIN_DST="$HOME/.local/bin/xaerotools"
DESKTOP="$HOME/.local/share/applications/xaerotools.desktop"
ICON="$HOME/.local/share/icons/hicolor/512x512/apps/xaerotools.png"

if [ "$UNINSTALL" = 1 ]; then
  for f in "$BIN_DST" "$DESKTOP" "$ICON"; do
    [ -e "$f" ] && { say "removing $f"; run rm -f "$f"; } || true
  done
  command -v update-desktop-database >/dev/null 2>&1 \
    && run update-desktop-database "$HOME/.local/share/applications" || true
  say "uninstalled (config, vault and ingest data were left untouched)"
  exit 0
fi

# 1. Binary: build via setup.sh only when missing or older than the sources.
if [ ! -x target/release/xaerotools ]; then
  say "no release binary — building via ./setup.sh"
  run ./setup.sh
fi
[ "$DRY" = 1 ] || [ -x target/release/xaerotools ] || die "build did not produce target/release/xaerotools"
say "installing binary -> $BIN_DST"
run install -Dm755 target/release/xaerotools "$BIN_DST"

# 2. Icon: a real rendered region from the sample corpus when it is around
# (bundle layout keeps it at ../sample data). Purely cosmetic — skipped
# silently when neither the corpus nor an existing icon is available.
if [ ! -f "$ICON" ]; then
  # Largest file = most mapped tiles = an icon that actually looks like a map.
  corpus_region=$(ls -S "../sample data/xaero1.21.8/world-map/Multiplayer_2b2t/null/mw\$default/"*.zip 2>/dev/null | head -1 || true)
  if [ -n "$corpus_region" ]; then
    say "rendering launcher icon from the sample corpus"
    run mkdir -p "$(dirname "$ICON")"
    if [ "$DRY" = 1 ]; then
      echo "DRY: target/release/xaerotools render-region \"$corpus_region\" -o \"$ICON\""
    else
      target/release/xaerotools render-region "$corpus_region" -o "$ICON" >/dev/null
    fi
  fi
fi

# 3. Desktop launcher. Terminal=true on purpose: the server's log stays
# visible and closing the window stops it — nothing lingers invisibly.
say "installing launcher -> $DESKTOP"
if [ "$DRY" = 1 ]; then
  echo "DRY: write $DESKTOP (Exec=$BIN_DST serve --open)"
else
  mkdir -p "$(dirname "$DESKTOP")"
  cat > "$DESKTOP" <<EOF
[Desktop Entry]
Type=Application
Name=XaeroTools Map
Comment=Browse, merge and live-share your Xaero world maps
Exec=$BIN_DST serve --open
Icon=xaerotools
Terminal=true
Categories=Game;
Keywords=minecraft;map;xaero;
EOF
fi
command -v update-desktop-database >/dev/null 2>&1 \
  && run update-desktop-database "$HOME/.local/share/applications" || true

# 4. Companion addon jar (optional): detected Minecraft instances get a copy.
if [ "$ADDON" = 1 ]; then
  jar=$(ls "$REPO/../xaerotools-companion/build/libs/"xaerotools-companion-*.jar \
           "$REPO/../../meteoraddon/xaerotools-companion/build/libs/"xaerotools-companion-*.jar \
           "$REPO/../meteoraddon/xaerotools-companion/build/libs/"xaerotools-companion-*.jar 2>/dev/null | head -1 || true)
  [ -n "$jar" ] || die "companion jar not found — clone github.com/dekrom/xaerotools-companion next to this repo and build it: ./gradlew build"
  if [ -n "$MODS_DIR" ]; then
    mods_dirs=("$MODS_DIR")
  else
    mods_dirs=()
    for d in "$HOME/.minecraft/mods" \
             "$HOME"/.local/share/PrismLauncher/instances/*/.minecraft/mods \
             "$HOME"/.local/share/multimc/instances/*/.minecraft/mods; do
      [ -d "$d" ] && mods_dirs+=("$d")
    done
    [ ${#mods_dirs[@]} -gt 0 ] || die "no mods folder found — pass --mods-dir PATH"
  fi
  for d in "${mods_dirs[@]}"; do
    say "installing addon -> $d/$(basename "$jar")"
    run install -Dm644 "$jar" "$d/$(basename "$jar")"
  done
fi

# 5. Verify: the installed binary must actually run on this machine.
if [ "$DRY" = 1 ]; then
  say "dry run complete — nothing was touched"
  exit 0
fi
"$BIN_DST" tokens list >/dev/null 2>&1 || die "installed binary failed to run"
case ":$PATH:" in
  *":$HOME/.local/bin:"*) ;;
  *) say "note: ~/.local/bin is not on PATH — the menu entry still works" ;;
esac
say "done. Launch 'XaeroTools Map' from the app menu (or: xaerotools serve --open)"
say "tokens & sharing live in the map's Share panel; addon settings in Meteor's GUI"
