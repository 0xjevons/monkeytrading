//! The RPC adapter's own checks: log decoding, the deposit count's return data, deadlines, and
//! keeping the endpoint URL out of errors.

use alloy::primitives::{Address, B256, Bytes, LogData, U256};
use alloy::rpc::types::Log;
use alloy::sol_types::{SolCall, SolEvent};
use engine_core::types::AccountId;
use tokio::net::TcpListener;

use crate::config::EvmCustodySection;
use crate::custody::evm::{Beneficiary, VaultReader};
use crate::custody::rpc::{Deposited, HouseDeposited, VaultRpc, decode_count, depositCountCall};
use crate::custody::tests::fakes::{VAULT, evm_toml};

#[test]
fn vault_logs_decode_into_deposits_with_their_position() {
    let rpc = rpc_at("http://127.0.0.1:1");
    let trader = Address::repeat_byte(0xaa);
    let deposit = rpc
        .decode(&log(Deposited { id: U256::from(3), account: trader, amount: U256::from(5) }.encode_log_data()))
        .unwrap();
    assert_eq!((deposit.block, deposit.block_hash, deposit.log_index, deposit.id), (7, B256::repeat_byte(7), 2, 3));
    assert_eq!((deposit.beneficiary, deposit.units), (Beneficiary::Trader(AccountId(trader.into())), U256::from(5)));
    let house = rpc
        .decode(&log(HouseDeposited { id: U256::from(4), from: trader, amount: U256::from(9) }.encode_log_data()))
        .unwrap();
    assert_eq!((house.beneficiary, house.id), (Beneficiary::House, 4));
}

#[test]
fn logs_that_are_not_final_vault_deposits_are_refused() {
    let rpc = rpc_at("http://127.0.0.1:1");
    let data =
        Deposited { id: U256::from(3), account: Address::repeat_byte(0xaa), amount: U256::from(5) }.encode_log_data();
    let good = log(data.clone());
    let refused = [
        Log { removed: true, ..good.clone() },
        Log { inner: alloy::primitives::Log { address: Address::repeat_byte(1), data: data.clone() }, ..good.clone() },
        Log { block_number: None, ..good.clone() },
        Log { block_hash: None, ..good.clone() },
        Log { transaction_hash: None, ..good.clone() },
        Log { log_index: None, ..good.clone() },
        log(LogData::new_unchecked(vec![B256::repeat_byte(9)], data.data.clone())),
        log(Deposited { id: U256::MAX, account: Address::repeat_byte(0xaa), amount: U256::from(5) }.encode_log_data()),
    ];
    for (i, bad) in refused.iter().enumerate() {
        assert!(rpc.decode(bad).is_err(), "case {i} decoded");
    }
}

#[test]
fn the_deposit_count_decodes_and_reads_no_code_as_zero() {
    assert_eq!(decode_count(&Bytes::new()).unwrap(), 0, "no vault deployed at that block yet");
    assert_eq!(decode_count(&depositCountCall::abi_encode_returns(&U256::from(7)).into()).unwrap(), 7);
    let beyond = depositCountCall::abi_encode_returns(&(U256::from(u64::MAX) + U256::from(1)));
    assert!(decode_count(&beyond.into()).is_err());
}

#[tokio::test]
async fn an_endpoint_that_never_answers_times_out() {
    // Accepts connections into its backlog and never replies.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rpc = rpc_at(&format!("http://{}", listener.local_addr().unwrap()));

    let err = format!("{:#}", rpc.finality().await.unwrap_err());
    assert!(err.contains("timed out after"), "{err}");
}

#[tokio::test]
async fn rpc_errors_never_carry_the_endpoint_url() {
    let rpc = rpc_at("http://127.0.0.1:1/v2/secret-key?token=hunter2");

    let err = format!("{:#}", rpc.finality().await.unwrap_err());
    assert!(err.contains("eth_getBlockByNumber failed"), "{err}");
    assert!(!err.contains("secret-key") && !err.contains("hunter2"), "{err}");
}

// ── Helpers ──

/// A client for `url` with a short deadline.
fn rpc_at(url: &str) -> VaultRpc {
    let toml = evm_toml(&std::env::temp_dir(), 1_000)
        .replace("rpc_url = \"http://127.0.0.1:1\"", &format!("rpc_url = \"{url}\""))
        .replace("rpc_timeout = \"1s\"", "rpc_timeout = \"200ms\"");
    let cfg: EvmCustodySection = toml::from_str(&toml).unwrap();
    VaultRpc::new(&cfg, &cfg.providers[0].rpc_url, VAULT).unwrap()
}

/// A vault log in block 7 at log index 2.
fn log(data: LogData) -> Log {
    Log {
        inner: alloy::primitives::Log { address: VAULT, data },
        block_hash: Some(B256::repeat_byte(7)),
        block_number: Some(7),
        transaction_hash: Some(B256::repeat_byte(1)),
        log_index: Some(2),
        ..Log::default()
    }
}
