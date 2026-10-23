//! The state root from protocol `V9`: where its definition takes over, what each root rehashes,
//! account proofs against it, and roots that match a rebuild from the state.

mod common;

use common::stress::with_divergence_shares;
use common::{BTC, CHAIN_ID, Client, DE40, ETH, EURUSD, Harness, VAULT, acct, f, operator, params};
use engine_core::Engine;
use engine_core::auth::ClientAuth;
use engine_core::command::{AdminCommand, Command, OrderKind, Sequenced};
use engine_core::eip712::B256;
use engine_core::event::Event;
use engine_core::market::QuoteCurrency;
use engine_core::merkle::RootWork;
use engine_core::protocol::ProtocolVersion;
use engine_core::types::{AccountId, OrderId, Side};
use sha3::{Digest, Keccak256};

#[test]
fn the_root_takes_the_tree_definition_at_v9_and_a_replay_reproduces_both() {
    let mut h = Harness::new();
    h.deposit(acct(1), "10000");
    h.price(BTC, "100000");
    h.market_order(acct(1), BTC, Side::Buy, "0.05", "100000");
    let legacy = h.engine.state_root();
    // Before `V9`, the root is the keccak-256 of the state's JSON encoding.
    assert_eq!(legacy, <[u8; 32]>::from(Keccak256::digest(serde_json::to_vec(&h.engine).unwrap())));
    assert_eq!((h.engine.last_root_work(), h.engine.account_proof(acct(1))), (None, None));
    let at_legacy = h.log.len();

    h.admin(AdminCommand::SetClientAuth(ClientAuth::Unsigned));
    h.admin(AdminCommand::SetOperator(operator()));
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V9 });
    let tree = h.engine.state_root();
    assert_ne!(tree, <[u8; 32]>::from(Keccak256::digest(serde_json::to_vec(&h.engine).unwrap())));
    assert_eq!(h.engine.account_proof(acct(1)).map(|proof| proof.root.0), Some(tree));
    let at_tree = h.log.len();
    h.market_order(acct(1), BTC, Side::Sell, "0.02", "100000");
    h.deposit(acct(2), "500");
    let later = h.engine.state_root();

    // A replica that computes a root after every command reaches the same roots at the same seqs.
    let mut replica = Engine::new();
    let roots: Vec<[u8; 32]> = h
        .log
        .iter()
        .map(|sequenced| {
            replica.apply(sequenced).unwrap();
            replica.state_root()
        })
        .collect();
    assert_eq!((roots[at_legacy - 1], roots[at_tree - 1], roots[roots.len() - 1]), (legacy, tree, later));
}

#[test]
fn a_root_rehashes_only_the_leaves_changed_since_the_last_one() {
    let mut engine = Engine::new();
    let mut seq = 0;
    let mut apply = |engine: &mut Engine, cmd: Command| {
        engine.apply(&Sequenced { seq, ts: 0, cmd }).unwrap();
        seq += 1;
    };
    apply(&mut engine, Command::Admin(AdminCommand::SetClientAuth(ClientAuth::Unsigned)));
    apply(&mut engine, Command::Admin(AdminCommand::SetOperator(operator())));
    apply(&mut engine, Command::Admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V9 }));
    let deposit = |n: u32, chain_ref: String| Command::Deposit { account: account(n), amount: f("100"), chain_ref };
    for n in 0..10_000 {
        apply(&mut engine, deposit(n, format!("first:{n}")));
    }
    engine.state_root();
    // The first root hashes every account, every deposit reference, and the remaining state.
    assert_eq!(engine.last_root_work().map(|work| work.leaves), Some(20_001));

    for n in 0..10 {
        apply(&mut engine, deposit(n * 1_000, format!("second:{n}")));
    }
    engine.state_root();
    let RootWork { leaves, nodes } = engine.last_root_work().unwrap();
    // Ten accounts, ten new references, and the remaining state; and only the paths above them.
    assert_eq!(leaves, 21);
    assert!(nodes > 0 && nodes <= 21 * 24, "{nodes}");

    // A root over unchanged state re-encodes the remaining-state leaf and rehashes nothing.
    let root = engine.state_root();
    assert_eq!(engine.last_root_work(), Some(RootWork { leaves: 1, nodes: 0 }));
    // A tick moves only the sequenced time, held in the remaining-state leaf.
    apply(&mut engine, Command::Tick);
    assert_ne!(engine.state_root(), root);
    assert_eq!(engine.last_root_work().map(|work| work.leaves), Some(1));
}

