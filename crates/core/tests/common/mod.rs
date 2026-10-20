//! Shared harness for the protocol tests: a fixed genesis, a sequencer that stamps commands like
//! the node's, and helpers that read events back.

// Each test binary uses a different subset of these helpers.
#![allow(dead_code)]

pub mod reference;

use std::collections::BTreeMap;

use chrono::TimeZone;
use engine_core::auth::{ClientAuth, RequestAuth};
use engine_core::command::{AdminCommand, Command, OrderKind, OrderRequest, Sequenced};
use engine_core::eip712::{self, B256, Eip712Domain, cancel_hash, order_hash, withdrawal_hash};
use engine_core::event::{Event, Fill, Reject};
use engine_core::market::{FinancingSpec, MarketSpec, PriceRules, Tier};
use engine_core::params::{ProtocolParams, Rollover, StressLevel, StressTrigger, ToxicityParams, ToxicityTier};
use engine_core::price::SourceQuote;
use engine_core::protocol::ProtocolVersion;
use engine_core::session::Schedule;
use engine_core::types::{AccountId, AssetClass, MarketId, Millis, OrderId, Side};
use engine_core::{Engine, Fixed};
use k256::ecdsa::SigningKey;

pub const BTC: MarketId = 1;
pub const ETH: MarketId = 2;
pub const AAPL: MarketId = 3;
/// A market id nothing lists.
pub const UNLISTED: MarketId = 99;
pub const HOUSE_CAPITAL: i64 = 100_000_000;
/// The custody chain and vault the signed harness records.
pub const CHAIN_ID: u64 = 31_337;
pub const VAULT: AccountId = AccountId([0xaa; 20]);

/// A sequenced engine with three markets and 100m of house capital, plus a record of the log and
/// of the cash it credited.
pub struct Harness {
    pub engine: Engine,
    pub log: Vec<Sequenced>,
    pub ts: Millis,
    pub deposits: Fixed,
    /// Keys `cmd` signs unsigned client requests with, by account.
    pub signers: BTreeMap<AccountId, SigningKey>,
}

impl Harness {
    /// Genesis: parameters, BTC and ETH (24/7, 10% margin), AAPL (US hours, 20% margin), and house
    /// capital, at Tue 20 Oct 2026 10:00 New York.
    pub fn new() -> Self {
        let mut h = Harness::empty();
        h.genesis();
        h
    }

