//! The operator's telemetry config: the metrics listener and the alert webhooks.

use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use anyhow::{anyhow, bail};
use reqwest::Url;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricsSection {
    /// Operator address for `GET /metrics` in Prometheus text format; keep it off the public API's
    /// interface.
    pub listen: SocketAddr,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AlertsSection {
    /// An open market that a configured feed prices raises a Critical `market_no_price` alert after
    /// this long with no usable price, and a sequencer that answers no monitor sample for this long
    /// raises `sequencer_stalled`.
    #[serde(with = "humantime_serde")]
    pub no_price_after: Duration,
    /// An alert that repeats with no resolve line pages again only after it has been quiet this
    /// long.
    #[serde(with = "humantime_serde")]
    pub episode_quiet: Duration,
    /// Where Critical alerts are posted. Without it they are only logged.
    pub critical: Option<WebhookRoute>,
    /// Where Warning alerts are posted. Without it they are only logged.
    pub warning: Option<WebhookRoute>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebhookRoute {
    /// The environment variable holding the webhook URL, which is a credential and stays out of
    /// config files.
    pub url_env: String,
}

/// A URL that may carry credentials. `Debug` hides it; read it with `expose` where it is used.
pub struct WebhookUrl(Url);

impl fmt::Debug for WebhookUrl {
    /// Never prints the URL.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WebhookUrl(<redacted>)")
    }
}

impl WebhookUrl {
    /// Reads the URL from the environment variable `var`, looked up through `env`. Plain HTTP is
    /// accepted only to this machine: the host controls the network.
    /// Error cases:
    /// - `var` is unset, or its value is not a URL
    /// - the scheme is not HTTPS, unless it is HTTP to a loopback address
    pub fn from_env(var: &str, env: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let value = env(var).ok_or_else(|| anyhow!("environment variable {var} is not set"))?;
        // `url::ParseError` names the problem, never the input, so nothing secret reaches a log.
        let url = Url::parse(&value).map_err(|e| anyhow!("{var}: {e}"))?;
        let loopback = url
            .host_str()
            .and_then(|host| host.trim_start_matches('[').trim_end_matches(']').parse::<IpAddr>().ok())
            .is_some_and(|ip| ip.is_loopback());
        match url.scheme() {
            "https" => Ok(Self(url)),
            "http" if loopback => Ok(Self(url)),
            _ => bail!("{var}: must be an https URL, or http to a loopback address"),
        }
    }

    /// The URL, for the one place that connects with it.
    pub fn expose(&self) -> &Url {
        &self.0
    }
}
