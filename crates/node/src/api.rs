//! Client API: REST for actions and reads, plus a WebSocket that streams
//! the public log live (every sequenced command with its events, every
//! signed checkpoint, every signed withdrawal).

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::StatusCode;
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
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::config::AuthMode;
use crate::sequencer::{Request, StreamMsg};
use crate::tee::Attestation;

#[derive(Clone)]
pub struct ApiState {
    pub tx: mpsc::Sender<Request>,
    pub stream: broadcast::Sender<Arc<StreamMsg>>,
    pub attestation: Arc<Attestation>,
    pub auth: AuthMode,
    pub dev_deposits: bool,
}

pub fn router(state: ApiState) -> Router {
    Router::new()
        .route("/v1/health", get(|| async { "ok" }))
        .route("/v1/attestation", get(attestation))
        .route("/v1/markets", get(markets))
        .route("/v1/house", get(house))
        .route("/v1/accounts/{address}", get(account))
        .route("/v1/orders", post(place_order))
        .route("/v1/orders/cancel", post(cancel_order))
        .route("/v1/withdrawals", post(withdraw))
        .route("/v1/dev/deposit", post(dev_deposit))
        .route("/v1/stream", get(stream))
        .with_state(state)
}

pub enum ApiError {
    Unavailable,
    NotFound,
    BadRequest(String),
    NotImplemented(&'static str),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, msg) = match self {
            ApiError::Unavailable => (StatusCode::SERVICE_UNAVAILABLE, "engine unavailable".to_owned()),
            ApiError::NotFound => (StatusCode::NOT_FOUND, "not found".to_owned()),
            ApiError::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            ApiError::NotImplemented(m) => (StatusCode::NOT_IMPLEMENTED, m.to_owned()),
        };
        (status, Json(serde_json::json!({ "error": msg }))).into_response()
    }
}

type ApiResult<T> = Result<Json<T>, ApiError>;

impl ApiState {
    /// TODO(auth): verify an EIP-712 signature from `account` over the
    /// request, with a per-account nonce to stop replays.
    fn authorise(&self, _account: AccountId) -> Result<(), ApiError> {
        match self.auth {
            AuthMode::None => Ok(()),
            AuthMode::Eip712 => Err(ApiError::NotImplemented("EIP-712 request signing is not implemented yet")),
        }
    }

    async fn submit(&self, cmd: Command) -> Result<Vec<Event>, ApiError> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(Request::Submit { cmd, reply: Some(reply) }).await.map_err(|_| ApiError::Unavailable)?;
        rx.await.map_err(|_| ApiError::Unavailable)
    }

    /// Reads run on the sequencer between batches, so they always see a
    /// consistent, already-durable state.
    async fn query<T: Send + 'static>(&self, f: impl FnOnce(&Engine) -> T + Send + 'static) -> Result<T, ApiError> {
        let (reply, rx) = oneshot::channel();
        let q = Box::new(move |e: &Engine| {
            let _ = reply.send(f(e));
        });
        self.tx.send(Request::Query(q)).await.map_err(|_| ApiError::Unavailable)?;
        rx.await.map_err(|_| ApiError::Unavailable)
    }
}

async fn attestation(State(s): State<ApiState>) -> Json<Attestation> {
    Json((*s.attestation).clone())
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

async fn markets(State(s): State<ApiState>) -> ApiResult<Vec<MarketView>> {
    let views = s
        .query(|e| {
            e.markets()
                .values()
                .map(|ms| MarketView {
                    spec: ms.spec.clone(),
                    open: ms.open,
                    reduce_only: ms.reduce_only,
                    mark: ms.mark,
                    stress_level: ms.stress.level,
                })
                .collect()
        })
        .await?;
    Ok(Json(views))
}

#[derive(Serialize)]
struct HouseView {
    cash: Fixed,
    nav: Fixed,
    gross_exposure: Fixed,
}

async fn house(State(s): State<ApiState>) -> ApiResult<HouseView> {
    let view = s
        .query(|e| HouseView { cash: e.house_cash(), nav: e.house_nav(), gross_exposure: e.house_gross_exposure() })
        .await?;
    Ok(Json(view))
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

async fn account(State(s): State<ApiState>, Path(address): Path<String>) -> ApiResult<AccountView> {
    let id: AccountId = address.parse().map_err(|e| ApiError::BadRequest(format!("{e}")))?;
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

async fn place_order(State(s): State<ApiState>, Json(req): Json<OrderRequest>) -> ApiResult<Vec<Event>> {
    s.authorise(req.account)?;
    Ok(Json(s.submit(Command::PlaceOrder(req)).await?))
}

#[derive(Deserialize)]
struct CancelBody {
    account: AccountId,
    order_id: OrderId,
}

async fn cancel_order(State(s): State<ApiState>, Json(b): Json<CancelBody>) -> ApiResult<Vec<Event>> {
    s.authorise(b.account)?;
    Ok(Json(s.submit(Command::CancelOrder { account: b.account, order_id: b.order_id }).await?))
}

#[derive(Deserialize)]
struct AmountBody {
    account: AccountId,
    amount: Fixed,
}

async fn withdraw(State(s): State<ApiState>, Json(b): Json<AmountBody>) -> ApiResult<Vec<Event>> {
    s.authorise(b.account)?;
    Ok(Json(s.submit(Command::Withdraw { account: b.account, amount: b.amount }).await?))
}

/// Mock custody only: credits an account without a chain deposit.
async fn dev_deposit(State(s): State<ApiState>, Json(b): Json<AmountBody>) -> ApiResult<Vec<Event>> {
    if !s.dev_deposits {
        return Err(ApiError::NotFound);
    }
    let cmd = Command::Deposit { account: b.account, amount: b.amount, chain_ref: "dev".into() };
    Ok(Json(s.submit(cmd).await?))
}

async fn stream(State(s): State<ApiState>, ws: WebSocketUpgrade) -> Response {
    let rx = s.stream.subscribe();
    ws.on_upgrade(move |socket| forward(socket, rx))
}

/// TODO: let clients resume from a seq (backfill from the log file) so a
/// lagging subscriber can catch up instead of being dropped.
async fn forward(mut socket: WebSocket, mut rx: broadcast::Receiver<Arc<StreamMsg>>) {
    loop {
        let msg = match rx.recv().await {
            Ok(msg) => msg,
            Err(broadcast::error::RecvError::Lagged(n)) => {
                let note = serde_json::json!({ "type": "lagged", "missed": n }).to_string();
                let _ = socket.send(Message::Text(note.into())).await;
                return;
            }
            Err(broadcast::error::RecvError::Closed) => return,
        };
        let Ok(json) = serde_json::to_string(&*msg) else { continue };
        if socket.send(Message::Text(json.into())).await.is_err() {
            return;
        }
    }
}
