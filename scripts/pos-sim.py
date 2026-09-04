#!/usr/bin/env python3
"""Simulates player accounts posting positions to a running XaeroTools server,
so live markers / the Players panel / the ingest API are testable without the
game. Stdlib only.

Usage:
  # Against a server on this machine no tokens are needed — bare names do:
  #   scripts/pos-sim.py --account Account1 --account Account2
  # For a remote server, generate one token per account (picked up live):
  #      xaerotools tokens generate Account1 [--config PATH]
  # then put one NAME=TOKEN per line in a file (chmod 600):
  scripts/pos-sim.py --url http://127.0.0.1:45746 --accounts-file accounts.txt \
      [--rate 1.0] [--center 0,0] [--speed 4.0] [--dim-hop]

Each account does a random walk (heading drift + speed jitter) around
--center, POSTing /ingest/v1/position at --rate per second. --dim-hop makes
accounts occasionally switch dimension to exercise the cross-dimension roster.
Tokens are sent only in the Authorization header, never in URLs or output.
Prefer --accounts-file: tokens passed via --account NAME=TOKEN are visible in
/proc/<pid>/cmdline and shell history.
"""

import argparse
import json
import math
import random
import signal
import sys
import time
import urllib.error
import urllib.request

DIMS = ["minecraft:overworld", "minecraft:the_nether", "minecraft:the_end"]


class Account:
    def __init__(self, name, token, cx, cz, speed):
        self.name = name
        self.token = token
        self.x = cx + random.uniform(-200, 200)
        self.y = 64.0
        self.z = cz + random.uniform(-200, 200)
        self.yaw = random.uniform(0, 360)
        self.speed = speed * random.uniform(0.7, 1.3)
        self.dim = DIMS[0]
        self.errors = 0
        self.sent = 0

    def step(self, dt, dim_hop):
        # Heading drift makes paths look like walking, not noise.
        self.yaw = (self.yaw + random.uniform(-25, 25)) % 360
        rad = math.radians(self.yaw)
        # Minecraft yaw 0 faces +Z; 90 faces -X.
        self.x += -math.sin(rad) * self.speed * dt
        self.z += math.cos(rad) * self.speed * dt
        self.y = max(1.0, min(255.0, self.y + random.uniform(-0.5, 0.5)))
        if dim_hop and random.random() < 0.01:
            self.dim = random.choice(DIMS)

    def body(self):
        return {
            "player": self.name,
            "dim": self.dim,
            "x": round(self.x, 2),
            "y": round(self.y, 2),
            "z": round(self.z, 2),
            "yaw": round(self.yaw, 1),
        }


def post(url, account):
    headers = {"Content-Type": "application/json"}
    if account.token:
        headers["Authorization"] = "Bearer " + account.token
    req = urllib.request.Request(
        url + "/ingest/v1/position",
        data=json.dumps(account.body()).encode(),
        headers=headers,
        method="POST",
    )
    try:
        with urllib.request.urlopen(req, timeout=5) as resp:
            account.sent += 1
            return resp.status
    except urllib.error.HTTPError as e:
        account.errors += 1
        return e.code
    except (urllib.error.URLError, OSError) as e:
        account.errors += 1
        return str(getattr(e, "reason", e))


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--url", default="http://127.0.0.1:45746")
    ap.add_argument(
        "--accounts-file",
        metavar="PATH",
        help="file with one NAME=TOKEN per line, or bare NAME for a loopback server (preferred: keeps tokens out of argv)",
    )
    ap.add_argument(
        "--account",
        action="append",
        default=[],
        metavar="NAME=TOKEN",
        help="repeat per account; NOTE: visible in /proc and shell history",
    )
    ap.add_argument("--rate", type=float, default=1.0, help="posts/sec per account")
    ap.add_argument("--center", default="0,0", metavar="X,Z")
    ap.add_argument("--speed", type=float, default=4.0, help="blocks/sec")
    ap.add_argument("--dim-hop", action="store_true", help="accounts sometimes switch dimension")
    args = ap.parse_args()

    specs = list(args.account)
    if args.account:
        print("warning: --account exposes tokens via /proc; prefer --accounts-file", file=sys.stderr)
    if args.accounts_file:
        with open(args.accounts_file) as f:
            specs += [ln.strip() for ln in f if ln.strip() and not ln.strip().startswith("#")]
    if not specs:
        ap.error("need --accounts-file PATH or at least one --account NAME=TOKEN")
    cx, cz = (float(v) for v in args.center.split(","))
    accounts = []
    for spec in specs:
        # A bare NAME (no token) is valid against a loopback server.
        name, _, token = spec.partition("=")
        if not name:
            ap.error(f"bad account spec {spec!r}")
        accounts.append(Account(name, token, cx, cz, args.speed))

    url = args.url.rstrip("/")
    interval = 1.0 / max(args.rate, 0.01)
    running = True

    def stop(_sig, _frm):
        nonlocal running
        running = False

    signal.signal(signal.SIGINT, stop)
    print(f"posting {len(accounts)} account(s) to {url} at {args.rate}/s each — Ctrl-C to stop")
    last_report = time.monotonic()
    while running:
        t0 = time.monotonic()
        for acc in accounts:
            acc.step(interval, args.dim_hop)
            status = post(url, acc)
            if status not in (200, 204):
                print(f"  {acc.name}: HTTP {status}", file=sys.stderr)
        if time.monotonic() - last_report >= 10:
            last_report = time.monotonic()
            for acc in accounts:
                print(
                    f"  {acc.name}: {acc.sent} sent, {acc.errors} errors, "
                    f"at {acc.x:.0f},{acc.z:.0f} [{acc.dim.split(':')[1]}]"
                )
        time.sleep(max(0.0, interval - (time.monotonic() - t0)))
    print("stopped.")
    for acc in accounts:
        print(f"  {acc.name}: {acc.sent} sent, {acc.errors} errors")


if __name__ == "__main__":
    main()