#[test]
fn a_v16_params_change_without_a_clamp_rehashes_no_account_leaf() {
    // The memory clamp touches only the accounts it changes: a `V16` `SetParams` that leaves
    // every history within the memory re-encodes no account leaf at the next root.
    let mut h = Harness::v16();
    h.deposit(acct(1), "100000");
    h.deposit(acct(2), "100000");
    h.engine.state_root();
    h.operator_admin(AdminCommand::SetParams(with_divergence_shares(params())));
    h.engine.state_root();
    // The remaining state, the three markets `restress_all` re-evaluates, and the operator's
    // spent nonce; no account leaf, since no history exceeds the memory this sets.
    assert_eq!(h.engine.last_root_work().map(|work| work.leaves), Some(5));
}

#[test]
fn an_accounts_resting_orders_are_leaves_of_their_own() {
    let mut h = v9(ClientAuth::Unsigned);
    let a = acct(1);
    h.deposit(a, "100000");
    h.price(BTC, "100000");
    let limit = OrderKind::Limit { price: f("50000") };
    let orders: Vec<OrderId> = (0..200).map(|_| accepted(&h.order(a, BTC, Side::Buy, "0.01", limit))).collect();
    h.engine.state_root();
    // A deposit re-encodes the account, the deposit's reference, and the remaining state, however
    // many orders the account rests.
    h.deposit(a, "1");
    h.engine.state_root();
    assert_eq!(h.engine.last_root_work().map(|work| work.leaves), Some(3));
    // A cancel re-encodes the account and the market whose order index it edits, and removes the
    // order's leaf.
    h.cmd(Command::CancelOrder { account: a, order_id: orders[0], auth: None });
    assert_eq!(h.engine.state_root(), rebuilt_root(&h.engine));
    assert_eq!(h.engine.last_root_work().map(|work| work.leaves), Some(4));
}

#[test]
fn an_account_proof_holds_its_balance_and_positions_and_its_markets_marks() {
    let mut h = v9(ClientAuth::Unsigned);
    let (a, b) = (acct(1), acct(2));
    h.deposit(a, "10000");
    h.deposit(b, "500");
    h.price(BTC, "100000");
    h.price(ETH, "3000");
    h.market_order(a, BTC, Side::Buy, "0.05", "100000");
    h.market_order(a, ETH, Side::Sell, "1", "3000");
    h.order(a, BTC, Side::Buy, "0.01", OrderKind::Limit { price: f("90000") });
    let root = B256::from(h.engine.state_root());

    let proof = h.engine.account_proof(a).unwrap();
    assert!(proof.verify(root));
    let account = &h.engine.accounts()[&a];
    assert_eq!(proof.leaf.balance, account.balance);
    assert!(proof.positions.iter().map(|held| (&held.market, &held.position)).eq(&account.positions));
    let marks: Vec<_> = proof.markets.iter().map(|market| (market.market, market.leaf.mark)).collect();
    assert_eq!(marks, [(BTC, Some(f("100000"))), (ETH, Some(f("3000")))]);
    let flat = h.engine.account_proof(b).unwrap();
    assert!(flat.verify(root) && flat.markets.is_empty());
    assert_eq!(h.engine.account_proof(acct(9)), None);

    // A proof is the last root's until the next root.
    h.price(BTC, "101000");
    assert_eq!(h.engine.account_proof(a).as_ref(), Some(&proof));
    let next = B256::from(h.engine.state_root());
    let moved = h.engine.account_proof(a).unwrap();
    assert!(moved.verify(next) && !proof.verify(next));
    assert_eq!(moved.markets[0].leaf.mark, Some(f("101000")));

    // Altering what the proof claims, or leaving out a position's market, fails it.
    let mut altered = vec![moved.clone(); 9];
    altered[0].leaf.balance += f("1");
    altered[1].positions.pop();
    altered[2].markets.pop();
    altered[3].markets[0].leaf.mark = Some(f("1"));
    altered[4].account = b;
    altered[5].positions[0].position.qty += f("1");
    // Claiming one position fewer changes the account leaf.
    altered[6].positions.pop();
    altered[6].markets.pop();
    altered[6].leaf.open_positions -= 1;
    // Leaving out a position and its market under the account's own count, or proving one position
    // twice to make up the count, would hide a loss.
    altered[7].positions.pop();
    altered[7].markets.pop();
    altered[8].positions[1] = altered[8].positions[0].clone();
    altered[8].markets[1] = altered[8].markets[0].clone();
    assert!(altered.iter().all(|proof| !proof.verify(next)));
}

