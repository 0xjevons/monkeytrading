//! The per-version invariant checks run after every generated command: the `V14` resting-order
//! cap, placement isolation, and unrated-benchmark financing, the `V15` below-minimum resting
//! reduction, and the `V16` toxicity-history memory.

use std::collections::BTreeSet;

use engine_core::Engine;
use engine_core::command::{Command, OrderKind};
use engine_core::event::Event;
use engine_core::fixed::Rounding;
use engine_core::types::MarketId;

/// Fails unless, under `V14`, a resting order was accepted only below the cap in force, placing an
/// order filled or cancelled no other order, and no market the events record as having no benchmark
/// rate was financed. What a command does before it acts, a rollover's close-outs or bringing
/// markets to the sequenced time, comes before the placed order's acceptance, which the check leaves
/// aside.
/// Error cases:
/// - an accepted limit or stop order left its account above the cap
/// - after its acceptance, an order placement filled or cancelled another order
/// - a financing charge in a market a missing-rate record names
pub(crate) fn check_v14(engine: &Engine, cmd: &Command, events: &[Event]) -> Result<(), String> {
    if let Command::PlaceOrder(req) = cmd {
        let accepted = events.iter().position(|event| matches!(event, Event::OrderAccepted { .. }));
        let placed = accepted.and_then(|at| match events[at] {
            Event::OrderAccepted { order_id, .. } => Some(order_id),
            _ => None,
        });
        let other = events[accepted.map_or(events.len(), |at| at + 1)..].iter().find(|event| match event {
            Event::Fill(fill) => Some(fill.order_id) != placed,
            Event::OrderCancelled { order_id, .. } => Some(*order_id) != placed,
            _ => false,
        });
        if let Some(event) = other {
            return Err(format!("placing an order also produced {event:?}"));
        }
        let rests = matches!(req.kind, OrderKind::Limit { .. } | OrderKind::Stop { .. });
        let cap = engine.params().map_or(0, |params| params.max_resting_orders);
        let resting = engine.accounts().get(&req.account).map_or(0, |acct| acct.orders.len());
        if rests && placed.is_some() && resting > cap {
            return Err(format!("{} rests {resting} orders, above the cap of {cap}", req.account));
        }
    }
    let unrated: BTreeSet<MarketId> = events
        .iter()
        .flat_map(|event| match event {
            Event::BenchmarkRateMissing { markets, .. } => markets.clone(),
            _ => Vec::new(),
        })
        .collect();
    match events.iter().find(|event| matches!(event, Event::Financing { market, .. } if unrated.contains(market))) {
        Some(event) => Err(format!("{event:?} in a market with no benchmark rate")),
        None => Ok(()),
    }
}

/// Fails unless, under `V15`, a limit or stop order that still rests after it was placed, and is
/// neither reduce-only nor meeting its market's minimum notional at its own price — valued as
/// `validate_order` values it, in USD at that price in a market that converts at its own index and
/// at its FX market's mark in any other, an FX market with no mark leaving no dollar value — can
/// only reduce its account's position in that market: opposite side, quantity at most the
/// position. An order that filled or was cancelled as it was placed no longer rests, and what it
/// did is governed by the fill's own checks; the account's position is the placement's. Returns
/// true when such a below-minimum resting reduction was seen, so the run can require the path.
/// Error cases:
/// - a resting order accepted below the minimum, or with no dollar value, could add risk
pub(crate) fn check_v15(engine: &Engine, cmd: &Command, events: &[Event]) -> Result<bool, String> {
    let Command::PlaceOrder(req) = cmd else { return Ok(false) };
    let price = match req.kind {
        OrderKind::Limit { price } | OrderKind::Stop { price } => price,
        OrderKind::Market { .. } => return Ok(false),
    };
    if req.reduce_only {
        return Ok(false);
    }
    let Some(order_id) = events.iter().find_map(|event| match event {
        Event::OrderAccepted { order_id, .. } => Some(*order_id),
        _ => None,
    }) else {
        return Ok(false);
    };
    let rests = engine.accounts().get(&req.account).is_some_and(|acct| acct.orders.contains_key(&order_id));
    if !rests {
        return Ok(false);
    }
    let market = &engine.markets()[&req.market];
    let currency = market.spec.quote_currency;
    let fx_of = |fx_market: MarketId| {
        if fx_market == req.market { Some(price) } else { engine.markets()[&fx_market].mark }
    };
    let below_min = match currency.fx_market().map(fx_of) {
        Some(None) => true,
        fx => currency.to_usd(req.qty.mul(price), fx.flatten(), Rounding::Down) < market.spec.min_order_notional,
    };
    if !below_min {
        return Ok(false);
    }
    let pos = engine.accounts()[&req.account].position(req.market);
    let reduces_only = pos.qty.signum() == -req.side.sign() && req.qty <= pos.qty.abs();
    if !reduces_only {
        return Err(format!(
            "{order_id} rests below the {} minimum in market {} without reducing its account's position",
            market.spec.min_order_notional, req.market
        ));
    }
    Ok(true)
}

/// Fails unless every account's toxicity history is at most the memory in force, which `V16`
/// clamps it to when parameters set a lower one.
/// Error cases:
/// - an account's toxicity history exceeds the memory in force
pub(crate) fn check_v16(engine: &Engine) -> Result<(), String> {
    let Some(memory) = engine.params().and_then(|params| params.toxicity.notional).map(|weighting| weighting.memory)
    else {
        return Ok(());
    };
    engine.accounts().values().try_for_each(|acct| {
        if acct.toxicity_history > memory {
            return Err(format!("toxicity history {} exceeds the memory {memory}", acct.toxicity_history));
        }
        Ok(())
    })
}
