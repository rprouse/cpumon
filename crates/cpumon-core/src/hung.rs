//! Windows that have stopped responding ("Not Responding").
//!
//! `IsHungAppWindow` reports a window whose thread has not checked its message queue
//! for about 5 seconds. Only windows the user can see count: hidden windows of
//! background apps can sit unpumped harmlessly. When a window hangs, Windows hides it
//! and shows a "ghost" copy (class `Ghost`, title ending in "(Not Responding)" in the
//! UI language), so a hidden hung window also counts when a visible ghost's title
//! starts with its title.
//!
//! This only sees windows on the caller's desktop, so the monitor must run in the
//! user's session (not as a session-0 service or S4U task).

use windows::Win32::Foundation::{HWND, LPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId,
    IsHungAppWindow, IsWindowVisible,
};
use windows::core::BOOL;

/// A visible window that is not responding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HungWindow {
    pub pid: u32,
    pub title: String,
    /// The window (or its ghost) is the foreground window.
    pub foreground: bool,
}

/// One top-level window, as seen by a scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WindowInfo {
    pub pid: u32,
    pub visible: bool,
    pub hung: bool,
    pub ghost: bool,
    pub foreground: bool,
    /// Read only for hung and ghost windows.
    pub title: String,
}

/// Picks the hung windows the user can see from a scan.
pub(crate) fn select_hung(windows: &[WindowInfo], out: &mut Vec<HungWindow>) {
    out.clear();
    let ghosts = || windows.iter().filter(|w| w.ghost && w.visible);
    // Ghosts belong to dwm.exe and may themselves report as hung; they stand in for
    // the hung window, so only the original counts.
    for w in windows
        .iter()
        .filter(|w| w.hung && !w.ghost && !w.title.is_empty())
    {
        let ghost = ghosts().find(|g| g.title.starts_with(&w.title));
        if !w.visible && ghost.is_none() {
            continue;
        }
        let foreground = w.foreground || ghost.is_some_and(|g| g.foreground);
        match out
            .iter_mut()
            .find(|h| h.pid == w.pid && h.title == w.title)
        {
            Some(existing) => existing.foreground |= foreground,
            None => out.push(HungWindow {
                pid: w.pid,
                title: w.title.clone(),
                foreground,
            }),
        }
    }
}

/// Finds hung windows. Reuses its buffers between scans.
#[derive(Debug, Default)]
pub struct HungWindowScanner {
    handles: Vec<HWND>,
    windows: Vec<WindowInfo>,
    hung: Vec<HungWindow>,
}

impl HungWindowScanner {
    pub fn new() -> Self {
        Self::default()
    }

    /// Scans the top-level windows of the current desktop.
    pub fn scan(&mut self) -> &[HungWindow] {
        self.handles.clear();
        // SAFETY: the callback only runs during this call, and `lparam` points to
        // `self.handles`, which outlives it and is not otherwise accessed meanwhile.
        let _ = unsafe {
            EnumWindows(
                Some(collect_window),
                LPARAM(std::ptr::from_mut(&mut self.handles) as isize),
            )
        };
        // SAFETY: no arguments.
        let foreground = unsafe { GetForegroundWindow() };

        self.windows.clear();
        for &hwnd in &self.handles {
            // SAFETY: `hwnd` came from EnumWindows; if the window has since been
            // destroyed these calls fail harmlessly.
            let (hung, visible) = unsafe {
                (
                    IsHungAppWindow(hwnd).as_bool(),
                    IsWindowVisible(hwnd).as_bool(),
                )
            };
            let ghost = visible && class_is(hwnd, "Ghost");
            if !hung && !ghost {
                continue;
            }
            let mut pid = 0u32;
            // SAFETY: `pid` is a valid out pointer.
            unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
            self.windows.push(WindowInfo {
                pid,
                visible,
                hung,
                ghost,
                foreground: hwnd == foreground,
                title: window_title(hwnd),
            });
        }
        select_hung(&self.windows, &mut self.hung);
        &self.hung
    }
}

