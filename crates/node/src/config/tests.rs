use super::*;

use crate::test_support::DEV_CONFIG as DEV;

#[test]
fn the_committed_dev_config_loads() {
    let cfg = NodeConfig::load(Path::new(DEV)).unwrap();
    assert_eq!(cfg.node.tick, Duration::from_millis(250));
    assert!(!cfg.genesis.markets.is_empty());
    assert!(!cfg.operator.rotation_enabled);
    assert_eq!((cfg.api.per_ip.per_second.get(), cfg.api.per_ip.burst.get()), (50, 100));
}

#[test]
fn a_mistyped_key_fails_the_boot() {
    let text = std::fs::read_to_string(DEV).unwrap().replace("checkpoint_every", "checkpoint_evry");
    assert!(toml::from_str::<NodeConfig>(&text).is_err());
    let text = std::fs::read_to_string(DEV).unwrap().replace("house_cap_bps", "house_capp_bps");
    assert!(toml::from_str::<NodeConfig>(&text).is_err());
}

#[test]
fn a_zero_segment_size_is_refused() {
    let text = std::fs::read_to_string(DEV).unwrap().replace("segment_commands = 1000", "segment_commands = 0");
    let cfg: NodeConfig = toml::from_str(&text).unwrap();
    assert!(cfg.validate().is_err());
}

#[test]
fn unsigned_requests_need_the_mock_tee_and_mock_custody() {
    let dev = std::fs::read_to_string(DEV).unwrap();
    let tee = dev.replace("provider = \"mock\" # \"configfs_tsm\"", "provider = \"configfs_tsm\" #");
    let custody = dev.replace("[custody]\nprovider = \"mock\"", "[custody]\nprovider = \"evm\"");
    for text in [&tee, &custody] {
        assert_ne!(text, &dev, "the replacement must change the config");
        let cfg: NodeConfig = toml::from_str(text).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("node.auth = \"none\""), "{err:#}");
        let signed: NodeConfig = toml::from_str(&text.replace("auth = \"none\"", "auth = \"eip712\"")).unwrap();
        signed.validate().unwrap();
    }
}

#[test]
fn signed_requests_take_their_domain_from_the_custody_chain_and_vault() {
    let dev = std::fs::read_to_string(DEV).unwrap();
    assert_eq!(toml::from_str::<NodeConfig>(&dev).unwrap().client_auth().unwrap(), ClientAuth::Unsigned);
    let vault = "0x5fbdb2315678afecb367f032d93f642f64180aa3";
    let signed = dev
        .replace("auth = \"none\"", "auth = \"eip712\"")
        .replace("chain_id = 31337", "chain_id = 42161")
        .replace("vault = \"0x0000000000000000000000000000000000000000\"", &format!("vault = \"{vault}\""));
    let cfg: NodeConfig = toml::from_str(&signed).unwrap();
    assert_eq!(cfg.client_auth().unwrap(), ClientAuth::Eip712 { chain_id: 42_161, vault: vault.parse().unwrap() });
}

#[test]
fn genesis_values_the_engine_would_refuse_fail_the_boot() {
    let dev = std::fs::read_to_string(DEV).unwrap();
    let capital = |amount: &str| dev.replace("house_capital = \"10000000\"", &format!("house_capital = \"{amount}\""));
    let rate = |bps: i64| {
        dev.replace("{ name = \"SOFR\", annual_bps = 400 }", &format!("{{ name = \"SOFR\", annual_bps = {bps} }}"))
    };
    // The first listing again, at the end: market id 1 twice.
    let first = &dev[dev.find("[[genesis.markets]]").unwrap()..];
    let first = &first[..first[1..].find("[[genesis.markets]]").unwrap() + 1];
    let repeated_market = format!("{dev}\n{first}");
    let refused = [capital("-1"), capital("1000000000.000000001"), rate(10_001), rate(i64::MIN), repeated_market];
    for text in &refused {
        assert_ne!(text, &dev, "the replacement must change the config");
        assert!(toml::from_str::<NodeConfig>(text).unwrap().validate().is_err());
    }
    // A house that starts with no capital, and the rate limit itself, are accepted.
    for text in [capital("0"), rate(-10_000)] {
        assert_ne!(text, dev, "the replacement must change the config");
        toml::from_str::<NodeConfig>(&text).unwrap().validate().unwrap();
    }
}

