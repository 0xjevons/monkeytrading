use super::*;

use crate::test_support::DEV_CONFIG as DEV;

#[test]
fn the_committed_dev_config_loads() {
    let cfg = NodeConfig::load(Path::new(DEV)).unwrap();
    assert_eq!(cfg.node.tick, Duration::from_millis(250));
    assert!(!cfg.genesis.markets.is_empty());
    assert!(!cfg.operator.rotation_enabled);
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
