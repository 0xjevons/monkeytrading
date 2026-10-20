//! Cash in and out of the engine: deposits, withdrawals, house capital, and overnight financing.
//!
//! Cash is conserved: client balances plus house cash always equal deposits plus house capital,
//! less approved withdrawals. Every movement below debits one side and credits the other.

use crate::Fixed;
use crate::account::Position;
use crate::auth::RequestAuth;
use crate::eip712::withdrawal_hash;
use crate::engine::Engine;
use crate::event::{Event, Reject};
use crate::protocol::ProtocolVersion;
use crate::types::{AccountId, MarketId};

impl Engine {
    /// Credits a finalised vault deposit, at most once per chain reference.
    pub(crate) fn deposit(&mut self, account: AccountId, amount: Fixed, chain_ref: &str, events: &mut Vec<Event>) {
        let reason = if !amount.is_valid_input() {
            Some(Reject::InvalidAmount)
        } else if self.credited_refs.contains(chain_ref) {
            Some(Reject::DuplicateReference)
        } else {
            None
        };
        if let Some(reason) = reason {
            events.push(Event::DepositRejected { account, amount, chain_ref: chain_ref.to_owned(), reason });
            return;
        }
        self.credited_refs.insert(chain_ref.to_owned());
        self.accounts.entry(account).or_default().balance += amount;
        events.push(Event::Deposited { account, amount });
    }

    /// Authorises the request, then approves a withdrawal of free cash: no more than the balance,
    /// and no more than equity above the margin requirement. Approval bumps the account's
    /// withdrawal nonce, which the enclave signs with the payout.
    pub(crate) fn withdraw(
        &mut self,
        account: AccountId,
        amount: Fixed,
        auth: Option<&RequestAuth>,
        events: &mut Vec<Event>,
    ) {
        let authorised = self.authorise(account, auth, |domain, auth| {
            withdrawal_hash(domain, account, amount, auth.nonce, auth.expires_at)
        });
        let available = self.accounts.get(&account).map_or(Fixed::ZERO, |acct| {
            let (equity, margin) = self.equity_and_margin(acct, None);
            acct.balance.min(equity - margin).max(Fixed::ZERO)
        });
        let reason = if let Err(reason) = authorised {
            Some(reason)
        } else if !amount.is_valid_input() {
            Some(Reject::InvalidAmount)
        } else if amount > available {
            Some(Reject::InsufficientFunds { available })
        } else {
            None
        };
        if let Some(reason) = reason {
            events.push(Event::WithdrawalRejected { account, amount, reason });
            return;
        }
        let acct = self.accounts.get_mut(&account).expect("an account with funds available exists");
        acct.balance -= amount;
        acct.withdrawal_nonce += 1;
        events.push(Event::WithdrawalApproved { account, amount, nonce: acct.withdrawal_nonce });
    }

    /// Moves house capital in (positive) or out (negative), at most once per chain reference, and
    /// never leaving house NAV at or below zero. From `V4`, a zero amount records its reference and
    /// moves nothing, so a genesis that starts the house with no capital still records its credit.
    pub(crate) fn house_capital(&mut self, amount: Fixed, chain_ref: &str, events: &mut Vec<Event>) {
        let recorded_zero = amount == Fixed::ZERO && self.protocol_version >= ProtocolVersion::V4;
        let reason = if !recorded_zero && !amount.abs().is_valid_input() {
            Some("house capital amount out of range")
        } else if self.credited_refs.contains(chain_ref) {
            Some("house capital reference already applied")
        } else if amount.is_negative() && !(self.house_nav() + amount).is_positive() {
            Some("house capital move would leave NAV <= 0")
        } else {
            None
        };
        if let Some(reason) = reason {
            events.push(Event::AdminRejected { reason: reason.into() });
            return;
        }
        self.credited_refs.insert(chain_ref.to_owned());
        self.house_cash += amount;
        events.push(Event::HouseCapitalMoved { amount, nav: self.house_nav() });
    }

    /// Charges financing for every rollover the sequenced time has passed. After downtime each
    /// missed day is charged in turn, at the marks the engine holds.
    pub(crate) fn run_rollovers(&mut self, events: &mut Vec<Event>) {
        let Some(params) = &self.params else { return };
        let rollover = params.rollover.clone();
        while let Some(at) = self.next_rollover.filter(|at| *at <= self.now) {
            self.charge_financing(events);
            self.next_rollover = Some(rollover.next_after(at));
        }
    }

    /// One day of overnight financing on every open position. Longs pay benchmark + markup; shorts
    /// receive benchmark - markup.
    fn charge_financing(&mut self, events: &mut Vec<Event>) {
        let charges: Vec<(AccountId, MarketId, Fixed)> = self
            .accounts
            .iter()
            .flat_map(|(account, acct)| acct.positions.iter().map(move |(market, pos)| (*account, *market, *pos)))
            .filter_map(|(account, market, pos)| {
                self.daily_financing(market, pos).map(|amount| (account, market, amount))
            })
            .filter(|(_, _, amount)| !amount.is_zero())
            .collect();
        for (account, market, amount) in charges {
            self.accounts.get_mut(&account).expect("account holds a position").balance -= amount;
            self.house_cash += amount;
            events.push(Event::Financing { account, market, amount });
        }
    }

    /// One day's financing on `pos`, positive when the account pays, or `None` when the market has
    /// no mark to value it at. A benchmark with no published rate counts as zero.
    fn daily_financing(&self, market_id: MarketId, pos: Position) -> Option<Fixed> {
        let market = self.markets.get(&market_id)?;
        let mark = market.mark?;
        let financing = &market.spec.financing;
        let benchmark = self.benchmarks.get(&financing.benchmark).copied().unwrap_or(0);
        let annual_bps =
            if pos.qty.is_positive() { benchmark + financing.markup_bps } else { financing.markup_bps - benchmark };
        Some(pos.qty.abs().mul(mark).mul_bps(annual_bps).div_int(financing.day_basis))
    }
}
