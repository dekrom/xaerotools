#!/usr/bin/env python3
"""Cross-check assets/blockprops.bin against a world download's own registry.

The zvcr dataset ships registries/<protocol>/blockstates.json, generated
independently from the same Minecraft version. If every state id resolves to
the same name and light level in both, the id spaces are provably identical
and the importer can trust blockprops.bin as its id resolver.

    ./verify.py ../../assets/blockprops.bin /path/to/registries/769/blockstates.json
"""
import json
import struct
import sys


def load_blockprops(path):
    b = open(path, "rb").read()
    pos = 0

    def take(n):
        nonlocal pos
        v = b[pos:pos + n]
        if len(v) != n:
            sys.exit("blockprops.bin truncated")
        pos += n
        return v

    def u8():
        return take(1)[0]

    def u16():
        return struct.unpack(">H", take(2))[0]

    def u32():
        return struct.unpack(">I", take(4))[0]

    def s():
        return take(u16()).decode()

    if take(4) != b"XBP1":
        sys.exit("bad magic")
    if u16() != 1:
        sys.exit("unsupported format")
    mc_version = s()
    blocks = [s() for _ in range(u32())]
    props = [s() for _ in range(u32())]
    for _ in range(u32()):
        u32()
    names, lights = [], []
    for _ in range(u32()):
        u32()                       # flags
        lights.append(u8())         # light emission
        u8()                        # light dampening
        block = blocks[u16()]
        u8()                        # fluid legacy index
        tokens = [props[u16()] for _ in range(u8())]
        names.append(block + ("[" + ",".join(tokens) + "]" if tokens else ""))
    if pos != len(b):
        sys.exit("trailing bytes in blockprops.bin")
    return mc_version, names, lights


def main():
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    mc_version, names, lights = load_blockprops(sys.argv[1])
    reg = json.load(open(sys.argv[2]))
    entries = reg["entries"]
    print(f"blockprops.bin: MC {mc_version}, {len(names)} states")
    print(f"registry:       protocol {reg['protocolVersion']}, {len(entries)} states")
    if len(entries) != len(names):
        sys.exit(f"STATE COUNT MISMATCH: {len(entries)} vs {len(names)}")

    bad_names, bad_light = [], []
    for e in entries:
        want = e["name"] if ":" in e["name"] else "minecraft:" + e["name"]
        if names[e["id"]] != want:
            bad_names.append((e["id"], names[e["id"]], want))
        if lights[e["id"]] != e["lightLevel"]:
            bad_light.append((e["id"], lights[e["id"]], e["lightLevel"]))

    for label, bad in (("name", bad_names), ("lightLevel", bad_light)):
        print(f"{label} mismatches: {len(bad)}")
        for row in bad[:10]:
            print("  ", row)
    sys.exit(1 if bad_names or bad_light else 0)


if __name__ == "__main__":
    main()
