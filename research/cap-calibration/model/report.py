"""Writes `results/report.md`: the data summary and every results table, from the stage outputs."""

import csv
import json
import math
from pathlib import Path

from model.experiments import SCALES, TARGETS, binding_point, chosen_level

MEASURE_NAMES = {
    "p_ever_tail": "NAV below zero at some hour",
    "p_short_tail": "house cash below zero (the vault short of client balances) at some hour",
    "p_end_tail": "NAV below zero at day 90",
}


def usd(x) -> str:
    if x is None or x == "":
        return "-"
    x = float(x)
    sign = "-" if x < 0 else ""
    x = abs(x)
    for unit, scale in (("B", 1e9), ("M", 1e6), ("k", 1e3)):
        if x >= scale:
            return f"{sign}${x / scale:.3g}{unit}"
    return f"{sign}${x:.0f}"


def pct(x, digits: int = 2) -> str:
    x = max(float(x), 0.0)  # refused shares can come out a rounding error below zero
    if x == 0:
        return "0"
    if x < 0.0001:
        return "<0.01%"
    return f"{100 * x:.{digits}f}%"


def table(header: list[str], rows: list[list]) -> str:
    lines = ["| " + " | ".join(header) + " |", "|" + "|".join("---" for _ in header) + "|"]
    lines += ["| " + " | ".join(str(c) for c in row) + " |" for row in rows]
    return "\n".join(lines)


def read(path: Path) -> list[dict]:
    """Rows with every numeric field as a float."""
    with path.open() as fh:
        rows = list(csv.DictReader(fh))
    for row in rows:
        for k, v in row.items():
            try:
                row[k] = float(v)
            except ValueError:
                pass
    return rows


def data_section(derived: Path) -> str:
    s = json.loads((derived / "papertrade_summary.json").read_text())
    rows = [
        ["Window", f"{s['from'][:16]} to {s['to'][:16]} UTC ({s['days']} days), blocks {s['blocks'][0]:,} to {s['blocks'][1]:,}"],
        ["Accounts, positions", f"{s['accounts']:,} accounts, {s['positions']:,} positions ({s['closed']:,} closed, {pct(s['liquidated_share'])} liquidated)"],
        ["Opened notional", f"{usd(s['opened_notional_usd'])}, {pct(s['btc_share_of_notional'], 1)} BTC"],
        ["Hold (count) p10 / p50 / p90 / p99", " / ".join(f"{v * 3600:.0f}s" if v < 1 / 30 else f"{v * 60:.0f}min" if v < 1 else f"{v:.1f}h" for v in s["hold_hours_quantiles_by_count"].values())],
        ["Exposure-time held over 1h / 24h", f"{pct(s['exposure_time_share_held_over_1h'], 1)} / {pct(s['exposure_time_share_held_over_24h'], 1)}"],
        ["Leverage (notional-weighted) p10 / p50 / p90", " / ".join(f"{v:.0f}x" for v in s["leverage_quantiles_by_notional"].values())],
        ["Hedged positions: share of notional / exposure-time", f"{pct(s['hedged_share_of_notional'], 1)} / {pct(s['hedged_share_of_exposure_time'], 1)}"],
        ["Exposure offset within accounts / market-wide", f"{pct(s['offset_within_accounts_share'], 1)} / {pct(s['offset_market_wide_share'], 1)}"],
        ["Hedged flow's cost per $1 of loss (haircut and fee on the winning leg)", f"${s['hedged_cost_per_usd_of_loss']:.3f}"],
        ["Winners keep, of raw gains", pct(s["winner_keeps_share_of_raw_gain"], 1)],
        ["Traders' raw PnL over notional (all / directional)", f"{s['raw_pnl_over_notional_bps']:.2f} / {s['raw_pnl_over_notional_bps_directional']:.2f} bps"],
        ["Directional long share: mean, hourly AR(1), residual sd, slope on last hour's return", f"{s['long_share_mean']:.3f}, {s['long_share_ar1']:.2f}, {s['long_share_resid_sd']:.3f}, {s['long_share_slope_on_prev_hour_return']:.2f}"],
        ["Deposits / withdrawals / net", f"{usd(s['deposits_usd'])} / {usd(s['withdrawals_usd'])} / {usd(s['net_deposits_usd'])}"],
        ["Checks", f"{s['unmatched_closes']} unmatched closes; fee / adjusted PnL median {s['fee_over_adjusted_pnl_median']}; BTC entry vs Binance hour close median gap {pct(s['btc_entry_vs_binance_hour_close_median_gap'], 3)}"],
    ]
    out = ["## papertrade.xyz behaviour", "", table(["Measure", "Value"], rows), ""]
    daily = read(derived / "papertrade_daily.csv")
    out += ["Daily flow:", "", table(
        ["Date", "Full day", "Positions", "Notional", "Margin", "Active accounts", "Hedged share", "Deposits", "Withdrawals"],
        [[d["date"], d["full_day"], f"{d['positions']:,.0f}", usd(d["opened_notional_usd"]), usd(d["opened_margin_usd"]), f"{d['active_accounts']:,.0f}", pct(d["hedged_share"], 1), usd(d["deposits_usd"]), usd(d["withdrawals_usd"])] for d in daily],
    ), ""]
    return "\n".join(out)


