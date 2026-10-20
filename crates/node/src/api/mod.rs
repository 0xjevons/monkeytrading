//! Client API: REST for actions and reads, a WebSocket that streams the public log (every
//! sequenced command with its events, every signed checkpoint, every signed withdrawal), and the
//! log archive.

mod archive;
mod stream;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::middleware::{Next, from_fn};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use engine_core::account::{Position, RestingOrder};
use engine_core::command::{Command, OrderRequest};
use engine_core::event::Event;
use engine_core::market::MarketSpec;
use engine_core::types::{AccountId, MarketId, OrderId};
use engine_core::{Engine, Fixed};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tracing::field::{Empty, display};
use tracing::{Instrument, Span, info_span};

use crate::clock::now_ms;
use crate::config::AuthMode;
use crate::log::Manifest;
use crate::sequencer::{self, StreamMsg};
use crate::tee::Attestation;

type ApiResult<T> = Result<Json<T>, ApiError>;

pub enum ApiError {
    Unavailable,
    NotFound,
    BadRequest(String),
    NotImplemented(&'static str),
    /// The node failed to serve the request; the cause is logged where it happened.
    Internal,
}

#[derive(Clone)]
pub struct ApiState {
    pub tx: mpsc::Sender<sequencer::Request>,
    pub stream: broadcast::Sender<Arc<StreamMsg>>,
    pub attestation: Arc<Attestation>,
    pub auth: AuthMode,
    pub dev_deposits: bool,
    pub log_dir: PathBuf,
    /// The log writer's manifest, held in enclave memory.
    pub manifest: watch::Receiver<Manifest>,
}

#[derive(Serialize)]
struct MarketView {
    #[serde(flatten)]
    spec: MarketSpec,
    open: bool,
    reduce_only: bool,
    mark: Option<Fixed>,
    stress_level: usize,
}

#[derive(Serialize)]
struct HouseView {
    cash: Fixed,
    nav: Fixed,
    gross_exposure: Fixed,
}

#[derive(Serialize)]
struct AccountView {
    balance: Fixed,
    equity: Fixed,
    margin: Fixed,
    positions: BTreeMap<MarketId, Position>,
    orders: BTreeMap<OrderId, RestingOrder>,
    toxicity_bps: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CancelBody {
    account: AccountId,
    order_id: OrderId,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AmountBody {
    account: AccountId,
    amount: Fixed,
}

impl IntoResponse for ApiError {
    /// A JSON `{ "error": … }` body with the matching status.
    fn into_response(self) -> Response {
        let (status, msg) = match self {
            ApiError::Unavailable => (StatusCode::SERVICE_UNAVAILABLE, "engine unavailable".to_owned()),
            ApiError::NotFound => (StatusCode::NOT_FOUND, "not found".to_owned()),
            ApiError::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            ApiError::NotImplemented(m) => (StatusCode::NOT_IMPLEMENTED, m.to_owned()),
            ApiError::Internal => (StatusCode::INTERNAL_SERVER_ERROR, "internal error".to_owned()),
        };
        (status, Json(json!({ "error": msg }))).into_response()
    }
}

impl ApiState {
    /// Checks the request may act for `account`, and records it as the request's id.
    ///
    /// TODO(auth): verify an EIP-712 signature from `account` over the request, with a per-account
    /// nonce to stop replays.
    /// Error cases:
    /// - EIP-712 auth is configured, which is not implemented yet
    fn authorise(&self, account: AccountId) -> Result<(), ApiError> {
        Span::current().record("id", display(account));
        match self.auth {
            AuthMode::None => Ok(()),
            AuthMode::Eip712 => Err(ApiError::NotImplemented("EIP-712 request signing is not implemented yet")),
        }
    }

    /// Sequences a command and waits for its events.
    /// Error cases:
    /// - the sequencer has stopped
    async fn submit(&self, cmd: Command) -> Result<Vec<Event>, ApiError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(sequencer::Request::Submit { cmd, reply: Some(reply) })
            .await
            .map_err(|_| ApiError::Unavailable)?;
        rx.await.map_err(|_| ApiError::Unavailable)
    }

    /// Runs a read on the sequencer between batches, so it sees a consistent, already-durable
    /// state.
    /// Error cases:
    /// - the sequencer has stopped
    async fn query<T: Send + 'static>(&self, f: impl FnOnce(&Engine) -> T + Send + 'static) -> Result<T, ApiError> {
        let (reply, rx) = oneshot::channel();
        let q = Box::new(move |e: &Engine| {
            let _ = reply.send(f(e));
        });
        self.tx.send(sequencer::Request::Query(q)).await.map_err(|_| ApiError::Unavailable)?;
        rx.await.map_err(|_| ApiError::Unavailable)
    }
}

/// Every route, with the request span outermost.
pub fn router(state: ApiState) -> Router {
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/attestation", get(attestation))
        .route("/v1/markets", get(markets))
        .route("/v1/house", get(house))
        .route("/v1/accounts/{address}", get(account))
        .route("/v1/orders", post(place_order))
        .route("/v1/orders/cancel", post(cancel_order))
        .route("/v1/withdrawals", post(withdraw))
        .route("/v1/dev/deposit", post(dev_deposit))
        .route("/v1/stream", get(stream::stream))
        .route("/v1/archive/{file}", get(archive::archive))
        .with_state(state)
        .layer(from_fn(trace_request))
}

/// Opens the request's correlation span; handlers record the account as its `id`.
async fn trace_request(req: Request, next: Next) -> Response {
    let span = info_span!("http", method = %req.method(), path = %req.uri().path(), id = Empty);
    next.run(req).instrument(span).await
}

/// Liveness.
async fn health() -> &'static str {
    "ok"
}

