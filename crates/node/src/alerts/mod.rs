//! Alert routing. An alert is a log line at `error` (Critical) or `warn` (Warning) carrying
//! `alert = "<name>"`; a line at `info` or above carrying `alert_resolved = "<name>"` and the same
//! fields ends it. A tracing layer copies each such line onto a bounded queue and never waits, so
//! the code that logged it, the sequencer included, is not slowed. The router task groups lines into
//! episodes, keyed by name and fields, and queues one page when an episode starts and one when it
//! is resolved. Each routed severity has its own queue and delivery task, which posts its pages in
//! order to that severity's webhook.
//! Episodes live in memory: after a restart, an alert still firing pages again under the same key,
//! and one that cleared while the node was down sends no resolve. Pages still queued or retrying
//! when the shutdown grace runs out are lost; their alert lines remain in the log.
//!
//! The host controls the network and can drop pages. `ops/prometheus/engine-node-rules.yml` has the
//! operator's Prometheus page when scrapes stop or posts fail, which catches an outage but not a
//! hostile host: it can answer the plain-HTTP scrape itself and supplies the environment the
//! webhook URL comes from.
//!
//! TODO: bound a hostile host with signed metrics, or a heartbeat the enclave signs, and a webhook
//! URL released to the attested enclave rather than read from the host's environment.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use engine_core::types::Millis;
use serde::Serialize;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, oneshot};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{Instant, timeout};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Level, Metadata, Subscriber, error};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::{Extensions, LookupSpan};

mod episodes;
use crate::alerts::episodes::{Episodes, Routed};
mod webhook;
pub use crate::alerts::webhook::Routes;
use crate::alerts::webhook::{Queues, RETRY, client};
use crate::clock::now_ms;
use crate::metrics::{DeliveryLabels, Metrics, SeverityLabel};

/// Alert lines waiting for the router.
const LINE_QUEUE: usize = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Critical,
    Warning,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Firing,
    Resolved,
}

/// One alert or resolve line, as logged.
#[derive(Clone, Debug, PartialEq)]
pub struct AlertLine {
    pub name: String,
    /// `None` for a resolve line.
    pub severity: Option<Severity>,
    /// Every field but the message, sorted by name. They identify the episode.
    pub fields: Vec<(String, String)>,
    pub summary: String,
    /// The spans the line was logged in, outermost first, such as `command{id=42}`.
    pub context: String,
    /// Wall-clock time of the line.
    pub ts_ms: Millis,
}

/// What a webhook receives: an episode starting or ending. `key` is the same for both, so a
/// receiver can match a resolve to its alert and drop duplicates.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Page {
    pub status: Status,
    pub severity: Severity,
    pub alert: String,
    pub key: String,
    pub summary: String,
    pub fields: BTreeMap<String, String>,
    pub context: String,
    pub ts_ms: Millis,
}

/// Copies alert and resolve lines onto the router's queue. Install it with the filter
/// `is_alert_callsite`.
pub struct AlertLayer {
    lines: mpsc::Sender<AlertLine>,
    metrics: Arc<Metrics>,
}

/// Routing before it starts: the receiving end of the layer's queue, and where pages go.
pub struct Router {
    lines: mpsc::Receiver<AlertLine>,
    routes: Routes,
    metrics: Arc<Metrics>,
}

/// The running router and delivery tasks.
pub struct Routing {
    router: JoinHandle<()>,
    deliveries: JoinSet<()>,
    drain: Option<oneshot::Sender<()>>,
    metrics: Arc<Metrics>,
}

/// A span's fields as `name=value` pairs, recorded by the alert layer so a page's context does not
/// depend on what the log formatter's level keeps.
#[derive(Default)]
struct SpanFields(String);

/// Collects an event's fields, setting aside the message and the alert names.
#[derive(Default)]
struct LineFields {
    alert: Option<String>,
    resolved: Option<String>,
    summary: String,
    fields: Vec<(String, String)>,
}

