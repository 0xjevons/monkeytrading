//! Deterministic signing and canonical-state RPC for submission regressions.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use alloy::primitives::{B256, U256};
use alloy::rpc::types::Block;
use alloy::sol_types::SolCall;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::{Json, Router, routing::post};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use crate::checkpoints::rpc::{
    balanceOfCall, checkpointSignerCall, depositCountCall, hasCheckpointCall, latestRootCall, latestSeqCall, usdcCall,
};
use crate::log::Checkpoint;
use crate::tee::EnclaveKey;

pub struct RpcState {
    pub chain_id: u64,
    pub latest_nonce: u64,
    pub pending_nonce: u64,
    pub sent: Vec<Value>,
    pub known: bool,
    pub lookup_override: Option<Value>,
    pub lose_response: bool,
    pub block_hash_store: bool,
    pub latest: Option<Checkpoint>,
    pub finalized: Option<Checkpoint>,
    pub hash: B256,
    path: PathBuf,
}

pub struct Server {
    pub url: String,
    pub state: Arc<Mutex<RpcState>>,
    task: JoinHandle<()>,
}

impl Server {
    /// Starts an isolated local RPC, with seven consumed account nonces and no pending work.
    pub async fn start(dir: &Path) -> Self {
        let state = Arc::new(Mutex::new(RpcState {
            chain_id: 31337,
            latest_nonce: 7,
            pending_nonce: 7,
            sent: Vec::new(),
            known: true,
            lookup_override: None,
            lose_response: false,
            block_hash_store: false,
            latest: None,
            finalized: None,
            hash: B256::repeat_byte(0x33),
            path: dir.join("checkpoint-submission.json"),
        }));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().route("/", post(answer)).with_state(state.clone());
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self { url, state, task }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Answers only the RPC methods the real adapter needs, checking durable ownership at send time.
async fn answer(State(state): State<Arc<Mutex<RpcState>>>, Json(request): Json<Value>) -> Response {
    let mut state = state.lock().unwrap();
    let params = &request["params"];
    let result = match request["method"].as_str().unwrap() {
        "eth_chainId" => json!(format!("{:#x}", state.chain_id)),
        "eth_getTransactionCount" => {
            let nonce = if params[1] == "pending" { state.pending_nonce } else { state.latest_nonce };
            json!(format!("{nonce:#x}"))
        }
        "eth_sendTransaction" => {
            let transaction = params[0].clone();
            let record: Value = serde_json::from_slice(&std::fs::read(&state.path).unwrap()).unwrap();
            let nonce =
                u64::from_str_radix(transaction["nonce"].as_str().unwrap().trim_start_matches("0x"), 16).unwrap();
            assert_eq!(record["nonce"], nonce, "nonce ownership must exist before send");
            assert!(record["checkpoint"]["signature"].as_str().unwrap().starts_with("0x"));
            state.sent.push(transaction);
            state.pending_nonce = state.pending_nonce.max(nonce + 1);
            if state.block_hash_store {
                std::fs::create_dir(state.path.with_extension("json.tmp")).unwrap();
            }
            if std::mem::take(&mut state.lose_response) {
                return "".into_response();
            }
            json!(state.hash)
        }
        "eth_getTransactionByHash" => {
            if state.known {
                state.lookup_override.clone().or_else(|| state.sent.last().cloned()).unwrap_or(Value::Null)
            } else {
                Value::Null
            }
        }
        "eth_getBlockByNumber" => {
            let finalized = params[0] == "finalized";
            let mut block: Block = Block::default();
            block.header.number = if finalized { 8 } else { 10 };
            block.header.hash = B256::repeat_byte(if finalized { 8 } else { 10 });
            serde_json::to_value(block).unwrap()
        }
        "eth_call" => {
            let finalized = params[1]["blockHash"] == json!(B256::repeat_byte(8));
            let checkpoint = if finalized { &state.finalized } else { &state.latest };
            let input = params[0].get("input").or_else(|| params[0].get("data")).unwrap().as_str().unwrap();
            let selector = &input[2..10];
            let bytes = if selector == hex::encode(usdcCall::SELECTOR) {
                usdcCall::abi_encode_returns(&alloy::primitives::Address::repeat_byte(2))
            } else if selector == hex::encode(checkpointSignerCall::SELECTOR) {
                checkpointSignerCall::abi_encode_returns(&alloy::primitives::Address::from(
                    EnclaveKey::test_key().address(),
                ))
            } else if selector == hex::encode(hasCheckpointCall::SELECTOR) {
                hasCheckpointCall::abi_encode_returns(&checkpoint.is_some())
            } else if selector == hex::encode(latestSeqCall::SELECTOR) {
                latestSeqCall::abi_encode_returns(&checkpoint.as_ref().unwrap().seq)
            } else if selector == hex::encode(latestRootCall::SELECTOR) {
                latestRootCall::abi_encode_returns(&checkpoint.as_ref().unwrap().root.parse().unwrap())
            } else if selector == hex::encode(balanceOfCall::SELECTOR) {
                balanceOfCall::abi_encode_returns(&U256::ZERO)
            } else {
                assert_eq!(selector, hex::encode(depositCountCall::SELECTOR));
                depositCountCall::abi_encode_returns(&U256::ZERO)
            };
            json!(format!("0x{}", hex::encode(bytes)))
        }
        other => panic!("unexpected RPC method {other}"),
    };
    Json(json!({"jsonrpc": "2.0", "id": request["id"], "result": result})).into_response()
}
