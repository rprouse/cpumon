//! Timestamps.
//!
//! Times are kept as FILETIMEs (100 ns intervals since 1601-01-01 UTC), stored as
//! fixed-width UTC strings such as `2026-11-12T14:05:52.1234567Z` (which sort
//! chronologically and are understood by SQLite's date functions), and shown in local
//! time.

use std::time::Duration;

use windows::Win32::System::SystemInformation::{GetLocalTime, GetSystemTimePreciseAsFileTime};

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

/// Current local time as `YYYY-MM-DD HH:MM:SS`.
pub fn local_timestamp() -> String {
    // SAFETY: no arguments; returns the time by value.
    let t = unsafe { GetLocalTime() };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    )
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
    fn formats_local_timestamp() {
        let ts = local_timestamp();
        assert_eq!(ts.len(), 19, "{ts}");
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[13..14], ":");
    }
}
