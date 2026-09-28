//! Notification-area icon and its menu.

use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateDIBSection, DIB_RGB_COLORS, DeleteObject,
};
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

use crate::icon_art;

pub const WM_TRAY: u32 = WM_APP + 1;

pub const ID_OPEN: u32 = 100;
pub const ID_HEATMAP: u32 = 101;
pub const ID_PAUSE_HOUR: u32 = 102;
pub const ID_PAUSE: u32 = 103;
pub const ID_RESUME: u32 = 104;
pub const ID_EXIT: u32 = 105;

pub struct Tray {
    hwnd: HWND,
    icon: HICON,
    paused: bool,
    tip: String,
    added: bool,
}

fn light_taskbar() -> bool {
    let mut value = 0u32;
    let mut size = 4u32;
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("SystemUsesLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut value as *mut u32 as _),
            Some(&mut size),
        )
    };
    r.is_ok() && value == 1
}

/// Builds an HICON from straight-alpha RGBA pixels.
pub fn icon_from_rgba(rgba: &[u8], size: i32) -> Option<HICON> {
    unsafe {
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size,
                biHeight: -size,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = std::ptr::null_mut();
        let color = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        let dst = std::slice::from_raw_parts_mut(bits as *mut u8, (size * size * 4) as usize);
        for (d, s) in dst.chunks_exact_mut(4).zip(rgba.chunks_exact(4)) {
            d.copy_from_slice(&[s[2], s[1], s[0], s[3]]);
        }
        let mask_bits = vec![0u8; ((size + 15) / 16 * 2 * size) as usize];
        let mask = CreateBitmap(size, size, 1, 1, Some(mask_bits.as_ptr() as _));
        let info = ICONINFO { fIcon: true.into(), xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
        let icon = CreateIconIndirect(&info).ok();
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mask.into());
        icon
    }
}

fn make_icon(paused: bool) -> HICON {
    let size = unsafe { GetSystemMetrics(SM_CXSMICON) }.max(16);
    let rgba = icon_art::tray_icon_rgba(size as u32, light_taskbar(), paused);
    icon_from_rgba(&rgba, size).unwrap_or_default()
}

fn copy_str(dst: &mut [u16], s: &str) {
    let w: Vec<u16> = s.encode_utf16().take(dst.len() - 1).collect();
    dst[..w.len()].copy_from_slice(&w);
    dst[w.len()] = 0;
}

impl Tray {
    pub fn new(hwnd: HWND) -> Self {
        let mut t = Tray { hwnd, icon: make_icon(false), paused: false, tip: "Wanelight".into(), added: false };
        t.add();
        t
    }

    fn data(&self) -> NOTIFYICONDATAW {
        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: 1,
            uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
            uCallbackMessage: WM_TRAY,
            hIcon: self.icon,
            ..Default::default()
        };
        copy_str(&mut nid.szTip, &self.tip);
        nid
    }

    /// (Re)adds the icon, e.g. after Explorer restarts.
    pub fn add(&mut self) {
        let nid = self.data();
        self.added = unsafe { Shell_NotifyIconW(NIM_ADD, &nid) }.as_bool();
    }

    pub fn update(&mut self, paused: bool, tip: &str) {
        if paused == self.paused && tip == self.tip {
            return;
        }
        if paused != self.paused {
            self.paused = paused;
            self.replace_icon();
        }
        self.tip = tip.to_string();
        let nid = self.data();
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
        }
    }

    fn replace_icon(&mut self) {
        let old = self.icon;
        self.icon = make_icon(self.paused);
        if !old.is_invalid() {
            unsafe {
                let _ = DestroyIcon(old);
            }
        }
    }

    /// Taskbar theme changed: redraw the icon in the matching colour.
    pub fn refresh_theme(&mut self) {
        self.replace_icon();
        let nid = self.data();
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
        }
    }

    pub fn balloon(&self, title: &str, text: &str) {
        let mut nid = self.data();
        nid.uFlags |= NIF_INFO;
        copy_str(&mut nid.szInfoTitle, title);
        copy_str(&mut nid.szInfo, text);
        nid.dwInfoFlags = NIIF_INFO | NIIF_NOSOUND;
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
        }
    }

    pub fn remove(&mut self) {
        if self.added {
            let nid = self.data();
            unsafe {
                let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
            }
            self.added = false;
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        self.remove();
        if !self.icon.is_invalid() {
            unsafe {
                let _ = DestroyIcon(self.icon);
            }
        }
    }
}

/// Shows the context menu and returns the chosen command id (0 = none).
/// Must be called without holding any agent borrow: the menu runs a modal loop.
pub fn show_menu(hwnd: HWND, status_line: &str, paused: bool) -> u32 {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return 0 };
        let status = crate::util::wide(status_line);
        let _ = AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, PCWSTR(status.as_ptr()));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(menu, MF_STRING, ID_OPEN as usize, w!("Open Wanelight…"));
        let _ = AppendMenuW(menu, MF_STRING, ID_HEATMAP as usize, w!("Wear heatmap…"));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        if paused {
            let _ = AppendMenuW(menu, MF_STRING, ID_RESUME as usize, w!("Resume protection"));
        } else {
            let _ = AppendMenuW(menu, MF_STRING, ID_PAUSE_HOUR as usize, w!("Pause for 1 hour"));
            let _ = AppendMenuW(menu, MF_STRING, ID_PAUSE as usize, w!("Pause until resumed"));
        }
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(menu, MF_STRING, ID_EXIT as usize, w!("Exit"));
        let _ = SetMenuDefaultItem(menu, ID_OPEN, 0);
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        // Required so the menu closes when the user clicks elsewhere.
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY | TPM_BOTTOMALIGN,
            pt.x,
            pt.y,
            None,
            hwnd,
            None,
        );
        let _ = PostMessageW(Some(hwnd), WM_NULL, Default::default(), Default::default());
        let _ = DestroyMenu(menu);
        cmd.0 as u32
    }
}
