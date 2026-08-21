#!/usr/bin/env python3
"""Mirror 2b2t Atlas whole-map WDL tile pyramids for local serving.

Downloads the blackportal.cloud AtlasTiles datasets once so the XaeroTools
viewer can serve them from disk (`serve --atlas-dir`) instead of hotlinking
their server on every pan/zoom. Resumable: files already on disk are skipped,
so re-running only fetches what's missing.

    scripts/atlas-mirror.py --dest ~/atlas-tiles              # dry run: what's missing
    scripts/atlas-mirror.py --dest ~/atlas-tiles --fetch      # actually download
    scripts/atlas-mirror.py --dest D --fetch Overworld/256k/day Nether/43k

That server is not ours, so we go out of our way to be quiet: the layout of a
dataset is derived from three autoindex listings instead of crawling ~1,000 of
them, the result is cached in the mirror, and downloads are rate limited and
back off on 429/503. A dry run against an already-derived mirror makes no
remote requests at all.

Defaults to the three whole-map datasets (Overworld day, Nether, End).
Uses only the Python standard library.
"""

import argparse
import concurrent.futures as cf
import json
import math
import os
import re
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request

BASE = "https://blackportal.cloud/AtlasTiles/"
DEFAULT_DATASETS = ["Overworld/256k/day", "Nether/43k", "End/42k"]
UA = "XaeroTools-atlas-mirror/0.2 (one-time local mirror; github.com/dekrom/xaerotools)"
LINE = re.compile(
    r'href="([^"?]+)"[^\r\n]*?(\d{2}-\w{3}-\d{4} \d{2}:\d{2})\s+(-|[\d.]+[KMG]?)\s*$', re.M
)
# Cached layout, written inside the dataset's own directory in the mirror.
LAYOUT_FILE = ".atlas-layout.json"


class NotFound(Exception):
    """404 — a derived tile that the dataset does not actually have."""


class RateLimiter:
    """Token bucket shared by every worker. Their server, their pace."""

    def __init__(self, rate: float) -> None:
        self.rate = rate
        self.lock = threading.Lock()
        self.next_at = 0.0

    def take(self) -> None:
        if self.rate <= 0:
            return
        with self.lock:
            now = time.monotonic()
            wait = max(0.0, self.next_at - now)
            self.next_at = max(now, self.next_at) + 1.0 / self.rate
        if wait:
            time.sleep(wait)

    def pause(self, seconds: float) -> None:
        """Push every worker's next request out, not just the one that got 429."""
        with self.lock:
            self.next_at = max(self.next_at, time.monotonic() + seconds)


LIMITER = RateLimiter(8.0)


def parse_size(s: str) -> int:
    """nginx autoindex size: '-' (dir), bytes, or human '41K'/'1.2M'."""
    if s == "-":
        return 0
    mult = {"K": 1 << 10, "M": 1 << 20, "G": 1 << 30}.get(s[-1], 1)
    return int(float(s[:-1] if mult != 1 else s) * mult)


def retry_after(header: str | None, fallback: float) -> float:
    """Honour Retry-After when the server sends a plain delta-seconds value."""
    if header and header.strip().isdigit():
        return min(float(header.strip()), 300.0)
    return fallback


def get(url: str, retries: int = 5) -> bytes:
    delay = 1.0
    for attempt in range(retries):
        LIMITER.take()
        try:
            req = urllib.request.Request(url, headers={"User-Agent": UA})
            with urllib.request.urlopen(req, timeout=60) as r:
                return r.read()
        except urllib.error.HTTPError as e:
            if e.code in (404, 410):
                raise NotFound(url) from None
            if e.code in (429, 503):
                wait = retry_after(e.headers.get("Retry-After"), delay)
                LIMITER.pause(wait)
                time.sleep(wait)
                delay = min(delay * 2, 300)
                continue
            if attempt == retries - 1:
                raise RuntimeError(f"{url}: {e}") from e
            time.sleep(delay)
            delay = min(delay * 2, 30)
        except Exception as e:  # noqa: BLE001 - retry everything, report last
            if attempt == retries - 1:
                raise RuntimeError(f"{url}: {e}") from e
            time.sleep(delay)
            delay = min(delay * 2, 30)
    raise RuntimeError(f"{url}: still throttled after {retries} attempts")


def list_dir(rel_dir: str) -> tuple[list[str], list[tuple[str, int]]]:
    """One autoindex listing -> (subdirs, [(relative file path, size bytes)])."""
    listing = get(BASE + urllib.parse.quote(rel_dir) + "/").decode("utf-8", "replace")
    files: list[tuple[str, int]] = []
    subdirs: list[str] = []
    for name, _date, size in LINE.findall(listing):
        if name == "../":
            continue
        name = urllib.parse.unquote(name)
        if name.endswith("/"):
            subdirs.append(rel_dir + "/" + name.rstrip("/"))
        else:
            files.append((rel_dir + "/" + name, parse_size(size)))
    return subdirs, files