def market_section(derived: Path) -> str:
    stats = read(derived / "market_stats.csv")
    mix = {r["symbol"]: r for r in read(derived / "market_mix.csv")}
    rows = [
        [
            r["symbol"],
            f"{float(r['margin_bps']) / 100:g}%",
            pct(mix[r["symbol"]]["share"], 3) + ("" if mix[r["symbol"]]["listed"] == "True" else " *"),
            pct(r["vol_ann_window"], 0),
            pct(r["vol_ann_long"], 0) if not math.isnan(r["vol_ann_long"]) else "-",
            pct(r["daily_abs_p99_window"], 1),
            pct(r["gap_abs_p99"], 2),
            pct(r["gap_abs_max"], 1),
            f"{r['gaps_past_half_margin_per_year']:g}",
            f"{r['gaps_past_margin_per_year']:g}",
            pct(r["hourly_abs_max"], 1),
            pct(r["daily_abs_max_long"], 1) if not math.isnan(r["daily_abs_max_long"]) else "-",
        ]
        for r in stats
    ]
    header = ["Market", "Margin", "Mix", "Vol (window)", "Vol (long)", "Daily p99", "Gap p99", "Gap max", "Gaps past margin/2 a year", "Gaps past margin a year", "Hour max", "Day max (long)"]
    return "\n".join(["## Launch markets", "", table(header, rows), "", "`*` no Hyperliquid listing: the asset class's smallest listed volume.", ""])


def per_market_section(results: Path) -> str:
    sweep = read(results / "per_market_sweep.csv")
    rows = []
    for symbol in dict.fromkeys(r["symbol"] for r in sweep):
        mine = sorted((r for r in sweep if r["symbol"] == symbol), key=lambda r: r["floor"])
        bind = binding_point(mine)
        at_bind = next((r for r in mine if r["floor"] == bind), mine[-1])
        rows.append([
            symbol,
            usd(bind) if bind is not None else "> $100M",
            pct(at_bind["p_ever_tail"], 3),
            pct(at_bind["p_short_tail"], 3),
            pct(at_bind["refused_share_week1"], 1),
            usd(at_bind["bad_debt_mean"]),
            usd(at_bind["fee_mean"]),
        ])
    header = ["Market", "Refusals end at", "NAV<0 ever", "Cash<0", "Refused, week 1", "Bad debt mean", "Fees mean"]
    intro = "Each market alone at its share of the flow, at the floor where its 90-day refused share falls under 1%. With a zero floor nothing opens."
    return "\n".join(["## Each market alone", "", intro, "", table(header, rows), ""])


JOINT_HEADER = ["Total floor", "NAV<0 ever", "Cash<0", "Queue", "NAV<0 day 90", "NAV<-$100k", "NAV<-$1M", "First dip p50 (day)", "Refused, week 1", "Refused, 90 days", "NAV day 90 p50", "Lowest NAV p1", "Bad debt mean"]


def joint_row(r: dict) -> list:
    return [
        usd(r["total_floor"]),
        pct(r["p_ever_tail"], 3),
        pct(r["p_short_tail"], 3),
        pct(r["p_queue"], 3),
        pct(r["p_end_tail"], 3),
        pct(r["p_dip_100k"], 2),
        pct(r["p_dip_1m"], 2),
        f"{r['first_negative_day_p50']:.1f}" if r["first_negative_day_p50"] >= 0 else "-",
        pct(r["refused_share_week1"], 1),
        pct(r["refused_share_directional"], 1),
        usd(r["nav_end_p50"]),
        usd(r["min_nav_p01"]),
        usd(r["bad_debt_mean"]),
    ]


