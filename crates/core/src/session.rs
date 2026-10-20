//! Trading hours. Crypto trades 24/7; RWA markets follow their venue's weekly windows in local
//! time (so DST is handled by the tz database), minus holiday closures. While a market is closed
//! nothing fills. Resting orders wait, and the first fresh price after the open fills them, gap
//! included.

use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Datelike, NaiveDate, Timelike, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::types::Millis;

const MINUTES_PER_DAY: u32 = 24 * 60;
const DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Schedule {
    AlwaysOpen,
    Weekly {
        tz: Tz,
        windows: Vec<Window>,
        /// Whole local days the venue is shut (exchange holidays).
        #[serde(default)]
        closed_dates: Vec<NaiveDate>,
    },
}

/// `[open, close)` in venue-local week time. A window whose close is before its open wraps over
/// the week boundary (e.g. FX: Sun 17:00 to Fri 17:00).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Window {
    pub open: WeekTime,
    pub close: WeekTime,
}

/// Minutes since Monday 00:00 local time, written as `"Mon 09:30"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct WeekTime(u32);

impl fmt::Display for WeekTime {
    /// `"Mon 09:30"` form.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let day = DAYS[(self.0 / MINUTES_PER_DAY) as usize];
        let mins = self.0 % MINUTES_PER_DAY;
        write!(f, "{day} {:02}:{:02}", mins / 60, mins % 60)
    }
}

impl FromStr for WeekTime {
    type Err = String;

    /// Parses `"<Day> HH:MM"` with a three-letter English day.
    /// Error cases:
    /// - no space between day and time, or no `:` in the time
    /// - an unknown day name
    /// - an hour above 23 or a minute above 59
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || format!("invalid week time {s:?}, expected e.g. \"Mon 09:30\"");
        let (day, hm) = s.split_once(' ').ok_or_else(err)?;
        let day = DAYS.iter().position(|d| *d == day).ok_or_else(err)? as u32;
        let (h, m) = hm.split_once(':').ok_or_else(err)?;
        let h: u32 = h.parse().map_err(|_| err())?;
        let m: u32 = m.parse().map_err(|_| err())?;
        if h > 23 || m > 59 {
            return Err(err());
        }
        Ok(WeekTime(day * MINUTES_PER_DAY + h * 60 + m))
    }
}

impl Serialize for WeekTime {
    /// Serializes in `"Mon 09:30"` form.
    /// Error cases:
    /// - the serializer rejects a string
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for WeekTime {
    /// Deserializes from `"Mon 09:30"` form.
    /// Error cases:
    /// - not a string
    /// - a string `WeekTime::from_str` rejects
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?.parse().map_err(serde::de::Error::custom)
    }
}

impl Schedule {
    /// Whether the venue is open at `ts`. A timestamp outside chrono's range reads as closed.
    pub fn is_open(&self, ts: Millis) -> bool {
        match self {
            Schedule::AlwaysOpen => true,
            Schedule::Weekly { tz, windows, closed_dates } => {
                let Some(utc) = DateTime::<Utc>::from_timestamp_millis(ts) else {
                    return false;
                };
                let local = utc.with_timezone(tz);
                if closed_dates.contains(&local.date_naive()) {
                    return false;
                }
                let t = WeekTime(
                    local.weekday().num_days_from_monday() * MINUTES_PER_DAY + local.hour() * 60 + local.minute(),
                );
                windows.iter().any(|w| w.contains(t))
            }
        }
    }

    /// Checks the schedule can ever open.
    /// Error cases:
    /// - a weekly schedule with no windows
    /// - a window whose open equals its close
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Schedule::AlwaysOpen => Ok(()),
            Schedule::Weekly { windows, .. } if windows.is_empty() => Err("weekly schedule has no windows".into()),
            Schedule::Weekly { windows, .. } => match windows.iter().find(|w| w.open == w.close) {
                Some(w) => Err(format!("empty window {}-{}", w.open, w.close)),
                None => Ok(()),
            },
        }
    }
}

