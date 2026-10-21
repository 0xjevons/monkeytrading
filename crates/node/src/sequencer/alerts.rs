//! The alerts the sequencer raises: for engine rejections an operator must look at, and for a house
//! in deficit.

use engine_core::Fixed;
use engine_core::event::Event;
use tokio::time::Instant;
use tracing::{debug, error, warn};

use crate::throttle::Throttle;

/// Whether house NAV was below zero after the last command, and the throttle on the alert for it.
#[derive(Default)]
pub(super) struct DeficitAlert {
    in_deficit: bool,
    throttle: Throttle,
}

impl DeficitAlert {
    /// Alerts when house NAV `nav` falls below zero, at most once per `REALERT_AFTER` while it keeps
    /// crossing zero, as a house that starts with no capital does with every price. Trading carries
    /// on in a deficit, but client balances then exceed what the house can cover, which only the
    /// operator can repair by adding capital.
    pub(super) fn note(&mut self, nav: Fixed) {
        if nav.is_negative() == self.in_deficit {
            return;
        }
        self.in_deficit = nav.is_negative();
        if !self.in_deficit {
            debug!(%nav, "house NAV back at or above zero");
        } else if let Some(suppressed) = self.throttle.ready(Instant::now()) {
            warn!(alert = "house_deficit", %nav, suppressed, "house NAV below zero; trading continues within the cap floors");
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
            warn!(alert = "prices_rejected", market, reason, "price update refused");
        }
        Event::BadDebt { account, amount } => {
            warn!(alert = "bad_debt", %account, %amount, "loss beyond balance written off against the house");
        }
        _ => {}
    }
}
