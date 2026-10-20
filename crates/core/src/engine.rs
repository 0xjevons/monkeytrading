//! The engine state machine: `apply(sequenced command) -> events`.
//!
//! Single-threaded and deterministic. Time comes only from the sequencer's stamp. This file holds
//! the state, dispatch, time, and prices; funds and financing live in `funds.rs`, admin and stress
//! in `admin.rs`, orders and pricing in `execution.rs`, and close-outs in `closeout.rs`.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::{Deserialize, Serialize};
use sha3::{Digest, Keccak256};

use crate::Fixed;
use crate::account::Account;
use crate::command::{Command, Sequenced};
use crate::event::Event;
use crate::market::MarketSpec;
use crate::params::ProtocolParams;
use crate::price::{MAX_QUOTE_LEAD_MS, SourceQuote, compute_index};
use crate::types::{AccountId, MarketId, Millis, OrderId, Side};

/// Clamp on a single markout, so one wild print cannot dominate an account's toxicity score.
const MAX_MARKOUT_BPS: i64 = 1_000;

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum SequenceError {
    #[error("expected seq {expected}, got {got}")]
    Gap { expected: u64, got: u64 },
    #[error("time went backwards: {last} -> {got}")]
    TimeReversed { last: Millis, got: Millis },
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Engine {
    pub(crate) params: Option<ProtocolParams>,
    pub(crate) markets: BTreeMap<MarketId, MarketState>,
    pub(crate) accounts: BTreeMap<AccountId, Account>,
    /// House cash. NAV is this minus clients' unrealised PnL.
    pub(crate) house_cash: Fixed,
    /// Annual benchmark rates in bps, keyed by name ("SOFR", ...).
    pub(crate) benchmarks: BTreeMap<String, i64>,
    /// Custody references already credited (deposits and house capital), so none applies twice.
    pub(crate) credited_refs: BTreeSet<String>,
    pub(crate) global_stress_floor: usize,
    pub(crate) next_order_id: OrderId,
    pub(crate) next_rollover: Option<Millis>,
    pub(crate) markouts: VecDeque<Markout>,
    pub(crate) last_seq: Option<u64>,
    pub(crate) now: Millis,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MarketState {
    pub spec: MarketSpec,
    pub quotes: BTreeMap<String, SourceQuote>,
    pub open: bool,
    pub reduce_only: bool,
    /// Last valid index mid. Positions are marked here, including while the market is closed.
    pub mark: Option<Fixed>,
    /// Sum of client position qty; the house holds the opposite.
    pub client_qty: Fixed,
    /// Sum of client `entry * qty`, so unrealised PnL is O(1) per market.
    pub client_cost: Fixed,
    pub stress: StressState,
    /// Resting orders in time priority.
    pub resting: BTreeMap<OrderId, AccountId>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StressState {
    /// Effective level: max(auto, floor, global floor).
    pub level: usize,
    pub auto: usize,
    pub auto_since: Millis,
    pub floor: usize,
    pub move_ref: Option<(Millis, Fixed)>,
}

/// A fill waiting to be marked against the index once its horizon has passed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Markout {
    pub due: Millis,
    pub account: AccountId,
    pub market: MarketId,
    pub side: Side,
    pub price: Fixed,
}

impl MarketState {
    /// A newly listed market: closed until its first session check, with no price or positions.
    pub(crate) fn new(spec: MarketSpec) -> Self {
        MarketState {
            spec,
            quotes: BTreeMap::new(),
            open: false,
            reduce_only: false,
            mark: None,
            client_qty: Fixed::ZERO,
            client_cost: Fixed::ZERO,
            stress: StressState::default(),
            resting: BTreeMap::new(),
        }
    }
}

impl Engine {
    /// An empty engine, before genesis.
    pub fn new() -> Self {
        Self::default()
    }

    /// Applies one sequenced command and returns the events it produced. A refused command is an
    /// event, not an error; an error means the sequencer broke its own contract.
    /// Error cases:
    /// - `seq` is not exactly one more than the last applied (or 0 for the first)
    /// - `ts` is earlier than the last applied
    pub fn apply(&mut self, sequenced: &Sequenced) -> Result<Vec<Event>, SequenceError> {
        let expected = self.last_seq.map_or(0, |n| n + 1);
        if sequenced.seq != expected {
            return Err(SequenceError::Gap { expected, got: sequenced.seq });
        }
        if sequenced.ts < self.now {
            return Err(SequenceError::TimeReversed { last: self.now, got: sequenced.ts });
        }
        self.last_seq = Some(sequenced.seq);
        self.now = sequenced.ts;

        let mut events = Vec::new();
        self.run_rollovers(&mut events);
        match &sequenced.cmd {
            Command::Prices { market, quotes } => self.on_prices(*market, quotes, &mut events),
            Command::Tick => self.refresh_all_sessions(&mut events),
            Command::Deposit { account, amount, chain_ref } => self.deposit(*account, *amount, chain_ref, &mut events),
            Command::Withdraw { account, amount } => self.withdraw(*account, *amount, &mut events),
            Command::HouseCapital { amount, chain_ref } => self.house_capital(*amount, chain_ref, &mut events),
            Command::PlaceOrder(req) => self.place_order(req, &mut events),
            Command::CancelOrder { account, order_id } => self.cancel_order(*account, *order_id, &mut events),
            Command::Admin(cmd) => match self.admin(cmd, &mut events) {
                Ok(()) => events.push(Event::AdminApplied),
                Err(reason) => events.push(Event::AdminRejected { reason }),
            },
        }
        // After the command, so a price arriving at the horizon is the one marked against.
        self.process_markouts();
        Ok(events)
    }

    /// Keccak-256 of the canonical state encoding. Posted on-chain at checkpoints; anyone replaying
    /// the log must get the same root.
    pub fn state_root(&self) -> [u8; 32] {
        // Every field is a map, list, string, or integer, all of which serde_json encodes.
        let bytes = serde_json::to_vec(self).expect("engine state serialises");
        Keccak256::digest(bytes).into()
    }

    /// Seq of the last applied command, or `None` before genesis.
    pub fn last_seq(&self) -> Option<u64> {
        self.last_seq
    }

    /// Sequenced time of the last applied command.
    pub fn now(&self) -> Millis {
        self.now
    }

    /// The protocol parameters, once set.
    pub fn params(&self) -> Option<&ProtocolParams> {
        self.params.as_ref()
    }

    /// Every listed market, by id.
    pub fn markets(&self) -> &BTreeMap<MarketId, MarketState> {
        &self.markets
    }

    /// Every account that has ever been credited, by id.
    pub fn accounts(&self) -> &BTreeMap<AccountId, Account> {
        &self.accounts
    }

    /// House cash, before clients' unrealised PnL.
    pub fn house_cash(&self) -> Fixed {
        self.house_cash
    }

    /// House cash minus clients' unrealised PnL at current marks.
    pub fn house_nav(&self) -> Fixed {
        let client_upnl: Fixed = self
            .markets
            .values()
            .filter_map(|market| market.mark.map(|mark| mark.mul(market.client_qty) - market.client_cost))
            .sum();
        self.house_cash - client_upnl
    }

    /// Sum over markets of the house's absolute net notional.
    pub fn house_gross_exposure(&self) -> Fixed {
        self.markets.values().filter_map(|market| market.mark.map(|mark| market.client_qty.abs().mul(mark))).sum()
    }

    /// (equity, margin requirement) at current marks, optionally overriding one market's mark.
    pub fn equity_and_margin(&self, acct: &Account, mark_override: Option<(MarketId, Fixed)>) -> (Fixed, Fixed) {
        acct.positions
            .iter()
            .filter_map(|(market_id, pos)| self.markets.get(market_id).map(|market| (*market_id, pos, market)))
            .fold((acct.balance, Fixed::ZERO), |(equity, margin), (market_id, pos, market)| {
                let mark = match mark_override {
                    Some((id, mid)) if id == market_id => mid,
                    _ => market.mark.unwrap_or(pos.entry),
                };
                (equity + pos.unrealised(mark), margin + market.spec.margin_for(pos.qty.abs().mul(mark)))
            })
    }

    /// Updates a market's open flag from its schedule, emitting the transition.
    pub(crate) fn refresh_session(&mut self, market_id: MarketId, events: &mut Vec<Event>) {
        let now = self.now;
        let Some(market) = self.markets.get_mut(&market_id) else { return };
        let open = market.spec.schedule.is_open(now);
        if open == market.open {
            return;
        }
        market.open = open;
        if open {
            market.stress.move_ref = None;
            events.push(Event::MarketOpened { market: market_id });
        } else {
            events.push(Event::MarketClosed { market: market_id });
        }
    }

    /// Refreshes every market's session, so opens and closes fire even without price traffic.
    fn refresh_all_sessions(&mut self, events: &mut Vec<Event>) {
        let ids: Vec<MarketId> = self.markets.keys().copied().collect();
        for market_id in ids {
            self.refresh_session(market_id, events);
        }
    }

    /// Stores venue quotes, then, if the market is open and priced, marks it, updates stress,
    /// triggers resting orders, and checks close-outs. Malformed quotes, and quotes stamped more than
    /// `MAX_QUOTE_LEAD_MS` after the sequenced time, are refused one by one.
    fn on_prices(&mut self, market_id: MarketId, quotes: &[SourceQuote], events: &mut Vec<Event>) {
        let now = self.now;
        let Some(market) = self.markets.get_mut(&market_id) else {
            events.push(Event::PricesRejected { market: market_id, reason: "unknown market".into() });
            return;
        };
        for quote in quotes {
            if !quote.is_well_formed() {
                let reason = format!("malformed quote from {}", quote.source);
                events.push(Event::PricesRejected { market: market_id, reason });
                continue;
            }
            let lead_ms = quote.ts.saturating_sub(now);
            if lead_ms > MAX_QUOTE_LEAD_MS {
                // The venue's clock disagrees with the sequencer's, so its stored quote's age is
                // unknown too: the venue has no price until it sends a quote within the limit.
                market.quotes.remove(&quote.source);
                let reason = format!("quote from {} stamped {lead_ms}ms ahead of the sequenced time", quote.source);
                events.push(Event::PricesRejected { market: market_id, reason });
                continue;
            }
            // A stored quote stamped after the sequenced time does not count yet, so it never holds
            // back a replacement.
            if market.quotes.get(&quote.source).is_none_or(|old| quote.ts >= old.ts || old.ts > now) {
                market.quotes.insert(quote.source.clone(), quote.clone());
            }
        }
        self.refresh_session(market_id, events);
        let market = &self.markets[&market_id];
        if !market.open || self.params.is_none() {
            return;
        }
        let Ok(index) = compute_index(&market.quotes, &market.spec.price_rules, self.now) else { return };
        self.markets.get_mut(&market_id).expect("market exists").mark = Some(index.mid);
        self.update_stress(market_id, index.mid, index.divergence_bps, events);
        self.trigger_resting(market_id, events);
        self.check_closeouts(market_id, events);
    }

    /// Marks every fill whose horizon has passed and folds the markout into its account's score.
    fn process_markouts(&mut self) {
        let Some(params) = &self.params else { return };
        let weight = params.toxicity.ewma_weight_bps;
        while self.markouts.front().is_some_and(|markout| markout.due <= self.now) {
            let markout = self.markouts.pop_front().expect("front exists");
            let mark = self.markets.get(&markout.market).and_then(|market| market.mark);
            let (Some(mark), Some(acct)) = (mark, self.accounts.get_mut(&markout.account)) else {
                continue;
            };
            let bps = (mark - markout.price)
                .mul_int(markout.side.sign())
                .bps_of(markout.price)
                .clamp(-MAX_MARKOUT_BPS, MAX_MARKOUT_BPS);
            acct.toxicity_bps += (bps - acct.toxicity_bps) * weight / 10_000;
        }
    }
}
