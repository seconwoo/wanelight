//! Start-with-Windows via the per-user Run key.

use windows::Win32::System::Registry::*;
use windows::core::{PCWSTR, w};

use crate::util::wide;

const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const VALUE: PCWSTR = w!("Wanelight");

fn command() -> Option<String> {
    std::env::current_exe().ok().map(|p| format!("\"{}\"", p.display()))
}

pub fn is_enabled() -> bool {
    let mut buf = [0u16; 1024];
    let mut size = (buf.len() * 2) as u32;
    let r = unsafe {
        RegGetValueW(HKEY_CURRENT_USER, RUN_KEY, VALUE, RRF_RT_REG_SZ, None, Some(buf.as_mut_ptr() as _), Some(&mut size))
    };
    r.is_ok() && command().is_some_and(|c| crate::util::from_wide(&buf).eq_ignore_ascii_case(&c))
}

pub fn set(enabled: bool) -> bool {
    unsafe {
        if enabled {
            let Some(cmd) = command() else { return false };
            let data = wide(&cmd);
            RegSetKeyValueW(HKEY_CURRENT_USER, RUN_KEY, VALUE, REG_SZ.0, Some(data.as_ptr() as _), (data.len() * 2) as u32)
                .is_ok()
        } else {
            let r = RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, VALUE);
            r.is_ok() || r == windows::Win32::Foundation::ERROR_FILE_NOT_FOUND
        }
    }
}
