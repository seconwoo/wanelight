//! Snapshot of visible top-level windows in z-order, painted onto a monitor's
//! cell grid so each cell knows which window it shows.

use std::collections::HashMap;

use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{BOOL, PWSTR};

use super::capture::{CELL, GridGeom};
use crate::util::from_wide;

pub const OWNER_NONE: u16 = u16::MAX;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Normal,
    Taskbar,
}

#[derive(Clone, Debug)]
pub struct Win {
    pub root: HWND,
    pub rect: RECT,
    pub kind: Kind,
    pub pid: u32,
}

#[derive(Default)]
pub struct Snapshot {
    /// Top of the z-order first.
    pub wins: Vec<Win>,
    pub fg_root: HWND,
    pub fg_rect: Option<RECT>,
    pub fg_pid: u32,
}

struct EnumCtx {
    own_pid: u32,
    wins: Vec<Win>,
}

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    unsafe {
        let ctx = &mut *(lparam.0 as *mut EnumCtx);
        if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
            return true.into();
        }
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        if ex & WS_EX_TRANSPARENT.0 != 0 {
            return true.into();
        }
        let mut cloaked = 0u32;
        if DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, &mut cloaked as *mut u32 as _, 4).is_ok() && cloaked != 0 {
            return true.into();
        }
        if ex & WS_EX_LAYERED.0 != 0 {
            let mut alpha = 255u8;
            let mut flags = LAYERED_WINDOW_ATTRIBUTES_FLAGS(0);
            if GetLayeredWindowAttributes(hwnd, None, Some(&mut alpha), Some(&mut flags)).is_ok()
                && flags.0 & LWA_ALPHA.0 != 0
                && alpha == 0
            {
                return true.into();
            }
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == ctx.own_pid {
            return true.into();
        }
        let mut rect = RECT::default();
        if DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut rect as *mut RECT as _,
            std::mem::size_of::<RECT>() as u32,
        )
        .is_err()
            && GetWindowRect(hwnd, &mut rect).is_err()
        {
            return true.into();
        }
        if rect.right <= rect.left || rect.bottom <= rect.top {
            return true.into();
        }
        let mut class = [0u16; 64];
        let n = GetClassNameW(hwnd, &mut class);
        let class = from_wide(&class[..n.max(0) as usize]);
        let kind = match class.as_str() {
            "Shell_TrayWnd" | "Shell_SecondaryTrayWnd" => Kind::Taskbar,
            // The desktop itself: cells showing it count as "no window".
            "Progman" | "WorkerW" => return true.into(),
            _ => Kind::Normal,
        };
        let root = GetAncestor(hwnd, GA_ROOTOWNER);
        ctx.wins.push(Win { root: if root.is_invalid() { hwnd } else { root }, rect, kind, pid });
        true.into()
    }
}

pub fn snapshot(own_pid: u32) -> Snapshot {
    let mut ctx = EnumCtx { own_pid, wins: Vec::new() };
    unsafe {
        let _ = EnumWindows(Some(enum_proc), LPARAM(&mut ctx as *mut EnumCtx as isize));
    }
    let fg = unsafe { GetForegroundWindow() };
    let fg_root = if fg.is_invalid() {
        HWND::default()
    } else {
        let r = unsafe { GetAncestor(fg, GA_ROOTOWNER) };
        if r.is_invalid() { fg } else { r }
    };
    let fg_win = ctx.wins.iter().find(|w| w.root == fg_root);
    let mut fg_pid = 0u32;
    if !fg.is_invalid() {
        unsafe { GetWindowThreadProcessId(fg, Some(&mut fg_pid)) };
    }
    Snapshot { fg_rect: fg_win.map(|w| w.rect), wins: ctx.wins, fg_root, fg_pid }
}

fn ceil_div(a: i32, b: i32) -> i32 {
    a.div_euclid(b) + (a.rem_euclid(b) != 0) as i32
}

/// Returns, for each cell of the monitor grid, the index into `snap.wins` of
/// the window visible at the cell's centre (or OWNER_NONE for the desktop).
pub fn paint(snap: &Snapshot, mon: &RECT, geom: &GridGeom, out: &mut Vec<u16>) {
    out.clear();
    out.resize(geom.len(), OWNER_NONE);
    let base_x = mon.left + geom.ox + CELL / 2;
    let base_y = mon.top + geom.oy + CELL / 2;
    for (idx, w) in snap.wins.iter().enumerate().rev() {
        let cx0 = ceil_div(w.rect.left - base_x, CELL).max(0) as usize;
        let cx1 = (ceil_div(w.rect.right - base_x, CELL).max(0) as usize).min(geom.gw);
        let cy0 = ceil_div(w.rect.top - base_y, CELL).max(0) as usize;
        let cy1 = (ceil_div(w.rect.bottom - base_y, CELL).max(0) as usize).min(geom.gh);
        if cx0 >= cx1 || cy0 >= cy1 || idx >= OWNER_NONE as usize {
            continue;
        }
        for y in cy0..cy1 {
            out[y * geom.gw + cx0..y * geom.gw + cx1].fill(idx as u16);
        }
    }
}

/// Caches pid -> lowercase executable name.
#[derive(Default)]
pub struct ProcessNames {
    cache: HashMap<u32, (String, f64)>,
}

impl ProcessNames {
    pub fn get(&mut self, pid: u32, now: f64) -> String {
        if pid == 0 {
            return String::new();
        }
        if let Some((name, t)) = self.cache.get(&pid)
            && now - t < 60.0 {
                return name.clone();
            }
        let name = unsafe { query_process_name(pid) }.unwrap_or_default();
        if self.cache.len() > 512 {
            self.cache.retain(|_, (_, t)| now - *t < 60.0);
        }
        self.cache.insert(pid, (name.clone(), now));
        name
    }
}

unsafe fn query_process_name(pid: u32) -> Option<String> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 520];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut len).is_ok();
        let _ = CloseHandle(h);
        if !ok {
            return None;
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        Some(path.rsplit(['\\', '/']).next().unwrap_or("").to_ascii_lowercase())
    }
}
