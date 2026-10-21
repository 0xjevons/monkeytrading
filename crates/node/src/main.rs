//! `engine`: runs the node, replays a published log and verifies its checkpoints, or signs and
//! submits an operator's admin command.

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use engine_core::auth::{ClientAuth, Operator};
use engine_node::admin::{self, AdminArgs, HexKey, KeyEnv};
use engine_node::{NodeConfig, init_logging, log, replay, run};

#[derive(Subcommand)]
enum Cmd {
    /// Run the node.
    Run {
        #[arg(short, long, env = "CONFIG_PATH", default_value = "config/dev.toml")]
        config: PathBuf,
    },
    /// Replay a published log and verify every checkpoint root. `--log` is a single log file, or a
    /// directory of segments with their manifest: a node's log directory or a downloaded archive.
    Replay {
        #[arg(short, long)]
        log: PathBuf,
    },
    /// Sign an admin command with the operator's key and submit it to a node. The key comes from
    /// `--key-file`, or the `OPERATOR_KEY_FILE` or `OPERATOR_KEY` environment variable, never the
    /// command line.
    Admin(AdminArgs),
}

#[derive(Parser)]
#[command(about = "House-counterparty CFD engine, built to run inside a TEE")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

/// Dispatches the subcommand.
/// Error cases:
/// - the config cannot be loaded, or the node stops with an error
/// - the log cannot be read, or fails its manifest check or verification
/// - an admin command cannot be signed or submitted, or the engine rejects it
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match Cli::parse().cmd {
        Cmd::Run { config } => {
            let cfg = NodeConfig::load(&config)?;
            init_logging(&cfg.logging.level)?;
            run(cfg).await
        }
        Cmd::Replay { log: path } => {
            init_logging("info")?;
            let entries = if path.is_dir() { log::SegmentedLog::open(&path)?.entries } else { log::read_all(&path)? };
            let mut replayed = replay(&entries)?;
            let activations: Vec<String> =
                replayed.activations.iter().map(|(version, seq)| format!("{version:?} from seq {seq}")).collect();
            println!(
                "verified: {} commands, {} checkpoints, final root 0x{}, protocol V2{}, client auth {}, operator {}",
                replayed.commands,
                replayed.checkpoints,
                hex::encode(replayed.engine.state_root()),
                activations.iter().map(|a| format!(", {a}")).collect::<String>(),
                match replayed.engine.client_auth() {
                    None => "none recorded".to_owned(),
                    Some(ClientAuth::Unsigned) => "unsigned".to_owned(),
                    Some(ClientAuth::Eip712 { chain_id, vault }) => format!("eip712 (chain {chain_id}, vault {vault})"),
                },
                match replayed.engine.operator() {
                    None => "none recorded".to_owned(),
                    Some(Operator { address, chain_id, vault }) =>
                        format!("{address} (chain {chain_id}, vault {vault})"),
                }
            );
            if !replayed.unsigned.is_empty() {
                println!(
                    "note: the log ends before signing {} approved withdrawal(s); the node signs them when it restarts",
                    replayed.unsigned.len()
                );
            }
            Ok(())
        }
        Cmd::Admin(args) => {
            let env = KeyEnv {
                key_file: std::env::var_os("OPERATOR_KEY_FILE").map(PathBuf::from),
                key: std::env::var("OPERATOR_KEY").ok().map(HexKey),
            };
            admin::run(args, env).await.map(drop)
        }
    }
}
