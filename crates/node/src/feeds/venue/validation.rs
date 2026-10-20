//! The checks every venue's books pass before they are sequenced, shared by the adapters.

use std::collections::BTreeMap;

use engine_core::Fixed;
use engine_core::price::SourceQuote;
use engine_core::types::{MarketId, Millis};

use crate::config::Venue;
use crate::feeds::venue::Book;

/// The venue's mid for one book and the market it prices, refusing a book no working venue
/// could show.
/// Error cases:
/// - the symbol is not one this connection subscribed to
/// - the bid or ask is outside the valid input range, or the book is crossed or locked
/// - the timestamp is before the epoch
pub(in crate::feeds) fn to_quote(
    venue: Venue,
    markets: &BTreeMap<String, MarketId>,
    book: Book,
) -> Result<(MarketId, SourceQuote), String> {
    let Book { symbol, bid, ask, ts } = book;
    let market = *markets.get(&symbol).ok_or_else(|| format!("unsubscribed symbol {symbol}"))?;
    if !bid.is_valid_input() || !ask.is_valid_input() {
        return Err(format!("{symbol} bid {bid} or ask {ask} out of range"));
    }
    if bid >= ask {
        return Err(format!("{symbol} book crossed: bid {bid}, ask {ask}"));
    }
    check_time(&symbol, ts)?;
    Ok((market, SourceQuote { source: venue.name().to_owned(), mid: (bid + ask).div_int(2), ts }))
}

/// Refuses a top-of-book size no working venue could show.
/// Error cases:
/// - `size` is not positive, or is above `Fixed::MAX_INPUT`
pub(in crate::feeds) fn check_size(symbol: &str, side: &str, size: Fixed) -> Result<(), String> {
    if !size.is_valid_input() {
        return Err(format!("{symbol} {side} size {size} out of range"));
    }
    Ok(())
}

/// Refuses a venue time no working venue could send. A time ahead of the host clock is not
/// refused: refusing it would leave the engine on older quotes when the host clock is behind.
/// Error cases:
/// - `ts` is before the epoch
pub(in crate::feeds) fn check_time(symbol: &str, ts: Millis) -> Result<(), String> {
    if ts < 0 {
        return Err(format!("{symbol} timestamp {ts} before the epoch"));
    }
    Ok(())
}
