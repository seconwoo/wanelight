//! Optional, reversible Windows settings that reduce burn-in risk. Every
//! change records the previous value in `hardening-backup.json` first.

use std::collections::BTreeMap;

use serde_json::{Value, json};
use windows::Win32::Foundation::{COLORREF, ERROR_SUCCESS, HLOCAL, HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Gdi::{COLOR_DESKTOP, GetSysColor, SYS_COLOR_INDEX};
use windows::Win32::Graphics::Gdi::SetSysColors;
use windows::Win32::System::Power::*;
use windows::Win32::System::Registry::*;
use windows::Win32::System::SystemServices::{GUID_VIDEO_POWERDOWN_TIMEOUT, GUID_VIDEO_SUBGROUP};
use windows::Win32::UI::Shell::{ABM_GETSTATE, ABM_SETSTATE, ABS_AUTOHIDE, APPBARDATA, SHAppBarMessage};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{GUID, PCWSTR, w};

use crate::util::{self, from_wide, wide};

pub struct Item {
    pub key: &'static str,
    pub title: &'static str,
    pub detail: &'static str,
}

pub const ITEMS: &[Item] = &[
    Item { key: "dark_mode", title: "Dark mode", detail: "Dark apps and taskbar: large white areas are the fastest way to wear a panel." },
    Item { key: "black_wallpaper", title: "Black desktop background", detail: "Replaces the wallpaper with solid black, so the desktop stays off-pixel." },
    Item { key: "hide_icons", title: "Hide desktop icons", detail: "Desktop icons sit in the same place for months." },
    Item { key: "accent_off", title: "No accent colour on title bars and taskbar", detail: "Saturated accent colours are static and bright." },
    Item { key: "taskbar_autohide", title: "Auto-hide the taskbar", detail: "The taskbar is the most common burn-in. Hides until you point at the screen edge." },
    Item { key: "screen_off", title: "Turn the screen off after 5 minutes", detail: "Windows' own display timeout, used when Wanelight isn't running." },
];

const PERSONALIZE: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize");
const DWM: PCWSTR = w!("Software\\Microsoft\\Windows\\DWM");
const ADVANCED: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\Advanced");
const COLORS: PCWSTR = w!("Control Panel\\Colors");

fn backup_path() -> std::path::PathBuf {
    util::data_dir().join("hardening-backup.json")
}

fn load_backup() -> BTreeMap<String, Value> {
    std::fs::read_to_string(backup_path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

fn store_backup(key: &str, v: Value) {
    let mut b = load_backup();
    // Keep the oldest backup: re-applying must not overwrite the true original.
    b.entry(key.to_string()).or_insert(v);
    let _ = util::write_atomic(&backup_path(), serde_json::to_string_pretty(&b).unwrap_or_default().as_bytes());
}

fn take_backup(key: &str) -> Option<Value> {
    let mut b = load_backup();
    let v = b.remove(key);
    let _ = util::write_atomic(&backup_path(), serde_json::to_string_pretty(&b).unwrap_or_default().as_bytes());
    v
}

pub fn has_backup(key: &str) -> bool {
    load_backup().contains_key(key)
}

fn read_dword(key: PCWSTR, name: PCWSTR) -> Option<u32> {
    let mut v = 0u32;
    let mut size = 4u32;
    let r = unsafe {
        RegGetValueW(HKEY_CURRENT_USER, key, name, RRF_RT_REG_DWORD, None, Some(&mut v as *mut u32 as _), Some(&mut size))
    };
    (r == ERROR_SUCCESS).then_some(v)
}

fn write_dword(key: PCWSTR, name: PCWSTR, v: Option<u32>) -> bool {
    unsafe {
        match v {
            Some(v) => RegSetKeyValueW(HKEY_CURRENT_USER, key, name, REG_DWORD.0, Some(&v as *const u32 as _), 4) == ERROR_SUCCESS,
            None => {
                let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, key, name);
                true
            }
        }
    }
}

fn read_string(key: PCWSTR, name: PCWSTR) -> Option<String> {
    let mut buf = [0u16; 512];
    let mut size = (buf.len() * 2) as u32;
    let r = unsafe {
        RegGetValueW(HKEY_CURRENT_USER, key, name, RRF_RT_REG_SZ, None, Some(buf.as_mut_ptr() as _), Some(&mut size))
    };
    (r == ERROR_SUCCESS).then(|| from_wide(&buf))
}

fn write_string(key: PCWSTR, name: PCWSTR, v: &str) -> bool {
    let data = wide(v);
    unsafe {
        RegSetKeyValueW(HKEY_CURRENT_USER, key, name, REG_SZ.0, Some(data.as_ptr() as _), (data.len() * 2) as u32)
            == ERROR_SUCCESS
    }
}

fn broadcast(area: &str) {
    let s = wide(area);
    unsafe {
        let _ = SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            WPARAM(0),
            LPARAM(s.as_ptr() as isize),
            SMTO_ABORTIFHUNG,
            1000,
            None,
        );
    }
}

fn wallpaper() -> String {
    let mut buf = [0u16; 520];
    unsafe {
        let _ = SystemParametersInfoW(SPI_GETDESKWALLPAPER, buf.len() as u32, Some(buf.as_mut_ptr() as _), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0));
    }
    from_wide(&buf)
}

