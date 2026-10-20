//! `engine`: runs the node, or replays a published log and verifies its checkpoints.

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use engine_node::{NodeConfig, init_logging, log, replay, run};

#[derive(Subcommand)]
enum Cmd {
    /// Run the node.
    Run {
        #[arg(short, long, env = "CONFIG_PATH", default_value = "config/dev.toml")]
        config: PathBuf,
    },
    /// Replay a published log and verify every checkpoint root.
    Replay {
        #[arg(short, long)]
        log: PathBuf,
    },
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
/// - the log cannot be read, or fails verification
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
            let replayed = replay(&log::read_all(&path)?)?;
            println!(
                "verified: {} commands, {} checkpoints, final root 0x{}",
                replayed.commands,
                replayed.checkpoints,
                hex::encode(replayed.engine.state_root())
            );
            Ok(())
        }
    }
}
