//! Thin wrapper over the PDH performance counter API for wildcard counters.

use windows::Win32::System::Performance::{
    PDH_CSTATUS_NEW_DATA, PDH_CSTATUS_VALID_DATA, PDH_FMT, PDH_FMT_COUNTERVALUE_ITEM_W,
    PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY, PDH_MORE_DATA, PDH_NO_DATA, PdhAddEnglishCounterW,
    PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW, PdhOpenQueryW,
};
use windows::core::{HSTRING, PCWSTR};

use crate::error::{Error, Result};

/// Report raw values instead of capping them at 100 (missing from the `windows` crate).
const PDH_FMT_NOCAP100: u32 = 0x0000_8000;
const ERROR_SUCCESS: u32 = 0;
/// `PDH_CSTATUS_NO_INSTANCE`: no instance currently exists for a wildcard counter.
const PDH_CSTATUS_NO_INSTANCE: u32 = 0x800007D1;

/// A PDH query holding one wildcard counter, whose instances are re-read on every
/// collection. Buffers are reused between collections.
pub(crate) struct CounterArray {
    query: PDH_HQUERY,
    counter: PDH_HCOUNTER,
    /// Item buffer; `u64` elements keep it aligned for `PDH_FMT_COUNTERVALUE_ITEM_W`.
    buffer: Vec<u64>,
    name: String,
}

impl CounterArray {
    /// Opens a query for `path` (an English counter path, e.g.
    /// `\GPU Engine(*)\Utilization Percentage`) and takes the baseline reading.
    pub(crate) fn open(path: &str) -> Result<Self> {
        let mut query = PDH_HQUERY::default();
        // SAFETY: `query` is a valid out pointer; a null data source means live data.
        check("PdhOpenQueryW", unsafe {
            PdhOpenQueryW(PCWSTR::null(), 0, &mut query)
        })?;
        // From here `Drop` closes the query, including on error.
        let mut this = Self {
            query,
            counter: PDH_HCOUNTER::default(),
            buffer: Vec::new(),
            name: String::new(),
        };
        let path = HSTRING::from(path);
        // SAFETY: `this.query` is open, `path` is NUL-terminated and outlives the call,
        // and `this.counter` is a valid out pointer.
        check("PdhAddEnglishCounterW", unsafe {
            PdhAddEnglishCounterW(this.query, &path, 0, &mut this.counter)
        })?;
        this.collect_raw()?;
        Ok(this)
    }

    fn collect_raw(&mut self) -> Result<()> {
        // SAFETY: `self.query` is an open query handle.
        let status = unsafe { PdhCollectQueryData(self.query) };
        // PDH_NO_DATA: the counter currently has no instances (e.g. no GPU activity).
        if status == PDH_NO_DATA {
            return Ok(());
        }
        check("PdhCollectQueryData", status)
    }

    /// Collects new data and calls `f(instance_name, value)` for each instance with a
    /// valid value. Rates are averaged over the time since the previous collection.
    /// Returns the number of instances PDH reported.
    pub(crate) fn collect(&mut self, mut f: impl FnMut(&str, f64)) -> Result<usize> {
        self.collect_raw()?;
        let format = PDH_FMT(PDH_FMT_DOUBLE.0 | PDH_FMT_NOCAP100);
        let count = loop {
            let mut size_bytes =
                u32::try_from(self.buffer.len() * size_of::<u64>()).unwrap_or(u32::MAX);
            let mut count = 0u32;
            let items = if self.buffer.is_empty() {
                None
            } else {
                Some(
                    self.buffer
                        .as_mut_ptr()
                        .cast::<PDH_FMT_COUNTERVALUE_ITEM_W>(),
                )
            };
            // SAFETY: `items` is either None or points to `size_bytes` writable bytes,
            // aligned to 8 bytes; both out pointers are valid.
            let status = unsafe {
                PdhGetFormattedCounterArrayW(
                    self.counter,
                    format,
                    &mut size_bytes,
                    &mut count,
                    items,
                )
            };
            match status {
                ERROR_SUCCESS => break count as usize,
                PDH_MORE_DATA => {
                    // Grow with headroom: instances can appear between calls.
                    let needed = (size_bytes as usize).div_ceil(size_of::<u64>());
                    self.buffer.resize(needed + needed / 4 + 64, 0);
                }
                PDH_NO_DATA | PDH_CSTATUS_NO_INSTANCE => return Ok(0),
                status => {
                    return Err(Error::Pdh {
                        call: "PdhGetFormattedCounterArrayW",
                        status,
                    });
                }
            }
        };

        let items = self.buffer.as_ptr().cast::<PDH_FMT_COUNTERVALUE_ITEM_W>();
        for i in 0..count {
            // SAFETY: PDH wrote `count` items to the start of the buffer.
            let item = unsafe { &*items.add(i) };
            if item.FmtValue.CStatus != PDH_CSTATUS_VALID_DATA
                && item.FmtValue.CStatus != PDH_CSTATUS_NEW_DATA
            {
                continue;
            }
            // SAFETY: `szName` points to a NUL-terminated string inside the buffer.
            let wide = unsafe { item.szName.as_wide() };
            self.name.clear();
            self.name.extend(
                char::decode_utf16(wide.iter().copied())
                    .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER)),
            );
            // SAFETY: PDH_FMT_DOUBLE was requested, so `doubleValue` is the active field.
            let value = unsafe { item.FmtValue.Anonymous.doubleValue };
            f(&self.name, value);
        }
        Ok(count)
    }
}

impl Drop for CounterArray {
    fn drop(&mut self) {
        // SAFETY: `self.query` was opened by `PdhOpenQueryW` and is closed only here.
        unsafe {
            PdhCloseQuery(self.query);
        }
    }
}

fn check(call: &'static str, status: u32) -> Result<()> {
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(Error::Pdh { call, status })
    }
}