#[test]
fn a_markets_quote_currency_and_fx_mark_are_in_its_leaf() {
    let mut h = v9_fx();
    let a = acct(1);
    h.deposit(a, "100000");
    h.price(EURUSD, "1.08");
    h.price(DE40, "20000");
    h.market_order(a, DE40, Side::Buy, "1", "20000");
    let root = B256::from(h.engine.state_root());
    let proof = h.engine.account_proof(a).unwrap();
    assert!(proof.verify(root));
    let leaf = &proof.markets[0].leaf;
    assert_eq!(leaf.quote_currency, QuoteCurrency::Eur { fx_market: EURUSD });
    assert_eq!((leaf.mark, leaf.fx_mark), (Some(f("20000")), Some(f("1.08"))));

    // An EUR-USD price re-encodes EUR-USD and the one market it converts, Germany 40, whose leaf now
    // holds the new FX mark; no other market is taken for change.
    h.price(EURUSD, "1.2");
    let next = B256::from(h.engine.state_root());
    assert_eq!(h.engine.last_root_work().map(|work| work.leaves), Some(3));
    let moved = h.engine.account_proof(a).unwrap();
    assert!(moved.verify(next) && !proof.verify(next));
    assert_eq!(moved.markets[0].leaf.fx_mark, Some(f("1.2")));
    let mut altered = moved.clone();
    altered.markets[0].leaf.fx_mark = Some(f("1.08"));
    assert!(!altered.verify(next));
}

#[test]
fn roots_under_signed_requests_match_a_rebuild_from_the_state_after_every_command() {
    let mut h = v9(ClientAuth::Eip712 { chain_id: CHAIN_ID, vault: VAULT });
    let (a, b) = (h.signer(1), h.signer(2));
    let check = |h: &mut Harness| assert_eq!(h.engine.state_root(), rebuilt_root(&h.engine));
    h.deposit(a, "10000");
    check(&mut h);
    h.deposit(b, "10000");
    check(&mut h);
    h.price(BTC, "100000");
    check(&mut h);
    h.market_order(a, BTC, Side::Buy, "0.05", "100000");
    check(&mut h);
    let order_id = accepted(&h.order(b, BTC, Side::Buy, "0.01", OrderKind::Limit { price: f("90000") }));
    check(&mut h);
    h.cmd(Command::CancelOrder { account: b, order_id, auth: None });
    check(&mut h);
    h.cmd(Command::Withdraw { account: a, amount: f("100"), auth: None });
    check(&mut h);
    // A request at a used nonce is refused and changes no account.
    h.cmd(Client::new(1).withdraw(0, "100"));
    check(&mut h);
    // Past the markout horizon, the next price marks the fill's markout and takes it off the queue.
    h.advance(2_000);
    h.price(BTC, "100100");
    check(&mut h);
    let encoded = serde_json::to_value(&h.engine).unwrap();
    assert_eq!(encoded["markout_base"], 1);
}

#[test]
fn versions_before_v9_leave_the_markout_index_out_of_their_encoding() {
    let mut h = Harness::new();
    h.deposit(acct(1), "10000");
    h.price(BTC, "100000");
    h.market_order(acct(1), BTC, Side::Buy, "0.05", "100000");
    h.advance(2_000);
    h.price(BTC, "100100");
    // The fill's markout was marked, which moves the index only from `V9`.
    assert_ne!(h.engine.accounts()[&acct(1)].toxicity_bps, 0);
    assert!(serde_json::to_value(&h.engine).unwrap().get("markout_base").is_none());
}

// ── Helpers ──

/// `Harness::v8` under `auth`, then `V9`, whose state root is the root of the state tree.
fn v9(auth: ClientAuth) -> Harness {
    let mut h = Harness::v8(auth);
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V9 });
    h
}

/// `Harness::fx`, with the FX markets listed under `V7`, then `V9`.
fn v9_fx() -> Harness {
    let mut h = Harness::fx();
    h.admin(AdminCommand::SetOperator(operator()));
    h.admin(AdminCommand::ActivateProtocol { version: ProtocolVersion::V9 });
    h
}

/// The account numbered `n`.
fn account(n: u32) -> AccountId {
    let mut id = [0x22; 20];
    id[16..].copy_from_slice(&n.to_be_bytes());
    AccountId(id)
}

/// The id of the order an `OrderAccepted` among `events` carries.
fn accepted(events: &[Event]) -> OrderId {
    events
        .iter()
        .find_map(|event| match event {
            Event::OrderAccepted { order_id, .. } => Some(*order_id),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no order accepted: {events:?}"))
}

/// The root of an engine rebuilt from `engine`'s encoding, which hashes its state tree from scratch.
fn rebuilt_root(engine: &Engine) -> [u8; 32] {
    let mut fresh: Engine = serde_json::from_slice(&serde_json::to_vec(engine).unwrap()).unwrap();
    fresh.state_root()
}
