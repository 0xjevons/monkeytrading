"""Pull papertrade.xyz's Exchange event logs from HyperEVM (chain 999) public RPCs.

Writes one gzipped JSON-lines file per 1000-block chunk under `<out>/logs/`, each line
`[block, tx_index, log_index, topics, data]`, and block timestamps every `ANCHOR_STEP` blocks to
`<out>/block_times.csv`. A chunk is written only from a well-formed response, so a re-run resumes
where the last one stopped. `<out>/manifest.json` records the range, the endpoints, and a
cross-check of a sample of chunks fetched again.

Run through `run.py fetch-papertrade`.
"""

import csv
import gzip
import json
import random
import sys
import threading
import time
import urllib.request
from pathlib import Path

EXCHANGE = "0x6cd5661646289fb6e65ea5c032310fded797d0a2"  # docs.papertrade.xyz/dev/contract-addresses
DEPLOY_BLOCK = 47_854_719  # block of the Exchange proxy's creation tx 0x6f6c80fd...79ce
CHUNK = 1000  # the public endpoints' getLogs block-range limit
ANCHOR_STEP = 10_000
# Public endpoint -> the largest getLogs block range it serves.
ENDPOINTS = {
    "https://rpc.hyperliquid.xyz/evm": 1000,
    "https://rpc.hypurrscan.io": 1000,
    "https://hyperliquid-json-rpc.stakely.io": 1000,
    "https://999.rpc.thirdweb.com": 1000,
    "https://rpc.hyperlend.finance": 1000,
    "https://hyperliquid.drpc.org": 100,
}
PACE_S = 0.8  # per-endpoint gap between requests


def rpc(url: str, method: str, params: list, timeout: float = 30.0):
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
    req = urllib.request.Request(url, body, {"content-type": "application/json", "user-agent": "cap-calibration"})
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        reply = json.load(resp)
    if "error" in reply or "result" not in reply:
        raise RuntimeError(f"{url} {method}: {reply.get('error')}")
    return reply["result"]


def get_logs(url: str, start: int, size: int = CHUNK) -> list:
    """Logs of the Exchange in [start, start + size), validated and compacted. A range wider than
    the endpoint serves is fetched in pieces, and one whose response is over its size limit in
    halves."""
    limit = ENDPOINTS[url]
    if size > limit:
        return [row for piece in range(start, start + size, limit) for row in get_logs(url, piece, min(limit, start + size - piece))]
    end = start + size - 1
    try:
        result = rpc(url, "eth_getLogs", [{"address": EXCHANGE, "fromBlock": hex(start), "toBlock": hex(end)}])
    except RuntimeError as exc:
        if "too big" not in str(exc).lower() or size == 1:
            raise
        half = size // 2
        return get_logs(url, start, half) + get_logs(url, start + half, size - half)
    if not isinstance(result, list):
        raise RuntimeError(f"{url}: getLogs returned {type(result).__name__}")
    rows = []
    for log in result:
        block = int(log["blockNumber"], 16)
        if log["address"].lower() != EXCHANGE or not start <= block <= end or log.get("removed"):
            raise RuntimeError(f"{url}: log outside the filter at block {block}")
        topics = [t for t in log["topics"] if isinstance(t, str) and t.startswith("0x") and len(t) == 66]
        if len(topics) != len(log["topics"]) or not topics or not log["data"].startswith("0x"):
            raise RuntimeError(f"{url}: malformed log at block {block}")
        rows.append([block, int(log["transactionIndex"], 16), int(log["logIndex"], 16), topics, log["data"]])
    return rows


def chunk_path(out: Path, start: int) -> Path:
    return out / "logs" / f"{start:09d}.jsonl.gz"


def write_chunk(out: Path, start: int, rows: list) -> None:
    path = chunk_path(out, start)
    tmp = path.with_suffix(".tmp")
    with gzip.open(tmp, "wt") as fh:
        for row in rows:
            fh.write(json.dumps(row, separators=(",", ":")) + "\n")
    tmp.rename(path)


