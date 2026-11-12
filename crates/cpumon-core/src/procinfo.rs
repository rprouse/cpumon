//! Details that need a process handle: image path, command line and user.
//!
//! Only used for new offenders and their ancestors, never every sample. Opening with
//! `PROCESS_QUERY_LIMITED_INFORMATION` works for most processes without elevation;
//! failures are reported in [`ProcessDetails::error`] rather than as errors.

use std::collections::HashMap;

use windows::Wdk::System::Threading::{NtQueryInformationProcess, ProcessCommandLineInformation};
use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, STATUS_INFO_LENGTH_MISMATCH};
use windows::Win32::Security::{
    GetLengthSid, GetTokenInformation, LookupAccountSidW, SID_NAME_USE, TOKEN_QUERY, TOKEN_USER,
    TokenUser,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetProcessTimes, OpenProcess, OpenProcessToken, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::core::{PCWSTR, PWSTR};

use crate::snapshot::{ProcessKey, UnicodeString};

/// What could be learned about a process.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProcessDetails {
    pub image_path: Option<String>,
    pub command_line: Option<String>,
    /// `DOMAIN\user`.
    pub user: Option<String>,
    /// Why some details are missing, e.g. `open: Access is denied.`
    pub error: Option<String>,
}

/// Reads process details, caching user names by SID.
#[derive(Debug, Default)]
pub struct ProcessInspector {
    users: HashMap<Vec<u8>, Option<String>>,
}

impl ProcessInspector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Reads details for a process. If the PID now belongs to a different process
    /// (the original exited and its PID was reused), nothing is returned.
    pub fn inspect(&mut self, key: ProcessKey) -> ProcessDetails {
        let mut details = ProcessDetails::default();
        let mut errors = Vec::new();

        if key.pid == 0 {
            return details;
        }
        let handle = match open(key.pid) {
            Ok(handle) => handle,
            Err(e) => {
                details.error = Some(format!("open: {}", e.message()));
                return details;
            }
        };
        match creation_time(&handle) {
            Ok(created) if created == key.create_time => {}
            Ok(_) => {
                details.error = Some("process exited (PID reused)".to_owned());
                return details;
            }
            Err(e) => errors.push(format!("times: {}", e.message())),
        }

        match image_path(&handle) {
            Ok(path) => details.image_path = Some(path),
            Err(e) => errors.push(format!("image path: {}", e.message())),
        }
        match command_line(&handle) {
            Ok(cmd) => details.command_line = Some(cmd),
            Err(e) => errors.push(format!("command line: {e}")),
        }
        match self.user(handle.0) {
            Ok(user) => details.user = user,
            Err(e) => errors.push(format!("user: {}", e.message())),
        }

        if !errors.is_empty() {
            details.error = Some(errors.join("; "));
        }
        details
    }

    fn user(&mut self, process: HANDLE) -> windows::core::Result<Option<String>> {
        let mut token = HANDLE::default();
        // SAFETY: `process` is a valid handle and `token` a valid out pointer.
        unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) }?;
        let token = OwnedHandle(token);

        let mut needed = 0u32;
        // SAFETY: a size query with no buffer; `needed` is a valid out pointer. It
        // "fails" with ERROR_INSUFFICIENT_BUFFER, which is expected.
        let _ = unsafe { GetTokenInformation(token.0, TokenUser, None, 0, &mut needed) };
        let mut buffer = vec![0u64; (needed as usize).div_ceil(8).max(1)];
        // SAFETY: `buffer` is 8-byte aligned and at least `needed` bytes long.
        unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                Some(buffer.as_mut_ptr().cast()),
                needed,
                &mut needed,
            )
        }?;
        // SAFETY: GetTokenInformation(TokenUser) wrote a TOKEN_USER at the start.
        let sid = unsafe { (*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid };
        // SAFETY: `sid` points to a valid SID inside `buffer`.
        let sid_len = unsafe { GetLengthSid(sid) } as usize;
        // SAFETY: the SID is `sid_len` bytes long and lives in `buffer`.
        let sid_bytes = unsafe { std::slice::from_raw_parts(sid.0.cast::<u8>(), sid_len) };
        if let Some(cached) = self.users.get(sid_bytes) {
            return Ok(cached.clone());
        }

        let mut name = [0u16; 256];
        let mut domain = [0u16; 256];
        let mut name_len = name.len() as u32;
        let mut domain_len = domain.len() as u32;
        let mut use_ = SID_NAME_USE::default();
        // SAFETY: buffers and their lengths match; `sid` is valid.
        let looked_up = unsafe {
            LookupAccountSidW(
                PCWSTR::null(),
                sid,
                Some(PWSTR(name.as_mut_ptr())),
                &mut name_len,
                Some(PWSTR(domain.as_mut_ptr())),
                &mut domain_len,
                &mut use_,
            )
        };
        // Unknown SIDs (e.g. deleted accounts) are cached as None.
        let user = looked_up.ok().map(|()| {
            let name = String::from_utf16_lossy(&name[..name_len as usize]);
            let domain = String::from_utf16_lossy(&domain[..domain_len as usize]);
            if domain.is_empty() {
                name
            } else {
                format!("{domain}\\{name}")
            }
        });
        self.users.insert(sid_bytes.to_vec(), user.clone());
        Ok(user)
    }
}