#[test]
fn the_dev_config_checks_time_against_three_roughtime_servers() {
    let cfg = NodeConfig::load(Path::new(DEV)).unwrap();
    let keys: Vec<String> = cfg.time.servers.iter().map(|s| STANDARD.encode(s.public_key.as_bytes())).collect();
    assert_eq!(
        keys,
        [
            "AW5uAoTSTDfG5NfY1bTh08GUnOqlRb+HVhbJ3ODJvsE=",
            "v2CievhgKsxzlWPwIkYFUXeA51Akhkv5uhJCj1/kbiY=",
            "iBVjxg/1j7y1+kQUTBYdTabxCppesU/07D4PMDJk2WA=",
        ]
    );
    assert_eq!(cfg.time.max_drift, Duration::from_secs(15));
}

#[test]
fn a_public_key_that_is_not_32_bytes_of_base64_fails_the_boot() {
    let key = "AW5uAoTSTDfG5NfY1bTh08GUnOqlRb+HVhbJ3ODJvsE=";
    for bad in
        ["not base64!", "AW5uAoTSTDfG5NfY1bTh08GUnOqlRb+HVhbJ3ODJvg==", "AW5uAoTSTDfG5NfY1bTh08GUnOqlRb+HVhbJ3ODJvsEA"]
    {
        let text = std::fs::read_to_string(DEV).unwrap().replace(key, bad);
        assert!(toml::from_str::<NodeConfig>(&text).is_err(), "{bad}");
    }
}

#[test]
fn fewer_than_two_servers_or_a_key_listed_twice_is_refused() {
    let mut cfg = NodeConfig::load(Path::new(DEV)).unwrap();
    cfg.time.servers.truncate(1);
    assert!(format!("{:#}", cfg.validate().unwrap_err()).contains("at least two servers"));

    let mut cfg = NodeConfig::load(Path::new(DEV)).unwrap();
    cfg.time.servers[2].public_key = cfg.time.servers[0].public_key;
    assert!(format!("{:#}", cfg.validate().unwrap_err()).contains("same public key twice"));
}

#[test]
fn a_zero_drift_bound_or_sampling_interval_is_refused() {
    for (from, to) in [("max_drift = \"15s\"", "max_drift = \"0s\""), ("interval = \"10s\"", "interval = \"0s\"")] {
        let text = std::fs::read_to_string(DEV).unwrap().replace(from, to);
        let cfg: NodeConfig = toml::from_str(&text).unwrap();
        assert!(format!("{:#}", cfg.validate().unwrap_err()).contains("time.max_drift and time.interval"), "{to}");
    }
}

#[test]
fn a_round_that_could_outlast_the_grace_is_refused() {
    let mut cfg = NodeConfig::load(Path::new(DEV)).unwrap();
    cfg.time.max_drift = Duration::from_millis(24_999);
    assert!(cfg.validate().is_ok());
    cfg.time.max_drift = Duration::from_secs(25);
    assert!(format!("{:#}", cfg.validate().unwrap_err()).contains("twice time.max_drift"));
}

#[test]
fn a_server_address_must_be_host_and_port() {
    for (address, ok) in [
        ("rough.time.nl:2002", true),
        ("[::1]:5319", true),
        ("rough.time.nl", false),
        (":2002", false),
        ("a:99999", false),
    ] {
        let mut cfg = NodeConfig::load(Path::new(DEV)).unwrap();
        cfg.time.servers[0].address = address.into();
        assert_eq!(cfg.validate().is_ok(), ok, "{address}");
    }
}

#[test]
fn a_zero_history_retention_is_refused() {
    let text = std::fs::read_to_string(DEV).unwrap().replace("retention = \"365d\"", "retention = \"0s\"");
    let cfg: NodeConfig = toml::from_str(&text).unwrap();
    assert!(format!("{:#}", cfg.validate().unwrap_err()).contains("history.retention"));
}

#[test]
fn a_zero_tick_is_refused() {
    let text = std::fs::read_to_string(DEV).unwrap().replace("tick = \"250ms\"", "tick = \"0s\"");
    let cfg: NodeConfig = toml::from_str(&text).unwrap();
    assert!(cfg.validate().is_err());
}