impl<S> Layer<S> for AlertLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    /// Records a new span's fields, for the context of alerts logged inside it.
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let mut fields = SpanFields::default();
        attrs.record(&mut fields);
        span.extensions_mut().insert(fields);
    }

    /// Adds fields recorded after the span was created, such as an HTTP request's account.
    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        if let Some(fields) = span.extensions_mut().get_mut::<SpanFields>() {
            values.record(fields);
        }
    }

    /// Queues an alert or resolve line, or counts it as dropped when the queue is full.
    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let mut line = LineFields::default();
        event.record(&mut line);
        let severity = match *event.metadata().level() {
            Level::ERROR => Some(Severity::Critical),
            Level::WARN => Some(Severity::Warning),
            _ => None,
        };
        let (name, severity) = match (line.alert, line.resolved) {
            (Some(name), _) if severity.is_some() => (name, severity),
            (None, Some(name)) => (name, None),
            _ => return,
        };
        let context = ctx
            .event_scope(event)
            .map(|scope| scope.from_root().map(|span| span_label(span.name(), &span.extensions())).collect::<Vec<_>>())
            .unwrap_or_default()
            .join(":");
        let mut fields = line.fields;
        fields.sort();
        let line = AlertLine { name, severity, fields, summary: line.summary, context, ts_ms: now_ms() };
        if self.lines.try_send(line).is_err() {
            self.metrics.alerts_dropped.inc();
        }
    }
}

impl Visit for LineFields {
    /// Keeps strings unquoted.
    fn record_str(&mut self, field: &Field, value: &str) {
        self.put(field, value.to_owned());
    }

    /// Every other value, by its `Debug` (or, for `%` fields, `Display`) form.
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.put(field, format!("{value:?}"));
    }
}

impl Visit for SpanFields {
    /// Keeps strings unquoted.
    fn record_str(&mut self, field: &Field, value: &str) {
        self.push(field, value);
    }

    /// Every other value, by its `Debug` (or, for `%` fields, `Display`) form.
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.push(field, &format!("{value:?}"));
    }
}

impl AlertLine {
    /// The episode key: the name and fields, such as `market_no_price{market=1}`.
    pub fn key(&self) -> String {
        let fields: Vec<String> = self.fields.iter().map(|(name, value)| format!("{name}={value}")).collect();
        format!("{}{{{}}}", self.name, fields.join(","))
    }
}

impl Page {
    /// The page for `line` starting or ending its episode, whose key is `key`.
    fn new(line: AlertLine, key: String, status: Status, severity: Severity) -> Self {
        Page {
            status,
            severity,
            alert: line.name,
            key,
            summary: line.summary,
            fields: line.fields.into_iter().collect(),
            context: line.context,
            ts_ms: line.ts_ms,
        }
    }
}

impl Severity {
    /// The label value for metrics.
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Critical => "critical",
            Severity::Warning => "warning",
        }
    }
}

impl SpanFields {
    /// Appends one `name=value` pair.
    fn push(&mut self, field: &Field, value: &str) {
        if !self.0.is_empty() {
            self.0.push(' ');
        }
        self.0.push_str(field.name());
        self.0.push('=');
        self.0.push_str(value);
    }
}

impl LineFields {
    /// Files one field by name.
    fn put(&mut self, field: &Field, value: String) {
        match field.name() {
            "alert" => self.alert = Some(value),
            "alert_resolved" => self.resolved = Some(value),
            "message" => self.summary = value,
            name => self.fields.push((name.to_owned(), value)),
        }
    }
}

impl Router {
    /// Starts the router and delivery tasks.
    /// Error cases:
    /// - the webhook HTTP client cannot be built
    pub fn spawn(self, episode_quiet: Duration) -> anyhow::Result<Routing> {
        let mut deliveries = JoinSet::new();
        let queues = Queues::start(self.routes, client()?, &self.metrics, RETRY, &mut deliveries);
        let (drain_tx, drain_rx) = oneshot::channel();
        let episodes = Episodes::new(episode_quiet);
        let router = tokio::spawn(route(self.lines, queues, episodes, drain_rx, self.metrics.clone()));
        Ok(Routing { router, deliveries, drain: Some(drain_tx), metrics: self.metrics })
    }
}

impl Routing {
    /// Resolves if any task stops before `drain`, which only a panic causes.
    pub async fn stopped(&mut self) -> anyhow::Error {
        let joined = tokio::select! {
            joined = &mut self.router => ("alert router", joined),
            Some(joined) = self.deliveries.join_next() => ("alert delivery", joined),
        };
        match joined {
            (name, Ok(())) => anyhow!("{name} task stopped"),
            (name, Err(e)) => anyhow!(e).context(format!("{name} task panicked")),
        }
    }

    /// Routes every line already logged and delivers its pages, giving up after `grace`.
    pub async fn drain(mut self, grace: Duration) {
        if let Some(drain) = self.drain.take() {
            let _ = drain.send(());
        }
        // A router `stopped` already returned has been awaited, and must not be polled again.
        let drained = async {
            if !self.router.is_finished() {
                let _ = (&mut self.router).await;
            }
            while self.deliveries.join_next().await.is_some() {}
        };
        if timeout(grace, drained).await.is_err() {
            let pending = |severity| self.metrics.alert_pages_pending.get_or_create(&SeverityLabel { severity }).get();
            let (critical, warning) = (pending("critical"), pending("warning"));
            // No `alert` field: routing is stopping, so it could not be paged.
            error!(critical, warning, "alert pages lost at shutdown: still queued or being posted");
            self.router.abort();
            self.deliveries.abort_all();
        }
    }
}

