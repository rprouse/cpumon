//! Time formatting for display.

use windows::Win32::System::SystemInformation::GetLocalTime;

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
    fn formats_local_timestamp() {
        let ts = local_timestamp();
        assert_eq!(ts.len(), 19, "{ts}");
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[13..14], ":");
    }
}
