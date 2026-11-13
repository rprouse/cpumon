//! Well-known file locations.

use std::path::{Path, PathBuf};

/// Directory name under `%LOCALAPPDATA%`.
const APP_DIR: &str = "cpumon";
/// Database file name.
const DB_FILE: &str = "cpumon.db";
/// Log file name (warnings and errors from the windowless monitor).
const LOG_FILE: &str = "cpumon.log";

/// Default database path: `%LOCALAPPDATA%\cpumon\cpumon.db`.
///
/// Returns `None` if `LOCALAPPDATA` is not set.
pub fn default_db_path() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|base| db_path_in(PathBuf::from(base)))
}

/// The log written by the windowless monitor (`cpumonw`): next to the database.
pub fn log_path(db: &Path) -> PathBuf {
    db.with_file_name(LOG_FILE)
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

    #[test]
    fn log_is_next_to_database() {
        assert_eq!(
            log_path(Path::new(r"D:\data\x.db")),
            PathBuf::from(r"D:\data\cpumon.log")
        );
    }
}
