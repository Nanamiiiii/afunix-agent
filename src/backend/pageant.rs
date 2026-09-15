use std::ffi::CString;
use std::fmt::Write as _;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::process;
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, anyhow, bail};
use windows::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, GetLastError, HANDLE, INVALID_HANDLE_VALUE, LPARAM, WPARAM,
};
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, PAGE_READWRITE,
    UnmapViewOfFile,
};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, SMTO_ABORTIFHUNG, SMTO_BLOCK, SendMessageTimeoutW, WM_COPYDATA,
};
use windows::core::HSTRING;

use super::AgentBackend;
use crate::framing::validate_frame;
use crate::model::BackendKind;

const PAGEANT_COPYDATA_ID: usize = 0x804E50BA;
const PAGEANT_MAPPING_LEN: usize = 8192;
const PAGEANT_TIMEOUT_MS: u32 = 30_000;

static NEXT_COOKIE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Default, Clone, Copy)]
pub struct PageantBackend;

impl AgentBackend for PageantBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Pageant
    }

    fn transact(&self, request_frame: &[u8]) -> Result<Vec<u8>> {
        validate_frame(request_frame).context("invalid SSH agent request frame")?;
        if request_frame.len() > PAGEANT_MAPPING_LEN {
            bail!(
                "SSH agent frame is too large for Pageant: {} bytes exceeds {} bytes",
                request_frame.len(),
                PAGEANT_MAPPING_LEN
            );
        }

        let window = find_pageant_window()?;
        let map_name = format!("PageantRequest{}", next_cookie()?);
        let mapping = MemoryMapping::create(&map_name)?;
        mapping.write(request_frame)?;

        let map_name = CString::new(map_name).context("invalid Pageant mapping name")?;
        let map_name_bytes = map_name.as_bytes_with_nul();
        let copy_data = COPYDATASTRUCT {
            dwData: PAGEANT_COPYDATA_ID,
            cbData: map_name_bytes.len() as u32,
            lpData: map_name_bytes.as_ptr().cast_mut().cast(),
        };
        let mut response_result = 0usize;
        let send_result = unsafe {
            SendMessageTimeoutW(
                window,
                WM_COPYDATA,
                WPARAM(0),
                LPARAM(ptr::from_ref(&copy_data) as isize),
                SMTO_ABORTIFHUNG | SMTO_BLOCK,
                PAGEANT_TIMEOUT_MS,
                Some(&mut response_result),
            )
        };
        if send_result.0 == 0 {
            return Err(anyhow!(
                "Pageant request timed out or failed after {} ms: {}",
                PAGEANT_TIMEOUT_MS,
                std::io::Error::last_os_error()
            ));
        }
        if response_result == 0 {
            bail!("Pageant rejected the SSH agent request");
        }

        let response = mapping.read_frame()?;
        validate_frame(&response).context("invalid SSH agent response frame")?;
        Ok(response)
    }
}

fn find_pageant_window() -> Result<windows::Win32::Foundation::HWND> {
    let name = HSTRING::from("Pageant");
    let window = unsafe { FindWindowW(&name, &name) }.context("failed to locate Pageant")?;
    if window.is_invalid() {
        bail!("Pageant is not running");
    }
    Ok(window)
}

fn next_cookie() -> Result<String> {
    let sequence = NEXT_COOKIE.fetch_add(1, Ordering::Relaxed);
    let mut random = [0u8; 16];
    getrandom::fill(&mut random)
        .map_err(|error| anyhow!("failed to generate a Pageant mapping cookie: {error}"))?;
    let mut cookie = format!("afunix-agent-{}-{sequence}-", process::id());
    for byte in random {
        write!(&mut cookie, "{byte:02x}").unwrap();
    }
    Ok(cookie)
}

struct MemoryMapping {
    _handle: OwnedHandle,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
}

impl MemoryMapping {
    fn create(name: &str) -> Result<Self> {
        let name = HSTRING::from(name);
        let handle = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                None,
                PAGE_READWRITE,
                0,
                PAGEANT_MAPPING_LEN as u32,
                &name,
            )
        }
        .context("failed to create the Pageant shared memory mapping")?;
        let last_error = unsafe { GetLastError() };
        let handle = unsafe { OwnedHandle::from_raw_handle(handle.0) };
        if last_error == ERROR_ALREADY_EXISTS {
            bail!("Pageant shared memory mapping name already exists");
        }
        let view = unsafe {
            MapViewOfFile(
                HANDLE(handle.as_raw_handle()),
                FILE_MAP_WRITE,
                0,
                0,
                PAGEANT_MAPPING_LEN,
            )
        };
        if view.Value.is_null() {
            return Err(std::io::Error::last_os_error())
                .context("failed to map the Pageant shared memory");
        }
        Ok(Self {
            _handle: handle,
            view,
        })
    }

    fn write(&self, data: &[u8]) -> Result<()> {
        if data.len() > PAGEANT_MAPPING_LEN {
            bail!("Pageant shared memory write exceeds its bounds");
        }
        unsafe {
            ptr::copy_nonoverlapping(data.as_ptr(), self.view.Value.cast(), data.len());
        }
        Ok(())
    }

    fn read_frame(&self) -> Result<Vec<u8>> {
        let bytes = unsafe {
            std::slice::from_raw_parts(self.view.Value.cast::<u8>(), PAGEANT_MAPPING_LEN)
        };
        let payload_len = u32::from_be_bytes(bytes[..4].try_into().unwrap()) as usize;
        let frame_len = 4usize
            .checked_add(payload_len)
            .ok_or_else(|| anyhow!("Pageant response length overflow"))?;
        if frame_len > PAGEANT_MAPPING_LEN {
            bail!("Pageant response frame exceeds shared memory bounds: {frame_len} bytes");
        }
        Ok(bytes[..frame_len].to_vec())
    }
}

impl Drop for MemoryMapping {
    fn drop(&mut self) {
        unsafe {
            let _ = UnmapViewOfFile(self.view);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{PAGEANT_MAPPING_LEN, next_cookie};

    #[test]
    fn cookies_include_the_process_id_and_are_unique() {
        let prefix = format!("afunix-agent-{}-", std::process::id());
        let first = next_cookie().unwrap();
        let second = next_cookie().unwrap();

        assert!(first.starts_with(&prefix));
        assert!(second.starts_with(&prefix));
        assert_ne!(first, second);
    }

    #[test]
    fn pageant_mapping_holds_its_maximum_wire_frame() {
        assert_eq!(PAGEANT_MAPPING_LEN, 8192);
    }
}
