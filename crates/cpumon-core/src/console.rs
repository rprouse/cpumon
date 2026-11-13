//! Console handling: the stop signal (Ctrl+C, Ctrl+Break, closing the window, logoff,
//! shutdown), and sending standard error to a log file when there is no console.

use std::fs::OpenOptions;
use std::os::windows::io::IntoRawHandle;
use std::path::Path;
use std::sync::OnceLock;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::time::Duration;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Console::{
    CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT, STD_ERROR_HANDLE,
    SetConsoleCtrlHandler, SetStdHandle,
};
use windows::core::BOOL;

use crate::error::{Error, Result};

/// A log larger than this is moved to `<name>.old` before appending.
const MAX_LOG_BYTES: u64 = 1024 * 1024;

/// Appends everything written to standard error (`eprintln!`, and the error a failing
/// `main` prints) to `path` for the rest of the process. For processes without a
/// console, whose standard error otherwise goes nowhere. Rust's standard error looks
/// up the handle on every write, so this takes effect immediately.
pub fn redirect_stderr(path: &Path) -> Result<()> {
    let io = |source| Error::Io {
        path: path.to_owned(),
        source,
    };
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).map_err(io)?;
    }
    if std::fs::metadata(path).is_ok_and(|m| m.len() > MAX_LOG_BYTES) {
        let mut old = path.as_os_str().to_owned();
        old.push(".old");
        let _ = std::fs::rename(path, old);
    }
    let file = OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)
        .map_err(io)?;
    // The handle stays open for the life of the process, as standard error.
    let handle = HANDLE(file.into_raw_handle());
    // SAFETY: `handle` is an open file handle that is never closed.
    unsafe { SetStdHandle(STD_ERROR_HANDLE, handle) }?;
    Ok(())
}

static STOP: OnceLock<SyncSender<()>> = OnceLock::new();

/// How long a close/logoff/shutdown event waits for the main thread to clean up
/// before Windows terminates the process.
const CLOSE_GRACE: Duration = Duration::from_secs(3);

/// Installs a console control handler and returns a receiver that gets a message when
/// the user asks to stop. Can be called once per process.
pub fn stop_signal() -> Result<Receiver<()>> {
    let (tx, rx) = sync_channel(1);
    STOP.set(tx)
        .map_err(|_| Error::Schema("stop signal already installed".into()))?;
    // SAFETY: `handler` is a valid `extern "system"` function for the process lifetime.
    unsafe { SetConsoleCtrlHandler(Some(handler), true) }?;
    Ok(rx)
}

unsafe extern "system" fn handler(ctrl_type: u32) -> BOOL {
    if let Some(tx) = STOP.get() {
        let _ = tx.try_send(());
    }
    // For these events Windows ends the process as soon as the handler returns, so
    // give the main thread a moment to close the incident and exit on its own.
    if matches!(
        ctrl_type,
        CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT
    ) {
        std::thread::sleep(CLOSE_GRACE);
    }
    BOOL::from(true)
}
