# Per-market ceiling representation — proposal, not approval

Store the immutable ceiling separately from mutable market specifications, keyed by the
genesis market ID. Require one explicit entry for every listed market and retain the pinned
catalogue revision and calibration provenance outside the protocol as deployment evidence.
Do not supply a default when an entry or its supporting evidence is missing.

An illustrative configuration shape is an array of `{ market_id, max_close_spread_bps }` entries.
The final names and serialization belong to #112's versioned design, not this research pipeline.
Reject duplicates, unknown IDs, missing IDs and values outside 0–9,999 nominal bps. This numerical
domain is an arithmetic validity limit, not a recommended economic range. Zero needs an
explicit rule for free closes; no nonzero charge can meet a strict zero ceiling.

`UpsertMarket` must never replace, erase or raise an existing registry entry. Reusing an ID,
changing quote currency or changing the underlying reference identity must not silently inherit
an unrelated calibration. New markets without adopted ceilings should be refused rather than
inheriting a class-wide or global fallback. The mechanism for adopting a new market's identity
and ceiling is a separate policy question; allowing the ordinary operator to assign any new
ceiling would undermine the original operator-power protection.

The current generic symbols and calendars do not identify every actual production reference
contract/roll algorithm. Resolve those references before declaring the calibrated registry
production-ready. Routine approved calendar corrections should not require changing a ceiling,
but changing what an instrument represents requires a new review of the calibration.

## Global charged parameters need an explicit choice

Current stress minimums and toxicity tiers are global. A pure close pays no size spread, and
liquidation waives toxicity. Therefore:

| Choice | Result | Status |
|---|---|---|
| Refuse a global `SetParams` when its maximum close charge exceeds any market's ceiling | The strictest market ceiling bounds the global charged parameters. This fits #112's current rejection wording and adds no new volatility-responsive charging mechanism. | Recommended conservative starting point, subject to owner adoption and precision rules |
| Clamp the actual charged close spread separately for each market | A low-cap market can pay less while a high-cap market pays more under the same stress level. | Additional execution behavior; requires an explicit decision and versioned accounting tests |
| Add per-market or volatility-responsive charged spreads | Charged prices can respond differently by market. | Separate design and product decision; this study does not invent it |

Market volatility cannot select a unique ceiling without an objective: the allowed trader exit
burden and tolerated house loss must be chosen after reviewing the conditional frontier. A
market moving 100 bps does not mean the house charged a 100-bps spread.

## Protect actual prices and define reversal treatment

Whole `Fill.spread_bps` truncates, so checking it alone can miss an excess. The exact nominal
inequality is `adjustment_raw * 10_000 <= index_raw * ceiling_bps`, using checked wide arithmetic.
Existing upward rounding can exceed that inequality, especially for a very small index: a
one-atom buy adjustment at a one-atom index is 10,000 bps even when the configured nominal
charge is one bp. Decide whether the ceiling permits a disclosed one-atom rounding allowance
or whether the final adjustment must be floored/clamped to the exact bound. Neither policy is
silently approved by these fixtures. A strict bound may make a dust close free.

The current reversal has one blended fill price: its opening slice's size cost is divided by
the entire fill quantity. Protecting just stress/toxicity would leave that size cost allocated
to the closing slice in realised PnL. Choose explicitly between splitting the execution into
closing/opening fills, or refusing a reversal whose actual blended closing cost exceeds the cap
while keeping a separate pure close available. This study does not count a size spread on a
pure close and does not claim that changing the event/accounting model is already approved.

## Development, migration and promotion

Keep synthetic dev/Anvil ceilings explicit and labelled as test fixtures. Exercise unequal
market values, missing/duplicate entries, update/ID bypass attempts, close/reversal/liquidation,
FX conversion and tiny-price rounding. Synthetic fixture values are not production estimates.
Production promotion requires adopted per-market values with evidence and the recorded risk
objective. Preserve #112's already-approved notice, activation, replacement and signed
initialization semantics; research does not change them. Add a new protocol version when #112
is ready, using the coordinator's then-current version map, and preserve every old reference log.