def numeric_leaves(paths: list[str]) -> list[int]:
    out = []
    for p in paths:
        leaf = p.rsplit("/", 1)[-1]
        if leaf.isdigit():
            out.append(int(leaf))
    return sorted(out)


def find_pyramid_root(ds: str, depth: int = 3) -> tuple[str, list[str], list[tuple[str, int]]]:
    """A dir holding blank.png is a vips pyramid root. Dataset names in the
    autoindex sometimes stop one level short (`Nether/43k` -> `Nether/43k/7`),
    so walk down until we find it — at most one listing per level."""
    subdirs, files = list_dir(ds)
    if any(f.endswith("/blank.png") for f, _ in files):
        return ds, subdirs, files
    if depth > 0:
        for sub in subdirs:
            try:
                return find_pyramid_root(sub, depth - 1)
            except (RuntimeError, NotFound):
                continue
    raise RuntimeError(f"{ds}: no blank.png — not a vips pyramid root")


def derive_layout(ds: str) -> dict:
    """Three listings instead of a full BFS crawl of the whole pyramid.

    vips "google" layout is dense: level z holds ceil(rows/2**(zMax-z)) row dirs
    of ceil(cols/2**(zMax-z)) files each, y (row) before x (col). So reading the
    deepest level's row count and one row's file count pins down every path in
    the dataset arithmetically — 1,012 remote listings become 3.
    """
    root, subdirs, files = find_pyramid_root(ds)
    levels = numeric_leaves(subdirs)
    if not levels:
        raise RuntimeError(f"{root}: no numeric level dirs — not a vips pyramid root")
    z_max = levels[-1]
    ds = root
    row_dirs, _ = list_dir(f"{ds}/{z_max}")
    row_ids = numeric_leaves(row_dirs)
    if not row_ids:
        raise RuntimeError(f"{ds}: level {z_max} has no row dirs")
    _, row_files = list_dir(f"{ds}/{z_max}/{row_ids[0]}")
    col_ids = sorted(
        int(f.rsplit("/", 1)[-1].removesuffix(".png"))
        for f, _ in row_files
        if f.endswith(".png") and f.rsplit("/", 1)[-1].removesuffix(".png").isdigit()
    )
    if not col_ids:
        raise RuntimeError(f"{ds}: level {z_max} row {row_ids[0]} has no tiles")
    sizes = [s for _, s in row_files if s > 0]
    return {
        "dataset": ds,
        "zMin": levels[0],
        "zMax": z_max,
        "rows": row_ids[-1] + 1,
        "cols": col_ids[-1] + 1,
        "extras": sorted(f.rsplit("/", 1)[-1] for f, _ in files),
        "avgTileBytes": int(sum(sizes) / len(sizes)) if sizes else 0,
        "derivedAt": int(time.time()),
    }


def load_layout(dest: str, ds: str, recrawl: bool) -> dict:
    """Cached layout beats a remote round trip; a dry run then costs nothing."""
    path = os.path.join(dest, ds, LAYOUT_FILE)
    if not recrawl and os.path.isfile(path):
        with open(path, encoding="utf-8") as f:
            cached = json.load(f)
        if cached.get("requested") == ds:
            return cached
    layout = derive_layout(ds)
    layout["requested"] = ds
    os.makedirs(os.path.dirname(path), exist_ok=True)
    tmp = path + ".part"
    with open(tmp, "w", encoding="utf-8") as f:
        json.dump(layout, f)
    os.replace(tmp, path)
    return layout


def enumerate_files(layout: dict, max_z: int | None) -> list[str]:
    """Every relative path in the dataset, derived — no listings involved."""
    ds, z_max = layout["dataset"], layout["zMax"]
    out = [f"{ds}/{name}" for name in layout["extras"]]
    for z in range(layout["zMin"], z_max + 1):
        if max_z is not None and z > max_z:
            continue
        shift = 2 ** (z_max - z)
        rows = math.ceil(layout["rows"] / shift)
        cols = math.ceil(layout["cols"] / shift)
        for row in range(rows):
            for col in range(cols):
                out.append(f"{ds}/{z}/{row}/{col}.png")
    return out


def per_level(files: list[str]) -> dict[str, int]:
    """Count tiles by zoom level (third-from-last path part)."""
    out: dict[str, int] = {}
    for rel in files:
        parts = rel.split("/")
        z = parts[-3] if len(parts) >= 3 and parts[-3].isdigit() else "-"
        out[z] = out.get(z, 0) + 1
    return out


def fetch_one(rel: str, dest: str) -> int:
    """Download one file unless it is already there (writes are atomic, so a
    file that exists is a file that finished)."""
    path = os.path.join(dest, rel)
    if os.path.isfile(path) and os.path.getsize(path) > 0:
        return 0
    try:
        data = get(BASE + urllib.parse.quote(rel))
    except NotFound:
        return 0
    os.makedirs(os.path.dirname(path), exist_ok=True)
    tmp = path + ".part"
    with open(tmp, "wb") as f:
        f.write(data)
    os.replace(tmp, path)
    return len(data)


