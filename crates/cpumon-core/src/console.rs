//! Console stop signal (Ctrl+C, Ctrl+Break, closing the window, logoff, shutdown).

use std::sync::OnceLock;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::time::Duration;

use windows::Win32::System::Console::{
    CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT, SetConsoleCtrlHandler,
};
use windows::core::BOOL;

use crate::error::{Error, Result};

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
