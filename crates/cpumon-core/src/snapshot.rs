//! Snapshot of all processes from a single `NtQuerySystemInformation` call.
//!
//! One call returns every process with its parent PID, create time and CPU times,
//! without opening any process handles.

use std::mem::offset_of;
use std::ops::Range;

use windows::Wdk::System::SystemInformation::{
    NtQuerySystemInformation, NtQuerySystemTime, SystemProcessInformation,
};
use windows::Win32::Foundation::STATUS_INFO_LENGTH_MISMATCH;

use crate::error::{Error, Result};

/// One process in a [`ProcessSnapshot`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessEntry {
    pub pid: u32,
    /// PID of the process that created this one. The parent may have exited and its
    /// PID been reused, so check its create time is earlier before trusting it.
    pub parent_pid: u32,
    /// Creation time as a FILETIME (100 ns intervals since 1601-01-01 UTC).
    pub create_time: i64,
    /// User-mode CPU time in 100 ns units.
    pub user_time: i64,
    /// Kernel-mode CPU time in 100 ns units.
    pub kernel_time: i64,
    pub thread_count: u32,
    pub session_id: u32,
    pub working_set_bytes: usize,
    name: Range<usize>,
}

/// All processes at one moment. Call [`refresh`](Self::refresh) to update in place;
/// buffers are reused between refreshes.
#[derive(Debug, Default)]
pub struct ProcessSnapshot {
    buffer: Vec<u64>,
    entries: Vec<ProcessEntry>,
    names: String,
    system_time: i64,
}

/// Identifies a process instance. PIDs are reused, so the create time is part of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProcessKey {
    pub pid: u32,
    /// Creation time as a FILETIME (100 ns intervals since 1601-01-01 UTC).
    pub create_time: i64,
}

impl ProcessEntry {
    /// The process's identity.
    pub fn key(&self) -> ProcessKey {
        ProcessKey {
            pid: self.pid,
            create_time: self.create_time,
        }
    }

    /// Total user + kernel CPU time in 100 ns units.
    pub fn cpu_time(&self) -> i64 {
        self.user_time.saturating_add(self.kernel_time)
    }
}

/// Current system time as a FILETIME.
fn system_time() -> Result<i64> {
    let mut now = 0i64;
    // SAFETY: `now` is a valid out pointer.
    let status = unsafe { NtQuerySystemTime(&mut now) };
    if status.is_err() {
        return Err(Error::NtStatus {
            call: "NtQuerySystemTime",
            status: status.0 as u32,
        });
    }
    Ok(now)
}

/// Leading fields of the native `SYSTEM_PROCESS_INFORMATION` (x64 layout), which the
/// `windows` crate hides behind reserved fields.
#[repr(C)]
#[derive(Clone, Copy)]
struct SystemProcessInfo {
    next_entry_offset: u32,
    number_of_threads: u32,
    working_set_private_size: i64,
    hard_fault_count: u32,
    number_of_threads_high_watermark: u32,
    cycle_time: u64,
    create_time: i64,
    user_time: i64,
    kernel_time: i64,
    image_name: UnicodeString,
    base_priority: i32,
    unique_process_id: usize,
    inherited_from_unique_process_id: usize,
    handle_count: u32,
    session_id: u32,
    unique_process_key: usize,
    peak_virtual_size: usize,
    virtual_size: usize,
    page_fault_count: u32,
    peak_working_set_size: usize,
    working_set_size: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct UnicodeString {
    /// Length in bytes, excluding any terminator.
    length: u16,
    maximum_length: u16,
    buffer: *const u16,
}

// Offsets documented for the public parts of SYSTEM_PROCESS_INFORMATION on x64.
const _: () = {
    assert!(offset_of!(SystemProcessInfo, create_time) == 32);
    assert!(offset_of!(SystemProcessInfo, image_name) == 56);
    assert!(offset_of!(SystemProcessInfo, unique_process_id) == 80);
    assert!(offset_of!(SystemProcessInfo, inherited_from_unique_process_id) == 88);
    assert!(offset_of!(SystemProcessInfo, session_id) == 100);
    assert!(offset_of!(SystemProcessInfo, working_set_size) == 144);
};

const INITIAL_BUFFER_BYTES: usize = 1 << 20;

impl ProcessSnapshot {
    /// Takes a new snapshot.
    pub fn new() -> Result<Self> {
        let mut snapshot = Self::default();
        snapshot.refresh()?;
        Ok(snapshot)
    }

    /// Replaces the contents with a fresh snapshot.
    pub fn refresh(&mut self) -> Result<()> {
        let len = self.query()?;
        self.system_time = system_time()?;
        self.parse(len);
        Ok(())
    }

