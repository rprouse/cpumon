//! Shared console formatting for the report commands.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use comfy_table::{CellAlignment, ContentArrangement, LineStyle, Table, TableStyle};
use cpumon_core::store::Store;
use cpumon_core::time::{filetime_minus, format_utc, now_filetime, parse_utc};

/// Opens an existing database for reading. Returns `None` (after saying so) if
/// nothing has been recorded yet.
pub fn open_existing(db: &Path) -> Result<Option<Store>> {
    if !db.exists() {
        println!(
            "No database at {} yet. Run `cpumon monitor` to start recording.",
            db.display()
        );
        return Ok(None);
    }
    let store = Store::open(db).with_context(|| format!("opening database {}", db.display()))?;
    Ok(Some(store))
}

/// The UTC timestamp `age` ago, for comparing with stored timestamps.
pub fn cutoff_utc(age: Duration) -> String {
    format_utc(filetime_minus(now_filetime(), age))
}

/// Plain columns with a line under the header.
const STYLE: TableStyle =
    TableStyle::new().header_separator(LineStyle::none().fill('─').junction(' '));

/// A table with a compact style that wraps to the terminal width. Columns listed in
/// `right` are right-aligned.
pub fn table(header: &[&str], right: &[usize]) -> Table {
    let mut table = Table::new();
    table
        .load_style(STYLE)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(header.to_vec());
    for &i in right {
        if let Some(column) = table.column_mut(i) {
            column.set_cell_alignment(CellAlignment::Right);
        }
    }
    table
}

/// Seconds as `12.3s`, `4m 05s` or `2h 03m`.
pub fn seconds(s: f64) -> String {
    if s < 60.0 {
        return format!("{s:.1}s");
    }
    let total = s.round() as u64;
    if total < 3600 {
        format!("{}m {:02}s", total / 60, total % 60)
    } else {
        format!("{}h {:02}m", total / 3600, total / 60 % 60)
    }
}

/// Time between two stored UTC timestamps, formatted with [`seconds`].
pub fn elapsed(from_utc: &str, to_utc: &str) -> Option<String> {
    let (from, to) = (parse_utc(from_utc)?, parse_utc(to_utc)?);
    Some(seconds((to - from).max(0) as f64 / 10_000_000.0))
}

pub fn percent(p: f64) -> String {
    format!("{p:.1}%")
}

/// Shortens `s` to `max_chars` characters, adding `...` if anything was cut.
pub fn truncate(s: &str, max_chars: usize) -> String {
    match s.char_indices().nth(max_chars) {
        Some((i, _)) => format!("{}...", &s[..i]),
        None => s.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_seconds_at_each_scale() {
        assert_eq!(seconds(0.0), "0.0s");
        assert_eq!(seconds(12.34), "12.3s");
        assert_eq!(seconds(65.0), "1m 05s");
        assert_eq!(seconds(3599.4), "59m 59s");
        assert_eq!(seconds(7380.0), "2h 03m");
    }

    #[test]
    fn formats_elapsed_between_timestamps() {
        assert_eq!(
            elapsed(
                "2026-01-01T00:00:00.0000000Z",
                "2026-01-01T00:01:30.0000000Z"
            )
            .as_deref(),
            Some("1m 30s")
        );
        assert_eq!(elapsed("bad", "2026-01-01T00:00:00.0000000Z"), None);
    }

    #[test]
    fn truncates_long_strings_on_char_boundaries() {
        assert_eq!(truncate("abc", 5), "abc");
        assert_eq!(truncate("abcdef", 3), "abc...");
        assert_eq!(truncate("ééé", 2), "éé...");
    }

    #[test]
    fn cutoff_is_in_the_past() {
        let now = format_utc(now_filetime());
        assert!(cutoff_utc(Duration::from_secs(60)) < now);
    }
}