fn set_wallpaper(path: &str) -> bool {
    let w = wide(path);
    unsafe { SystemParametersInfoW(SPI_SETDESKWALLPAPER, 0, Some(w.as_ptr() as _), SPIF_UPDATEINIFILE | SPIF_SENDCHANGE).is_ok() }
}

fn set_desktop_color(rgb: (u8, u8, u8)) {
    let c = COLORREF(rgb.0 as u32 | (rgb.1 as u32) << 8 | (rgb.2 as u32) << 16);
    unsafe {
        let _ = SetSysColors(1, &{ COLOR_DESKTOP.0 }, &c);
    }
    write_string(COLORS, w!("Background"), &format!("{} {} {}", rgb.0, rgb.1, rgb.2));
}

fn desktop_color_is_black() -> bool {
    unsafe { GetSysColor(SYS_COLOR_INDEX(COLOR_DESKTOP.0)) == 0 }
}

/// The desktop's list view host; its WM_COMMAND 0x7402 toggles icon visibility.
fn desktop_defview() -> Option<HWND> {
    unsafe {
        let progman = FindWindowW(w!("Progman"), PCWSTR::null()).ok()?;
        if let Ok(v) = FindWindowExW(Some(progman), None, w!("SHELLDLL_DefView"), PCWSTR::null()) {
            return Some(v);
        }
        let mut worker = None;
        while let Ok(wk) = FindWindowExW(None, worker, w!("WorkerW"), PCWSTR::null()) {
            if let Ok(v) = FindWindowExW(Some(wk), None, w!("SHELLDLL_DefView"), PCWSTR::null()) {
                return Some(v);
            }
            worker = Some(wk);
        }
        None
    }
}

fn desktop_icons_visible() -> Option<bool> {
    let dv = desktop_defview()?;
    unsafe {
        let lv = FindWindowExW(Some(dv), None, w!("SysListView32"), PCWSTR::null()).ok()?;
        Some(IsWindowVisible(lv).as_bool())
    }
}

fn set_desktop_icons_visible(visible: bool) {
    write_dword(ADVANCED, w!("HideIcons"), Some(!visible as u32));
    if desktop_icons_visible().is_some_and(|v| v != visible)
        && let Some(dv) = desktop_defview() {
            unsafe {
                let _ = SendMessageW(dv, WM_COMMAND, Some(WPARAM(0x7402)), Some(LPARAM(0)));
            }
        }
}

fn taskbar_state() -> u32 {
    let mut abd = APPBARDATA { cbSize: std::mem::size_of::<APPBARDATA>() as u32, ..Default::default() };
    unsafe { SHAppBarMessage(ABM_GETSTATE, &mut abd) as u32 }
}

fn set_taskbar_autohide(on: bool) {
    let mut abd = APPBARDATA { cbSize: std::mem::size_of::<APPBARDATA>() as u32, ..Default::default() };
    abd.lParam = LPARAM(if on { ABS_AUTOHIDE as isize } else { 0 });
    unsafe {
        SHAppBarMessage(ABM_SETSTATE, &mut abd);
    }
}

/// Current (AC, DC) display timeout in seconds on the active power scheme.
fn display_timeouts() -> Option<(GUID, u32, u32)> {
    unsafe {
        let mut scheme_ptr: *mut GUID = std::ptr::null_mut();
        if PowerGetActiveScheme(None, &mut scheme_ptr) != ERROR_SUCCESS || scheme_ptr.is_null() {
            return None;
        }
        let scheme = *scheme_ptr;
        let _ = windows::Win32::Foundation::LocalFree(Some(HLOCAL(scheme_ptr as _)));
        let (mut ac, mut dc) = (0u32, 0u32);
        let ok_ac = PowerReadACValueIndex(None, Some(&scheme), Some(&GUID_VIDEO_SUBGROUP), Some(&GUID_VIDEO_POWERDOWN_TIMEOUT), &mut ac) == ERROR_SUCCESS;
        let ok_dc = PowerReadDCValueIndex(None, Some(&scheme), Some(&GUID_VIDEO_SUBGROUP), Some(&GUID_VIDEO_POWERDOWN_TIMEOUT), &mut dc) == ERROR_SUCCESS.0;
        (ok_ac && ok_dc).then_some((scheme, ac, dc))
    }
}

fn set_display_timeouts(ac: u32, dc: u32) -> bool {
    let Some((scheme, _, _)) = display_timeouts() else { return false };
    unsafe {
        let a = PowerWriteACValueIndex(None, &scheme, Some(&GUID_VIDEO_SUBGROUP), Some(&GUID_VIDEO_POWERDOWN_TIMEOUT), ac);
        let d = PowerWriteDCValueIndex(None, &scheme, Some(&GUID_VIDEO_SUBGROUP), Some(&GUID_VIDEO_POWERDOWN_TIMEOUT), dc);
        let s = PowerSetActiveScheme(None, Some(&scheme));
        a == ERROR_SUCCESS && d == ERROR_SUCCESS.0 && s == ERROR_SUCCESS
    }
}