    /// When the snapshot was taken, as a FILETIME (100 ns intervals since 1601 UTC).
    pub fn system_time(&self) -> i64 {
        self.system_time
    }

    /// The processes, in the order the kernel returned them.
    pub fn entries(&self) -> &[ProcessEntry] {
        &self.entries
    }

    /// Image name of a process, e.g. `explorer.exe`.
    pub fn name<'a>(&'a self, entry: &ProcessEntry) -> &'a str {
        self.names.get(entry.name.clone()).unwrap_or_default()
    }

    /// Finds a process by PID.
    pub fn find(&self, pid: u32) -> Option<&ProcessEntry> {
        self.entries.iter().find(|e| e.pid == pid)
    }

    /// Image name for a PID, if the process is in the snapshot.
    pub fn name_of(&self, pid: u32) -> Option<&str> {
        self.find(pid).map(|e| self.name(e))
    }

    /// Fills `self.buffer` and returns the number of valid bytes.
    fn query(&mut self) -> Result<usize> {
        if self.buffer.is_empty() {
            self.buffer
                .resize(INITIAL_BUFFER_BYTES / size_of::<u64>(), 0);
        }
        loop {
            let capacity = u32::try_from(self.buffer.len() * size_of::<u64>()).unwrap_or(u32::MAX);
            let mut needed = 0u32;
            // SAFETY: the buffer is writable for `capacity` bytes and 8-byte aligned;
            // `needed` is a valid out pointer.
            let status = unsafe {
                NtQuerySystemInformation(
                    SystemProcessInformation,
                    self.buffer.as_mut_ptr().cast(),
                    capacity,
                    &mut needed,
                )
            };
            if status == STATUS_INFO_LENGTH_MISMATCH {
                // Processes can start between calls, so leave headroom.
                let needed = (needed as usize).max(self.buffer.len() * size_of::<u64>());
                let words = needed.div_ceil(size_of::<u64>());
                self.buffer.resize(words + words / 4, 0);
                continue;
            }
            if status.is_err() {
                return Err(Error::NtStatus {
                    call: "NtQuerySystemInformation(SystemProcessInformation)",
                    status: status.0 as u32,
                });
            }
            return Ok(needed as usize);
        }
    }

    fn parse(&mut self, len: usize) {
        self.entries.clear();
        self.names.clear();
        let base = self.buffer.as_ptr().cast::<u8>();
        let len = len.min(self.buffer.len() * size_of::<u64>());
        let mut offset = 0usize;
        while offset + size_of::<SystemProcessInfo>() <= len {
            // SAFETY: the bounds check above keeps the read inside the buffer;
            // `read_unaligned` makes no alignment assumption.
            let info = unsafe {
                base.add(offset)
                    .cast::<SystemProcessInfo>()
                    .read_unaligned()
            };

            let start = self.names.len();
            if info.unique_process_id == 0 {
                self.names.push_str("System Idle Process");
            } else if !info.image_name.buffer.is_null() {
                // SAFETY: the kernel points `buffer` at `length` bytes of UTF-16 inside
                // the same output buffer, which is still alive.
                let wide = unsafe {
                    std::slice::from_raw_parts(
                        info.image_name.buffer,
                        usize::from(info.image_name.length) / 2,
                    )
                };
                self.names.extend(
                    char::decode_utf16(wide.iter().copied())
                        .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER)),
                );
            }

            self.entries.push(ProcessEntry {
                pid: info.unique_process_id as u32,
                parent_pid: info.inherited_from_unique_process_id as u32,
                create_time: info.create_time,
                user_time: info.user_time,
                kernel_time: info.kernel_time,
                thread_count: info.number_of_threads,
                session_id: info.session_id,
                working_set_bytes: info.working_set_size,
                name: start..self.names.len(),
            });

            if info.next_entry_offset == 0 {
                break;
            }
            offset += info.next_entry_offset as usize;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_contains_this_process_and_system() {
        let snapshot = ProcessSnapshot::new().expect("snapshot");
        let me = snapshot.find(std::process::id()).expect("current process");
        assert!(me.create_time > 0);
        assert!(me.thread_count >= 1);
        assert!(snapshot.name(me).ends_with(".exe"), "{}", snapshot.name(me));
        assert_eq!(snapshot.name_of(4), Some("System"));
        assert_eq!(snapshot.name_of(0), Some("System Idle Process"));
    }

    #[test]
    fn refresh_reuses_snapshot() {
        let mut snapshot = ProcessSnapshot::new().expect("snapshot");
        let before = snapshot.find(std::process::id()).unwrap().create_time;
        snapshot.refresh().expect("refresh");
        let after = snapshot.find(std::process::id()).unwrap().create_time;
        assert_eq!(before, after);
    }
}