/// The user running this process, as `DOMAIN\user`.
pub fn current_user() -> Option<String> {
    // SAFETY: returns a pseudo-handle that is always valid and needs no closing.
    let process = unsafe { GetCurrentProcess() };
    ProcessInspector::new().user(process).ok().flatten()
}

/// A handle closed on drop.
struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: the handle was opened by us and is closed only here.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn open(pid: u32) -> windows::core::Result<OwnedHandle> {
    // SAFETY: no pointer arguments.
    unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.map(OwnedHandle)
}

fn creation_time(process: &OwnedHandle) -> windows::core::Result<i64> {
    let mut created = FILETIME::default();
    let (mut exited, mut kernel, mut user) = Default::default();
    // SAFETY: `process` is valid and all out pointers point to FILETIMEs.
    unsafe { GetProcessTimes(process.0, &mut created, &mut exited, &mut kernel, &mut user) }?;
    Ok((i64::from(created.dwHighDateTime) << 32) | i64::from(created.dwLowDateTime))
}

fn image_path(process: &OwnedHandle) -> windows::core::Result<String> {
    let mut buffer = vec![0u16; 32 * 1024];
    let mut len = buffer.len() as u32;
    // SAFETY: `buffer` holds `len` UTF-16 units; `len` is a valid in/out pointer.
    unsafe {
        QueryFullProcessImageNameW(
            process.0,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut len,
        )
    }?;
    Ok(String::from_utf16_lossy(&buffer[..len as usize]))
}

fn command_line(process: &OwnedHandle) -> Result<String, String> {
    let mut buffer = vec![0u64; 512];
    loop {
        let capacity = (buffer.len() * size_of::<u64>()) as u32;
        let mut needed = 0u32;
        // SAFETY: `buffer` is writable for `capacity` bytes and 8-byte aligned;
        // `needed` is a valid out pointer.
        let status = unsafe {
            NtQueryInformationProcess(
                process.0,
                ProcessCommandLineInformation,
                buffer.as_mut_ptr().cast(),
                capacity,
                &mut needed,
            )
        };
        if status == STATUS_INFO_LENGTH_MISMATCH && needed as usize > capacity as usize {
            buffer.resize((needed as usize).div_ceil(size_of::<u64>()), 0);
            continue;
        }
        if status.is_err() {
            return Err(windows::core::Error::from_hresult(status.to_hresult())
                .message()
                .to_string());
        }
        // SAFETY: on success the buffer starts with a UNICODE_STRING.
        let s = unsafe { buffer.as_ptr().cast::<UnicodeString>().read() };
        if s.buffer.is_null() || s.length == 0 {
            return Ok(String::new());
        }
        // SAFETY: the string's characters follow the header inside `buffer`.
        let wide = unsafe { std::slice::from_raw_parts(s.buffer, usize::from(s.length) / 2) };
        return Ok(String::from_utf16_lossy(wide));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::ProcessSnapshot;

    fn own_key() -> ProcessKey {
        let snapshot = ProcessSnapshot::new().unwrap();
        snapshot.find(std::process::id()).unwrap().key()
    }

    #[test]
    fn inspects_current_process() {
        let mut inspector = ProcessInspector::new();
        let details = inspector.inspect(own_key());
        assert_eq!(details.error, None);
        let path = details.image_path.expect("image path");
        assert!(path.ends_with(".exe"), "{path}");
        let cmd = details.command_line.expect("command line");
        assert!(!cmd.is_empty());
        let user = details.user.expect("user");
        assert!(user.contains('\\'), "{user}");
    }

    #[test]
    fn finds_current_user() {
        let user = current_user().expect("current user");
        assert_eq!(Some(user), ProcessInspector::new().inspect(own_key()).user);
    }

    #[test]
    fn detects_pid_reuse() {
        let key = ProcessKey {
            create_time: own_key().create_time - 1,
            ..own_key()
        };
        let details = ProcessInspector::new().inspect(key);
        assert_eq!(
            details.error.as_deref(),
            Some("process exited (PID reused)")
        );
        assert_eq!(details.command_line, None);
    }

    #[test]
    fn reports_missing_process_as_error() {
        // PIDs are multiples of 4, so this one never exists.
        let key = ProcessKey {
            pid: 4_000_001,
            create_time: 1,
        };
        let details = ProcessInspector::new().inspect(key);
        assert!(details.error.unwrap().starts_with("open:"));
    }
}