/// The layer, and the router it feeds, which posts to `routes`.
pub fn channel(routes: Routes, metrics: Arc<Metrics>) -> (AlertLayer, Router) {
    let (tx, rx) = mpsc::channel(LINE_QUEUE);
    (AlertLayer { lines: tx, metrics: metrics.clone() }, Router { lines: rx, routes, metrics })
}

/// Whether the alert layer wants this callsite: events at `info` or above with an `alert` or
/// `alert_resolved` field, and the spans they sit in, for their context. `on_event` applies the
/// level rules.
pub fn is_alert_callsite(meta: &Metadata<'_>) -> bool {
    let has = |name| meta.fields().field(name).is_some();
    *meta.level() <= Level::INFO && (meta.is_span() || has("alert") || has("alert_resolved"))
}

/// Turns alert lines into pages on the queue of their severity's route. On `drain` it takes no new
/// lines, routes those already queued, and returns, which ends each delivery task once its queued
/// pages are posted.
async fn route(
    mut lines: mpsc::Receiver<AlertLine>,
    queues: Queues,
    mut episodes: Episodes,
    mut drain: oneshot::Receiver<()>,
    metrics: Arc<Metrics>,
) {
    let mut draining = false;
    loop {
        tokio::select! {
            line = lines.recv() => {
                let Some(line) = line else { return };
                match episodes.on_line(line, Instant::now()) {
                    Some(Routed::One(page)) => enqueue(&queues, &mut episodes, page, &metrics),
                    Some(Routed::Every(line, key)) => queues.all().for_each(|(severity, _)| {
                        let page = Page::new(line.clone(), key.clone(), Status::Resolved, severity);
                        enqueue(&queues, &mut episodes, page, &metrics);
                    }),
                    None => {}
                }
            }
            _ = &mut drain, if !draining => {
                draining = true;
                lines.close();
            }
        }
    }
}

/// Queues `page` for its severity's route, if it has one, counting it as pending until delivery
/// finishes with it. When that queue is full, or its delivery task has stopped, the page is dropped,
/// logged, and counted, and its episode forgotten so that a repeat pages again.
fn enqueue(queues: &Queues, episodes: &mut Episodes, page: Page, metrics: &Metrics) {
    let Some(queue) = queues.get(page.severity) else { return };
    let severity = page.severity.as_str();
    let pending = metrics.alert_pages_pending.get_or_create(&SeverityLabel { severity }).clone();
    // Counted before it is queued, so delivery cannot finish with it first.
    pending.inc();
    if let Err(refused) = queue.try_send(page) {
        pending.dec();
        let (page, why) = match refused {
            TrySendError::Full(page) => (page, "the route's queue is full"),
            TrySendError::Closed(page) => (page, "the route's delivery task has stopped"),
        };
        // No `alert` field: a dropped page must not queue another page.
        error!(key = %page.key, severity, context = %page.context, "alert page dropped: {why}");
        episodes.forget(&page.key);
        metrics.alert_deliveries.get_or_create(&DeliveryLabels { severity, outcome: "dropped" }).inc();
    }
}

/// `name{fields}` for a span with recorded fields, or else `name`.
fn span_label(name: &str, extensions: &Extensions<'_>) -> String {
    match extensions.get::<SpanFields>() {
        Some(SpanFields(fields)) if !fields.is_empty() => format!("{name}{{{fields}}}"),
        _ => name.to_owned(),
    }
}

/// The alert lines `f` logs, through the layer and its filter.
#[cfg(test)]
pub fn capture(f: impl FnOnce()) -> Vec<AlertLine> {
    use tracing::subscriber::with_default;
    use tracing_subscriber::filter::filter_fn;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::registry;

    crate::test_support::keep_callsites_interested();
    let (layer, router) = channel(Routes::default(), Arc::new(Metrics::new()));
    with_default(registry().with(layer.with_filter(filter_fn(is_alert_callsite))), f);
    router.queued()
}

#[cfg(test)]
impl Router {
    /// The lines queued so far.
    pub fn queued(mut self) -> Vec<AlertLine> {
        std::iter::from_fn(|| self.lines.try_recv().ok()).collect()
    }
}

#[cfg(test)]
mod tests;
