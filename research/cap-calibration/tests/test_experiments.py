"""The calibration's probability estimates and floor choice."""

import numpy as np
import pytest

from model import experiments


def test_tail_probability_reads_the_sample_when_it_has_enough_events():
    values = np.concatenate([np.full(50, -1.0), np.full(950, 1.0)])
    assert experiments.tail_probability(values) == pytest.approx(0.05)


def test_tail_probability_extrapolates_an_exponential_tail():
    # P(10 - E < 0) = exp(-10) for a unit exponential E: about 4.5e-5, too rare for 20,000 paths.
    values = 10.0 - np.random.default_rng(0).exponential(1.0, 20_000)
    assert experiments.tail_probability(values) == pytest.approx(np.exp(-10), rel=0.5)


def test_tail_probability_falls_back_to_the_rule_of_three():
    values = np.concatenate([np.zeros(500), np.full(500, np.inf)])  # no tail below the cut to fit
    assert experiments.tail_probability(values) == pytest.approx(3 / 1000)


def sweep(points):
    return [{"floor": f, "p": p, "refused_share_directional": r} for f, p, r in points]


def test_a_level_takes_the_largest_floor_before_the_first_miss():
    rows = sweep([(0, 0.0, 0.3), (1, 0.001, 0.2), (2, 0.02, 0.1), (3, 0.0, 0.0)])
    assert experiments.floor_at_level(rows, "p", 0.01) == 1
    assert experiments.floor_at_level(sweep([(0, 0.5, 0.3), (1, 0.6, 0.0)]), "p", 0.01) == 0


def test_a_level_never_sets_a_floor_past_the_binding_point():
    # Refusals end at a floor of 2, so a level every floor meets stops there.
    rows = sweep([(0, 0.0, 0.3), (1, 0.0, 0.2), (2, 0.0, 0.005), (3, 0.0, 0.0)])
    assert experiments.floor_at_level(rows, "p", 0.01) == 2


def test_the_chosen_level_is_the_largest_total_floor_meeting_the_target():
    rows = [{"total_floor": f, "p": p} for f, p in ((0, 0.0), (10, 0.0005), (20, 0.003), (30, 0.0))]
    assert experiments.chosen_level(rows, "p", 0.001)["total_floor"] == 10
    assert experiments.chosen_level([{"total_floor": 0, "p": 0.5}], "p", 0.01) is None