def joint_section(results: Path) -> str:
    out = ["## All markets together", ""]
    for measure in ("p_ever_tail", "p_short_tail"):
        path = results / f"joint_{measure}.csv"
        if not path.exists():
            continue
        rows = sorted(read(path), key=lambda r: r["total_floor"])
        body = [["binding" if r["level"] == 1.0 else pct(r["level"], 2)] + joint_row(r) for r in rows]
        out += [f"### Floors set for {MEASURE_NAMES[measure]}", "", table(["Level"] + JOINT_HEADER, body), ""]
        chosen = {t: chosen_level(rows, measure, t) for t in TARGETS}
        lines = [
            f"- {pct(t)}: "
            + (f"level {pct(c['level'], 2)}, total floor {usd(c['total_floor'])}, refused {pct(c['refused_share_week1'], 1)} in week 1" if c else "no level meets it; the smallest level gives " + pct(rows[0][measure], 3))
            for t, c in chosen.items()
        ]
        out += ["Chosen per target:", "", *lines, ""]
        symbols = list(json.loads(rows[0]["market_floors"]))
        body = [[s] + [usd(json.loads(c["market_floors"])[s]) if c else "-" for c in chosen.values()] for s in symbols]
        body.append(["Total"] + [usd(c["total_floor"]) if c else "-" for c in chosen.values()])
        out += [table(["Market"] + [f"Floor at {pct(t)}" for t in TARGETS], body), ""]
        total_path = results / f"joint_total_{measure}.csv"
        if total_path.exists():
            totals = sorted(read(total_path), key=lambda r: r["total_floor"])
            body = [[f"x{r['level']:g}"] + joint_row(r) for r in totals]
            out += [
                f"Total floor below the sum, at level {pct(totals[0]['base_level'], 2)}'s market floors:",
                "",
                table(["Share of sum"] + JOINT_HEADER, body),
                "",
            ]
    return "\n".join(out)


def sensitivity_section(results: Path) -> str:
    path = results / "sensitivity.csv"
    if not path.exists():
        return ""
    rows = read(path)
    names = list(dict.fromkeys(r["scenario"] for r in rows))
    body = []
    for name in names:
        mine = {r["floor_scale"]: r for r in rows if r["scenario"] == name}
        base = mine[1.0]
        largest = lambda key: max((s for s in SCALES if mine[float(s)][key] <= 0.01), default=None)  # noqa: E731
        body.append([
            name,
            pct(base["p_ever_tail"], 3),
            pct(base["p_short_tail"], 3),
            pct(base["p_dip_100k"], 2),
            pct(base["refused_share_week1"], 1),
            usd(base["nav_end_p50"]),
            usd(base["fee_mean"]),
            usd(base["bad_debt_mean"]),
            " / ".join(f"x{v:g}" if v is not None else "none" for v in (largest("p_ever_tail"), largest("p_short_tail"))),
        ])
    first = rows[0]
    measure, level, met = first["base_measure"], first["base_level"], first["base_meets_target"] == "True"
    base = f"the floors chosen for {MEASURE_NAMES[measure]} at 1% (level {pct(level, 2)})" if met else "zero floors, as no level met 1%"
    header = ["Scenario", "NAV<0 ever", "Cash<0", "NAV<-$100k", "Refused, week 1", "NAV day 90 p50", "Fees mean", "Bad debt mean", "Largest scale at 1%: NAV / cash"]
    scales = ", ".join(f"x{s:g}" for s in SCALES)
    intro = f"Every scenario at {base}, and with every floor scaled together by {scales}."
    return "\n".join(["## Sensitivity", "", intro, "", table(header, body), ""])


def write(derived: Path, results: Path) -> None:
    parts = [
        "# Cap calibration results",
        "",
        "Generated by `run.py report` from `data/derived/` and `results/`. Probabilities are over 90 days; refused shares are of opening notional, directional flow and churn.",
        "",
        data_section(derived),
        market_section(derived),
    ]
    if (results / "per_market_sweep.csv").exists():
        parts.append(per_market_section(results))
    parts.append(joint_section(results))
    parts.append(sensitivity_section(results))
    (results / "report.md").write_text("\n".join(parts))
