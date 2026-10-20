//! The EIP-712 vectors clients sign against. `fixtures/eip712.json` holds, for each request type,
//! the API body, the typed data a wallet signs with `eth_signTypedData_v4`, and the signature of
//! the Anvil test key over that typed data, made by Foundry's `cast wallet sign --data`. Signing
//! is deterministic (RFC 6979), so the engine reproducing a signature from the body shows that the
//! typed data and the engine's structs gave the same digest when it was signed. The test also
//! hashes the published typed data as a wallet does, and checks its message against the body, so a
//! change to either side fails here.

use alloy_dyn_abi::TypedData;
use alloy_primitives::hex;
use engine_core::command::{CancelRequest, OrderKind, OrderRequest, WithdrawalRequest};
use engine_core::eip712::{self, Signature, cancel_hash, order_hash, withdrawal_hash};
use engine_core::types::{AccountId, Millis};
use k256::ecdsa::SigningKey;
use serde::Deserialize;
use serde_json::{Value, json};

const VECTORS: &str = include_str!("fixtures/eip712.json");

#[derive(Deserialize)]
struct Vectors {
    private_key: String,
    account: AccountId,
    chain_id: u64,
    vault: AccountId,
    requests: Vec<Vector>,
}

#[derive(Deserialize)]
struct Vector {
    kind: String,
    nonce: u64,
    expires_at: Millis,
    body: Value,
    typed_data: Value,
    signature: Signature,
}

#[test]
fn the_engine_reproduces_each_vector_signature_from_its_request_body() {
    let vectors: Vectors = serde_json::from_str(VECTORS).unwrap();
    let key = SigningKey::from_slice(&hex::decode(&vectors.private_key).unwrap()).unwrap();
    assert_eq!(eip712::address(key.verifying_key()), vectors.account);
    let domain = eip712::domain(vectors.chain_id, vectors.vault);
    for vector in &vectors.requests {
        let (nonce, expires_at) = (vector.nonce, vector.expires_at);
        // The digest the engine signs from the body, and the wallet message the body stands for.
        let (hash, message) = match vector.kind.as_str() {
            "order" => {
                let req: OrderRequest = serde_json::from_value(vector.body.clone()).unwrap();
                let (kind, price, max_slippage_bps) = match req.kind {
                    OrderKind::Market { quoted, max_slippage_bps } => ("market", quoted, max_slippage_bps),
                    OrderKind::Limit { price } => ("limit", price, None),
                    OrderKind::Stop { price } => ("stop", price, None),
                };
                let message = json!({
                    "account": req.account.to_string(), "nonce": nonce, "expiresAt": expires_at,
                    "clientId": req.client_id, "market": req.market, "side": req.side,
                    "qty": req.qty.raw().to_string(), "kind": kind, "price": price.raw().to_string(),
                    "hasMaxSlippage": max_slippage_bps.is_some(), "maxSlippageBps": max_slippage_bps.unwrap_or(0),
                    "reduceOnly": req.reduce_only,
                });
                (order_hash(&domain, &req, nonce, expires_at), message)
            }
            "cancel" => {
                let body: CancelRequest = serde_json::from_value(vector.body.clone()).unwrap();
                let message = json!({
                    "account": body.account.to_string(), "nonce": nonce, "expiresAt": expires_at,
                    "orderId": body.order_id,
                });
                (cancel_hash(&domain, body.account, body.order_id, nonce, expires_at), message)
            }
            "withdrawal" => {
                let body: WithdrawalRequest = serde_json::from_value(vector.body.clone()).unwrap();
                let message = json!({
                    "account": body.account.to_string(), "nonce": nonce, "expiresAt": expires_at,
                    "amount": body.amount.raw().to_string(),
                });
                (withdrawal_hash(&domain, body.account, body.amount, nonce, expires_at), message)
            }
            other => panic!("unknown request kind {other}"),
        };
        assert_eq!(eip712::sign(&key, &hash), vector.signature, "{} with nonce {}", vector.kind, vector.nonce);
        assert_eq!(eip712::recover(&hash, &vector.signature), Some(vectors.account));
        // The typed data names this deployment and says exactly what the body asks for, so a client
        // copying it signs the request the engine runs.
        let typed = &vector.typed_data;
        // Hashed as a wallet hashes it: the published types, primary type, and domain give the
        // engine's digest.
        let wallet: TypedData = serde_json::from_value(typed.clone()).unwrap();
        assert_eq!(wallet.eip712_signing_hash().unwrap(), hash, "{} with nonce {}", vector.kind, vector.nonce);
        assert_eq!(typed["domain"]["chainId"], vectors.chain_id);
        assert_eq!(typed["domain"]["verifyingContract"], vectors.vault.to_string());
        assert_eq!(typed["message"], message, "{} with nonce {}", vector.kind, vector.nonce);
    }
}
