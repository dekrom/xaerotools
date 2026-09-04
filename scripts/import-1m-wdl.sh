#!/usr/bin/env bash
# Converts the 2b2t 1M world download to Xaero's World Map format, ring by ring.
#
# The download ships as SquashFS images, one per concentric ring, which overlay
# to form a single zvcr directory. Rather than extracting all of it at once
# (110 GiB for the Nether and End together), each ring is extracted, converted
# and deleted in turn, so peak scratch use is one ring.
#
# Rings are processed in ascending order and `import-zvcr` leaves an existing
# output alone, so where two rings carry the same region the inner one wins —
# the same precedence the download's own extract.sh applies. That also makes
# the whole run resumable: rerun it and it picks up where it stopped.
#
#   ./import-1m-wdl.sh --wdl /path/to/1million_2b2t/wdl \
#                      --out /path/to/output-tree --work /path/to/scratch
set -euo pipefail

BIN=""
WDL=""
OUT=""
WORK=""
DIMS="nether end"
WORLD="Multiplayer_2b2t"
THREADS=""

usage() {
    cat >&2 <<USAGE
Usage: $0 --wdl <dir> --out <xaero-root> --work <scratch-dir> [options]

  --wdl <dir>      directory holding nether/ and end/ ringN.squashfs images
  --out <dir>      Xaero root to write into (a new tree; nothing is merged)
  --work <dir>     scratch space for one extracted ring (needs ~30 GiB free)
  --dims <list>    space or comma separated: nether, end (default: both)
  --world <name>   world folder name to write under (default: $WORLD)
  --threads <n>    conversion threads (default: all cores)
  --bin <path>     xaerotools binary (default: target/release/xaerotools)
USAGE
    exit 2
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --wdl) WDL="${2:?}"; shift 2 ;;
        --out) OUT="${2:?}"; shift 2 ;;
        --work) WORK="${2:?}"; shift 2 ;;
        --dims) DIMS="${2//,/ }"; shift 2 ;;
        --world) WORLD="${2:?}"; shift 2 ;;
        --threads) THREADS="${2:?}"; shift 2 ;;
        --bin) BIN="${2:?}"; shift 2 ;;
        -h|--help) usage ;;
        *) echo "unknown argument: $1" >&2; usage ;;
    esac
done

[[ -n "$WDL" && -n "$OUT" && -n "$WORK" ]] || usage
[[ -n "${DIMS// /}" ]] || { echo "--dims needs at least one of: nether end" >&2; exit 2; }
[[ -d "$WDL" ]] || { echo "no such directory: $WDL" >&2; exit 1; }

if [[ -z "$BIN" ]]; then
    BIN="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/target/release/xaerotools"
fi
[[ -x "$BIN" ]] || { echo "xaerotools binary not found or not executable: $BIN" >&2; exit 1; }
command -v 7z >/dev/null || { echo "7z is required to read the SquashFS images" >&2; exit 1; }

mkdir -p "$OUT" "$WORK"
STAMPS="$WORK/.done"
mkdir -p "$STAMPS"

# A ring's scratch directory is removed whether the conversion succeeded, failed
# or was interrupted: 26 GiB left behind would strand the next ring.
RING_DIR=""
cleanup() {
    [[ -n "$RING_DIR" && -d "$RING_DIR" ]] && rm -rf "$RING_DIR"
}
trap cleanup EXIT INT TERM

for dim in $DIMS; do
    dim_dir="$WDL/$dim"
    if [[ ! -d "$dim_dir" ]]; then
        echo "== $dim: no $dim_dir, skipping"
        continue
    fi
    # Numeric sort so ring2 is processed before ring10; inner rings must win.
    mapfile -t images < <(find "$dim_dir" -maxdepth 1 -name 'ring*.squashfs' -printf '%f\n' \
        | sed -E 's/^ring([0-9]+)\.squashfs$/\1 &/' | sort -n | cut -d' ' -f2)
    echo "== $dim: ${#images[@]} rings"

    for image in "${images[@]}"; do
        stamp="$STAMPS/$dim.$image"
        if [[ -f "$stamp" ]]; then
            echo "-- $dim/$image already done, skipping"
            continue
        fi
        RING_DIR="$WORK/$dim.${image%.squashfs}"
        rm -rf "$RING_DIR"
        mkdir -p "$RING_DIR"

        echo "-- $dim/$image: extracting"
        7z x -bd -bso0 -y -o"$RING_DIR" "$dim_dir/$image" >/dev/null

        echo "-- $dim/$image: converting"
        # shellcheck disable=SC2086 # THREADS is an intentional word split
        "$BIN" import-zvcr --src "$RING_DIR" -o "$OUT" --world "$WORLD" \
            ${THREADS:+--threads $THREADS}

        rm -rf "$RING_DIR"
        RING_DIR=""
        date -Iseconds > "$stamp"
    done
done

echo "done. Xaero tree at $OUT"
