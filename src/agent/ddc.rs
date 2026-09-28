//! Hardware brightness over DDC/CI, run on a worker thread because every I2C
//! transaction takes tens of milliseconds.
//!
//! Original brightness values are written to `ddc-restore.json` before the
//! first change and removed after restoring, so a crash or a kill from Task
//! Manager is repaired on the next start.

use std::collections::BTreeMap;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

use windows::Win32::Devices::Display::*;
use windows::Win32::Foundation::{LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFOEXW};
use windows::core::BOOL;

use crate::log;
use crate::util::{self, from_wide};

pub enum Cmd {
    /// Set every DDC-capable monitor to `percent` of its original brightness.
    Dim(u32),
    Restore,
    Probe,
}

/// Which GDI displays answered DDC/CI brightness queries.
pub type Support = Arc<Mutex<BTreeMap<String, bool>>>;

pub struct DdcWorker {
    tx: Sender<Cmd>,
    pub support: Support,
}

impl DdcWorker {
    pub fn start() -> Self {
        let (tx, rx) = channel();
        let support: Support = Arc::default();
        let s = support.clone();
        let _ = std::thread::Builder::new().name("ddc".into()).spawn(move || worker(rx, s));
        let w = DdcWorker { tx, support };
        // Repair a previous crash before anything else.
        w.send(Cmd::Restore);
        w
    }

    pub fn send(&self, cmd: Cmd) {
        let _ = self.tx.send(cmd);
    }
}

fn restore_path() -> std::path::PathBuf {
    util::data_dir().join("ddc-restore.json")
}

struct Physical {
    key: String,
    gdi: String,
    handle: PHYSICAL_MONITOR,
}

fn monitors() -> Vec<(HMONITOR, String)> {
    unsafe extern "system" fn cb(h: HMONITOR, _: HDC, _: *mut RECT, lp: LPARAM) -> BOOL {
        unsafe {
            let list = &mut *(lp.0 as *mut Vec<(HMONITOR, String)>);
            let mut info = MONITORINFOEXW::default();
            info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
            if GetMonitorInfoW(h, &mut info.monitorInfo as *mut _).as_bool() {
                list.push((h, from_wide(&info.szDevice)));
            }
            true.into()
        }
    }
    let mut list: Vec<(HMONITOR, String)> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(cb), LPARAM(&mut list as *mut _ as isize));
    }
    list
}

fn open_physical() -> Vec<Physical> {
    let mut out = Vec::new();
    for (h, gdi) in monitors() {
        unsafe {
            let mut n = 0u32;
            if GetNumberOfPhysicalMonitorsFromHMONITOR(h, &mut n).is_err() || n == 0 {
                continue;
            }
            let mut arr = vec![PHYSICAL_MONITOR::default(); n as usize];
            if GetPhysicalMonitorsFromHMONITOR(h, &mut arr).is_err() {
                continue;
            }
            for (i, pm) in arr.into_iter().enumerate() {
                out.push(Physical { key: format!("{gdi}#{i}"), gdi: gdi.clone(), handle: pm });
            }
        }
    }
    out
}

fn close_physical(list: Vec<Physical>) {
    let handles: Vec<PHYSICAL_MONITOR> = list.into_iter().map(|p| p.handle).collect();
    if !handles.is_empty() {
        unsafe {
            let _ = DestroyPhysicalMonitors(&handles);
        }
    }
}

fn get_brightness(p: &Physical) -> Option<(u32, u32, u32)> {
    let (mut min, mut cur, mut max) = (0u32, 0u32, 0u32);
    let ok = unsafe { GetMonitorBrightness(p.handle.hPhysicalMonitor, &mut min, &mut cur, &mut max) } != 0;
    (ok && max > min).then_some((min, cur, max))
}

fn set_brightness(p: &Physical, v: u32) -> bool {
    unsafe { SetMonitorBrightness(p.handle.hPhysicalMonitor, v) != 0 }
}

fn load_saved() -> BTreeMap<String, u32> {
    std::fs::read_to_string(restore_path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn worker(rx: Receiver<Cmd>, support: Support) {
    // Minimum spacing between dims protects the monitor's settings EEPROM.
    let mut last_dim: Option<std::time::Instant> = None;
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::Probe => {
                let list = open_physical();
                let mut map = BTreeMap::new();
                for p in &list {
                    let ok = get_brightness(p).is_some();
                    let e = map.entry(p.gdi.clone()).or_insert(false);
                    *e |= ok;
                }
                close_physical(list);
                *support.lock().unwrap_or_else(|e| e.into_inner()) = map;
            }
            Cmd::Dim(percent) => {
                if last_dim.is_some_and(|t| t.elapsed().as_secs() < 20) {
                    continue;
                }
                last_dim = Some(std::time::Instant::now());
                let list = open_physical();
                let mut saved = load_saved();
                for p in &list {
                    let Some((min, cur, _max)) = get_brightness(p) else { continue };
                    let original = *saved.entry(p.key.clone()).or_insert(cur);
                    // Persist before touching the monitor.
                    let _ = util::write_atomic(&restore_path(), serde_json::to_string(&saved).unwrap_or_default().as_bytes());
                    let target = min + (original.saturating_sub(min)) * percent.min(100) / 100;
                    if target != cur && set_brightness(p, target) {
                        log!("ddc: {} brightness {cur} -> {target}", p.key);
                    }
                }
                close_physical(list);
            }
            Cmd::Restore => {
                let saved = load_saved();
                if saved.is_empty() {
                    continue;
                }
                let list = open_physical();
                let mut remaining = saved.clone();
                for p in &list {
                    if let Some(&v) = saved.get(&p.key)
                        && set_brightness(p, v) {
                            log!("ddc: {} brightness restored to {v}", p.key);
                            remaining.remove(&p.key);
                        }
                }
                close_physical(list);
                if remaining.is_empty() {
                    let _ = std::fs::remove_file(restore_path());
                } else {
                    log!("ddc: could not restore {:?}; will retry", remaining.keys().collect::<Vec<_>>());
                }
            }
        }
    }
}

/// Synchronous restore used from the panic hook.
pub fn restore_now() {
    let saved = load_saved();
    if saved.is_empty() {
        return;
    }
    let list = open_physical();
    for p in &list {
        if let Some(&v) = saved.get(&p.key) {
            set_brightness(p, v);
        }
    }
    close_physical(list);
    let _ = std::fs::remove_file(restore_path());
}
