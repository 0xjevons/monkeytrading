//! Kraken spot WebSocket v2. The `ticker` channel with `event_trigger = "bbo"` publishes on every
//! change to the best bid or ask, stamped with the venue time. It publishes nothing while the top
//! holds, so a top that holds past a market's `max_age_ms` ages out in the engine and the index
//! runs on the other venues until it moves. Prices and sizes arrive as JSON numbers, so they are
//! read from the raw number text, never through a float.

use chrono::DateTime;
use engine_core::Fixed;
use serde::Deserialize;
use serde_json::json;
use serde_json::value::RawValue;

use crate::feeds::venue::{Book, Frame, check_size};

/// Every Kraken message: a channel message, or a reply to a method call.
#[derive(Deserialize)]
struct Message<'a> {
    #[serde(borrow)]
    channel: Option<&'a str>,
    #[serde(borrow)]
    method: Option<&'a str>,
    success: Option<bool>,
    error: Option<String>,
    #[serde(borrow)]
    data: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct Ticker<'a> {
    symbol: String,
    #[serde(borrow)]
    bid: &'a RawValue,
    #[serde(borrow)]
    bid_qty: &'a RawValue,
    #[serde(borrow)]
    ask: &'a RawValue,
    #[serde(borrow)]
    ask_qty: &'a RawValue,
    timestamp: &'a str,
}

/// The subscribe message for best-bid-and-offer tickers on `symbols`.
pub(super) fn subscriptions(symbols: &[&str]) -> Vec<String> {
    let params = json!({ "channel": "ticker", "symbol": symbols, "event_trigger": "bbo" });
    vec![json!({ "method": "subscribe", "params": params }).to_string()]
}

/// Reads one Kraken message.
/// Error cases:
/// - an unknown channel or method, a missing field, or a price or time that does not parse
/// - a failed subscription
pub(super) fn parse(text: &str) -> Result<Frame, String> {
    let message: Message = serde_json::from_str(text).map_err(|e| format!("unparseable message: {e}"))?;
    match (message.channel, message.method) {
        (Some("ticker"), None) => {
            let data = message.data.ok_or("ticker without data")?;
            let tickers: Vec<Ticker> = serde_json::from_str(data.get()).map_err(|e| format!("bad ticker: {e}"))?;
            tickers.into_iter().map(book).collect::<Result<_, _>>().map(Frame::Books)
        }
        (Some("heartbeat" | "status"), None) => Ok(Frame::Control),
        (None, Some("subscribe")) if message.success == Some(true) => Ok(Frame::Control),
        (None, Some(method)) => Err(format!("venue error on {method}: {}", message.error.unwrap_or_default())),
        _ => Err("unexpected message".into()),
    }
}