pub fn is_applied(key: &str) -> bool {
    match key {
        "dark_mode" => {
            read_dword(PERSONALIZE, w!("AppsUseLightTheme")) == Some(0)
                && read_dword(PERSONALIZE, w!("SystemUsesLightTheme")) == Some(0)
        }
        "black_wallpaper" => wallpaper().is_empty() && desktop_color_is_black(),
        "hide_icons" => desktop_icons_visible() == Some(false),
        "accent_off" => {
            read_dword(PERSONALIZE, w!("ColorPrevalence")).unwrap_or(0) == 0
                && read_dword(DWM, w!("ColorPrevalence")).unwrap_or(0) == 0
        }
        "taskbar_autohide" => taskbar_state() & ABS_AUTOHIDE != 0,
        "screen_off" => display_timeouts().is_some_and(|(_, ac, _)| ac != 0 && ac <= 600),
        _ => false,
    }
}

pub fn apply(key: &str) -> Result<(), String> {
    match key {
        "dark_mode" => {
            store_backup(key, json!({
                "apps": read_dword(PERSONALIZE, w!("AppsUseLightTheme")),
                "system": read_dword(PERSONALIZE, w!("SystemUsesLightTheme")),
            }));
            write_dword(PERSONALIZE, w!("AppsUseLightTheme"), Some(0));
            write_dword(PERSONALIZE, w!("SystemUsesLightTheme"), Some(0));
            broadcast("ImmersiveColorSet");
        }
        "black_wallpaper" => {
            store_backup(key, json!({
                "wallpaper": wallpaper(),
                "background": read_string(COLORS, w!("Background")),
            }));
            set_desktop_color((0, 0, 0));
            if !set_wallpaper("") {
                return Err("Windows refused to change the wallpaper".into());
            }
        }
        "hide_icons" => {
            store_backup(key, json!({ "visible": desktop_icons_visible().unwrap_or(true) }));
            set_desktop_icons_visible(false);
        }
        "accent_off" => {
            store_backup(key, json!({
                "personalize": read_dword(PERSONALIZE, w!("ColorPrevalence")),
                "dwm": read_dword(DWM, w!("ColorPrevalence")),
            }));
            write_dword(PERSONALIZE, w!("ColorPrevalence"), Some(0));
            write_dword(DWM, w!("ColorPrevalence"), Some(0));
            broadcast("ImmersiveColorSet");
        }
        "taskbar_autohide" => {
            store_backup(key, json!({ "autohide": taskbar_state() & ABS_AUTOHIDE != 0 }));
            set_taskbar_autohide(true);
        }
        "screen_off" => {
            let Some((_, ac, dc)) = display_timeouts() else { return Err("cannot read the power plan".into()) };
            store_backup(key, json!({ "ac": ac, "dc": dc }));
            let pick = |v: u32| if v == 0 || v > 300 { 300 } else { v };
            if !set_display_timeouts(pick(ac), pick(dc)) {
                return Err("cannot write the power plan".into());
            }
        }
        _ => return Err("unknown item".into()),
    }
    Ok(())
}

pub fn revert(key: &str) -> Result<(), String> {
    let Some(b) = take_backup(key) else { return Err("nothing to revert".into()) };
    let dword = |v: &Value| v.as_u64().map(|x| x as u32);
    match key {
        "dark_mode" => {
            write_dword(PERSONALIZE, w!("AppsUseLightTheme"), dword(&b["apps"]));
            write_dword(PERSONALIZE, w!("SystemUsesLightTheme"), dword(&b["system"]));
            broadcast("ImmersiveColorSet");
        }
        "black_wallpaper" => {
            if let Some(bg) = b["background"].as_str() {
                let p: Vec<u8> = bg.split_whitespace().filter_map(|s| s.parse().ok()).collect();
                if p.len() == 3 {
                    set_desktop_color((p[0], p[1], p[2]));
                }
            }
            set_wallpaper(b["wallpaper"].as_str().unwrap_or(""));
        }
        "hide_icons" => set_desktop_icons_visible(b["visible"].as_bool().unwrap_or(true)),
        "accent_off" => {
            write_dword(PERSONALIZE, w!("ColorPrevalence"), dword(&b["personalize"]));
            write_dword(DWM, w!("ColorPrevalence"), dword(&b["dwm"]));
            broadcast("ImmersiveColorSet");
        }
        "taskbar_autohide" => set_taskbar_autohide(b["autohide"].as_bool().unwrap_or(false)),
        "screen_off" => {
            if !set_display_timeouts(dword(&b["ac"]).unwrap_or(0), dword(&b["dc"]).unwrap_or(0)) {
                return Err("cannot write the power plan".into());
            }
        }
        _ => return Err("unknown item".into()),
    }
    Ok(())
}
