//! The state root under generated sessions. From `V9`, a root computed from what changed since the
//! previous one, at random points in the session, equals the root of the same state hashed from
//! scratch, and every account's proof verifies against it. The sessions run under the latest
//! version, so every rule that changes state reaches the tree.

use engine_core::Engine;
use engine_core::command::{AdminCommand, Command};
use engine_core::eip712::B256;
use engine_core::protocol::ProtocolVersion;
use proptest::bool::weighted;
use proptest::collection::vec;
use proptest::test_runner::{TestCaseError, TestRunner};

use crate::common::{Harness, operator};
use crate::session::{command, session};
use crate::{config, funded};

#[test]
fn incremental_roots_match_a_rebuild_and_every_account_proves_against_them() {
    // Whether to compute a root after each step, so roots fall after one command or after many.
    let roots = vec(weighted(0.1), 200);
    let result = TestRunner::new(config()).run(&(session(), roots), |(session, roots)| {
        // The FX markets are listed, so sessions also move FX marks and euro and yen positions.
        let mut genesis = Harness::fx();
        genesis.admin(AdminCommand::SetOperator(operator()));
        genesis.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::LATEST });
        let mut h = funded(genesis, &session);
        check(&mut h.engine)?;
        for (step, root) in session.steps.into_iter().zip(roots) {
            h.advance(step.advance_ms);
            let cmd = command(&step.action, &h.engine, h.ts);
            // From `V8` an operator command applies only signed by the operator.
            match cmd.clone() {
                Command::Admin(admin) if admin.is_operator_command() => h.operate(admin),
                other => h.cmd(other),
            };
            if root {
                check(&mut h.engine).map_err(|e| TestCaseError::fail(format!("after {cmd:?}: {e}")))?;
            }
        }
        check(&mut h.engine)
    });
    if let Err(failure) = result {
        panic!("{failure}");
    }
}

// ── Helpers ──

/// Fails unless the engine's root equals a rebuild's, and every account's proof verifies against it
/// with the account's balance, toxicity score, and positions.
/// Error cases:
/// - the incremental root differs from the root of the state hashed from scratch
/// - an account has no proof, or one that does not verify or does not match the account
fn check(engine: &mut Engine) -> Result<(), TestCaseError> {
    let root = engine.state_root();
    let mut rebuilt: Engine = serde_json::from_slice(&serde_json::to_vec(engine).unwrap()).unwrap();
    if rebuilt.state_root() != root {
        return Err(TestCaseError::fail("the incremental root differs from a rebuild's"));
    }
    for (id, account) in engine.accounts() {
        let proof = engine.account_proof(*id).ok_or_else(|| TestCaseError::fail(format!("no proof for {id}")))?;
        if !proof.verify(B256::from(root))
            || proof.leaf.balance != account.balance
            || proof.leaf.toxicity_score() != account.toxicity_score()
            || !proof.positions.iter().map(|held| (&held.market, &held.position)).eq(&account.positions)
        {
            return Err(TestCaseError::fail(format!("the proof for {id} does not hold its account")));
        }
    }
    Ok(())
}
