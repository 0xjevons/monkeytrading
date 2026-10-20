//! Account close-out and negative balance protection.
//!
//! Margin is shared across an account's positions. When equity falls below the close-out level of
//! the margin requirement, the largest losing position that can trade right now is closed, and so
//! on until the account is back above the level. A loss beyond the account's balance is written
//! off against the house once the account is flat.

use crate::Fixed;
use crate::account::Position;
use crate::engine::Engine;
use crate::event::Event;
use crate::execution::{Exec, PriceGuard};
use crate::price::compute_index;
use crate::types::{AccountId, MarketId, Side};

impl Engine {
    /// Closes out every account holding a position in `market_id` that has fallen below the
    /// close-out level.
    ///
    /// TODO: this scans every account on each price update; index accounts by market before the
    /// account count makes the scan a cost on the sequencer path.
    pub(crate) fn check_closeouts(&mut self, market_id: MarketId, events: &mut Vec<Event>) {
        let level_bps = self.params.as_ref().expect("callers check params are set").closeout_level_bps;
        let accounts: Vec<AccountId> = self
            .accounts
            .iter()
            .filter(|(_, acct)| acct.positions.contains_key(&market_id))
            .map(|(account, _)| *account)
            .collect();
        for account in accounts {
            self.close_out(account, level_bps, events);
        }
    }

    /// Writes off a flat account's negative balance against the house.
    pub(crate) fn settle_bad_debt(&mut self, account: AccountId, events: &mut Vec<Event>) {
        let Some(acct) = self.accounts.get_mut(&account) else { return };
        if acct.positions.is_empty() && acct.balance.is_negative() {
            let amount = -acct.balance;
            acct.balance = Fixed::ZERO;
            self.house_cash -= amount;
            events.push(Event::BadDebt { account, amount });
        }
    }

    /// Closes the account's positions, largest loser first, while it is below `level_bps` of its
    /// margin requirement, then settles any bad debt.
    fn close_out(&mut self, account: AccountId, level_bps: i64, events: &mut Vec<Event>) {
        let mut announced = false;
        while let Some((market, pos, equity, margin)) = self.next_closeout(account, level_bps) {
            if !announced {
                events.push(Event::CloseOut { account, equity, margin });
                announced = true;
            }
            let order_id = self.alloc_order_id();
            let exec = Exec {
                account,
                order_id,
                market,
                side: if pos.qty.is_positive() { Side::Sell } else { Side::Buy },
                qty: pos.qty.abs(),
                reduce_only: true,
                guard: PriceGuard::None,
                liquidation: true,
            };
            match self.execute(exec) {
                Ok(fill) => self.after_fill(fill, events),
                Err(_) => break,
            }
        }
        self.settle_bad_debt(account, events);
    }

    /// The position to close next, with the account's equity and margin, or `None` when the
    /// account is at or above the close-out level or nothing it holds can trade now.
    fn next_closeout(&self, account: AccountId, level_bps: i64) -> Option<(MarketId, Position, Fixed, Fixed)> {
        let acct = self.accounts.get(&account)?;
        let (equity, margin) = self.equity_and_margin(acct, None);
        if margin.is_zero() || equity >= margin.mul_bps(level_bps) {
            return None;
        }
        acct.positions
            .iter()
            .filter(|(market_id, _)| self.tradeable_now(**market_id))
            .filter_map(|(market_id, pos)| Some((pos.unrealised(self.markets[market_id].mark?), *market_id, *pos)))
            .min_by_key(|(upnl, market_id, _)| (*upnl, *market_id))
            .map(|(_, market_id, pos)| (market_id, pos, equity, margin))
    }

    /// Whether a fill in `market_id` could happen now: open, with a valid index.
    fn tradeable_now(&self, market_id: MarketId) -> bool {
        let market = &self.markets[&market_id];
        market.open && compute_index(&market.quotes, &market.spec.price_rules, self.now).is_ok()
    }
}
