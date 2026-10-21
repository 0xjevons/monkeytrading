//! Commands the engine must accept before anything else: the client auth, protocol activation,
//! and genesis. Like any command, they wait for a stamp trusted time admits.

use anyhow::bail;
use engine_core::command::{AdminCommand, Command};
use engine_core::event::Event;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::Millis;
use tracing::info;

use crate::config::Genesis;
use crate::sequencer::{GENESIS_REF, Sequencer, genesis_commands};

impl Sequencer {
    /// The seq of the last command in the log, if any.
    pub fn last_seq(&self) -> Option<u64> {
        self.engine.last_seq()
    }

    /// Whether genesis has yet to finish: its last command, the house capital credit, is not in
    /// the log. Genesis commands are safe to repeat, so a genesis cut short runs again in full.
    pub fn needs_genesis(&self) -> bool {
        !self.engine.is_credited(GENESIS_REF)
    }

    /// Records the config's client auth and activates the latest protocol version, whichever the
    /// log lacks, then checkpoints. Both run at the log's next seq, so earlier commands keep the
    /// rules they ran under, once trusted time admits a stamp (see `sequence_required`). `recover`
    /// has already refused a log that records other client auth.
    /// Error cases:
    /// - the engine rejects either command
    /// - the log cannot be written
    pub fn activate_protocol(&mut self) -> anyhow::Result<()> {
        let mut cmds = Vec::new();
        if self.engine.client_auth().is_none() {
            cmds.push(Command::Admin(AdminCommand::SetClientAuth(self.client_auth.clone())));
        }
        if self.engine.protocol_version() < ProtocolVersion::LATEST {
            cmds.push(Command::Admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::LATEST }));
        }
        if cmds.is_empty() {
            return Ok(());
        }
        info!(version = ?ProtocolVersion::LATEST, seq = self.next_seq, "activating protocol");
        self.sequence_required("protocol activation", cmds)
    }

    /// Sequences the genesis config as ordinary admin commands, then checkpoints, once trusted time
    /// admits a stamp (see `sequence_required`).
    /// Error cases:
    /// - the engine rejects any genesis command
    /// - the log cannot be written
    pub fn genesis(&mut self, genesis: &Genesis) -> anyhow::Result<()> {
        self.sequence_required("genesis", genesis_commands(genesis))
    }

    /// Queues commands the engine must accept, `what` they are for, and sequences everything queued
    /// if trusted time admits a stamp now. Otherwise they wait, like any command, and run ahead of
    /// the first stamp it admits: nothing, at start, runs on the host clock alone.
    /// Error cases:
    /// - the engine rejects any queued command
    /// - the log cannot be written
    pub(super) fn sequence_required(&mut self, what: &'static str, cmds: Vec<Command>) -> anyhow::Result<()> {
        self.required.push((what, cmds));
        self.sequence_queued_if_admitted()?;
        self.flush()
    }

    /// Sequences the queued required commands if trusted time admits a stamp now.
    /// Error cases:
    /// - the engine rejects any of them
    /// - the log cannot be written
    pub(super) fn sequence_queued_if_admitted(&mut self) -> anyhow::Result<()> {
        if self.required.is_empty() {
            return Ok(());
        }
        match self.admitted_ts() {
            Some(ts) => self.sequence_queued(ts),
            None => Ok(()),
        }
    }

    /// Sequences the queued required commands at `ts`, checkpointing after each group.
    /// Error cases:
    /// - the engine rejects any of them
    /// - the log cannot be written
    pub(super) fn sequence_queued(&mut self, ts: Millis) -> anyhow::Result<()> {
        for (what, cmds) in std::mem::take(&mut self.required) {
            info!(what, commands = cmds.len(), "sequencing required commands");
            for cmd in cmds {
                let events = self.stamp(ts, cmd)?;
                if let Some(Event::AdminRejected { reason }) =
                    events.iter().find(|e| matches!(e, Event::AdminRejected { .. }))
                {
                    bail!("{what}: rejected: {reason}");
                }
            }
            self.checkpoint()?;
        }
        Ok(())
    }
}
