//! Trading hours. Crypto trades 24/7; RWA markets follow their venue's
//! weekly windows in local time (so DST is handled by the tz database),
//! minus holiday closures. While a market is closed nothing fills. Resting
//! orders wait, and the first fresh price after the open fills them, gap
//! included.

use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Datelike, NaiveDate, Timelike, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::types::Millis;

const MINUTES_PER_DAY: u32 = 24 * 60;
const MINUTES_PER_WEEK: u32 = 7 * MINUTES_PER_DAY;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
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

impl Schedule {
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

/// `[open, close)` in venue-local week time. A window whose close is before
/// its open wraps over the week boundary (e.g. FX: Sun 17:00 to Fri 17:00).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Window {
    pub open: WeekTime,
    pub close: WeekTime,
}

impl Window {
    fn contains(&self, t: WeekTime) -> bool {
        if self.open.0 < self.close.0 {
            self.open.0 <= t.0 && t.0 < self.close.0
        } else {
            t.0 >= self.open.0 || t.0 < self.close.0
        }
    }
}

/// Minutes since Monday 00:00 local time, written as `"Mon 09:30"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct WeekTime(u32);

const DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

impl fmt::Display for WeekTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let day = DAYS[(self.0 / MINUTES_PER_DAY) as usize];
        let mins = self.0 % MINUTES_PER_DAY;
        write!(f, "{day} {:02}:{:02}", mins / 60, mins % 60)
    }
}

impl FromStr for WeekTime {
    type Err = String;

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
        let t = day * MINUTES_PER_DAY + h * 60 + m;
        debug_assert!(t < MINUTES_PER_WEEK);
        Ok(WeekTime(t))
    }
}

impl Serialize for WeekTime {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for WeekTime {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?.parse().map_err(serde::de::Error::custom)
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
            windows: ["Mon", "Tue", "Wed", "Thu", "Fri"]
                .iter()
                .map(|d| w(&format!("{d} 09:30"), &format!("{d} 16:00")))
                .collect(),
            closed_dates: vec![NaiveDate::from_ymd_opt(2026, 11, 26).unwrap()],
        }
    }

    #[test]
    fn equity_session_respects_local_time_and_holidays() {
        let s = us_equities();
        // Tue 20 Oct 2026
        assert!(!s.is_open(ny_ms(2026, 10, 20, 9, 29)));
        assert!(s.is_open(ny_ms(2026, 10, 20, 9, 30)));
        assert!(s.is_open(ny_ms(2026, 10, 20, 15, 59)));
        assert!(!s.is_open(ny_ms(2026, 10, 20, 16, 0)));
        // Saturday
        assert!(!s.is_open(ny_ms(2026, 10, 24, 12, 0)));
        // Thanksgiving
        assert!(!s.is_open(ny_ms(2026, 11, 26, 12, 0)));
        // DST ends 1 Nov 2026; 09:30 local is still the open
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
    fn week_time_round_trips() {
        let t: WeekTime = "Sun 17:05".parse().unwrap();
        assert_eq!(t.to_string(), "Sun 17:05");
        assert!("Xyz 10:00".parse::<WeekTime>().is_err());
        assert!("Mon 24:00".parse::<WeekTime>().is_err());
    }
}
