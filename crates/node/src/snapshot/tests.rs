use super::*;
use std::num::NonZeroUsize;

use engine_core::command::{Command, Sequenced};

use crate::test_support::scratch_dir;

#[test]
fn snapshot_configuration_refuses_zero_limits_and_unknown_keys() {
    let text = "dir = 'cache'\nretain = 3\nmax_bytes = 1024\ntrusted_signers = []\n";
    let cfg: SnapshotSection = toml::from_str(text).unwrap();
    assert_eq!((cfg.retain.get(), cfg.max_bytes.get()), (3, 1024));
    for bad in [
        text.replace("retain = 3", "retain = 0"),
        text.replace("max_bytes = 1024", "max_bytes = 0"),
        text.replace("retain = 3", "retian = 3"),
        format!("{text}retian = 3\n"),
        text.replace("trusted_signers = []", "trusted_signers = ['not an address']"),
        text.replace("trusted_signers = []", ""),
    ] {
        assert!(toml::from_str::<SnapshotSection>(&bad).is_err(), "{bad}");
    }
}

#[test]
fn authenticates_payload_and_deployment_before_loading_state() {
    let (cfg, key, mut snapshot) = fixture("snapshot-trust");
    write(&cfg, &snapshot, &key).unwrap();
    let path = candidates(&cfg).unwrap().remove(0);
    let (_, mut restored) = read(&cfg, &path, 31337, snapshot.vault).unwrap();
    assert_eq!(hex::encode(restored.state_root()), snapshot.checkpoint.root[2..]);
    let untrusted = SnapshotSection { trusted_signers: vec![], ..cfg.clone() };
    assert!(
        read(&untrusted, &path, 31337, snapshot.vault).err().unwrap().to_string().contains("not independently trusted")
    );
    assert!(read(&cfg, &path, 1, snapshot.vault).is_err());
    assert!(read(&cfg, &path, 31337, AccountId([1; 20])).is_err());
    let mut envelope: Envelope = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    envelope.payload.push(' ');
    fs::write(&path, serde_json::to_vec(&envelope).unwrap()).unwrap();
    assert!(read(&cfg, &path, 31337, snapshot.vault).is_err(), "self-reported root cannot authorize altered payload");
    snapshot.checkpoint.root = format!("0x{}", "00".repeat(32));
    write(&cfg, &snapshot, &key).unwrap();
    assert!(read(&cfg, &path, 31337, snapshot.vault).err().unwrap().to_string().contains("state root mismatch"));
}

#[test]
fn retains_newest_caches_and_ignores_staging_files() {
    let (cfg, key, mut snapshot) = fixture("snapshot-retain");
    for index in 1..5 {
        snapshot.offset.index = index;
        write(&cfg, &snapshot, &key).unwrap();
    }
    fs::write(cfg.dir.join("99999999999999999999-0000000000.json.tmp"), b"torn").unwrap();
    let paths = candidates(&cfg).unwrap();
    assert_eq!(paths.len(), 2);
    assert!(paths[0].ends_with("00000000000000000000-0000000004.json"));
    fs::write(&paths[0], b"{").unwrap();
    assert!(read(&cfg, &paths[0], 31337, snapshot.vault).is_err());
    let small = SnapshotSection { max_bytes: NonZeroUsize::new(1).unwrap(), ..cfg };
    assert!(write(&small, &snapshot, &key).is_err());
    assert!(read(&small, &paths[1], 31337, snapshot.vault).err().unwrap().to_string().contains("max_bytes"));
}

fn fixture(label: &str) -> (SnapshotSection, EnclaveKey, Snapshot) {
    let key = EnclaveKey::generate();
    let cfg = SnapshotSection {
        dir: scratch_dir(label),
        retain: NonZeroUsize::new(2).unwrap(),
        max_bytes: NonZeroUsize::new(16 * 1024 * 1024).unwrap(),
        trusted_signers: vec![AccountId(key.address())],
    };
    let mut engine = Engine::new();
    engine.apply(&Sequenced { seq: 0, ts: 0, cmd: Command::Tick }).unwrap();
    let root = engine.state_root();
    let vault = AccountId([0; 20]);
    let checkpoint = Checkpoint {
        seq: 0,
        root: format!("0x{}", hex::encode(root)),
        signer: key.address_hex(),
        signature: key.sign_checkpoint(31337, &vault.0, 0, &root),
    };
    let snapshot = Snapshot {
        chain_id: 31337,
        vault,
        checkpoint,
        offset: Offset { seq: 0, index: 1 },
        engine: serde_json::to_string(&engine).unwrap(),
        progress: Progress::default(),
    };
    (cfg, key, snapshot)
}
