# xaero-blockprops

Dev-time generator of `assets/blockprops.bin` — the per-blockstate table the
zvcr importer needs to run Xaero's column algorithm outside the game.

The zvcr world-download format stores blocks as **global blockstate ids** in
`Block.BLOCK_STATE_REGISTRY` order. This tool walks that registry in the same
order and records, for every state, the properties `MapWriter.loadPixel` and
`loadPixelHelp` consult: map colour, render shape, light emission and
dampening, fluid state, piston push reaction, the render layer, the collision
shape, and the block-class tests. That makes the table both the id resolver and
the behaviour source, so the importer never has to guess.

Regenerating (one-time per Minecraft version):

```bash
CACHE=$(mktemp -d)
CP=$(./fetch.sh 1.21.4 "$CACHE")            # client jar + libs, every SHA1 verified
java -jar SpecialSource.jar --in-jar "$CACHE/client.jar" \
     --out-jar "$CACHE/client-mapped.jar" --srg-in "$CACHE/client.txt"
javac -cp "$CP" -d "$CACHE/classes" BlockProps.java
java -cp "$CACHE/classes:$CP" BlockProps 1.21.4 ../../assets/blockprops.bin
```

`fetch.sh` does not download `client.txt` (Mojang's official mappings) or
SpecialSource; grab both from the URLs in the version JSON it caches and from
Maven Central. The official jar is obfuscated, so the remap step is required.

The artifact contains only derived per-state booleans and small integers — no
Minecraft code or assets.

## Verifying a regenerated table

The world download ships its own copy of the same registry. The state count,
every state name and every light level must agree:

```bash
./verify.py ../../assets/blockprops.bin \
    /path/to/1million_2b2t/etc/registries/769/blockstates.json
```

A mismatch means the Minecraft version does not match the one the zvcr files
were written with (their header records the protocol version — 769 = 1.21.4).