#[test]
fn the_committed_venue_feeds_price_both_crypto_markets_from_three_venues() {
    let mut cfg = NodeConfig::load(Path::new(DEV)).unwrap();
    cfg.feeds.provider = FeedProvider::Venues;
    cfg.validate().unwrap();
    let connections = cfg.feeds.venues.unwrap().connections;
    let venues: Vec<Venue> = connections.iter().map(|c| c.venue).collect();
    assert_eq!(venues, [Venue::Coinbase, Venue::Kraken, Venue::Bitstamp]);
    for conn in connections {
        let markets: Vec<MarketId> = conn.symbols.iter().map(|s| s.market).collect();
        assert_eq!(markets, [1, 2], "{}", conn.venue.name());
    }
}

#[test]
fn an_interval_that_outlasts_the_venue_timeout_is_refused() {
    let mut cfg = NodeConfig::load(Path::new(DEV)).unwrap();
    cfg.feeds.interval = cfg.feeds.venues.as_ref().unwrap().timeout;
    let err = format!("{:#}", cfg.validate().unwrap_err());
    assert!(err.contains("feeds.interval must be shorter than feeds.venues.timeout"), "{err}");
}

#[test]
fn venue_feeds_need_their_section() {
    let mut cfg = NodeConfig::load(Path::new(DEV)).unwrap();
    cfg.feeds.provider = FeedProvider::Venues;
    cfg.feeds.venues = None;
    let err = format!("{:#}", cfg.validate().unwrap_err());
    assert!(err.contains("needs [feeds.venues]"), "{err}");
}

#[test]
fn venue_settings_that_would_fail_silently_are_refused() {
    type Edit = fn(&mut VenuesSection);
    let cases: [(&str, Edit); 13] = [
        ("non-zero", |v| v.timeout = Duration::ZERO),
        ("non-zero", |v| v.reconnect_min = Duration::ZERO),
        ("non-zero", |v| v.stale_after = Duration::ZERO),
        ("exceeds reconnect_max", |v| v.reconnect_max = v.reconnect_min / 2),
        ("no connections", |v| v.connections.clear()),
        ("coinbase is listed twice", |v| v.connections.push(v.connections[0].clone())),
        ("must be wss://", |v| v.connections[0].url = "ws://advanced-trade-ws.coinbase.com".into()),
        ("must be on coinbase.com", |v| v.connections[0].url = "wss://ws.kraken.com/v2".into()),
        ("must be on coinbase.com", |v| v.connections[0].url = "wss://coinbase.com.example.net".into()),
        ("must be on coinbase.com", |v| v.connections[0].url = "wss://evilcoinbase.com".into()),
        ("has no symbols", |v| v.connections[0].symbols.clear()),
        ("lists market 1 twice", |v| v.connections[0].symbols[1].market = 1),
        ("lists symbol BTC-USD twice", |v| v.connections[0].symbols[1].symbol = "BTC-USD".into()),
    ];
    for (expected, edit) in cases {
        let mut cfg = NodeConfig::load(Path::new(DEV)).unwrap();
        edit(cfg.feeds.venues.as_mut().unwrap());
        let err = format!("{:#}", cfg.validate().unwrap_err());
        assert!(err.contains(expected), "expected {expected:?}, got {err}");
    }
}

#[test]
fn a_zero_api_timeout_is_refused() {
    let dev = std::fs::read_to_string(DEV).unwrap();
    for (from, to) in [
        ("send_timeout = \"30s\"", "send_timeout = \"0s\""),
        ("read_timeout = \"30s\"", "read_timeout = \"0s\""),
        ("handshake_timeout = \"10s\"", "handshake_timeout = \"0s\""),
    ] {
        let cfg: NodeConfig = toml::from_str(&dev.replace(from, to)).unwrap();
        assert!(cfg.validate().is_err(), "{to}");
    }
}

#[test]
fn a_zero_api_rate_or_cap_fails_the_boot() {
    let dev = std::fs::read_to_string(DEV).unwrap();
    for (from, to) in [
        ("per_ip = { per_second = 50,", "per_ip = { per_second = 0,"),
        ("per_second = 10, burst = 20 }", "per_second = 10, burst = 0 }"),
        ("max_subscribers = 256", "max_subscribers = 0"),
    ] {
        // Each edit touches one value, so nothing else can be what fails the parse.
        assert_eq!(dev.matches(from).count(), 1, "{from}");
        assert!(toml::from_str::<NodeConfig>(&dev.replace(from, to)).is_err(), "{to}");
    }
}