def worker(url: str, todo: list, pending: list, lock: threading.Lock, out: Path, failures: dict) -> None:
    """Fetches chunks from `todo` until every chunk is written. A failed chunk goes back on `todo`
    for any endpoint to take, and the failing endpoint backs off; `pending` holds the count of
    chunks not yet written, so a worker waits rather than exits while another holds one."""
    backoff = PACE_S
    while True:
        with lock:
            if pending[0] == 0:
                return
            start = todo.pop() if todo else None
        if start is None:
            time.sleep(PACE_S)
            continue
        try:
            write_chunk(out, start, get_logs(url, start))
            with lock:
                pending[0] -= 1
            backoff = PACE_S
        except Exception as exc:  # rate limits and upstream errors: hand the chunk back and slow down
            with lock:
                todo.append(start)
                failures[url] = failures.get(url, 0) + 1
                if failures[url] % 20 == 1:
                    print(f"{url}: {failures[url]} failures, last: {exc}", file=sys.stderr)
            backoff = min(backoff * 2, 60.0)
        time.sleep(backoff)


def block_time(start: int) -> int:
    for _ in range(20):
        url = random.choice(list(ENDPOINTS))
        try:
            return int(rpc(url, "eth_getBlockByNumber", [hex(start), False])["timestamp"], 16)
        except Exception:
            time.sleep(3)
    raise RuntimeError(f"no timestamp for block {start}")


def cross_check(out: Path, starts: list, sample: int) -> list:
    """Re-fetches a random sample of chunks, each from an endpoint chosen at random, and compares
    log counts."""
    checks = []
    for start in random.Random(0).sample(starts, min(sample, len(starts))):
        with gzip.open(chunk_path(out, start), "rt") as fh:
            stored = sum(1 for _ in fh)
        for _ in range(20):
            url = random.choice(list(ENDPOINTS))
            try:
                checks.append({"start": start, "stored": stored, "refetched": len(get_logs(url, start)), "url": url})
                break
            except Exception:
                time.sleep(3)
    return checks


def fetch(out: Path, to_block: int | None) -> None:
    """Pulls every whole chunk from the deployment block to `to_block` (default: the manifest's,
    else the latest block)."""
    (out / "logs").mkdir(parents=True, exist_ok=True)
    manifest_path = out / "manifest.json"
    manifest = json.loads(manifest_path.read_text()) if manifest_path.exists() else {}
    to_block = to_block or manifest.get("to_block") or int(rpc(next(iter(ENDPOINTS)), "eth_blockNumber", []), 16)
    last_start = to_block + 1 - (to_block + 1 - DEPLOY_BLOCK) % CHUNK  # whole chunks up to to_block
    starts = list(range(DEPLOY_BLOCK, last_start, CHUNK))
    todo = [s for s in starts if not chunk_path(out, s).exists()]
    print(f"{len(starts)} chunks to block {last_start - 1}, {len(todo)} missing", file=sys.stderr)

    lock = threading.Lock()
    failures: dict = {}
    pending = [len(todo)]
    threads = [threading.Thread(target=worker, args=(url, todo, pending, lock, out, failures)) for url in ENDPOINTS]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()

    times_path = out / "block_times.csv"
    known = {}
    if times_path.exists():
        with times_path.open() as fh:
            known = {int(r["block"]): int(r["timestamp"]) for r in csv.DictReader(fh)}
    anchors = list(range(DEPLOY_BLOCK, last_start, ANCHOR_STEP)) + [last_start]
    for block in anchors:
        if block not in known:
            known[block] = block_time(block)
    with times_path.open("w", newline="") as fh:
        writer = csv.writer(fh)
        writer.writerow(["block", "timestamp"])
        writer.writerows(sorted(known.items()))

    manifest.update(
        {
            "chain_id": 999,
            "exchange": EXCHANGE,
            "from_block": DEPLOY_BLOCK,
            "to_block": to_block,
            "last_block_pulled": last_start - 1,
            "chunks": len(starts),
            "endpoints": ENDPOINTS,
            "cross_check": cross_check(out, starts, 25),
        }
    )
    manifest_path.write_text(json.dumps(manifest, indent=1))
    mismatches = [c for c in manifest["cross_check"] if c["stored"] != c["refetched"]]
    print(f"done; cross-check mismatches: {len(mismatches)}", file=sys.stderr)
