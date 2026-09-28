//! `wanelight --selftest`: checks capture, the GPU reduction, and that the
//! overlay is invisible to screen capture. Briefly dims part of the primary
//! screen (about two seconds).

use std::time::{Duration, Instant};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{GetStockObject, HBRUSH, UpdateWindow, WHITE_BRUSH};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::w;

use super::capture::{self, Cell, Sample};
use super::color;
use super::overlay::Overlay;
use crate::util;

fn pump(ms: u64) {
    let end = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < end {
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Mean brightness of the cells fully inside `r` (monitor-relative pixels).
fn region_mean(cells: &[Cell], gw: usize, r: (i32, i32, i32, i32)) -> f32 {
    let (mut sum, mut n) = (0.0, 0.0);
    let cell = super::capture::CELL;
    for cy in (r.1 + cell - 1) / cell..r.3 / cell {
        for cx in (r.0 + cell - 1) / cell..r.2 / cell {
            sum += cells[cy as usize * gw + cx as usize].mean;
            n += 1.0;
        }
    }
    if n > 0.0 { sum / n } else { 0.0 }
}

unsafe extern "system" fn white_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

/// A plain white, non-activating topmost window used as a known test surface.
fn white_window(x: i32, y: i32, size: i32) -> Option<HWND> {
    unsafe {
        let hinst = GetModuleHandleW(None).ok()?;
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(white_proc),
            hInstance: hinst.into(),
            hbrBackground: HBRUSH(GetStockObject(WHITE_BRUSH).0),
            lpszClassName: w!("WanelightSelfTestSurface"),
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            w!("WanelightSelfTestSurface"),
            w!(""),
            WS_POPUP,
            x,
            y,
            size,
            size,
            None,
            None,
            Some(hinst.into()),
            None,
        )
        .ok()?;
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        let _ = UpdateWindow(hwnd);
        Some(hwnd)
    }
}

/// (Windows reports a fullscreen app, the taskbar is topmost).
fn fullscreen_state() -> (bool, bool) {
    use windows::Win32::UI::Shell::{QUNS_BUSY, QUNS_RUNNING_D3D_FULL_SCREEN, SHQueryUserNotificationState};
    unsafe {
        let busy = matches!(SHQueryUserNotificationState(), Ok(s) if s == QUNS_BUSY || s == QUNS_RUNNING_D3D_FULL_SCREEN);
        let topmost = FindWindowW(w!("Shell_TrayWnd"), windows::core::PCWSTR::null())
            .map(|h| GetWindowLongPtrW(h, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST.0 != 0)
            .unwrap_or(true);
        (busy, topmost)
    }
}

fn next_frame(d: &mut super::capture::Display, tries: usize) -> Option<Vec<Cell>> {
    for _ in 0..tries {
        if let Sample::Frame { cells, .. } = d.sample(util::now()) {
            return Some(cells);
        }
        pump(50);
    }
    None
}

/// Shows the white test surface for `secs` seconds (manual dimming checks).
pub fn surface(secs: u64) -> i32 {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let Some(h) = white_window(96, 96, 384) else { return 1 };
    pump(secs * 1000);
    unsafe {
        let _ = DestroyWindow(h);
    }
    0
}

pub fn run(exclude_from_capture: bool, map_only: bool) -> i32 {
    util::attach_console();
    util::init_log("selftest");
    let say = |s: String| {
        println!("{s}");
        util::log_line(&s);
    };
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    say(format!("Wanelight {} self-test", env!("CARGO_PKG_VERSION")));
    let t = Instant::now();
    let shader_ok = capture::compile_shader().is_ok();
    say(format!("shader compile: {} ({:.0} ms)", if shader_ok { "ok" } else { "FAILED" }, t.elapsed().as_secs_f64() * 1000.0));
    let mut e = match capture::enumerate() {
        Ok(e) => e,
        Err(err) => {
            say(format!("display enumeration FAILED: {}", err.message()));
            return 1;
        }
    };
    for d in &e.displays {
        say(format!(
            "display {} \"{}\" {} {}x{} at ({},{}) hdr={} sdr_white={:.2} grid={}x{} offset=({},{})",
            d.id,
            d.name,
            d.gdi_name,
            d.width(),
            d.height(),
            d.rect.left,
            d.rect.top,
            d.hdr,
            d.sdr_white,
            d.geom.gw,
            d.geom.gh,
            d.geom.ox,
            d.geom.oy
        ));
    }
    let Some(idx) = e.displays.iter().position(|d| d.rect.left == 0 && d.rect.top == 0).or(Some(0)) else {
        return 1;
    };
    if e.displays.is_empty() {
        say("no displays found".into());
        return 1;
    }
    let d = &mut e.displays[idx];
    let (gw, gh) = (d.geom.gw, d.geom.gh);

    // First frame (duplication always delivers the current image first).
    let mut before = None;
    let mut ms = 0.0;
    for _ in 0..60 {
        let t = Instant::now();
        match d.sample(util::now()) {
            Sample::Frame { cells, .. } => {
                ms = t.elapsed().as_secs_f64() * 1000.0;
                before = Some(cells);
                break;
            }
            _ => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    let Some(before) = before else {
        say(format!("capture of {} FAILED: no frame (secure desktop or unsupported GPU?)", d.name));
        return 1;
    };
    let bright = before.iter().filter(|c| c.max > 0.5).count() as f32 / before.len() as f32;
    let avg = before.iter().map(|c| c.mean).sum::<f32>() / before.len() as f32;
    say(format!(
        "capture of {}: ok, sample+reduce {:.1} ms, average brightness {:.3}, cells with bright pixels {:.0}%",
        d.name,
        ms,
        avg,
        bright * 100.0
    ));

    if !map_only {
    // Overlay test on a known surface: a white square, then 60 % dim over it.
    let (sx, sy, size) = (96, 96, 384);
    let Some(surface) = white_window(d.rect.left + sx, d.rect.top + sy, size) else {
        say("test surface creation FAILED".into());
        return 1;
    };
    pump(600);
    let region = (sx, sy, sx + size, sy + size);
    let white = next_frame(d, 40).map(|c| region_mean(&c, gw, region));
    let gpu = e.gpus.iter().find(|g| g.luid == d.gpu.luid).cloned().unwrap_or_else(|| d.gpu.clone());
    let mut ov = match Overlay::new(&gpu, d.rect, d.geom, exclude_from_capture) {
        Ok(o) => o,
        Err(err) => {
            say(format!("overlay creation FAILED: {}", err.message()));
            return 1;
        }
    };
    let cell = super::capture::CELL;
    let mask: Vec<f32> = (0..gw * gh)
        .map(|i| {
            let (cx, cy) = ((i % gw) as i32 * cell, (i / gw) as i32 * cell);
            let inside = cx >= sx - cell && cx < sx + size && cy >= sy - cell && cy < sy + size;
            if inside { 0.6 } else { 0.0 }
        })
        .collect();
    let t = Instant::now();
    if let Err(err) = ov.show(&mask) {
        say(format!("overlay show FAILED: {}", err.message()));
        return 1;
    }
    let show_ms = t.elapsed().as_secs_f64() * 1000.0;
    pump(900);
    // With exclusion working the duplicated image does not change at all, so
    // "no new frame" means the overlay is invisible to capture.
    let dimmed = next_frame(d, 12).map(|c| region_mean(&c, gw, region));
    match (white, dimmed) {
        (None, _) => say("overlay capture exclusion: inconclusive (no frame with the test surface)".into()),
        (Some(w), None) => say(format!("overlay capture exclusion: ok (surface {w:.3}, overlay produced no captured change)")),
        (Some(w), Some(dm)) => {
            let verdict = if dm > w * 0.9 { "ok (overlay not captured)" } else { "NOT excluded (capture sees the dimming)" };
            say(format!("overlay capture exclusion: {verdict}; test surface brightness {w:.3} -> {dm:.3}"));
        }
    }
    say(format!("overlay first show: {show_ms:.1} ms"));
    ov.hide();
    pump(300);

    // Color matrix: does duplication see it? Briefly scales the screen to 70 %.
    let plain = next_frame(d, 12).map(|c| region_mean(&c, gw, region)).or(white);
    match color::ColorEffect::new() {
        None => say("color effect: FAILED (MagInitialize)".into()),
        Some(mut fx) => {
            let mut m = color::IDENTITY;
            for i in 0..3 {
                m[i * 5 + i] = 0.7;
            }
            let set = fx.set(&m);
            pump(700);
            let seen = next_frame(d, 12).map(|c| region_mean(&c, gw, region));
            drop(fx);
            pump(300);
            match (set, plain, seen) {
                (false, ..) => say("color effect: FAILED (MagSetFullscreenColorEffect)".into()),
                (_, None, _) => say("color effect vs capture: inconclusive (no frame with the test surface)".into()),
                (_, Some(w), None) => say(format!("color effect vs capture: not captured (surface {w:.3}, no new frame)")),
                (_, Some(w), Some(c)) => {
                    let verdict = if c < w * 0.9 { "ok (capture sees what the panel shows)" } else { "not captured" };
                    say(format!("color effect vs capture: {verdict}; test surface brightness {w:.3} -> {c:.3}"));
                }
            }
        }
    }
    unsafe {
        let _ = DestroyWindow(surface);
    }

    // Fullscreen check: an overlay spanning the screen (here at an invisible
    // 0.4 %) must not make Windows think a fullscreen app is running, or the
    // auto-hide taskbar stops appearing.
    let before = fullscreen_state();
    let _ = ov.show(&vec![1.0 / 255.0; gw * gh]);
    pump(800);
    let during = fullscreen_state();
    let verdict = match (before, during) {
        ((true, _), _) => "inconclusive (a fullscreen app was already running)".to_string(),
        (_, (false, true)) => "ok (taskbar stays on top, no fullscreen app reported)".to_string(),
        (_, (busy, topmost)) => format!("PROBLEM (fullscreen app reported: {busy}, taskbar on top: {topmost})"),
    };
    say(format!("full-screen overlay vs taskbar: {verdict}"));
    ov.hide();
    drop(ov);
    pump(100);
    }
    // Activity map: which parts of the screen change over a few seconds.
    let secs = 8;
    say(format!("activity map over {secs} s ('#' = changed in most samples, '+' some, '.' once, ' ' never):"));
    let mut counts = vec![0u32; gw * gh];
    let mut frames = 0;
    let (mut frac_sum, mut frac_n) = (0.0f32, 0.0f32);
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        if let Sample::Frame { cells, exact: true } = d.sample(util::now()) {
            frames += 1;
            for (c, s) in counts.iter_mut().zip(cells.iter()) {
                if s.changed >= (0.015 * s.n).max(4.0) {
                    *c += 1;
                    frac_sum += s.changed / s.n;
                    frac_n += 1.0;
                }
            }
        }
        pump(250);
    }
    let (cols, rows) = (72usize, (72 * gh / gw).max(8));
    for r in 0..rows {
        let mut line = String::from("  |");
        for c in 0..cols {
            let (x0, x1) = (c * gw / cols, ((c + 1) * gw / cols).max(c * gw / cols + 1));
            let (y0, y1) = (r * gh / rows, ((r + 1) * gh / rows).max(r * gh / rows + 1));
            let mut m = 0;
            for y in y0..y1 {
                for x in x0..x1 {
                    m = m.max(counts[y * gw + x]);
                }
            }
            line.push(match m {
                0 => ' ',
                1 => '.',
                n if n * 2 < frames => '+',
                _ => '#',
            });
        }
        line.push('|');
        say(line);
    }
    let changed_any = counts.iter().filter(|&&c| c > 0).count();
    say(format!(
        "{frames} frames; {:.1}% of cells changed at least once; changed cells had {:.0}% of their pixels changed on average",
        changed_any as f32 * 100.0 / counts.len() as f32,
        if frac_n > 0.0 { frac_sum / frac_n * 100.0 } else { 0.0 }
    ));
    say("self-test finished".into());
    0
}

/// `wanelight --panel-probe`: prints the UI Automation container chains at
/// sample points of the foreground window (without moving the pointer) and
/// around the keyboard focus. Geometry and roles only.
pub fn panel_probe(brief: bool) -> i32 {
    use windows::Win32::Foundation::{POINT, RECT};
    use windows::Win32::Graphics::Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};

    use super::panels::{self, Node, Uia};

    util::attach_console();
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let uia = match Uia::new() {
        Ok(u) => u,
        Err(e) => {
            println!("UI Automation unavailable: {}", e.message());
            return 1;
        }
    };
    let fg = unsafe { GetForegroundWindow() };
    let mut win = RECT::default();
    unsafe {
        let _ = DwmGetWindowAttribute(fg, DWMWA_EXTENDED_FRAME_BOUNDS, &mut win as *mut RECT as _, std::mem::size_of::<RECT>() as u32);
    }
    let wa = panels::area(&win).max(1.0);
    println!("foreground window {}x{} at ({},{})", win.right - win.left, win.bottom - win.top, win.left, win.top);
    let show = |label: &str, chain: &[Node]| {
        println!("{label}");
        for (i, n) in chain.iter().enumerate() {
            let frac = panels::area(&n.rect) / wa;
            if frac < 0.004 && i > 0 {
                continue;
            }
            println!(
                "  d{:<2} {:<8} {:<10} {:<14} {:>5}x{:<5} at ({:>5},{:>5})  {:>5.1}%",
                i,
                panels::control_type_name(n.control_type),
                panels::landmark_name(n.landmark),
                n.aria_role,
                n.rect.right - n.rect.left,
                n.rect.bottom - n.rect.top,
                n.rect.left,
                n.rect.top,
                frac * 100.0
            );
        }
    };
    let (w, h) = ((win.right - win.left) as f32, (win.bottom - win.top) as f32);
    let fmt_rect = |r: Option<RECT>| match r {
        Some(r) => format!("{}x{} at ({},{})  {:.1}% of window", r.right - r.left, r.bottom - r.top, r.left, r.top, panels::area(&r) / wa * 100.0),
        None => "none (whole window)".to_string(),
    };
    let mut points = Vec::new();
    for fx in [0.06, 0.15, 0.3, 0.5, 0.7, 0.85, 0.95] {
        points.push((fx, 0.5));
    }
    for fx in [0.3, 0.5, 0.7] {
        points.push((fx, 0.93));
    }
    points.push((0.5, 0.04));
    for (fx, fy) in points {
        let pt = POINT { x: win.left + (w * fx) as i32, y: win.top + (h * fy) as i32 };
        let t = Instant::now();
        match uia.chain_at(pt) {
            Ok(c) => {
                if !brief {
                    show(
                        &format!("point {:.0}%,{:.0}% of window ({} levels, {:.0} ms):", fx * 100.0, fy * 100.0, c.nodes.len(), t.elapsed().as_secs_f64() * 1000.0),
                        &c.nodes,
                    );
                }
                println!("  point {:>3.0}%,{:>3.0}% -> panel {}", fx * 100.0, fy * 100.0, fmt_rect(panels::choose_panel(&uia, &c, &win, pt)));
            }
            Err(e) => println!("point {:.0}%,{:.0}%: {}", fx * 100.0, fy * 100.0, e.message()),
        }
    }
    let t = Instant::now();
    match uia.focused_chain() {
        Ok(c) => {
            if !brief {
                show(&format!("keyboard focus ({} levels, {:.0} ms):", c.nodes.len(), t.elapsed().as_secs_f64() * 1000.0), &c.nodes);
            }
            println!("  keyboard focus -> input {}", fmt_rect(panels::choose_input(&uia, &c, &win)));
        }
        Err(e) => println!("keyboard focus: {}", e.message()),
    }
    0
}