/// The attestation produced at boot.
async fn attestation(State(s): State<ApiState>) -> Json<Attestation> {
    Json((*s.attestation).clone())
}

/// Every market with its live state.
/// Error cases:
/// - the sequencer has stopped
async fn markets(State(s): State<ApiState>) -> ApiResult<Vec<MarketView>> {
    let views = s
        .query(|e| {
            e.markets()
                .values()
                .map(|m| MarketView {
                    spec: m.spec.clone(),
                    open: m.open,
                    reduce_only: m.reduce_only,
                    mark: m.mark,
                    stress_level: m.stress.level,
                })
                .collect()
        })
        .await?;
    Ok(Json(views))
}

/// House cash, NAV, and gross exposure.
/// Error cases:
/// - the sequencer has stopped
async fn house(State(s): State<ApiState>) -> ApiResult<HouseView> {
    let view = s
        .query(|e| HouseView { cash: e.house_cash(), nav: e.house_nav(), gross_exposure: e.house_gross_exposure() })
        .await?;
    Ok(Json(view))
}

/// One account's balance, equity, margin, positions, and orders.
/// Error cases:
/// - the address is malformed
/// - the account has never been credited
/// - the sequencer has stopped
async fn account(State(s): State<ApiState>, Path(address): Path<String>) -> ApiResult<AccountView> {
    let id: AccountId = address.parse().map_err(|e| ApiError::BadRequest(format!("{e}")))?;
    Span::current().record("id", display(id));
    let view = s
        .query(move |e| {
            let acct = e.accounts().get(&id)?;
            let (equity, margin) = e.equity_and_margin(acct, None);
            Some(AccountView {
                balance: acct.balance,
                equity,
                margin,
                positions: acct.positions.clone(),
                orders: acct.orders.clone(),
                toxicity_bps: acct.toxicity_bps,
            })
        })
        .await?;
    view.map(Json).ok_or(ApiError::NotFound)
}

/// Places an order and returns the events it produced.
/// Error cases:
/// - the request is not authorised for its account
/// - the sequencer has stopped
async fn place_order(State(s): State<ApiState>, Json(req): Json<OrderRequest>) -> ApiResult<Vec<Event>> {
    s.authorise(req.account)?;
    Ok(Json(s.submit(Command::PlaceOrder(req)).await?))
}

/// Cancels a resting order.
/// Error cases:
/// - the request is not authorised for its account
/// - the sequencer has stopped
async fn cancel_order(State(s): State<ApiState>, Json(b): Json<CancelBody>) -> ApiResult<Vec<Event>> {
    s.authorise(b.account)?;
    Ok(Json(s.submit(Command::CancelOrder { account: b.account, order_id: b.order_id }).await?))
}

/// Requests a withdrawal; an approval is signed and streamed.
/// Error cases:
/// - the request is not authorised for its account
/// - the sequencer has stopped
async fn withdraw(State(s): State<ApiState>, Json(b): Json<AmountBody>) -> ApiResult<Vec<Event>> {
    s.authorise(b.account)?;
    Ok(Json(s.submit(Command::Withdraw { account: b.account, amount: b.amount }).await?))
}

/// Mock custody only: credits an account without a chain deposit, under a fresh reference.
/// Error cases:
/// - custody is not the mock provider
/// - the sequencer has stopped
async fn dev_deposit(State(s): State<ApiState>, Json(b): Json<AmountBody>) -> ApiResult<Vec<Event>> {
    if !s.dev_deposits {
        return Err(ApiError::NotFound);
    }
    Span::current().record("id", display(b.account));
    let chain_ref = format!("dev:{}:{:016x}", now_ms(), rand::random::<u64>());
    Ok(Json(s.submit(Command::Deposit { account: b.account, amount: b.amount, chain_ref }).await?))
}

#[cfg(test)]
mod tests;
