#!/usr/bin/env bash
# Downloads one official Minecraft client jar plus its runtime libraries into a
# cache dir, verifying every artifact against the SHA1 Mojang pins in the
# version manifest. Prints the resulting classpath on stdout.
#
#   ./fetch.sh 1.21.4 /path/to/cache   # -> "<cache>/client.jar:<cache>/libs/..."
set -euo pipefail

VERSION="${1:?usage: fetch.sh <mc-version> <cache-dir>}"
CACHE="${2:?usage: fetch.sh <mc-version> <cache-dir>}"
MANIFEST_URL="https://piston-meta.mojang.com/mc/game/version_manifest_v2.json"

mkdir -p "$CACHE/libs"

# Fetch to a temp file, verify, then move into place: a failed check must never
# leave a half-written artifact behind that the next run would trust.
fetch_verified() {
    local url="$1" want="$2" dest="$3"
    if [[ -f "$dest" ]] && [[ "$(sha1sum "$dest" | cut -d' ' -f1)" == "$want" ]]; then
        return 0
    fi
    local tmp
    tmp="$(mktemp "$dest.XXXXXX")"
    trap 'rm -f "$tmp"' RETURN
    curl -sSfL --retry 5 --retry-all-errors --retry-delay 2 --connect-timeout 20 "$url" -o "$tmp"
    local got
    got="$(sha1sum "$tmp" | cut -d' ' -f1)"
    if [[ "$got" != "$want" ]]; then
        echo "sha1 mismatch for $url: want $want got $got" >&2
        return 1
    fi
    mv "$tmp" "$dest"
}

manifest="$CACHE/version_manifest_v2.json"
curl -sSfL --retry 5 --retry-all-errors --retry-delay 2 --connect-timeout 20 "$MANIFEST_URL" -o "$manifest"

# The manifest pins each version JSON by sha1; verifying it is what makes every
# artifact hash below trustworthy.
read -r meta_url meta_sha < <(
    python3 -c '
import json, sys
m = json.load(open(sys.argv[1]))
for v in m["versions"]:
    if v["id"] == sys.argv[2]:
        print(v["url"], v["sha1"])
        break
else:
    sys.exit("version %s not in manifest" % sys.argv[2])
' "$manifest" "$VERSION"
)

meta="$CACHE/$VERSION.json"
fetch_verified "$meta_url" "$meta_sha" "$meta"

client_url="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["downloads"]["client"]["url"])' "$meta")"
client_sha="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["downloads"]["client"]["sha1"])' "$meta")"
fetch_verified "$client_url" "$client_sha" "$CACHE/client.jar"

# Native-only library entries carry no plain artifact; skip them.
while read -r path url sha; do
    dest="$CACHE/libs/$(basename "$path")"
    fetch_verified "$url" "$sha" "$dest"
done < <(python3 -c '
import json, sys
d = json.load(open(sys.argv[1]))
for lib in d["libraries"]:
    art = lib.get("downloads", {}).get("artifact")
    if art:
        print(art["path"], art["url"], art["sha1"])
' "$meta")

printf '%s' "$CACHE/client.jar"
for j in "$CACHE"/libs/*.jar; do printf ':%s' "$j"; done
printf '\n'