/// One ticker's top of book.
/// Error cases:
/// - a price or size is not a plain decimal number, or a size is out of range (see `check_size`)
/// - the timestamp is not RFC 3339
fn book(ticker: Ticker) -> Result<Book, String> {
    let decimal = |raw: &RawValue| raw.get().parse::<Fixed>().map_err(|e| format!("bad number: {e}"));
    check_size(&ticker.symbol, "bid", decimal(ticker.bid_qty)?)?;
    check_size(&ticker.symbol, "ask", decimal(ticker.ask_qty)?)?;
    let ts = DateTime::parse_from_rfc3339(ticker.timestamp)
        .map_err(|e| format!("bad timestamp {}: {e}", ticker.timestamp))?
        .timestamp_millis();
    Ok(Book { symbol: ticker.symbol, bid: decimal(ticker.bid)?, ask: decimal(ticker.ask)?, ts })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Recorded from wss://ws.kraken.com/v2.
    const SNAPSHOT: &str = r#"{"channel":"ticker","type":"snapshot","data":[{"symbol":"ETH/USD","bid":2379.13,"bid_qty":3.30968537,"ask":2379.14,"ask_qty":3.78223814,"last":2379.07,"volume":19749.97257955,"vwap":2399.14,"low":2370.41,"high":2442.47,"change":-62.02,"change_pct":-2.54,"trades":47710,"timestamp":"2026-10-20T09:43:52.289915Z"}]}"#;
    const HEARTBEAT: &str = r#"{"channel":"heartbeat"}"#;
    const STATUS: &str = r#"{"channel":"status","type":"update","data":[{"version":"2.0.10","system":"online","api_version":"v2","connection_id":5169314452737955867,"upcoming_maintenance":[],"emergency":[]}]}"#;
    const SUBSCRIBED: &str = r#"{"method":"subscribe","result":{"channel":"ticker","event_trigger":"bbo","snapshot":true,"symbol":"ETH/USD"},"success":true,"time_in":"2026-10-20T09:43:52.433866Z","time_out":"2026-10-20T09:43:52.433915Z"}"#;
    const REJECTED: &str = r#"{"error":"Currency pair not supported BTC/USX","method":"subscribe","success":false,"symbol":"BTC/USX","time_in":"2026-10-20T09:43:52.433866Z","time_out":"2026-10-20T09:43:52.433915Z"}"#;

    #[test]
    fn a_ticker_gives_the_exact_decimal_bid_and_ask_at_the_venue_time() {
        let book = Book {
            symbol: "ETH/USD".into(),
            bid: "2379.13".parse().unwrap(),
            ask: "2379.14".parse().unwrap(),
            // 2026-10-20T09:43:52.289915Z, truncated to the millisecond.
            ts: 1_792_489_432_289,
        };
        assert_eq!(parse(SNAPSHOT), Ok(Frame::Books(vec![book])));
    }

    #[test]
    fn heartbeats_status_and_acknowledgements_carry_no_price() {
        assert_eq!(parse(HEARTBEAT), Ok(Frame::Control));
        assert_eq!(parse(STATUS), Ok(Frame::Control));
        assert_eq!(parse(SUBSCRIBED), Ok(Frame::Control));
    }

    #[test]
    fn a_failed_subscription_is_refused_with_its_reason() {
        let err = parse(REJECTED).unwrap_err();
        assert!(err.contains("Currency pair not supported BTC/USX"), "{err}");
    }

    #[test]
    fn malformed_tickers_are_refused() {
        let cases = [
            // Exponent notation is not a plain decimal; reading it would mean going through a float.
            SNAPSHOT.replace(r#""bid":2379.13"#, r#""bid":2.37913e3"#),
            SNAPSHOT.replace(r#""ask":2379.14"#, r#""ask":"2379.14""#),
            SNAPSHOT.replace(r#""bid":2379.13"#, r#""bid":null"#),
            SNAPSHOT.replace(r#""bid_qty":3.30968537"#, r#""bid_qty":0"#),
            SNAPSHOT.replace(r#""ask_qty":3.78223814"#, r#""ask_qty":-1"#),
            SNAPSHOT.replace(r#""ask_qty":3.78223814,"#, ""),
            SNAPSHOT.replace(r#""timestamp":"2026-10-20T09:43:52.289915Z""#, r#""timestamp":1792489432"#),
            SNAPSHOT.replace(r#""channel":"ticker""#, r#""channel":"book""#),
            r#"{"channel":"ticker","type":"update"}"#.to_owned(),
        ];
        for case in cases {
            assert!(parse(&case).is_err(), "accepted {case}");
        }
    }

    #[test]
    fn the_subscription_asks_for_best_bid_and_offer_updates_on_every_symbol() {
        let sent: serde_json::Value = serde_json::from_str(&subscriptions(&["BTC/USD", "ETH/USD"])[0]).unwrap();
        let params = json!({ "channel": "ticker", "symbol": ["BTC/USD", "ETH/USD"], "event_trigger": "bbo" });
        assert_eq!(sent, json!({ "method": "subscribe", "params": params }));
    }
}