def missing_locally(files: list[str], dest: str) -> list[str]:
    return [
        rel
        for rel in files
        if not (
            os.path.isfile(os.path.join(dest, rel))
            and os.path.getsize(os.path.join(dest, rel)) > 0
        )
    ]


def check_meta(dest: str, ds: str, complete: bool) -> None:
    """A meta.json is what makes the server advertise a dataset to the viewer.
    Advertising a mirror that has no tiles means every underlay request 404s,
    so say so loudly rather than leaving it to be discovered on the map."""
    root = os.path.join(dest, ds)
    metas = []
    for dirpath, _dirnames, filenames in os.walk(root):
        if "meta.json" in filenames:
            metas.append(os.path.join(dirpath, "meta.json"))
    if metas and not complete:
        for m in metas:
            print(f"  WARNING: {m} advertises this dataset, but it is not fully mirrored")
            print("           — the viewer's Atlas underlay will 404 on the missing tiles")
    if not metas and complete:
        print(f"  note: no meta.json under {root}; the server needs one to advertise it")
        print("        (dim/originX/originZ/bptMax/zMin/zMax — verify against the dataset)")


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("datasets", nargs="*", default=DEFAULT_DATASETS)
    ap.add_argument("--dest", required=True, help="local mirror root directory")
    ap.add_argument("--fetch", action="store_true", help="download (default: dry-run report)")
    ap.add_argument("--jobs", type=int, default=4, help="parallel connections (default 4)")
    ap.add_argument("--rate", type=float, default=8.0, help="max requests/sec (default 8)")
    ap.add_argument("--max-z", type=int, default=None, help="skip pyramid levels deeper than this")
    ap.add_argument("--recrawl", action="store_true", help="re-derive layouts, ignoring the cache")
    args = ap.parse_args()
    datasets = args.datasets or DEFAULT_DATASETS
    LIMITER.rate = args.rate

    total_files = 0
    failed = 0
    todo: list[str] = []
    per_dataset: list[tuple[str, list[str]]] = []
    for ds in datasets:
        ds = ds.rstrip("/")
        try:
            layout = load_layout(args.dest, ds, args.recrawl)
        except NotFound:
            print(f"{ds}: not found on the server — check the dataset name")
            failed += 1
            continue
        except RuntimeError as e:
            # One unreachable or renamed dataset must not take the others down.
            print(f"skipping — {e}")
            failed += 1
            continue
        files = enumerate_files(layout, args.max_z)
        per_dataset.append((ds, files))
        want = missing_locally(files, args.dest)
        avg = layout["avgTileBytes"]
        print(
            f"{ds}: {len(files)} files, ~{len(files) * avg / 1e9:.2f} GB "
            f"— {len(files) - len(want)} on disk, {len(want)} missing"
        )
        for z, c in sorted(per_level(files).items(), key=lambda kv: kv[0].rjust(3)):
            print(f"    z{z}: {c} tiles")
        check_meta(args.dest, ds, not want)
        total_files += len(files)
        todo.extend(want)

    print(f"TOTAL: {total_files} files, {len(todo)} missing locally")
    if not args.fetch:
        print("dry run — re-run with --fetch to download")
        return 1 if failed else 0
    if not todo:
        print("nothing to do — mirror is complete")
        return 1 if failed else 0

    done = 0
    fetched_bytes = 0
    t0 = time.time()

    def account(fut: cf.Future) -> None:
        nonlocal done, fetched_bytes
        fetched_bytes += fut.result()
        done += 1
        if done % 2000 == 0:
            rate = fetched_bytes / 1e6 / max(time.time() - t0, 1)
            print(f"  {done}/{len(todo)} files, {fetched_bytes / 1e9:.2f} GB new ({rate:.1f} MB/s)")

    # A Future costs ~1.7 KB, so submitting a whole-map pyramid up front would
    # cost gigabytes before a single byte is downloaded. Keep a window in flight.
    window = max(args.jobs * 4, 8)
    with cf.ThreadPoolExecutor(max_workers=args.jobs) as pool:
        inflight: set[cf.Future] = set()
        for rel in todo:
            inflight.add(pool.submit(fetch_one, rel, args.dest))
            if len(inflight) >= window:
                ready, inflight = cf.wait(inflight, return_when=cf.FIRST_COMPLETED)
                for fut in ready:
                    account(fut)
        for fut in cf.as_completed(inflight):
            account(fut)
    print(f"done: {done} files, {fetched_bytes / 1e9:.2f} GB downloaded to {args.dest}")
    for ds, files in per_dataset:
        check_meta(args.dest, ds, not missing_locally(files, args.dest))
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
