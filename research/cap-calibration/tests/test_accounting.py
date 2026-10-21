"""The model's engine rules against hand cases, several taken from the engine's own tests."""

import numpy as np
import pytest

from model import accounting


def test_profit_fee_is_ten_percent_of_gains_and_nothing_on_losses():
    # crates/core/src/execution.rs, profit_fee_only_on_gains
    assert accounting.profit_fee(100.0) == pytest.approx(10.0)
    assert accounting.profit_fee(-100.0) == 0.0
    assert accounting.profit_fee(0.0) == 0.0


def test_margin_charges_each_slice_at_its_band_rate():
    # crates/core/src/market.rs, staircase_charges_each_slice_at_its_band_rate
    tiers = [{"up_to": "1000000", "bps": 0}, {"up_to": "5000000", "bps": 2}, {"bps": 5}]
    assert accounting.margin_for(500_000, tiers) == pytest.approx(0.0)
    assert accounting.margin_for(2_000_000, tiers) == pytest.approx(200.0)
    assert accounting.margin_for(6_000_000, tiers) == pytest.approx(1_300.0)
    assert accounting.margin_for(-6_000_000, tiers) == pytest.approx(1_300.0)


def test_closeout_trigger_puts_equity_at_half_the_margin():
    # A 1,000 balance long 0.09 BTC at 100,000 with 10% margin (crates/core/tests/risk.rs).
    balance, notional = 1_000.0, 9_000.0
    x = accounting.closeout_trigger(1, balance / notional, 0.10)
    equity = balance + notional * (x - 1)
    margin = 0.10 * notional * x
    assert equity == pytest.approx(0.5 * margin)
    # 200:1 at the maximum leverage: a long closes out after a 0.2506% fall, a short after a
    # 0.2494% rise, since the requirement moves with the mark.
    for side, move in ((1, -0.002506), (-1, 0.002494)):
        x = accounting.closeout_trigger(side, 0.005, 0.005)
        equity = 0.005 + side * (x - 1)
        assert equity == pytest.approx(0.5 * 0.005 * x)
        assert x - 1 == pytest.approx(move, abs=1e-6)


def test_closeout_fills_at_the_open_after_a_gap_and_at_the_trigger_otherwise():
    trigger = 0.997
    assert accounting.closeout_fill(1, trigger, 0.99, 0.98) == pytest.approx(0.99)
    assert accounting.closeout_fill(1, trigger, 1.0, 0.99) == pytest.approx(trigger)
    assert np.isnan(accounting.closeout_fill(1, trigger, 1.0, 0.998))
    assert accounting.closeout_fill(-1, 1.003, 1.01, 1.02) == pytest.approx(1.01)
    assert accounting.closeout_fill(-1, 1.003, 1.0, 1.005) == pytest.approx(1.003)


def test_negative_balance_protection_writes_off_the_loss_past_the_balance():
    # crates/core/tests/risk.rs: a 20% gap on 0.09 BTC loses 1,800 on a 1,000 balance; the
    # engine books 803.6 of bad debt, which adds its 5 bps stress spread (3.6) that the model omits.
    client, bad = accounting.settle(1, 0.8, 1_000.0 / 9_000.0)
    assert client * 9_000 == pytest.approx(-1_000.0)
    assert bad * 9_000 == pytest.approx(800.0)
    client, bad = accounting.settle(-1, 0.97, 0.01)
    assert client == pytest.approx(0.03)
    assert bad == 0.0


def test_financing_charges_longs_benchmark_plus_markup_and_shorts_markup_minus_benchmark():
    # SOFR at 3.87% with the 2.5% index markup on a 360-day basis, one rollover on 1,000,000.
    assert accounting.daily_financing(1, 1_000_000, 387, 250, 360) == pytest.approx(176.94, abs=0.01)
    assert accounting.daily_financing(-1, 1_000_000, 387, 250, 360) == pytest.approx(-38.06, abs=0.01)


def test_exposure_cap_is_the_larger_of_the_nav_share_and_the_floor():
    assert accounting.exposure_cap(-5_000_000, 5000, 1_000_000) == 1_000_000
    assert accounting.exposure_cap(10_000_000, 5000, 1_000_000) == 5_000_000


def test_accepted_share_fills_the_reducing_side_and_the_crowded_side_up_to_the_cap():
    long_ok, short_ok = accounting.accepted_share(0.0, 100.0, 150.0, 20.0)
    assert (long_ok, short_ok) == (pytest.approx(120 / 150), 1.0)
    long_ok, short_ok = accounting.accepted_share(-90.0, 100.0, 0.0, 50.0)
    assert (long_ok, short_ok) == (1.0, pytest.approx(10 / 50))
    long_ok, short_ok = accounting.accepted_share(150.0, 100.0, 10.0, 0.0)  # already past the cap
    assert (long_ok, short_ok) == (0.0, 1.0)
    assert accounting.accepted_share(0.0, 100.0, 50.0, 40.0) == (1.0, 1.0)
