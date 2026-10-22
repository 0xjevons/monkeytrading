//! The alerts the sequencer raises: for engine rejections an operator must look at, and for a house
//! in deficit.

use engine_core::Fixed;
use engine_core::event::Event;
use tokio::time::Instant;
use tracing::{debug, error, info, warn};

use crate::throttle::{REALERT_AFTER, Throttle};

/// Whether house NAV was below zero after the last command, the throttle on the alert for it, and
/// the alerted deficit not yet resolved, with since when NAV has been back at or above zero.
#[derive(Default)]
pub(super) struct DeficitAlert {
    in_deficit: bool,
    throttle: Throttle,
    alerted: bool,
    recovered_since: Option<Instant>,
}

impl DeficitAlert {
    /// Alerts when house NAV `nav` falls below zero, at most once per `REALERT_AFTER` while it keeps
    /// crossing zero, as a house that starts with no capital does with every price. Trading carries
    /// on in a deficit, but client balances then exceed what the house can cover, which only the
    /// operator can repair by adding capital. Resolves the alert once NAV has stayed at or above
    /// zero for `REALERT_AFTER`, so a house crossing zero with every price does not churn pages.
    pub(super) fn note(&mut self, nav: Fixed) {
        let now = Instant::now();
        if !nav.is_negative() {
            if self.in_deficit {
                self.in_deficit = false;
                self.recovered_since = Some(now);
                debug!(%nav, "house NAV back at or above zero");
            }
            if self.alerted && self.recovered_since.is_some_and(|since| now.duration_since(since) >= REALERT_AFTER) {
                info!(alert_resolved = "house_deficit", "house NAV has stayed at or above zero: {nav}");
                self.alerted = false;
            }
            return;
        }
        self.recovered_since = None;
        if self.in_deficit {
            return;
        }
        self.in_deficit = true;
        if let Some(suppressed) = self.throttle.ready(now) {
            self.alerted = true;
            // The reading and count change with each crossing, so they go in the message, not the episode key.
            warn!(
                alert = "house_deficit",
                "house NAV below zero at {nav} ({suppressed} more since the last alert); trading continues within \
                 the cap floors"
            );
        }
    }
}

/// Raises an alert for an engine rejection an operator must look at.
pub(super) fn alert_on(event: &Event) {
    match event {
        // A finalised deposit the engine will not credit leaves user funds in the vault and
        // nothing re-examines it.
        Event::DepositRejected { account, amount, chain_ref, reason } => {
            error!(alert = "deposit_rejected", %account, %amount, chain_ref, ?reason, "deposit not credited");
        }
        Event::PricesRejected { market, reason } => {
            // The reason can carry a per-quote value, so it goes in the message, not the episode key.
            warn!(alert = "prices_rejected", market, "price update refused: {reason}");
        }
        Event::BadDebt { account, amount } => {
            warn!(alert = "bad_debt", %account, %amount, "loss beyond balance written off against the house");
        }
        _ => {}
    }
}
