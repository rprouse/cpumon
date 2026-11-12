//! Timestamps.
//!
//! Times are kept as FILETIMEs (100 ns intervals since 1601-01-01 UTC), stored as
//! fixed-width UTC strings such as `2026-11-12T14:05:52.1234567Z` (which sort
//! chronologically and are understood by SQLite's date functions), and shown in local
//! time.

use std::time::Duration;

use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows::Win32::System::SystemInformation::{GetLocalTime, GetSystemTimePreciseAsFileTime};
use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};

/// FILETIME of 1970-01-01T00:00:00Z.
const UNIX_EPOCH_FILETIME: i64 = 116_444_736_000_000_000;
const TICKS_PER_SECOND: i64 = 10_000_000;
const SECONDS_PER_DAY: i64 = 86_400;

/// Current time as a FILETIME.
pub fn now_filetime() -> i64 {
    // SAFETY: no arguments; returns the time by value.
    let ft = unsafe { GetSystemTimePreciseAsFileTime() };
    (i64::from(ft.dwHighDateTime) << 32) | i64::from(ft.dwLowDateTime)
}

/// A FILETIME `duration` before `filetime`.
pub fn filetime_minus(filetime: i64, duration: Duration) -> i64 {
    let ticks = i64::try_from(duration.as_nanos() / 100).unwrap_or(i64::MAX);
    filetime.saturating_sub(ticks)
}

/// Formats a FILETIME as `YYYY-MM-DDTHH:MM:SS.fffffffZ` (UTC, 100 ns precision).
pub fn format_utc(filetime: i64) -> String {
    let ticks = filetime - UNIX_EPOCH_FILETIME;
    let seconds = ticks.div_euclid(TICKS_PER_SECOND);
    let fraction = ticks.rem_euclid(TICKS_PER_SECOND);
    let days = seconds.div_euclid(SECONDS_PER_DAY);
    let second_of_day = seconds.rem_euclid(SECONDS_PER_DAY);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{fraction:07}Z",
        second_of_day / 3600,
        second_of_day / 60 % 60,
        second_of_day % 60,
    )
}

/// Converts days since 1970-01-01 to a (year, month, day) civil date.
/// Howard Hinnant's algorithm: <https://howardhinnant.github.io/date_algorithms.html>.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Parses a timestamp written by [`format_utc`] back into a FILETIME.
pub fn parse_utc(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() != 28
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
        || b[19] != b'.'
        || b[27] != b'Z'
    {
        return None;
    }
    let num = |range: std::ops::Range<usize>| -> Option<i64> {
        let digits = s.get(range)?;
        if digits.bytes().all(|c| c.is_ascii_digit()) {
            digits.parse().ok()
        } else {
            None
        }
    };
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hour, minute, second) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let fraction = num(20..27)?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    let seconds =
        days_from_civil(year, month, day) * SECONDS_PER_DAY + hour * 3600 + minute * 60 + second;
    Some(UNIX_EPOCH_FILETIME + seconds * TICKS_PER_SECOND + fraction)
}

/// Converts a (year, month, day) civil date to days since 1970-01-01 (Hinnant).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Formats a FILETIME as local time, `YYYY-MM-DD HH:MM:SS`, using the time zone rules
/// (including daylight saving) in effect at that time.
pub fn format_local(filetime: i64) -> String {
    let ft = FILETIME {
        dwLowDateTime: filetime as u32,
        dwHighDateTime: (filetime >> 32) as u32,
    };
    let mut utc = SYSTEMTIME::default();
    let mut local = SYSTEMTIME::default();
    // SAFETY: all pointers refer to valid local structs.
    let converted = unsafe {
        FileTimeToSystemTime(&ft, &mut utc)
            .and_then(|()| SystemTimeToTzSpecificLocalTime(None, &utc, &mut local))
    };
    match converted {
        Ok(()) => format_systemtime(&local),
        Err(_) => format_utc(filetime),
    }
}

/// Formats a stored UTC timestamp as local time; unparseable input is returned as is.
pub fn utc_to_local(s: &str) -> String {
    parse_utc(s).map_or_else(|| s.to_owned(), format_local)
}

fn format_systemtime(t: &SYSTEMTIME) -> String {
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    )
}

/// Current local time as `YYYY-MM-DD HH:MM:SS`.
pub fn local_timestamp() -> String {
    // SAFETY: no arguments; returns the time by value.
    let t = unsafe { GetLocalTime() };
    format_systemtime(&t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_unix_epoch() {
        assert_eq!(
            format_utc(UNIX_EPOCH_FILETIME),
            "1970-01-01T00:00:00.0000000Z"
        );
    }

    #[test]
    fn formats_known_instant_with_full_precision() {
        // 2026-11-12T14:05:52Z is 1_794_492_352 s after the Unix epoch.
        let ft = UNIX_EPOCH_FILETIME + 1_794_492_352 * TICKS_PER_SECOND + 1_234_567;
        assert_eq!(format_utc(ft), "2026-11-12T14:05:52.1234567Z");
    }

    #[test]
    fn formats_leap_day_and_year_end() {
        // 2024-02-29T00:00:00Z = 1_709_164_800; 1999-12-31T23:59:59Z = 946_684_799.
        let at = |s: i64| UNIX_EPOCH_FILETIME + s * TICKS_PER_SECOND;
        assert_eq!(
            format_utc(at(1_709_164_800)),
            "2024-02-29T00:00:00.0000000Z"
        );
        assert_eq!(format_utc(at(946_684_799)), "1999-12-31T23:59:59.0000000Z");
    }

    #[test]
    fn formats_filetime_zero() {
        assert_eq!(format_utc(0), "1601-01-01T00:00:00.0000000Z");
    }

    #[test]
    fn formatted_times_sort_chronologically() {
        let a = format_utc(UNIX_EPOCH_FILETIME + 5);
        let b = format_utc(UNIX_EPOCH_FILETIME + TICKS_PER_SECOND);
        assert!(a < b);
    }

    #[test]
    fn subtracts_durations() {
        assert_eq!(
            filetime_minus(10 * TICKS_PER_SECOND, Duration::from_secs(3)),
            7 * TICKS_PER_SECOND
        );
    }

    #[test]
    fn now_is_after_2025() {
        assert!(format_utc(now_filetime()).as_str() > "2025");
    }

    #[test]
    fn parses_what_it_formats() {
        for ft in [
            0,
            UNIX_EPOCH_FILETIME,
            UNIX_EPOCH_FILETIME + 1_709_164_800 * TICKS_PER_SECOND + 9_999_999,
            now_filetime(),
        ] {
            assert_eq!(parse_utc(&format_utc(ft)), Some(ft));
        }
    }

    #[test]
    fn rejects_malformed_timestamps() {
        for s in [
            "",
            "2026-11-12",
            "2026-11-12T14:05:52Z",
            "2026-13-12T14:05:52.1234567Z",
            "2026-11-12T24:05:52.1234567Z",
            "2026-11-12 14:05:52.1234567Z",
            "2026-11-12T14:05:52.12345+7Z",
        ] {
            assert_eq!(parse_utc(s), None, "{s}");
        }
    }

    #[test]
    fn formats_local_time_of_stored_timestamp() {
        let local = utc_to_local("2026-11-12T14:05:52.1234567Z");
        assert_eq!(local.len(), 19, "{local}");
        assert!(local.starts_with("2026-11-1"), "{local}");
        assert_eq!(utc_to_local("garbage"), "garbage");
    }

    #[test]
    fn formats_local_timestamp() {
        let ts = local_timestamp();
        assert_eq!(ts.len(), 19, "{ts}");
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[13..14], ":");
    }
}