unsafe extern "system" fn collect_window(hwnd: HWND, lparam: LPARAM) -> BOOL {
    // SAFETY: `lparam` is the `&mut Vec<HWND>` passed by `scan`.
    let handles = unsafe { &mut *(lparam.0 as *mut Vec<HWND>) };
    handles.push(hwnd);
    true.into()
}

fn class_is(hwnd: HWND, name: &str) -> bool {
    let mut buffer = [0u16; 64];
    // SAFETY: the buffer and its length match.
    let len = unsafe { GetClassNameW(hwnd, &mut buffer) };
    let len = usize::try_from(len).unwrap_or(0);
    len == name.len() && buffer[..len].iter().copied().eq(name.encode_utf16())
}

/// A window's title. For windows of other processes this reads the stored caption
/// without sending a message, so it is safe on hung windows.
fn window_title(hwnd: HWND) -> String {
    let mut buffer = [0u16; 256];
    // SAFETY: the buffer and its length match.
    let len = unsafe { GetWindowTextW(hwnd, &mut buffer) };
    let len = usize::try_from(len).unwrap_or(0).min(buffer.len());
    String::from_utf16_lossy(&buffer[..len])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(pid: u32, title: &str) -> WindowInfo {
        WindowInfo {
            pid,
            visible: true,
            hung: true,
            ghost: false,
            foreground: false,
            title: title.into(),
        }
    }

    fn ghost(title: &str, foreground: bool) -> WindowInfo {
        WindowInfo {
            pid: 999,
            visible: true,
            hung: false,
            ghost: true,
            foreground,
            title: title.into(),
        }
    }

    fn select(windows: &[WindowInfo]) -> Vec<HungWindow> {
        let mut out = Vec::new();
        select_hung(windows, &mut out);
        out
    }

    #[test]
    fn reports_visible_hung_windows() {
        let hung = select(&[window(10, "Editor")]);
        assert_eq!(
            hung,
            [HungWindow {
                pid: 10,
                title: "Editor".into(),
                foreground: false
            }]
        );
    }

    #[test]
    fn ignores_hidden_and_untitled_hung_windows() {
        let hidden = WindowInfo {
            visible: false,
            ..window(10, "Tray helper")
        };
        assert!(select(&[hidden, window(11, "")]).is_empty());
    }

    #[test]
    fn matches_hidden_window_to_its_ghost() {
        let hidden = WindowInfo {
            visible: false,
            ..window(10, "Meeting | Teams")
        };
        let hung = select(&[
            hidden,
            ghost("Meeting | Teams (Not Responding)", true),
            ghost("Other (Not Responding)", false),
        ]);
        assert_eq!(hung.len(), 1);
        assert_eq!(hung[0].pid, 10);
        assert!(hung[0].foreground);
    }

    #[test]
    fn ghosts_reporting_hung_are_not_counted() {
        let hidden = WindowInfo {
            visible: false,
            ..window(10, "Meeting")
        };
        // Seen live: the ghost keeps the original title and reports as hung.
        let hung_ghost = WindowInfo {
            hung: true,
            ..ghost("Meeting", false)
        };
        let hung = select(&[hidden, hung_ghost]);
        assert_eq!(hung.len(), 1);
        assert_eq!(hung[0].pid, 10);
    }

    #[test]
    fn deduplicates_by_process_and_title() {
        let fg = WindowInfo {
            foreground: true,
            ..window(10, "Editor")
        };
        let hung = select(&[window(10, "Editor"), fg, window(11, "Editor")]);
        assert_eq!(hung.len(), 2);
        assert!(hung[0].foreground);
    }

    #[test]
    fn scans_live_desktop_without_failing() {
        let mut scanner = HungWindowScanner::new();
        let first = scanner.scan().len();
        let second = scanner.scan().len();
        // Usually 0; just make sure scanning works and is repeatable.
        assert!(first < 1000 && second < 1000);
    }
}