    /// `new`'s genesis under `V3`, after recording `auth`.
    pub fn v3(auth: ClientAuth) -> Self {
        let mut h = Harness::empty();
        h.admin(AdminCommand::SetClientAuth(auth));
        h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V3 });
        h.genesis();
        h
    }

    /// No commands yet, at the harness start time.
    fn empty() -> Self {
        Harness { engine: Engine::new(), log: vec![], ts: start(), deposits: Fixed::ZERO, signers: BTreeMap::new() }
    }

    /// `V3` with signed requests under `CHAIN_ID` and `VAULT`.
    pub fn signed() -> Self {
        Harness::v3(ClientAuth::Eip712 { chain_id: CHAIN_ID, vault: VAULT })
    }

    /// The account of the key from `seed`, whose unsigned requests `cmd` signs from now on.
    pub fn signer(&mut self, seed: u8) -> AccountId {
        let client = Client::new(seed);
        self.signers.insert(client.account, client.key);
        client.account
    }

    /// Parameters, three markets, and house capital.
    fn genesis(&mut self) {
        self.admin(AdminCommand::SetParams(params()));
        self.admin(AdminCommand::UpsertMarket(market(BTC, "BTC-USD", AssetClass::Crypto, Schedule::AlwaysOpen, 1_000)));
        self.admin(AdminCommand::UpsertMarket(market(ETH, "ETH-USD", AssetClass::Crypto, Schedule::AlwaysOpen, 1_000)));
        self.admin(AdminCommand::UpsertMarket(market(AAPL, "AAPL", AssetClass::Equity, us_equities(), 2_000)));
        self.cmd(Command::HouseCapital { amount: Fixed::from_int(HOUSE_CAPITAL), chain_ref: "genesis".into() });
    }

    /// Sequences and applies one command at the current time, recording it in the log. An unsigned
    /// client request from a `signer` account is signed first, with its next nonce and an expiry a
    /// minute ahead.
    pub fn cmd(&mut self, cmd: Command) -> Vec<Event> {
        let cmd = self.sign(cmd);
        let sequenced = Sequenced { seq: self.log.len() as u64, ts: self.ts, cmd };
        let events = self.engine.apply(&sequenced).expect("in sequence");
        self.log.push(sequenced);
        events
    }

    /// `cmd` signed by its account's key, if it is an unsigned client request from a `signer`.
    fn sign(&self, cmd: Command) -> Command {
        let domain = eip712::domain(CHAIN_ID, VAULT);
        let expires_at = self.ts + 60_000;
        let auth = |account: AccountId, hash: &dyn Fn(u64) -> B256| {
            let key = self.signers.get(&account)?;
            let nonce = self.engine.next_request_nonce(account);
            Some(RequestAuth { nonce, expires_at, signature: eip712::sign(key, &hash(nonce)) })
        };
        match cmd {
            Command::PlaceOrder(mut req) if req.auth.is_none() => {
                req.auth = auth(req.account, &|nonce| order_hash(&domain, &req, nonce, expires_at));
                Command::PlaceOrder(req)
            }
            Command::CancelOrder { account, order_id, auth: None } => {
                let auth = auth(account, &|nonce| cancel_hash(&domain, account, order_id, nonce, expires_at));
                Command::CancelOrder { account, order_id, auth }
            }
            Command::Withdraw { account, amount, auth: None } => {
                let auth = auth(account, &|nonce| withdrawal_hash(&domain, account, amount, nonce, expires_at));
                Command::Withdraw { account, amount, auth }
            }
            cmd => cmd,
        }
    }

    /// Applies an admin command that must succeed.
    pub fn admin(&mut self, cmd: AdminCommand) {
        let events = self.cmd(Command::Admin(cmd));
        assert!(events.contains(&Event::AdminApplied), "admin rejected: {events:?}");
    }

    /// Moves the sequencer clock forward by `ms`.
    pub fn advance(&mut self, ms: Millis) {
        self.ts += ms;
    }

    /// Moves the sequencer clock forward to `ts`.
    pub fn advance_to(&mut self, ts: Millis) {
        assert!(ts >= self.ts);
        self.ts = ts;
    }

    /// Two venues quoting the same mid, stamped now.
    pub fn price(&mut self, market: MarketId, mid: &str) -> Vec<Event> {
        self.quotes(market, &[mid, mid])
    }

    /// One venue per mid, stamped now.
    pub fn quotes(&mut self, market: MarketId, mids: &[&str]) -> Vec<Event> {
        let quotes = mids
            .iter()
            .enumerate()
            .map(|(i, mid)| SourceQuote { source: format!("venue{i}"), mid: f(mid), ts: self.ts })
            .collect();
        self.cmd(Command::Prices { market, quotes })
    }

    /// Credits a deposit under a fresh chain reference.
    pub fn deposit(&mut self, account: AccountId, amount: &str) {
        let chain_ref = format!("0xdeposit{}", self.log.len());
        let events = self.cmd(Command::Deposit { account, amount: f(amount), chain_ref });
        assert!(matches!(events[..], [Event::Deposited { .. }]), "deposit rejected: {events:?}");
        self.deposits += f(amount);
    }

    /// Places an order with client id 7.
    pub fn order(
        &mut self,
        account: AccountId,
        market: MarketId,
        side: Side,
        qty: &str,
        kind: OrderKind,
    ) -> Vec<Event> {
        self.cmd(Command::PlaceOrder(OrderRequest {
            account,
            client_id: 7,
            market,
            side,
            qty: f(qty),
            kind,
            reduce_only: false,
            auth: None,
        }))
    }

    /// Places a market order quoted at `quoted`, with the house's slippage bound.
    pub fn market_order(
        &mut self,
        account: AccountId,
        market: MarketId,
        side: Side,
        qty: &str,
        quoted: &str,
    ) -> Vec<Event> {
        self.order(account, market, side, qty, OrderKind::Market { quoted: f(quoted), max_slippage_bps: None })
    }

    /// The account's cash balance.
    pub fn balance(&self, account: AccountId) -> Fixed {
        self.engine.accounts()[&account].balance
    }

    /// Cash is never created or destroyed: client balances plus house cash equal deposits plus
    /// house capital, less approved withdrawals.
    pub fn assert_conserved(&self, withdrawn: Fixed) {
        let balances: Fixed = self.engine.accounts().values().map(|a| a.balance).sum();
        let expected = self.deposits + Fixed::from_int(HOUSE_CAPITAL) - withdrawn;
        assert_eq!(balances + self.engine.house_cash(), expected);
    }
}

/// A client key, the account it controls, and the domain it signs under.
pub struct Client {
    pub key: SigningKey,
    pub account: AccountId,
    pub domain: Eip712Domain,
    /// The expiry every request this client signs carries.
    pub expires_at: Millis,
}

impl Client {
    /// A distinct key per `seed`, signing under the signed harness's domain, with requests that
    /// expire a minute after the harness starts.
    pub fn new(seed: u8) -> Self {
        let key = SigningKey::from_slice(&[seed; 32]).expect("a valid scalar");
        let account = eip712::address(key.verifying_key());
        Client { key, account, domain: eip712::domain(CHAIN_ID, VAULT), expires_at: start() + 60_000 }
    }

    /// `req` with this key's signature over it and `nonce`, whatever account it names.
    pub fn order(&self, nonce: u64, mut req: OrderRequest) -> Command {
        let hash = order_hash(&self.domain, &req, nonce, self.expires_at);
        req.auth = Some(self.auth(nonce, hash));
        Command::PlaceOrder(req)
    }

