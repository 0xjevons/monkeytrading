//! Bitstamp. Each symbol's `order_book_<symbol>` channel publishes the top of its book on change,
//! stamped with the venue time in microseconds.

use engine_core::Fixed;
use serde::Deserialize;
use serde_json::json;
use serde_json::value::RawValue;

use crate::feeds::venue::{Book, Frame, check_size};

const CHANNEL_PREFIX: &str = "order_book_";

/// Every Bitstamp message, tagged by `event`.
#[derive(Deserialize)]
struct Message<'a> {
    event: &'a str,
    channel: &'a str,
    #[serde(borrow)]
    data: &'a RawValue,
}

#[derive(Deserialize)]
struct OrderBook<'a> {
    microtimestamp: &'a str,
    bids: Vec<(Fixed, Fixed)>,
    asks: Vec<(Fixed, Fixed)>,
}

#[derive(Deserialize)]
struct VenueError {
    message: String,
}

/// One subscribe message per symbol's order book channel.
pub(super) fn subscriptions(symbols: &[&str]) -> Vec<String> {
    symbols
        .iter()
        .map(|symbol| json!({ "event": "bts:subscribe", "data": { "channel": format!("{CHANNEL_PREFIX}{symbol}") } }))
        .map(|message| message.to_string())
        .collect()
}

/// Reads one Bitstamp message.
/// Error cases:
/// - an unknown event or channel, a missing field, an empty book side, a price, size, or time
///   that does not parse, or a top-of-book size out of range (see `check_size`)
/// - a `bts:error` message from the venue
pub(super) fn parse(text: &str) -> Result<Frame, String> {
    let message: Message = serde_json::from_str(text).map_err(|e| format!("unparseable message: {e}"))?;
    match message.event {
        "data" => {
            let symbol = message.channel.strip_prefix(CHANNEL_PREFIX).ok_or("data on an unknown channel")?;
            let book: OrderBook = serde_json::from_str(message.data.get()).map_err(|e| format!("bad book: {e}"))?;
            let micros: i64 = book.microtimestamp.parse().map_err(|e| format!("bad microtimestamp: {e}"))?;
            let (Some((bid, bid_size)), Some((ask, ask_size))) = (book.bids.first(), book.asks.first()) else {
                return Err(format!("{symbol} book has an empty side"));
            };
            check_size(symbol, "bid", *bid_size)?;
            check_size(symbol, "ask", *ask_size)?;
            // Floored, so a time before the epoch stays before it and is refused.
            let ts = micros.div_euclid(1_000);
            Ok(Frame::Books(vec![Book { symbol: symbol.to_owned(), bid: *bid, ask: *ask, ts }]))
        }
        "bts:subscription_succeeded" => Ok(Frame::Control),
        "bts:request_reconnect" => Ok(Frame::Reconnect),
        "bts:error" => {
            let error: VenueError = serde_json::from_str(message.data.get()).map_err(|e| format!("bad error: {e}"))?;
            Err(format!("venue error: {}", error.message))
        }
        event => Err(format!("unexpected event {event}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Recorded from wss://ws.bitstamp.net, with each book side cut to its top two levels.
    const BOOK: &str = r#"{"data":{"timestamp":"1792489472","microtimestamp":"1792489472108111","bids":[["2379.38","18.981795"],["2379.32","0.019034"]],"asks":[["2379.39","0.004202"],["2379.50","1.948232"]]},"channel":"order_book_ethusd","event":"data"}"#;
    const SUBSCRIBED: &str = r#"{"event":"bts:subscription_succeeded","channel":"order_book_ethusd","data":{}}"#;
    const ERROR: &str =
        r#"{"event":"bts:error","channel":"","data":{"code":null,"message":"Bad subscription string."}}"#;
    const RECONNECT: &str = r#"{"event":"bts:request_reconnect","channel":"","data":""}"#;

    #[test]
    fn a_book_gives_its_top_level_at_the_venue_time() {
        let book = Book {
            symbol: "ethusd".into(),
            bid: "2379.38".parse().unwrap(),
            ask: "2379.39".parse().unwrap(),
            ts: 1_792_489_472_108,
        };
        assert_eq!(parse(BOOK), Ok(Frame::Books(vec![book])));
    }

    #[test]
    fn a_microsecond_time_before_the_epoch_stays_before_it() {
        for (micros, ms) in [("-1", -1), ("-999", -1), ("-1000", -1), ("-1001", -2)] {
            let book = BOOK.replace("1792489472108111", micros);
            let Ok(Frame::Books(books)) = parse(&book) else { panic!("refused {micros}") };
            assert_eq!(books[0].ts, ms, "{micros}");
        }
    }

    #[test]
    fn acknowledgements_carry_no_price_and_a_reconnect_request_ends_the_connection() {
        assert_eq!(parse(SUBSCRIBED), Ok(Frame::Control));
        assert_eq!(parse(RECONNECT), Ok(Frame::Reconnect));
    }

    #[test]
    fn a_venue_error_is_refused_with_its_reason() {
        let err = parse(ERROR).unwrap_err();
        assert!(err.contains("Bad subscription string."), "{err}");
    }

    #[test]
    fn malformed_books_are_refused() {
        let cases = [
            BOOK.replace(r#""bids":[["2379.38","18.981795"],["2379.32","0.019034"]]"#, r#""bids":[]"#),
            BOOK.replace(r#"["2379.39","0.004202"]"#, r#"["2379,39","0.004202"]"#),
            BOOK.replace(r#""microtimestamp":"1792489472108111""#, r#""microtimestamp":"soon""#),
            BOOK.replace(r#""channel":"order_book_ethusd""#, r#""channel":"live_trades_ethusd""#),
            BOOK.replace(r#""event":"data""#, r#""event":"bts:unknown""#),
            BOOK.replace(r#"["2379.38","18.981795"]"#, r#"["2379.38","0"]"#),
            BOOK.replace(r#"["2379.39","0.004202"]"#, r#"["2379.39","-0.004202"]"#),
            r#"{"event":"data","channel":"order_book_ethusd"}"#.to_owned(),
        ];
        for case in cases {
            assert!(parse(&case).is_err(), "accepted {case}");
        }
    }

    #[test]
    fn each_symbol_gets_its_own_order_book_subscription() {
        let sent: Vec<serde_json::Value> =
            subscriptions(&["btcusd", "ethusd"]).iter().map(|text| serde_json::from_str(text).unwrap()).collect();
        let expected = ["order_book_btcusd", "order_book_ethusd"]
            .map(|channel| json!({ "event": "bts:subscribe", "data": { "channel": channel } }));
        assert_eq!(sent, expected);
    }
}
