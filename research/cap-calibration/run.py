"""Entry point for the cap calibration: fetch, derive, simulate, and report.

Run with `python -I run.py <command>` from any directory; every path is relative to this file.
`all` runs every simulation stage and the report from the committed derived data.
"""

import argparse
import hashlib
import json
import sys
import time
from dataclasses import asdict, replace
from pathlib import Path

ROOT = Path(__file__).resolve().parent
sys.path.insert(0, str(ROOT))

RAW = ROOT / "data" / "raw"
DERIVED = ROOT / "data" / "derived"
RESULTS = ROOT / "results"
STAGES = ("per-market", "joint", "sensitivity")
SENSITIVITY_TARGET = 0.001  # the sensitivities start from the floors chosen for this target


def inputs():
    import numpy as np

    from catalogue import load_genesis
    from model import prices, simulate
    from model.flow import load, load_mix

    bars, symbols = prices.load_bars(DERIVED / "hourly_bars.npz")
    flow = load(DERIVED)
    markets = simulate.load_markets(load_genesis(), symbols, load_mix(DERIVED, symbols), bars)
    return np, simulate, bars, flow, markets


def manifest(args, scenario) -> dict:
    hashes = {p.name: hashlib.sha256(p.read_bytes()).hexdigest()[:16] for p in sorted(DERIVED.iterdir()) if p.is_file()}
    return {"command": sys.argv[1:], "scenario": asdict(scenario), "derived_sha256": hashes, "workers": args.workers}


def read_rows(path: Path) -> list[dict]:
    import csv

    with path.open() as fh:
        rows = list(csv.DictReader(fh))
    for row in rows:
        for k, v in row.items():
            try:
                row[k] = float(v)
            except ValueError:
                pass
    return rows


def simulate_stage(stage: str, args) -> None:
    from model import experiments

    np, simulate, bars, flow, markets = inputs()
    base = simulate.Scenario(seed=args.seed)
    RESULTS.mkdir(exist_ok=True)
    started = time.time()
    if stage == "per-market":
        scenario = replace(base, paths=args.paths or 2000, batch_paths=args.batch_paths or 500)
        rows = experiments.per_market(scenario, markets, bars, flow, args.workers)
        experiments.write_rows(RESULTS / "per_market_sweep.csv", rows)
    elif stage == "joint":
        scenario = replace(base, paths=args.paths or 2000, batch_paths=args.batch_paths or 100)
        sweep = read_rows(RESULTS / "per_market_sweep.csv")
        for key, rows in experiments.joint(scenario, markets, bars, flow, sweep, args.workers).items():
            experiments.write_rows(RESULTS / f"joint_{key}.csv", rows)
    elif stage == "sensitivity":
        scenario = replace(base, paths=args.paths or 1000, batch_paths=args.batch_paths or 100)
        rows = read_rows(RESULTS / "joint_p_ever_tail.csv")
        chosen = experiments.chosen_level(rows, "p_ever_tail", SENSITIVITY_TARGET) or rows[0]
        floors = np.array([json.loads(chosen["market_floors"])[s] for s in markets.symbols])
        measured = sum(c.margin_share for c in flow.classes if c.kind == "hedged")
        out = experiments.sensitivity(scenario, markets, bars, flow, floors, measured, args.workers)
        for row in out:
            row["base_level"] = chosen["level"]
        experiments.write_rows(RESULTS / "sensitivity.csv", out)
    record = manifest(args, scenario) | {"seconds": round(time.time() - started)}
    (RESULTS / f"manifest_{stage}.json").write_text(json.dumps(record, indent=1, default=str))


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    pt = sub.add_parser("fetch-papertrade", help="pull papertrade.xyz Exchange logs from HyperEVM")
    pt.add_argument("--to-block", type=int, help="last block; defaults to the manifest's, else the latest")
    sub.add_parser("fetch-markets", help="pull public price history and volume for the launch markets")
    sub.add_parser("derive-papertrade", help="decode papertrade positions into the behaviour dataset")
    sub.add_parser("derive-markets", help="build the hourly price dataset, market statistics, and mix")
    for name in (*STAGES, "all"):
        sim = sub.add_parser(name, help=f"run the {name} stage" if name != "all" else "run every stage and the report")
        sim.add_argument("--workers", type=int, default=8)
        sim.add_argument("--paths", type=int, help="paths per run; defaults per stage")
        sim.add_argument("--seed", type=int, default=1)
        sim.add_argument("--batch-paths", type=int, help="paths per worker job; smaller uses less memory")
    sub.add_parser("report", help="write results/report.md from the stage outputs")
    args = parser.parse_args()

    if args.command == "fetch-papertrade":
        from fetch import papertrade

        papertrade.fetch(RAW / "papertrade", args.to_block)
    elif args.command == "fetch-markets":
        from fetch import markets

        markets.fetch(RAW / "markets")
    elif args.command == "derive-papertrade":
        from derive import papertrade

        papertrade.derive(RAW / "papertrade", RAW / "markets", DERIVED)
    elif args.command == "derive-markets":
        from derive import markets

        markets.derive(RAW / "markets", DERIVED)
    elif args.command in STAGES:
        simulate_stage(args.command, args)
    elif args.command == "all":
        for stage in STAGES:
            simulate_stage(stage, args)
        from model import report

        report.write(DERIVED, RESULTS)
    elif args.command == "report":
        from model import report

        report.write(DERIVED, RESULTS)


if __name__ == "__main__":
    main()
