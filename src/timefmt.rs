//! Wall-clock formatting without a date/time crate.
//!
//! DYODE only needs a few things: ISO-8601 UTC timestamps with milliseconds
//! for the event log and the log streams (they sort lexically and have no
//! repeated hour when the clocks go back), a local timestamp for the human
//! log, and ISO week numbers for the weekly log archive.  The civil-date
//! arithmetic is Howard Hinnant's well-known `days_from_civil` /
//! `civil_from_days` pair.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A calendar date (proleptic Gregorian).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Date {
    pub year: i64,
    pub month: u32,
    pub day: u32,
}

/// Days since 1970-01-01 for a civil date.
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Civil date for a number of days since 1970-01-01.
pub fn civil_from_days(z: i64) -> Date {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    Date {
        year: if m <= 2 { y + 1 } else { y },
        month: m,
        day: d,
    }
}

impl Date {
    pub fn from_days(days: i64) -> Self {
        civil_from_days(days)
    }

    pub fn days(&self) -> i64 {
        days_from_civil(self.year, self.month, self.day)
    }

    /// Today, in UTC.
    pub fn today_utc() -> Self {
        let secs = unix_now().as_secs() as i64;
        civil_from_days(secs.div_euclid(86_400))
    }

    /// ISO weekday, Monday = 1 .. Sunday = 7.
    pub fn iso_weekday(&self) -> u32 {
        // 1970-01-01 was a Thursday (4).
        ((self.days() + 3).rem_euclid(7) + 1) as u32
    }

    /// (ISO year, ISO week).
    pub fn iso_week(&self) -> (i64, u32) {
        // The ISO week belongs to the year of its Thursday.
        let thursday = self.days() - i64::from(self.iso_weekday()) + 4;
        let year = civil_from_days(thursday).year;
        let jan1 = days_from_civil(year, 1, 1);
        let week = ((thursday - jan1) / 7 + 1) as u32;
        (year, week)
    }

    /// Monday of an ISO week, or None if that week does not exist.
    pub fn from_iso_week(year: i64, week: u32) -> Option<Self> {
        if week == 0 || week > 53 {
            return None;
        }
        // Jan 4th is always in week 1.
        let jan4 = Date {
            year,
            month: 1,
            day: 4,
        };
        let week1_monday = jan4.days() - i64::from(jan4.iso_weekday()) + 1;
        let monday = Date::from_days(week1_monday + i64::from(week - 1) * 7);
        if monday.iso_week() != (year, week) {
            return None;
        }
        Some(monday)
    }

    /// '2026-W38'.
    pub fn week_tag(&self) -> String {
        let (y, w) = self.iso_week();
        format!("{y:04}-W{w:02}")
    }

    /// Parse 'YYYY-MM-DD'.
    pub fn parse(text: &str) -> Option<Self> {
        let mut it = text.trim().splitn(3, '-');
        let year = it.next()?.parse().ok()?;
        let month = it.next()?.parse().ok()?;
        let day = it.next()?.parse().ok()?;
        if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
            return None;
        }
        Some(Date { year, month, day })
    }
}

pub fn unix_now() -> Duration {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
}

/// Seconds since the epoch, as a float (like Python's time.time()).
pub fn unix_now_f64() -> f64 {
    unix_now().as_secs_f64()
}

fn split(secs: i64) -> (Date, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    (
        civil_from_days(days),
        (rem / 3600) as u32,
        ((rem % 3600) / 60) as u32,
        (rem % 60) as u32,
    )
}

/// ISO-8601 UTC with milliseconds for a unix timestamp in seconds,
/// e.g. 2026-09-25T20:32:14.108Z.
pub fn iso_utc(ts: f64) -> String {
    let ms_total = (ts * 1000.0).floor() as i64;
    let secs = ms_total.div_euclid(1000);
    let ms = ms_total.rem_euclid(1000);
    let (d, h, mi, s) = split(secs);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        d.year, d.month, d.day, h, mi, s, ms
    )
}

/// The current time as ISO-8601 UTC with milliseconds.
pub fn utc_now() -> String {
    iso_utc(unix_now_f64())
}

/// Local time for the human-readable log: '2026-09-25 22:32:14,108'
/// (the format Python's logging module used, so existing greps still work).
pub fn local_log_stamp() -> String {
    let now = unix_now();
    let secs = now.as_secs() as libc::time_t;
    let ms = now.subsec_millis();
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: localtime_r writes into the tm we own; secs is a valid time_t.
    let ok = unsafe { !libc::localtime_r(&secs, &mut tm).is_null() };
    if !ok {
        let (d, h, mi, s) = split(secs as i64);
        return format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02},{:03}",
            d.year, d.month, d.day, h, mi, s, ms
        );
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02},{:03}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
        ms
    )
}

/// Local 'YYYYmmdd-HHMMSS' for backup file names.
pub fn local_compact_stamp() -> String {
    let s = local_log_stamp();
    // '2026-09-25 22:32:14,108' -> '20260925-223214'
    let digits: String = s.chars().take(19).filter(|c| c.is_ascii_digit()).collect();
    format!("{}-{}", &digits[..8], &digits[8..14])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_roundtrip() {
        for days in [-1_000_000, -1, 0, 1, 19_000, 20_000, 2_000_000] {
            assert_eq!(civil_from_days(days).days(), days);
        }
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(
            civil_from_days(0),
            Date {
                year: 1970,
                month: 1,
                day: 1
            }
        );
    }

    #[test]
    fn iso_format() {
        assert_eq!(iso_utc(0.0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso_utc(1_758_832_334.108), "2025-09-25T20:32:14.108Z");
    }

    #[test]
    fn iso_weeks() {
        // Known values (Python's date.isocalendar()).
        assert_eq!(
            Date {
                year: 2026,
                month: 9,
                day: 21
            }
            .iso_week(),
            (2026, 39)
        );
        assert_eq!(
            Date {
                year: 2021,
                month: 1,
                day: 3
            }
            .iso_week(),
            (2020, 53)
        );
        assert_eq!(
            Date {
                year: 2024,
                month: 12,
                day: 30
            }
            .iso_week(),
            (2025, 1)
        );
        assert_eq!(
            Date {
                year: 2026,
                month: 10,
                day: 26
            }
            .iso_weekday(),
            1
        );
        assert_eq!(
            Date::from_iso_week(2026, 39),
            Some(Date {
                year: 2026,
                month: 9,
                day: 21
            })
        );
        assert_eq!(Date::from_iso_week(2025, 53), None);
        assert_eq!(
            Date::from_iso_week(2026, 53),
            Some(Date {
                year: 2026,
                month: 12,
                day: 28
            })
        );
        assert_eq!(
            Date {
                year: 2026,
                month: 9,
                day: 24
            }
            .week_tag(),
            "2026-W39"
        );
    }
}
