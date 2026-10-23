//! Parse the official publisher CSVs, selecting the catalogue series by its publisher header.

use std::collections::BTreeMap;

use anyhow::{Context, bail, ensure};
use chrono::NaiveDate;
use engine_core::Fixed;
use engine_core::benchmark::BenchmarkRate;

use crate::config::Publisher;

/// Parses publisher dates and exact decimal percent values, retaining the predecessor of `from`.
/// Error cases:
/// - wrong series/schema, malformed dates/rates, conflicting duplicates, or no observations
pub(super) fn parse(
    publisher: Publisher,
    bytes: &[u8],
    from: NaiveDate,
    to: NaiveDate,
) -> anyhow::Result<Vec<BenchmarkRate>> {
    let mut reader = csv::ReaderBuilder::new().has_headers(false).flexible(true).from_reader(bytes);
    let mut columns = None;
    let mut observations = BTreeMap::new();
    let mut started = false;
    for record in reader.records() {
        let record = record.context("invalid publisher CSV")?;
        if record.iter().all(|s| s.trim().is_empty()) {
            continue;
        }
        if columns.is_none() {
            columns = match publisher {
                Publisher::Sofr => named_columns(&record, "Effective Date", "Rate (%)"),
                Publisher::Estr => named_columns(&record, "TIME_PERIOD", "OBS_VALUE"),
                Publisher::Sonia => named_columns(&record, "DATE", "IUDSOIA"),
                Publisher::Aonia if record.get(0) == Some("Series ID") => {
                    named_columns(&record, "Series ID", "FIRMMCRID")
                }
                Publisher::Tona if record.get(0) == Some("Series code") && record.get(1) == Some("FM01'STRDCLUCON") => {
                    Some((0, 1))
                }
                _ => None,
            };
            continue;
        }
        let (date_column, rate_column) = columns.expect("checked above");
        let day = record.get(date_column).context("missing observation date")?.trim();
        let format = match publisher {
            Publisher::Sofr => "%m/%d/%Y",
            Publisher::Estr => "%Y-%m-%d",
            Publisher::Sonia => "%d %b %Y",
            Publisher::Aonia => "%d-%b-%Y",
            Publisher::Tona => "%Y/%m/%d",
        };
        let date = match NaiveDate::parse_from_str(day, format) {
            Ok(date) => date,
            Err(_) if publisher == Publisher::Tona && !started => continue,
            Err(_) => bail!("invalid publisher observation date"),
        };
        started = true;
        if publisher == Publisher::Sofr {
            ensure!(record.get(1) == Some("SOFR"), "wrong NY Fed rate type");
        }
        if publisher == Publisher::Estr {
            ensure!(record.get(0) == Some("EST.B.EU000A2X2A25.WT"), "wrong ECB series");
        }
        let value = record.get(rate_column).context("missing observation rate")?.trim();
        if matches!(value, "" | "NA" | "N/A" | "..") {
            continue;
        }
        let percent: Fixed = value.parse().context("invalid publisher percent rate")?;
        ensure!(
            percent >= Fixed::from_int(-100) && percent <= Fixed::from_int(100),
            "publisher rate is outside +/-100%"
        );
        let rate = percent.div_int(100);
        ensure!(rate.mul(Fixed::from_int(100)) == percent, "publisher rate loses decimal precision");
        if date > to {
            continue;
        }
        if let Some(old) = observations.insert(date, rate) {
            ensure!(old == rate, "conflicting publisher observations for one date");
        }
    }
    ensure!(columns.is_some() && !observations.is_empty(), "publisher returned no matching observations");
    let predecessor = observations.range(..from).next_back().map(|(date, _)| *date);
    Ok(observations
        .into_iter()
        .filter(|(date, _)| *date >= from || Some(*date) == predecessor)
        .map(|(effective_date, annual_rate)| BenchmarkRate {
            benchmark: publisher.benchmark().into(),
            effective_date,
            annual_rate,
        })
        .collect())
}

/// Finds both required header columns without relying on their positions.
fn named_columns(record: &csv::StringRecord, date: &str, rate: &str) -> Option<(usize, usize)> {
    Some((record.iter().position(|s| s.trim() == date)?, record.iter().position(|s| s.trim() == rate)?))
}
