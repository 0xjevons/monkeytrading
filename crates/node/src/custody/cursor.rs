//! The deposit watcher's place in the chain, kept at `custody.evm.cursor_path` so a restart
//! resumes where it stopped. It is stored only after the log is synced past the deposits it covers, so it
//! never runs ahead of the log. The engine's credited references, not the cursor, stop a double
//! credit: a cursor that lags only makes the watcher read some deposits again.
//!
//! The cursor also records the log's last seq when it last moved past a deposit, so a log restored
//! or truncated under a newer cursor is refused instead of skipping deposits it no longer holds.
//!
//! It also keeps the hash of the last block read, and of the last block of a range being credited,
//! so a restart proves the chain through both and stalls, with an alert, if either has changed.
//!
//! TODO: the file sits on host storage. A cursor moved forward hides the deposits it skips, with no
//! alert, and so does a log rewritten along with it. Reconciling the vault's USDC balance with the
//! ledger at each checkpoint, and checking the log against the posted root, will bound both.

use std::ffi::OsString;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use alloy::primitives::{Address, B256};
use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cursor {
    pub chain_id: u64,
    pub vault: Address,
    /// The first block not yet read.
    pub next_block: u64,
    /// The vault deposit id expected next, which is also the number of deposits read so far.
    pub next_deposit_id: u64,
    /// The log's last seq when the cursor last moved past a deposit; the recovered log must
    /// reach it.
    pub log_seq: Option<u64>,
    /// The hash of block `next_block - 1`, once a block has been read.
    pub last_block_hash: Option<B256>,
    /// The last block, and its hash, of a range whose deposits are being credited: stored before
    /// the first credit, so a restart part-way proves the chain through that block too.
    pub crediting: Option<(u64, B256)>,
}

impl Cursor {
    /// The cursor stored at `path`, or one at `start_block` when there is none. `log_seq` is the
    /// last seq of the recovered log, `None` when the log is empty.
    /// Error cases:
    /// - the file exists but cannot be read or parsed
    /// - it belongs to another chain or vault
    /// - it exists although the log is fresh, or it has read past deposits the log does not hold
    pub fn load(
        path: &Path,
        chain_id: u64,
        vault: Address,
        start_block: u64,
        log_seq: Option<u64>,
    ) -> anyhow::Result<Self> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Cursor {
                    chain_id,
                    vault,
                    next_block: start_block,
                    next_deposit_id: 0,
                    log_seq: None,
                    last_block_hash: None,
                    crediting: None,
                });
            }
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        if log_seq.is_none() {
            bail!("{} exists but the log is new; remove it to rescan the vault from the start", path.display());
        }
        let cursor: Cursor = serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        if cursor.chain_id != chain_id || cursor.vault != vault {
            bail!(
                "{} is for vault {} on chain {}, not the configured vault {vault} on chain {chain_id}",
                path.display(),
                cursor.vault,
                cursor.chain_id
            );
        }
        if cursor.log_seq > log_seq {
            bail!(
                "{} has credited deposits up to log seq {:?}, but the log ends at seq {:?}; restore the log, \
                 or remove the cursor to rescan the vault",
                path.display(),
                cursor.log_seq,
                log_seq
            );
        }
        Ok(cursor)
    }

    /// Replaces the file at `path` atomically: a crash leaves either the old cursor or this one.
    /// Error cases:
    /// - the directory cannot be created, or the file cannot be written, synced, or renamed
    pub fn store(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let tmp = temp_path(path);
        let mut file = File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        serde_json::to_writer(&mut file, self).context("writing deposit cursor")?;
        file.write_all(b"\n").context("writing deposit cursor")?;
        file.sync_all().context("syncing deposit cursor")?;
        fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
    }
}

/// The file a store writes before renaming it over `path`: `path` with `.tmp` appended, which
/// differs from `path` whatever its extension.
pub(super) fn temp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(OsString::from).unwrap_or_default();
    name.push(".tmp");
    path.with_file_name(name)
}
