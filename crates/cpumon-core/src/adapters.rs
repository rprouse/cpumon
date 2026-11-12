//! GPU adapter names, used to label the LUIDs in GPU counter instances.

use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, DXGI_ERROR_NOT_FOUND, IDXGIFactory1};

use crate::error::Result;
use crate::gpu::Luid;

/// A GPU adapter known to DXGI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdapterInfo {
    pub luid: Luid,
    pub description: String,
}

/// Lists the system's GPU adapters. Call once at startup; adapters rarely change.
pub fn list_adapters() -> Result<Vec<AdapterInfo>> {
    // SAFETY: plain COM factory creation with no pointer arguments.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }?;
    let mut adapters = Vec::new();
    for index in 0.. {
        // SAFETY: `factory` is a valid interface; out-of-range indexes return an error.
        let adapter = match unsafe { factory.EnumAdapters1(index) } {
            Ok(adapter) => adapter,
            Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(e) => return Err(e.into()),
        };
        // SAFETY: `adapter` is a valid interface returned above.
        let desc = unsafe { adapter.GetDesc1() }?;
        let len = desc
            .Description
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(desc.Description.len());
        adapters.push(AdapterInfo {
            luid: Luid {
                high: desc.AdapterLuid.HighPart as u32,
                low: desc.AdapterLuid.LowPart,
            },
            description: String::from_utf16_lossy(&desc.Description[..len]),
        });
    }
    Ok(adapters)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_adapters() {
        // Even machines without a GPU have the Microsoft Basic Render Driver.
        let adapters = list_adapters().expect("enumerate adapters");
        assert!(!adapters.is_empty());
    }
}
