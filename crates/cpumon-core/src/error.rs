//! Error type for `cpumon-core`.

use std::path::PathBuf;

/// Errors returned by `cpumon-core`.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A PDH (performance counter) call failed.
    #[error("{call} failed with PDH status 0x{status:08X}")]
    Pdh { call: &'static str, status: u32 },

    /// An `Nt*` call failed.
    #[error("{call} failed with NTSTATUS 0x{status:08X}")]
    NtStatus { call: &'static str, status: u32 },

    /// A Win32 or COM call failed.
    #[error(transparent)]
    Windows(#[from] windows::core::Error),

    /// A database operation failed.
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// The database schema is unusable.
    #[error("{0}")]
    Schema(String),

    /// A file system operation failed.
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

/// Result type for `cpumon-core`.
pub type Result<T> = std::result::Result<T, Error>;
