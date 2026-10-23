"""Build descriptive research figures and a per-market report from the pinned pipeline output."""

import csv
import json
import os
from pathlib import Path

os.environ.setdefault("MPLBACKEND", "Agg")
os.environ.setdefault("MPLCONFIGDIR", "/tmp/close-spread-matplotlib")
import matplotlib.pyplot as plt
import numpy as np

ROOT = Path(__file__).resolve().parent


def write_report(data):
    rows = []
    for item in data["markets"]:
        market = item["market"]
        margin_bps = market["margin_tiers"][0]["bps"]
        rows.append({
            "symbol": market["symbol"], "asset_class": market["asset_class"],
            "minimum_margin_bps": margin_bps,
            "configured_13bps_cost_percent_initial_margin_at_unchanged_price": 13 / margin_bps * 100,
            "calibration_hourly_abs_p99_bps": item["calibration"]["abs_hourly_p99_bps"],
            "validation_hourly_abs_p99_bps": item["validation"]["abs_hourly_p99_bps"],
            "transformed_validation_gap_max_bps": item["validation"]["abs_gap_max_bps"],
            "proposed_ceiling_bps": None, "status": item["status"],
        })
    with (ROOT / "results/market_summary.csv").open("w", newline="") as handle:
        writer = csv.DictWriter(handle, fieldnames=list(rows[0]), lineterminator="\n")
        writer.writeheader()
        writer.writerows(rows)
    lines = [
        "# Preliminary close-spread calibration", "",
        "**All 37 production recommendations remain blocked pending evidence. No ceiling is adopted.**", "",
        "The pipeline pins genesis and inherited source hashes. Hourly observations are transformed",
        "proxies, with selected gaps removed. They do not measure actual execution-horizon reference",
        "divergence, staleness, contract rolls or all corporate actions. Source and licensing gaps",
        "are listed in [the source audit](../source-audit.md).", "",
        f"Calibration: {data['week0_utc']} to {data['validation_start_utc']} exclusive.",
        f"Validation: {data['validation_start_utc']} to {data['end_exclusive_utc']} exclusive.", "",
        "A fixed ceiling limits operator power. The charged spread is a separate engine calculation.",
        "Current global parameters charge at most 13 nominal bps for a voluntary pure close and",
        "10 for liquidation, before raw-unit rounding. A pure close pays no size spread. Market",
        "volatility and gaps affect solvency and margin; they are not themselves spread charges.", "",
        "The same nominal cost consumes different shares of initial margin: 13 bps is 26% of a",
        "50-bps margin and 1.3% of a 1,000-bps margin at an unchanged price. Neither amount is a",
        "recommended ceiling. The conditional gap sweep separates trader cost from house shortfall,",
        "with zero/seeded capital and explicit hypothetical operator-charge changes. Scenario counts",
        "are not probabilities. Weekly bootstrap intervals capture only sampling sensitivity.", "",
        "![Inherited hourly tails](hourly_tails.png)", "",
        "![Configured charge versus minimum margin](margin_burden.png)", "",
        "| Market | Calibration hourly p99 (bps) | Validation hourly p99 (bps) | Proposed ceiling |",
        "|---|---:|---:|---|",
    ]
    for row in rows:
        lines.append(f"| {row['symbol']} | {row['calibration_hourly_abs_p99_bps']:.2f} | "
                     f"{row['validation_hourly_abs_p99_bps']:.2f} | Blocked: missing evidence |")
    lines += ["", "## Adoption gates", "",
              "Obtain instrument-specific raw/reference evidence, resolve its permitted use and",
              "audit session gaps, corporate actions and rolls. Select a trader-burden/house-loss",
              "objective after reviewing the frontier, then adopt per-market values explicitly.",
              "Missing evidence must remain missing; no uniform fallback is justified by this report.", "",
              "This initial risk model conditions on two existing isolated positions and one unseen",
              "price jump. It does not establish correlated portfolio or full cross-margin risk,",
              "admission capacity, continuous-path liquidation timing, or modelled probabilities.",
              "Its account-cost arithmetic is not a substitute for complete Engine::apply agreement.",
              "The separate shared V17 pricing vectors exercise actual close/reversal/liquidation",
              "and FX rounding. No protocol fixture or production config has been changed.", ""]
    (ROOT / "results/report.md").write_text("\n".join(lines))


def plots(data):
    items = data["markets"]
    labels = [item["market"]["symbol"] for item in items]
    y = np.arange(len(items))
    calibration = np.array([item["calibration"]["abs_hourly_p99_bps"] for item in items])
    validation = np.array([item["validation"]["abs_hourly_p99_bps"] for item in items])
    low = np.array([item["calibration"]["hourly_p99_week_block_bootstrap"]["p025_bps"] for item in items])
    high = np.array([item["calibration"]["hourly_p99_week_block_bootstrap"]["p975_bps"] for item in items])
    fig, ax = plt.subplots(figsize=(10, 12))
    ax.hlines(y, low, high, color="#5b75a6", alpha=.65, label="Calibration weekly bootstrap 2.5–97.5%")
    ax.scatter(calibration, y, label="Calibration hourly p99", s=20, color="#174d91")
    ax.scatter(validation, y, label="Validation hourly p99", s=20, marker="x", color="#ba4d22")
    ax.set(yticks=y, yticklabels=labels, xlabel="Absolute proxy bar return including encoded gaps (basis points)",
           title="Inherited bar tails: descriptive evidence only")
    ax.invert_yaxis()
    ax.grid(axis="x", alpha=.2)
    ax.legend(loc="upper right", fontsize=8)
    fig.tight_layout(rect=(0, .035, 1, 1))
    fig.text(.5, .008, "Bar returns may span closures; transformed gaps are not execution-horizon evidence.",
             ha="center", fontsize=8)
    fig.savefig(ROOT / "results/hourly_tails.png", dpi=150, metadata={"Software": "close-spread-calibration"})
    plt.close(fig)
    burden = [13 / item["market"]["margin_tiers"][0]["bps"] * 100 for item in items]
    fig, ax = plt.subplots(figsize=(10, 12))
    ax.barh(y, burden, color="#466e92")
    ax.set(yticks=y, yticklabels=labels, xlabel="13 nominal bps as % of initial margin at unchanged price",
           title="Current maximum close charge versus minimum margin")
    ax.invert_yaxis()
    ax.grid(axis="x", alpha=.2)
    fig.tight_layout(rect=(0, .035, 1, 1))
    fig.text(.5, .008, "Configured nominal charges, before raw-unit rounding. No recommended ceiling.",
             ha="center", fontsize=8)
    fig.savefig(ROOT / "results/margin_burden.png", dpi=150, metadata={"Software": "close-spread-calibration"})
    plt.close(fig)


if __name__ == "__main__":
    source = json.loads((ROOT / "results/preliminary.json").read_text())
    write_report(source)
    plots(source)
    print("37-market preliminary report and two research figures generated; no adoption")
