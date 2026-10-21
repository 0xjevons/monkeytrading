"""The simulation's accounting on one deterministic week of one market.

The history is a single week, so every path replays it. Each hour 1 USD of margin opens a long at
200:1 (200 of notional, a balance of 1) that the model closes an hour later.
"""

import numpy as np
import pytest

from model import prices, simulate
from model.flow import Flow, FlowClass

LEVERAGE = 200.0


def bars(gap_at: int | None = None, gap: float = 0.0) -> np.ndarray:
    b = np.zeros((prices.WEEK, 1, 4), dtype=np.int32)
    if gap_at is not None:
        b[gap_at, 0, 0] = round(np.log1p(gap) * 1e6)
    return b


def market(fin: float = 0.0) -> simulate.Markets:
    return simulate.Markets(
        symbols=["TEST"],
        margin_rate=np.array([1 / LEVERAGE]),
        cap_bps=np.array([5000]),
        total_cap_bps=20000,
        fin_long=np.array([fin]),
        fin_short=np.array([fin]),
        mix=np.array([1.0]),
        open_hours_per_week=np.array([168.0]),
    )


def flow(kind: str = "directional") -> Flow:
    return Flow(
        classes=[FlowClass(kind=kind, hold_h=1, bucket=0, margin_share=1.0, mean_sqrt_hold_h=1.0, long_share=1.0)],
        bucket_leverage=np.array([LEVERAGE]),
        daily_margin=np.array([24.0]),
        long_share_mean=1.0,
        long_share_ar1=0.0,
        long_share_resid_sd=0.0,
        long_share_slope=0.0,
    )


def run(b, m, f, floor=1e12, **overrides):
    settings = dict(paths=1, batch_paths=1, days=7, block_weeks=1, demean=False, withdraw_on_exit=0.0, idle_withdraw_per_day=0.0, run_withdraw_per_day=0.0)
    scenario = simulate.Scenario(**(settings | overrides))
    out = simulate.run_batch(scenario, m, b, f, np.array([[floor]]), np.array([1e12]), seed=0, n_paths=1)
    return {k: v[0, 0] if np.ndim(v) >= 2 else v for k, v in out.items()}


def test_a_flat_path_conserves_cash():
    out = run(bars(), market(), flow())
    assert out["nav_end"] == pytest.approx(0.0, abs=1e-9)
    assert out["fee"] == out["bad_debt"] == out["financing"] == 0.0
    assert out["refused"] == 0.0
    assert float(out["identity_gap"]) < 1e-9
    # Margin is recycled: the first hour's deposit funds every later position.
    assert out["deposits"] == pytest.approx(1.0)


def test_financing_is_the_only_income_on_a_flat_path():
    rate = 1e-4
    out = run(bars(), market(fin=rate), flow())
    # 200 of notional is open across each 22:00 rollover; 7 rollovers in the week.
    assert out["financing"] == pytest.approx(7 * 200 * rate)
    assert out["nav_end"] == pytest.approx(out["financing"])


def test_a_gap_through_the_balance_is_bad_debt():
    out = run(bars(gap_at=50, gap=-0.01), market(), flow())
    # The long opened at hour 49 is closed out at the 1% gap: it loses its balance of 1 and the
    # other 1 of the 2 loss is written off.
    assert out["bad_debt"] == pytest.approx(1.0, rel=1e-4)
    assert out["mint_basis"] == pytest.approx(1.0, rel=1e-4)
    assert out["nav_end"] == pytest.approx(1.0, rel=1e-4)
    assert out["fee"] == 0.0


def test_a_winning_close_pays_the_profit_fee():
    out = run(bars(gap_at=50, gap=0.01), market(), flow())
    # The long opened at hour 49 closes at hour 50, 1% up: a gain of 2 that pays 0.2.
    assert out["fee"] == pytest.approx(0.2, rel=1e-4)
    assert out["nav_end"] == pytest.approx(-1.8, rel=1e-4)


def test_the_cap_refuses_the_crowded_side_past_the_floor():
    out = run(bars(), market(), flow(), floor=100.0)
    # Each hour wants 200 long against a 100 cap at zero NAV: half is refused.
    assert out["refused"] / out["desired"] == pytest.approx(0.5)


def test_a_pair_never_moves_exposure_and_gaps_cost_the_house():
    out = run(bars(gap_at=50, gap=-0.01), market(), flow("hedged"), floor=0.0)
    assert out["refused"] == 0.0
    # Legs of 100 each: the long loses its balance of 0.5 and 0.5 more is bad debt, the short
    # gains 1 and pays 0.1.
    assert out["bad_debt"] == pytest.approx(0.5, rel=1e-4)
    assert out["fee"] == pytest.approx(0.1, rel=1e-4)
    assert out["nav_end"] == pytest.approx(-0.4, rel=1e-4)


def test_a_shortfall_queues_the_withdrawal_the_vault_cannot_pay():
    out = run(bars(gap_at=50, gap=0.01), market(), flow(), withdraw_on_exit=1.0)
    # The winner is owed 2.8 (balance 1 plus 1.8 net profit). Its 1 funds the next position and
    # the vault holds that position's 1, so 1 is paid and 0.8 waits.
    assert out["queued"]
    assert out["max_queue"] == pytest.approx(0.8, rel=1e-4)
    assert float(out["identity_gap"]) < 1e-9


def test_the_shortfall_is_house_cash_past_the_margin_in_positions():
    # After the winning close the house owes 1.8 against the 1 of margin the vault holds.
    out = run(bars(gap_at=50, gap=0.01), market(), flow())
    assert out["min_cover"] == pytest.approx(-0.8, rel=1e-4)
    assert run(bars(), market(), flow())["min_cover"] >= 0.0