impl Window {
    /// Whether `t` falls inside this window, wrapping over the week boundary when needed.
    fn contains(&self, t: WeekTime) -> bool {
        if self.open.0 < self.close.0 {
            self.open.0 <= t.0 && t.0 < self.close.0
        } else {
            t.0 >= self.open.0 || t.0 < self.close.0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn w(open: &str, close: &str) -> Window {
        Window { open: open.parse().unwrap(), close: close.parse().unwrap() }
    }

    fn ny_ms(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> Millis {
        chrono_tz::America::New_York.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap().timestamp_millis()
    }

    fn us_equities() -> Schedule {
        Schedule::Weekly {
            tz: chrono_tz::America::New_York,
            windows: DAYS[..5].iter().map(|d| w(&format!("{d} 09:30"), &format!("{d} 16:00"))).collect(),
            closed_dates: vec![NaiveDate::from_ymd_opt(2026, 11, 26).unwrap()],
        }
    }

    #[test]
    fn equity_session_respects_local_time_and_holidays() {
        let s = us_equities();
        // Tue 20 Oct 2026: closed one minute before the open, open until the minute of the close.
        assert!(!s.is_open(ny_ms(2026, 10, 20, 9, 29)));
        assert!(s.is_open(ny_ms(2026, 10, 20, 9, 30)));
        assert!(s.is_open(ny_ms(2026, 10, 20, 15, 59)));
        assert!(!s.is_open(ny_ms(2026, 10, 20, 16, 0)));
        // Saturday.
        assert!(!s.is_open(ny_ms(2026, 10, 24, 12, 0)));
        // Thanksgiving is a listed closure.
        assert!(!s.is_open(ny_ms(2026, 11, 26, 12, 0)));
        // DST ends 1 Nov 2026; 09:30 local is still the open.
        assert!(s.is_open(ny_ms(2026, 11, 2, 9, 30)));
    }

    #[test]
    fn fx_window_wraps_the_week() {
        let s = Schedule::Weekly {
            tz: chrono_tz::America::New_York,
            windows: vec![w("Sun 17:00", "Fri 17:00")],
            closed_dates: vec![],
        };
        assert!(s.is_open(ny_ms(2026, 10, 19, 3, 0))); // Mon
        assert!(!s.is_open(ny_ms(2026, 10, 23, 17, 0))); // Fri close
        assert!(!s.is_open(ny_ms(2026, 10, 24, 12, 0))); // Sat
        assert!(!s.is_open(ny_ms(2026, 10, 25, 16, 59))); // Sun pre-open
        assert!(s.is_open(ny_ms(2026, 10, 25, 17, 0))); // Sun open
    }

    #[test]
    fn out_of_range_timestamps_read_as_closed() {
        assert!(!us_equities().is_open(i64::MIN));
        assert!(!us_equities().is_open(i64::MAX));
    }

    #[test]
    fn validation_rejects_schedules_that_never_open() {
        let empty = Schedule::Weekly { tz: chrono_tz::UTC, windows: vec![], closed_dates: vec![] };
        assert!(empty.validate().is_err());
        let zero =
            Schedule::Weekly { tz: chrono_tz::UTC, windows: vec![w("Mon 09:00", "Mon 09:00")], closed_dates: vec![] };
        assert!(zero.validate().is_err());
        assert!(us_equities().validate().is_ok());
    }

    #[test]
    fn unknown_schedule_fields_are_rejected() {
        let typo = serde_json::json!({ "kind": "weekly", "tz": "UTC", "windows": [], "closed_date": [] });
        assert!(serde_json::from_value::<Schedule>(typo).is_err());
    }

    #[test]
    fn week_time_round_trips() {
        let t: WeekTime = "Sun 17:05".parse().unwrap();
        assert_eq!(t.to_string(), "Sun 17:05");
        assert!("Xyz 10:00".parse::<WeekTime>().is_err());
        assert!("Mon 24:00".parse::<WeekTime>().is_err());
    }
}