    /// A signed cancel of `order_id` for this account.
    pub fn cancel(&self, nonce: u64, order_id: OrderId) -> Command {
        let hash = cancel_hash(&self.domain, self.account, order_id, nonce, self.expires_at);
        Command::CancelOrder { account: self.account, order_id, auth: Some(self.auth(nonce, hash)) }
    }

    /// A signed withdrawal of `amount` for this account.
    pub fn withdraw(&self, nonce: u64, amount: &str) -> Command {
        let hash = withdrawal_hash(&self.domain, self.account, f(amount), nonce, self.expires_at);
        Command::Withdraw { account: self.account, amount: f(amount), auth: Some(self.auth(nonce, hash)) }
    }

    /// This key's signature over `hash`, with `nonce` and this client's expiry.
    fn auth(&self, nonce: u64, hash: B256) -> RequestAuth {
        RequestAuth { nonce, expires_at: self.expires_at, signature: eip712::sign(&self.key, &hash) }
    }
}

/// Parses a decimal.
pub fn f(s: &str) -> Fixed {
    s.parse().unwrap()
}

/// A distinct account per `n`.
pub fn acct(n: u8) -> AccountId {
    AccountId([n; 20])
}

/// 10% profit fee, 50% close-out, one stress level tripped at 10bps divergence, a 200bps move, or
/// 80% cap use, and a 3bps toxicity add-on from a 20bps score.
pub fn params() -> ProtocolParams {
    ProtocolParams {
        profit_fee_bps: 1_000,
        closeout_level_bps: 5_000,
        house_total_cap_bps: 20_000,
        stress_levels: vec![
            StressLevel { size_spread_multiplier_bps: 10_000, min_spread_bps: 0, trigger: None },
            StressLevel {
                size_spread_multiplier_bps: 20_000,
                min_spread_bps: 5,
                trigger: Some(StressTrigger {
                    divergence_bps: Some(10),
                    move_bps: Some(200),
                    utilisation_bps: Some(8_000),
                }),
            },
        ],
        stress_move_window_ms: 60_000,
        stress_cooldown_ms: 300_000,
        toxicity: ToxicityParams {
            horizon_ms: 1_000,
            ewma_weight_bps: 5_000,
            tiers: vec![ToxicityTier { min_score_bps: 20, extra_spread_bps: 3 }],
        },
        rollover: Rollover { tz: chrono_tz::Europe::London, hour: 22, minute: 0 },
    }
}

/// A market with one margin rate, size spreads of 0 / 2 / 5 bps at 1m and 5m, a 20bps slippage
/// bound, and a house cap of 50% of NAV.
pub fn market(id: MarketId, symbol: &str, class: AssetClass, schedule: Schedule, margin_bps: i64) -> MarketSpec {
    MarketSpec {
        id,
        symbol: symbol.into(),
        asset_class: class,
        schedule,
        margin_tiers: vec![Tier { up_to: None, bps: margin_bps }],
        spread_tiers: vec![
            Tier { up_to: Some(Fixed::from_int(1_000_000)), bps: 0 },
            Tier { up_to: Some(Fixed::from_int(5_000_000)), bps: 2 },
            Tier { up_to: None, bps: 5 },
        ],
        price_rules: PriceRules { max_age_ms: 1_000, max_divergence_bps: 50, min_sources: 2 },
        max_slippage_bps: 20,
        financing: FinancingSpec { benchmark: "SOFR".into(), markup_bps: 250, day_basis: 365 },
        house_cap_bps: 5_000,
        min_order_notional: Fixed::ZERO,
    }
}

/// US equity exchange hours, Monday to Friday.
pub fn us_equities() -> Schedule {
    let windows: Vec<_> = ["Mon", "Tue", "Wed", "Thu", "Fri"]
        .iter()
        .map(|d| serde_json::json!({ "open": format!("{d} 09:30"), "close": format!("{d} 16:00") }))
        .collect();
    serde_json::from_value(serde_json::json!({ "kind": "weekly", "tz": "America/New_York", "windows": windows }))
        .unwrap()
}

/// Tue 20 Oct 2026 10:00 New York (US equities open).
pub fn start() -> Millis {
    ny(2026, 10, 20, 10, 0)
}

/// A New York wall-clock time as epoch millis.
pub fn ny(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> Millis {
    chrono_tz::America::New_York.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap().timestamp_millis()
}

/// The fills among `events`.
pub fn fills(events: &[Event]) -> Vec<&Fill> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::Fill(fill) => Some(fill),
            _ => None,
        })
        .collect()
}

/// The order rejection among `events`; panics if there is none.
pub fn rejection(events: &[Event]) -> &Reject {
    events
        .iter()
        .find_map(|e| match e {
            Event::OrderRejected { reason, .. } => Some(reason),
            _ => None,
        })
        .unwrap_or_else(|| panic!("expected a rejection, got {events:?}"))
}
