//! Well-known file locations.

use std::path::PathBuf;

/// Directory name under `%LOCALAPPDATA%`.
const APP_DIR: &str = "cpumon";
/// Database file name.
const DB_FILE: &str = "cpumon.db";

/// Default database path: `%LOCALAPPDATA%\cpumon\cpumon.db`.
///
/// Returns `None` if `LOCALAPPDATA` is not set.
pub fn default_db_path() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|base| db_path_in(PathBuf::from(base)))
}

fn db_path_in(base: PathBuf) -> PathBuf {
    base.join(APP_DIR).join(DB_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_path_is_under_app_dir() {
        let path = db_path_in(PathBuf::from(r"C:\Users\me\AppData\Local"));
        assert_eq!(
            path,
            PathBuf::from(r"C:\Users\me\AppData\Local\cpumon\cpumon.db")
        );
    }
}
