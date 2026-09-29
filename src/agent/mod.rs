//! The background agent: a hidden window that owns the tray icon, runs a 1 Hz
//! control loop (sample screens -> update model -> choose dims -> animate), and
//! a 30 Hz loop only while something is animating or the screen is resting.

mod capture;
mod cat_frames;
mod color;
mod critter;
mod ddc;
mod model;
mod overlay;
mod panels;
mod power;
pub mod selftest;
mod spook;
mod tray;
mod winmap;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::*;
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Power::{POWERBROADCAST_SETTING, RegisterPowerSettingNotification};
use windows::Win32::System::SystemServices::GUID_CONSOLE_DISPLAY_STATE;
use windows::Win32::System::Threading::{CreateMutexW, GetCurrentProcessId};
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook};
use windows::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Input::{RAWINPUTDEVICE, RIDEV_INPUTSINK, RIDEV_REMOVE, RegisterRawInputDevices};
use windows::Win32::UI::Shell::{QUNS_PRESENTATION_MODE, SHQueryUserNotificationState};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

use crate::config::{Config, MonitorPrefs, TorchMode};
use crate::ipc;
use crate::ledger::Ledger;
use crate::log;
use crate::util::{self, wide};
use capture::Gpu;

const TIMER_TICK: usize = 1;
const TIMER_FAST: usize = 2;
const WM_APP_FOCUS: u32 = WM_APP + 2;
/// Posted by the panel worker when UI Automation answers are ready.
const WM_APP_PANEL: u32 = WM_APP + 3;
/// Halo around the pointer in panel mode (px).
const PANEL_HALO_PX: f32 = 90.0;
const HOTKEY_ID: i32 = 1;
const HOTKEY_TORCH: i32 = 2;
const HOTKEY_BLACKS: i32 = 3;
/// Rebuilds to wait for a waking monitor's device path before using a fallback id.
const ID_RETRIES: u32 = 10;
/// Frame interval while animating (~60 fps).
const FAST_MS: u32 = 16;
/// Frame interval while only watching the pointer or input (~30 fps).
const WATCH_MS: u32 = 33;
/// Halo kept lit around the cursor in torch "window" mode (px).
const TORCH_HALO_PX: f32 = 200.0;

pub struct Options {
    pub exclude_from_capture: bool,
}

thread_local! {
    static AGENT: RefCell<Option<Agent>> = const { RefCell::new(None) };
}

static AGENT_HWND: AtomicIsize = AtomicIsize::new(0);
static CMD_MSG: AtomicU32 = AtomicU32::new(0);
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

/// Runs `f` on the agent unless it is already borrowed (re-entrant message).
fn with_agent<R>(f: impl FnOnce(&mut Agent) -> R) -> Option<R> {
    AGENT.with(|a| a.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

struct Screen {
    d: capture::Display,
    model: model::Model,
    cur: Vec<f32>,
    target: Vec<f32>,
    motion: Vec<model::Motion>,
    owners: Vec<u16>,
    composed: Vec<f32>,
    composed_away: f32,
    overlay: Option<overlay::Overlay>,
    overlay_failed: bool,
    ledger: Ledger,
    last_attention: f64,
    fullscreen: bool,
    enabled: bool,
}

#[derive(Serialize, Deserialize, Default)]
struct PersistedState {
    panel_on_secs: f64,
    saved_at: u64,
}

fn state_path() -> std::path::PathBuf {
    util::data_dir().join("state.json")
}

struct Agent {
    hwnd: HWND,
    opts: Options,
    cfg: Config,
    cfg_mtime: Option<SystemTime>,
    screens: Vec<Screen>,
    overlay_gpu: Option<Rc<Gpu>>,
    tray: tray::Tray,
    ddc: ddc::DdcWorker,
    audio: power::AudioMeter,
    procs: winmap::ProcessNames,
    snap: winmap::Snapshot,
    high_risk: Vec<bool>,
    own_pid: u32,
    fg_name: String,
    last_tick: f64,
    last_anim: f64,
    paused_until: Option<f64>,
    excluded_fg: bool,
    presentation: bool,
    away_cur: f32,
    away_target: f32,
    ddc_dimmed: bool,
    display_off_sent: bool,
    display_on: bool,
    display_off_at: Option<f64>,
    panel_on_secs: f64,
    /// Current frame-timer interval in ms (0 = off).
    frame_ms: u32,
    rebuild_pending: bool,
    /// (GDI name, monitor id) pairs seen with a real device path.
    known_ids: Vec<(String, String)>,
    id_retries: u32,
    last_ledger_save: f64,
    last_state_save: f64,
    reminder_shown: bool,
    shut_down: bool,
    /// Last time the cursor was near each chrome band (top, bottom, left, right).
    chrome_reveal: [f64; 4],
    /// Chrome or torch is active: follow the cursor at frame rate.
    interactive: bool,
    last_pointer: (i32, i32, bool),
    /// Chrome bands of the current foreground window (px) and their lit state.
    chrome_zones: Option<[RECT; 4]>,
    chrome_revealed: [bool; 4],
    torch_active: bool,
    panel: PanelTracker,
    color: ColorState,
    /// Spooky mode's animations, while it and torch are on.
    spook: Option<spook::Spook>,
}

/// Full-screen color matrix for deeper blacks.
struct ColorState {
    fx: Option<color::ColorEffect>,
    failed: bool,
    fader: color::Fader,
    target: color::Matrix,
}

/// Panel torch mode: what is lit and how we learn about it.
#[derive(Default)]
struct PanelTracker {
    worker: Option<panels::PanelWorker>,
    /// Raw keyboard input registered (only while panel mode is active).
    keyboard_sink: bool,
    /// A key event arrived since the last frame (content is never read).
    key_pending: bool,
    typing: bool,
    last_input_tick: u32,
    last_ptr: (i32, i32),
    /// Panel under the pointer and input area around the focus (screen px).
    mouse_rect: Option<RECT>,
    focus_rect: Option<RECT>,
    last_point_query: f64,
    last_query_ptr: (i32, i32),
    last_focus_query: f64,
    retry_at: Option<f64>,
    /// Scheduled fresh lookups after something may have changed the layout
    /// (a click, a shortcut, a large redraw), even if the pointer hasn't moved.
    requery_at: Vec<f64>,
    last_layout_requery: f64,
    /// Retries left for an app that hasn't exposed its layout yet.
    retries_left: u32,
    /// Input without pointer movement or a key event yet (a click or scroll,
    /// or a keystroke whose raw-input message hasn't arrived yet).
    unexplained_input_at: Option<f64>,
}

impl Agent {
    fn new(hwnd: HWND, opts: Options) -> Agent {
        let now = util::now();
        let cfg = Config::load();
        let persisted: PersistedState = std::fs::read_to_string(state_path())
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        // If we were not running for longer than a rest period (e.g. PC was off
        // overnight), the panel has had its rest.
        let rest = cfg.refresh.rest_minutes as u64 * 60;
        let panel_on_secs = if util::unix_time().saturating_sub(persisted.saved_at) >= rest {
            0.0
        } else {
            persisted.panel_on_secs
        };
        let mut a = Agent {
            hwnd,
            opts,
            cfg_mtime: Config::modified(),
            cfg,
            screens: Vec::new(),
            overlay_gpu: None,
            tray: tray::Tray::new(hwnd),
            ddc: ddc::DdcWorker::start(),
            audio: power::AudioMeter::new(),
            procs: winmap::ProcessNames::default(),
            snap: winmap::Snapshot::default(),
            high_risk: Vec::new(),
            own_pid: unsafe { GetCurrentProcessId() },
            fg_name: String::new(),
            last_tick: now,
            last_anim: now,
            paused_until: None,
            excluded_fg: false,
            presentation: false,
            away_cur: 0.0,
            away_target: 0.0,
            ddc_dimmed: false,
            display_off_sent: false,
            display_on: true,
            display_off_at: None,
            panel_on_secs,
            frame_ms: 0,
            rebuild_pending: false,
            known_ids: Vec::new(),
            id_retries: ID_RETRIES,
            last_ledger_save: now,
            last_state_save: now,
            reminder_shown: false,
            shut_down: false,
            chrome_reveal: [f64::NEG_INFINITY; 4],
            interactive: false,
            last_pointer: (0, 0, false),
            chrome_zones: None,
            chrome_revealed: [false; 4],
            torch_active: false,
            panel: PanelTracker::default(),
            color: ColorState { fx: None, failed: false, fader: color::Fader::default(), target: color::IDENTITY },
            spook: None,
        };
        a.rebuild();
        a.register_hotkey();
        a.ddc.send(ddc::Cmd::Probe);
        a
    }

    fn rebuild(&mut self) {
        self.rebuild_pending = false;
        self.save_ledgers();
        self.screens.clear();
        let now = util::now();
        if let Some(k) = &mut self.spook {
            k.cancel(now);
        }
        let mut unresolved = false;
        match capture::enumerate() {
            Ok(e) => {
                self.overlay_gpu = e.gpus.first().cloned().or_else(|| Gpu::new(None, None).ok().map(Rc::new));
                for mut d in e.displays {
                    // Right after the displays wake, Windows can report a monitor without
                    // its device path, which gives it a fallback id and a separate ledger.
                    // Reuse the id seen before, or wait a few seconds for the real one.
                    if capture::is_fallback_id(&d.id) {
                        if let Some((_, id)) = self.known_ids.iter().find(|(g, _)| *g == d.gdi_name) {
                            d.id = id.clone();
                        } else if self.id_retries > 0 {
                            unresolved = true;
                            continue;
                        }
                    } else {
                        self.known_ids.retain(|(g, _)| *g != d.gdi_name);
                        self.known_ids.push((d.gdi_name.clone(), d.id.clone()));
                    }
                    let len = d.geom.len();
                    log!(
                        "display {} \"{}\" {} {}x{} at ({},{}) hdr={} sdr_white={:.2} grid={}x{}",
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
                        d.geom.gh
                    );
                    let ledger = Ledger::load_or_new(&d.id, &d.name, d.geom.gw, d.geom.gh);
                    self.screens.push(Screen {
                        model: model::Model::new(len, now),
                        cur: vec![0.0; len],
                        target: vec![0.0; len],
                        motion: vec![model::Motion::default(); len],
                        owners: Vec::new(),
                        composed: vec![0.0; len],
                        composed_away: 0.0,
                        overlay: None,
                        overlay_failed: false,
                        ledger,
                        last_attention: now,
                        fullscreen: false,
                        enabled: true,
                        d,
                    });
                }
            }
            Err(e) => log!("display enumeration failed: {}", e.message()),
        }
        if unresolved {
            self.id_retries -= 1;
            self.rebuild_pending = true;
            log!("display without a device path; looking again ({} tries left)", self.id_retries);
        } else {
            self.id_retries = ID_RETRIES;
        }
        // Remember monitors in the config so the settings window can list them.
        let mut changed = false;
        for s in &self.screens {
            let entry = self.cfg.monitors.entry(s.d.id.clone()).or_insert_with(|| {
                changed = true;
                MonitorPrefs { enabled: true, name: s.d.name.clone() }
            });
            if entry.name != s.d.name {
                entry.name = s.d.name.clone();
                changed = true;
            }
        }
        if changed {
            let _ = self.cfg.save();
            self.cfg_mtime = Config::modified();
        }
    }

    fn is_paused(&self, now: f64) -> bool {
        self.paused_until.is_some_and(|t| now < t)
    }

    fn register_hotkey(&mut self) {
        for (id, text) in [
            (HOTKEY_ID, self.cfg.hotkey.clone()),
            (HOTKEY_TORCH, self.cfg.torch.hotkey.clone()),
            (HOTKEY_BLACKS, self.cfg.blacks.hotkey.clone()),
        ] {
            unsafe {
                let _ = UnregisterHotKey(Some(self.hwnd), id);
            }
            if text.trim().is_empty() {
                continue;
            }
            match parse_hotkey(&text) {
                Some((mods, vk)) => {
                    if let Err(e) = unsafe { RegisterHotKey(Some(self.hwnd), id, mods | MOD_NOREPEAT, vk) } {
                        log!("hotkey {} unavailable: {}", text, e.message());
                    }
                }
                None => log!("hotkey \"{}\" not understood", text),
            }
        }
    }

    fn reload_config(&mut self) {
        let new = Config::load();
        if new == self.cfg {
            return;
        }
        log!("config reloaded");
        let hotkey_changed = new.hotkey != self.cfg.hotkey
            || new.torch.hotkey != self.cfg.torch.hotkey
            || new.blacks.hotkey != self.cfg.blacks.hotkey;
        if new.blacks.enabled != self.cfg.blacks.enabled {
            self.color.failed = false;
        }
        self.cfg = new;
        if hotkey_changed {
            self.register_hotkey();
        }
        self.sync_panel_mode();
        if !self.cfg.ddc.enabled && self.ddc_dimmed {
            self.ddc.send(ddc::Cmd::Restore);
            self.ddc_dimmed = false;
        }
        let now = util::now();
        self.refresh_windows(now);
        self.compute_targets(now);
        self.kick_animation();
    }

    fn tick(&mut self) {
        let now = util::now();
        let mut dt = (now - self.last_tick) as f32;
        self.last_tick = now;
        if !(0.0..=5.0).contains(&dt) {
            // Resumed from sleep or the loop stalled: the gap is not static time.
            dt = 0.0;
            for s in &mut self.screens {
                s.d.capture.reset();
            }
        }
        let mtime = Config::modified();
        if mtime != self.cfg_mtime {
            self.cfg_mtime = mtime;
            self.reload_config();
        }
        if self.rebuild_pending {
            self.rebuild();
        }
        if self.paused_until.is_some_and(|t| now >= t) {
            self.paused_until = None;
            log!("resumed after timed pause");
        }

        let idle = power::idle_secs();
        self.audio.poll(now);
        self.refresh_windows(now);
        self.presentation =
            matches!(unsafe { SHQueryUserNotificationState() }, Ok(s) if s == QUNS_PRESENTATION_MODE);
        self.update_attention(now);
        self.sync_panel_mode();

        if self.display_on {
            for s in &mut self.screens {
                let sample = s.d.sample(now);
                s.model.apply(&sample, dt, now);
            }
            self.panel_on_secs += dt as f64;
        }

        if self.panel_mode_active(now)
            && now - self.panel.last_layout_requery >= 1.0
            && self.screens.iter().any(|s| s.model.last_change_fraction >= 0.08)
        {
            self.panel.last_layout_requery = now;
            self.panel.requery_at.push(now);
            if util::debug_enabled() {
                log!("panels: large redraw, looking up panels again");
            }
        }
        self.update_away(now, idle);
        self.compute_targets(now);
        let animating = self.animate() | self.drive_cat(now);
        self.accumulate_ledgers(dt);
        self.update_frame_timer(animating);
        for s in &self.screens {
            if let Some(o) = &s.overlay {
                o.keep_on_top();
            }
        }
        if now - self.last_ledger_save >= 600.0 {
            self.save_ledgers();
        }
        if now - self.last_state_save >= 300.0 {
            self.save_state();
        }
        self.maybe_remind();
        self.update_tray(now);
    }

    fn fast_tick(&mut self) {
        if self.needs_input_watch() && power::idle_secs() < 0.5 {
            self.release_away("input");
        }
        if self.interactive {
            // Cheap per-frame check; the full policy only reruns when something visible flips.
            let now = util::now();
            let (p, alt) = pointer();
            let moved = (p.x, p.y, alt) != self.last_pointer;
            let mut recompute = self.torch_active && (moved || self.spook.as_ref().is_some_and(|k| k.flickering(now)));
            if let Some(zones) = &self.chrome_zones {
                let flags = reveal_flags(zones, p, alt, now, &mut self.chrome_reveal, &self.cfg.chrome);
                recompute |= flags != self.chrome_revealed;
            }
            if self.panel_mode_active(now) {
                recompute |= self.track_panel_input(now, p);
            }
            if recompute {
                self.compute_targets(now);
            }
        }
        let animating = self.animate() | self.drive_cat(util::now());
        self.update_frame_timer(animating);
    }

    fn needs_input_watch(&self) -> bool {
        self.away_target > 0.0 || self.away_cur > 0.0 || self.ddc_dimmed
    }

    /// Frame timer: ~60 fps while something animates, ~30 fps while only
    /// watching the pointer or input, off otherwise.
    fn update_frame_timer(&mut self, animating: bool) {
        let ms = if animating {
            FAST_MS
        } else if self.needs_input_watch() || self.interactive {
            WATCH_MS
        } else {
            0
        };
        if ms == self.frame_ms {
            return;
        }
        self.frame_ms = ms;
        unsafe {
            if ms > 0 {
                SetTimer(Some(self.hwnd), TIMER_FAST, ms, None);
            } else {
                let _ = KillTimer(Some(self.hwnd), TIMER_FAST);
            }
        }
    }

    fn kick_animation(&mut self) {
        let animating = self.animate();
        self.update_frame_timer(animating);
    }

    fn refresh_windows(&mut self, now: f64) {
        self.snap = winmap::snapshot(self.own_pid);
        self.fg_name = self.procs.get(self.snap.fg_pid, now);
        self.excluded_fg = !self.fg_name.is_empty() && self.cfg.apps.excluded.contains(&self.fg_name);
        let (procs, high_risk_apps) = (&mut self.procs, &self.cfg.apps.high_risk);
        self.high_risk = if high_risk_apps.is_empty() {
            vec![false; self.snap.wins.len()]
        } else {
            self.snap.wins.iter().map(|w| high_risk_apps.contains(&procs.get(w.pid, now))).collect()
        };
        for s in &mut self.screens {
            winmap::paint(&self.snap, &s.d.rect, &s.d.geom, &mut s.owners);
            let m = s.d.rect;
            s.fullscreen = self
                .snap
                .fg_rect
                .is_some_and(|r| r.left <= m.left && r.top <= m.top && r.right >= m.right && r.bottom >= m.bottom);
            s.enabled = self.cfg.monitor_enabled(&s.d.id);
        }
    }

    fn update_attention(&mut self, now: f64) {
        let mut cursor = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut cursor);
        }
        let fg_center = self.snap.fg_rect.map(|r| POINT { x: (r.left + r.right) / 2, y: (r.top + r.bottom) / 2 });
        let inside = |r: &RECT, p: &POINT| p.x >= r.left && p.x < r.right && p.y >= r.top && p.y < r.bottom;
        for s in &mut self.screens {
            if inside(&s.d.rect, &cursor) || fg_center.is_some_and(|c| inside(&s.d.rect, &c)) {
                s.last_attention = now;
            }
        }
    }

    fn compute_targets(&mut self, now: f64) {
        let paused = self.is_paused(now);
        let multi = self.screens.len() > 1;
        let (cursor, alt) = pointer();
        self.last_pointer = (cursor.x, cursor.y, alt);
        let blocked = !self.cfg.enabled || paused || self.excluded_fg || self.presentation;
        let mut interactive = false;
        self.chrome_zones = None;
        self.torch_active = false;
        let torch_on = self.cfg.torch.enabled;
        self.start_spook(now, cursor, blocked);
        for (i, s) in self.screens.iter_mut().enumerate() {
            let suspended = blocked || !s.enabled || (s.fullscreen && !self.cfg.dim_fullscreen_apps);
            let neglected =
                multi && now - s.last_attention >= self.cfg.static_dimming.neglected_after_secs as f64;
            // Torch mode replaces chrome dimming while it is on.
            let chrome = if suspended || !self.cfg.chrome.enabled || torch_on {
                None
            } else {
                chrome_ctx(&self.cfg.chrome, &self.snap, s, cursor, alt, now, &mut self.chrome_reveal)
            };
            if let Some((ctx, zones)) = &chrome {
                self.chrome_zones = Some(*zones);
                self.chrome_revealed = ctx.revealed;
            }
            let chrome = chrome.map(|(ctx, _)| ctx);
            let torch = if suspended || !torch_on {
                None
            } else {
                let t = &self.cfg.torch;
                let cell = capture::CELL as f32;
                let to_cells = |x: i32, y: i32| {
                    ((x - s.d.rect.left - s.d.geom.ox) as f32 / cell, (y - s.d.rect.top - s.d.geom.oy) as f32 / cell)
                };
                let (cx, cy) = to_cells(cursor.x, cursor.y);
                let halo = |px: f32| Some((cx, cy, px / cell, px * 0.5 / cell));
                let (lit_foreground, lit_rect, halo) = match t.mode {
                    TorchMode::Window => (true, None, halo(TORCH_HALO_PX)),
                    TorchMode::Spotlight => (false, None, halo(t.spotlight_radius_px as f32)),
                    TorchMode::Panel => {
                        let rect = if self.panel.typing { self.panel.focus_rect } else { self.panel.mouse_rect };
                        let lit_rect = rect.map(|r| {
                            // Half a cell of margin keeps the panel's own edge fully lit.
                            let (x0, y0) = to_cells(r.left, r.top);
                            let (x1, y1) = to_cells(r.right, r.bottom);
                            [x0 - 0.5, y0 - 0.5, x1 + 0.5, y1 + 0.5]
                        });
                        let halo = if self.panel.typing { None } else { halo(PANEL_HALO_PX) };
                        // Until an app answers (or if it can't), light its whole window.
                        (rect.is_none(), lit_rect, halo)
                    }
                };
                let dim = if self.spook.is_some() { spook::DIM } else { t.dim };
                let (flicker, snap) = self.spook.as_ref().map_or((1.0, false), |k| k.flicker(now, i));
                Some(model::TorchCtx { dim, lit_foreground, lit_rect, halo, flicker, snap })
            };
            self.torch_active |= torch.is_some();
            interactive |= chrome.is_some() || torch.is_some();
            let ctx = model::PolicyCtx {
                cfg: &self.cfg,
                snap: &self.snap,
                owners: &s.owners,
                gw: s.d.geom.gw,
                high_risk: &self.high_risk,
                neglected,
                suspended,
                chrome,
                torch,
            };
            model::targets(&s.model, &ctx, &mut s.target, &mut s.motion);
        }
        self.interactive = interactive;
        self.color.target = if self.cfg.blacks.enabled && !blocked {
            color::crush(color::srgb_to_linear(self.cfg.blacks.level))
        } else {
            color::IDENTITY
        };
    }


    /// Steps the color matrix; holds the Magnification API only while it isn't identity.
    fn animate_color(&mut self, dt: f32) -> bool {
        let (m, moving) = self.color.fader.step(&self.color.target, dt);
        if m == color::IDENTITY && !moving {
            self.color.fx = None;
            return false;
        }
        if self.color.fx.is_none() && !self.color.failed {
            self.color.fx = color::ColorEffect::new();
            if self.color.fx.is_none() {
                log!("color: Magnification API unavailable");
                self.color.failed = true;
            }
        }
        if let Some(fx) = &mut self.color.fx
            && !fx.set(&m)
        {
            log!("color: setting the color effect failed (Magnifier or Color filters in use?)");
            self.color.fx = None;
            self.color.failed = true;
        }
        moving && !self.color.failed
    }

    fn update_away(&mut self, now: f64, idle: f64) {
        let a = self.cfg.away.clone();
        let blocked = !self.cfg.enabled || self.is_paused(now) || self.excluded_fg || self.presentation;
        let capturing: Vec<&Screen> = self.screens.iter().filter(|s| s.model.capturing).collect();
        // Without capture (e.g. protected content) fall back to input idleness alone.
        let static_for = if capturing.is_empty() {
            idle
        } else {
            now - capturing.iter().map(|s| s.model.last_activity).fold(f64::NEG_INFINITY, f64::max)
        };
        let away = !blocked && a.enabled && idle >= a.dim_after_secs as f64 && static_for >= 30.0;
        if away {
            if self.away_target == 0.0 {
                log!("away: resting the screen (idle {idle:.0}s, static {static_for:.0}s)");
            }
            self.away_target = a.dim_amount;
        } else if self.away_target > 0.0 || self.away_cur > 0.0 {
            self.release_away(if idle < a.dim_after_secs as f64 { "input" } else { "screen activity" });
        }
        if away && self.cfg.ddc.enabled && !self.ddc_dimmed && self.away_cur >= self.away_target - 1e-3 {
            self.ddc.send(ddc::Cmd::Dim(self.cfg.ddc.away_brightness_percent));
            self.ddc_dimmed = true;
        }
        let off_due = a.display_off_after_secs > 0 && idle >= a.display_off_after_secs as f64 && static_for >= 60.0;
        let refresh_due = self.cfg.refresh.enabled
            && self.panel_on_secs >= self.cfg.refresh.hours_between as f64 * 3600.0
            && idle >= 180.0
            && static_for >= 60.0;
        let audio_block = a.respect_audio && self.audio.playing(now);
        if !blocked && (off_due || refresh_due) && !audio_block && !self.display_off_sent && self.display_on {
            log!("power: turning displays off ({})", if off_due { "away" } else { "pixel-refresh rest" });
            self.display_off_sent = true;
            power::displays_off(self.hwnd);
        }
        if idle < 2.0 {
            self.display_off_sent = false;
        }
    }

    fn release_away(&mut self, reason: &str) {
        if self.away_target > 0.0 || self.away_cur > 0.0 {
            log!("away: released ({reason})");
        }
        self.away_target = 0.0;
        self.away_cur = 0.0;
        if self.ddc_dimmed {
            self.ddc.send(ddc::Cmd::Restore);
            self.ddc_dimmed = false;
        }
    }

    /// Advances all ramps and pushes changed masks to the overlays.
    fn animate(&mut self) -> bool {
        let now = util::now();
        let dt = ((now - self.last_anim) as f32).clamp(0.0, 1.5);
        self.last_anim = now;
        let away_rate = self.cfg.away.dim_amount / self.cfg.away.fade_secs.max(1.0);
        if self.away_target > self.away_cur {
            self.away_cur = (self.away_cur + away_rate * dt).min(self.away_target);
        } else {
            self.away_cur = self.away_target;
        }
        let up = self.cfg.static_dimming.fade_in_percent_per_minute / 6000.0;
        // Time constants: ~95 % of the way after 3 tau.
        let easing = model::Easing { release: 0.06, chrome_in: self.cfg.chrome.fade_secs / 3.0, torch_in: 0.12 };
        let mut animating = self.away_cur < self.away_target;
        animating |= self.animate_color(dt);
        for s in &mut self.screens {
            let (changed, easing_now) = model::ramp(&mut s.cur, &s.target, &s.motion, dt, up, &easing);
            animating |= easing_now;
            let away = if s.enabled { self.away_cur } else { 0.0 };
            if !changed && away == s.composed_away && s.composed.len() == s.cur.len() {
                continue;
            }
            s.composed_away = away;
            model::compose(&s.cur, s.d.geom.gw, s.d.geom.gh, away, &mut s.composed);
            let any = s.composed.iter().any(|&a| a >= 0.5 / 255.0);
            if any && s.overlay.is_none() && !s.overlay_failed
                && let Some(gpu) = &self.overlay_gpu {
                    match overlay::Overlay::new(gpu, s.d.rect, s.d.geom, self.opts.exclude_from_capture) {
                        Ok(o) => s.overlay = Some(o),
                        Err(e) => {
                            log!("overlay: cannot create for {}: {}", s.d.name, e.message());
                            s.overlay_failed = true;
                        }
                    }
                }
            if let Some(o) = &mut s.overlay
                && let Err(e) = o.show(&s.composed) {
                    log!("overlay: update failed for {}: {}", s.d.name, e.message());
                    s.overlay = None;
                }
        }
        animating
    }

    fn accumulate_ledgers(&mut self, dt: f32) {
        if !self.display_on || dt <= 0.0 {
            return;
        }
        for s in &mut self.screens {
            if !s.model.capturing {
                continue;
            }
            s.ledger.seconds += dt as f64;
            for (i, c) in s.model.cells.iter().enumerate() {
                if !c.has_data {
                    continue;
                }
                let a = s.composed.get(i).copied().unwrap_or(0.0);
                s.ledger.emitted[i] += c.mean * (1.0 - a) * dt;
                s.ledger.avoided[i] += c.mean * a * dt;
            }
        }
    }

    fn save_ledgers(&mut self) {
        self.last_ledger_save = util::now();
        for s in &self.screens {
            if let Err(e) = s.ledger.save(&s.d.id) {
                log!("ledger: save failed for {}: {e}", s.d.id);
            }
        }
    }

    fn save_state(&mut self) {
        self.last_state_save = util::now();
        let st = PersistedState { panel_on_secs: self.panel_on_secs, saved_at: util::unix_time() };
        if let Ok(text) = serde_json::to_string(&st) {
            let _ = util::write_atomic(&state_path(), text.as_bytes());
        }
    }

    fn maybe_remind(&mut self) {
        let r = &self.cfg.refresh;
        if r.enabled && r.remind && !self.reminder_shown && self.panel_on_secs >= r.hours_between as f64 * 2.0 * 3600.0 {
            self.reminder_shown = true;
            self.tray.balloon(
                "Time to rest the screen",
                &format!(
                    "Your display has been on for {:.0} hours without a rest. Stepping away for {} minutes lets the panel run its pixel refresh.",
                    self.panel_on_secs / 3600.0,
                    r.rest_minutes
                ),
            );
        }
    }

    fn on_display_state(&mut self, state: u8) {
        let now = util::now();
        if state == 0 {
            if self.display_on {
                self.display_on = false;
                self.display_off_at = Some(now);
                log!("power: displays off");
            }
        } else if !self.display_on {
            self.display_on = true;
            if let Some(t) = self.display_off_at.take() {
                let off = now - t;
                log!("power: displays on after {:.1} min", off / 60.0);
                if off >= self.cfg.refresh.rest_minutes as f64 * 60.0 {
                    self.panel_on_secs = 0.0;
                    self.reminder_shown = false;
                }
            }
            for s in &mut self.screens {
                s.d.capture.reset();
            }
        }
    }

    fn on_focus_change(&mut self) {
        let now = util::now();
        // Layout or focus moved: forget cached panel rects so they are looked up again.
        self.panel.last_point_query = f64::NEG_INFINITY;
        self.panel.last_focus_query = f64::NEG_INFINITY;
        self.panel.focus_rect = None;
        self.refresh_windows(now);
        self.compute_targets(now);
        self.kick_animation();
    }

    fn pause(&mut self, secs: Option<f64>) {
        let now = util::now();
        self.paused_until = Some(secs.map(|s| now + s).unwrap_or(f64::INFINITY));
        log!("paused ({})", secs.map(|s| format!("{:.0} min", s / 60.0)).unwrap_or("until resumed".into()));
        self.release_away("paused");
        self.compute_targets(now);
        self.kick_animation();
        self.update_tray(now);
    }

    fn resume(&mut self) {
        let now = util::now();
        self.paused_until = None;
        log!("resumed");
        self.compute_targets(now);
        self.kick_animation();
        self.update_tray(now);
    }

    fn panel_mode_active(&self, now: f64) -> bool {
        self.cfg.enabled
            && self.cfg.torch.enabled
            && self.cfg.torch.mode == TorchMode::Panel
            && !self.is_paused(now)
            && !self.excluded_fg
            && !self.presentation
    }

    /// Starts/stops the UI Automation worker and the keyboard-activity sink.
    fn sync_panel_mode(&mut self) {
        let on = self.panel_mode_active(util::now());
        if on && self.panel.worker.is_none() {
            self.panel.worker = Some(panels::PanelWorker::start(self.hwnd.0 as isize, WM_APP_PANEL));
        }
        if on != self.panel.keyboard_sink {
            let dev = RAWINPUTDEVICE {
                usUsagePage: 0x01,
                usUsage: 0x06,
                dwFlags: if on { RIDEV_INPUTSINK } else { RIDEV_REMOVE },
                hwndTarget: if on { self.hwnd } else { HWND::default() },
            };
            match unsafe { RegisterRawInputDevices(&[dev], std::mem::size_of::<RAWINPUTDEVICE>() as u32) } {
                Ok(()) => self.panel.keyboard_sink = on,
                Err(e) => log!("panels: keyboard activity sink: {}", e.message()),
            }
        }
        if !on {
            self.panel.typing = false;
            self.panel.mouse_rect = None;
            self.panel.focus_rect = None;
        }
    }

    /// Follows pointer vs keyboard activity and asks for fresh panel rects.
    /// Returns true when what should be lit changed.
    fn track_panel_input(&mut self, now: f64, p: POINT) -> bool {
        let pt = &mut self.panel;
        let before = pt.typing;
        let moved = (p.x - pt.last_ptr.0).abs() + (p.y - pt.last_ptr.1).abs() >= 3;
        if moved {
            pt.last_ptr = (p.x, p.y);
        }
        let tick = power::last_input_tick();
        let other_input = tick != pt.last_input_tick;
        pt.last_input_tick = tick;
        if std::mem::take(&mut pt.key_pending) {
            pt.typing = true;
            pt.unexplained_input_at = None;
            pt.requery_at = vec![now + 0.15, now + 0.7];
            if now - pt.last_focus_query > 0.25 {
                pt.retries_left = pt.retries_left.max(5);
                pt.last_focus_query = now;
                if let Some(w) = &pt.worker {
                    w.ask(panels::Query::Focus);
                }
            }
        } else if moved {
            if pt.typing {
                // Back from typing: the layout may have changed meanwhile.
                pt.requery_at.push(now);
            }
            pt.typing = false;
            pt.unexplained_input_at = None;
        } else if other_input {
            pt.unexplained_input_at.get_or_insert(now);
        }
        // Clicked or scrolled (no key followed within 50 ms): follow the pointer again.
        if pt.unexplained_input_at.is_some_and(|t| now - t >= 0.05) {
            pt.unexplained_input_at = None;
            pt.typing = false;
            pt.requery_at = vec![now + 0.15, now + 0.7];
        }
        let before_len = pt.requery_at.len();
        pt.requery_at.retain(|&t| t > now);
        let force = pt.requery_at.len() != before_len;
        if !pt.typing {
            let inside = pt.mouse_rect.is_some_and(|r| p.x >= r.left && p.x < r.right && p.y >= r.top && p.y < r.bottom);
            let new_spot = (p.x, p.y) != pt.last_query_ptr;
            // Window events already trigger a fresh lookup; the slow refresh only catches
            // layout changes inside an app (a pane opened or resized).
            let due = force || (!inside && new_spot && now - pt.last_point_query > 0.04) || now - pt.last_point_query > 4.0;
            if due {
                if new_spot {
                    pt.retries_left = 5;
                }
                pt.last_point_query = now;
                pt.last_query_ptr = (p.x, p.y);
                if let Some(w) = &pt.worker {
                    w.ask(panels::Query::Point(p.x, p.y));
                }
            }
        } else if force || now - pt.last_focus_query > 1.0 {
            // The text box can grow as you type.
            pt.last_focus_query = now;
            if let Some(w) = &pt.worker {
                w.ask(panels::Query::Focus);
            }
        }
        if pt.retry_at.is_some_and(|t| now >= t) {
            pt.retry_at = None;
            pt.last_point_query = f64::NEG_INFINITY;
            pt.last_focus_query = f64::NEG_INFINITY;
        }
        if pt.typing != before && util::debug_enabled() {
            log!("panels: {}", if pt.typing { "typing" } else { "pointer" });
        }
        pt.typing != before
    }

    fn on_panel_answers(&mut self) {
        let Some(w) = &self.panel.worker else { return };
        let now = util::now();
        let mut changed = false;
        for a in w.take_answers() {
            if util::debug_enabled() {
                log!("panels: answer {:?} rect={:?} retry={}", a.query, a.rect.map(|r| (r.left, r.top, r.right, r.bottom)), a.retry);
            }
            if a.retry && self.panel.retries_left > 0 {
                // The app may still be building its accessibility tree; ask again soon.
                self.panel.retries_left -= 1;
                self.panel.retry_at = Some(now + 0.4);
            }
            let slot = match a.query {
                panels::Query::Point(..) => &mut self.panel.mouse_rect,
                panels::Query::Focus => &mut self.panel.focus_rect,
            };
            if *slot != a.rect {
                if util::debug_enabled() {
                    let what = match a.query { panels::Query::Point(..) => "pointer panel", panels::Query::Focus => "input area" };
                    match a.rect {
                        Some(r) => log!("panels: {what} {}x{} at ({},{})", r.right - r.left, r.bottom - r.top, r.left, r.top),
                        None => log!("panels: {what} unknown (whole window)"),
                    }
                }
                *slot = a.rect;
                changed = true;
            }
        }
        if changed {
            self.compute_targets(now);
            self.kick_animation();
        }
    }

    fn toggle_pause(&mut self) {
        if self.is_paused(util::now()) { self.resume() } else { self.pause(None) }
    }

    fn toggle_torch(&mut self) {
        self.cfg.torch.enabled = !self.cfg.torch.enabled;
        log!("torch mode {}", if self.cfg.torch.enabled { "on" } else { "off" });
        if let Err(e) = self.cfg.save() {
            log!("config: save failed: {e}");
        }
        self.cfg_mtime = Config::modified();
        self.sync_panel_mode();
        let now = util::now();
        self.compute_targets(now);
        self.kick_animation();
        self.update_tray(now);
    }

    /// Keeps spooky mode in step with the settings and starts the next show
    /// on the screen under the pointer when one is due.
    fn start_spook(&mut self, now: f64, cursor: POINT, blocked: bool) {
        let want = self.cfg.torch.enabled && self.cfg.torch.spooky;
        if want != self.spook.is_some() {
            self.spook = want.then(|| spook::Spook::new(now));
        }
        let Some(k) = self.spook.as_mut() else { return };
        if blocked {
            k.cancel(now);
            return;
        }
        if !k.due(now) {
            return;
        }
        let inside = |r: &RECT| cursor.x >= r.left && cursor.x < r.right && cursor.y >= r.top && cursor.y < r.bottom;
        if let Some(i) = self.screens.iter().position(|s| inside(&s.d.rect) && s.enabled) {
            k.start(now, i);
        }
    }

    /// Moves spooky mode's cat and fireflies and hands them to the overlays.
    /// True while anything is on screen.
    fn drive_cat(&mut self, now: f64) -> bool {
        let (cursor, _) = pointer();
        let mut shown = false;
        for (i, s) in self.screens.iter_mut().enumerate() {
            let scene = self.spook.as_mut().filter(|_| s.enabled).and_then(|k| {
                let env = critter::Env {
                    mon: s.d.rect,
                    geom: s.d.geom,
                    target: &s.target,
                    dim: spook::DIM,
                    pointer: (cursor.x as f32, cursor.y as f32),
                };
                k.cat(now, i, &env)
            });
            shown |= scene.is_some();
            if let Some(o) = &mut s.overlay
                && let Err(e) = o.set_scene(&scene.unwrap_or_default())
            {
                log!("overlay: cat update failed for {}: {}", s.d.name, e.message());
            }
        }
        shown
    }

    fn toggle_blacks(&mut self) {
        self.cfg.blacks.enabled = !self.cfg.blacks.enabled;
        self.color.failed = false;
        log!("deeper blacks {}", if self.cfg.blacks.enabled { "on" } else { "off" });
        if let Err(e) = self.cfg.save() {
            log!("config: save failed: {e}");
        }
        self.cfg_mtime = Config::modified();
        let now = util::now();
        self.compute_targets(now);
        self.kick_animation();
        self.update_tray(now);
    }

    fn state_label(&self, now: f64) -> String {
        if !self.cfg.enabled {
            return "Protection is off".into();
        }
        if let Some(t) = self.paused_until.filter(|&t| now < t) {
            return if t.is_finite() {
                format!("Paused · resumes in {} min", ((t - now) / 60.0).ceil() as u64)
            } else {
                "Paused".into()
            };
        }
        if self.excluded_fg {
            return format!("Standing by for {}", self.fg_name);
        }
        if self.away_target > 0.0 {
            return "Resting the screen".into();
        }
        let n = self.screens.iter().filter(|s| s.enabled).count();
        let torch = if self.spook.is_some() {
            " · you are not alone"
        } else if self.cfg.torch.enabled {
            " · torch mode"
        } else {
            ""
        };
        let blacks = if self.cfg.blacks.enabled { " · deeper blacks" } else { "" };
        format!("Protecting {} display{}{torch}{blacks}", n, if n == 1 { "" } else { "s" })
    }

    fn update_tray(&mut self, now: f64) {
        let label = self.state_label(now);
        let paused = self.is_paused(now) || !self.cfg.enabled;
        self.tray.update(paused, &format!("Wanelight · {label}"));
    }

    fn on_menu(&mut self, cmd: u32) {
        match cmd {
            tray::ID_OPEN => crate::ui::launch("overview"),
            tray::ID_HEATMAP => crate::ui::launch("heatmap"),
            tray::ID_PAUSE_HOUR => self.pause(Some(3600.0)),
            tray::ID_PAUSE => self.pause(None),
            tray::ID_RESUME => self.resume(),
            tray::ID_TORCH => self.toggle_torch(),
            tray::ID_BLACKS => self.toggle_blacks(),
            tray::ID_EXIT => unsafe {
                let _ = PostMessageW(Some(self.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
            },
            _ => {}
        }
    }

    fn on_command(&mut self, cmd: usize, arg: isize) {
        match cmd {
            ipc::CMD_WRITE_STATUS => self.write_status(),
            ipc::CMD_FLUSH_LEDGER => self.save_ledgers(),
            ipc::CMD_PAUSE => self.pause((arg > 0).then_some(arg as f64 * 60.0)),
            ipc::CMD_RESUME => self.resume(),
            ipc::CMD_RELOAD_CONFIG => self.reload_config(),
            ipc::CMD_QUIT => unsafe {
                let _ = PostMessageW(Some(self.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
            },
            ipc::CMD_RESET_LEDGER => {
                for s in &mut self.screens {
                    s.ledger.reset();
                }
                self.save_ledgers();
                log!("ledger: reset by user");
            }
            _ => {}
        }
    }

    fn write_status(&mut self) {
        let now = util::now();
        let support = self.ddc.support.lock().map(|m| m.clone()).unwrap_or_default();
        let st = ipc::Status {
            written_at: util::unix_time(),
            version: env!("CARGO_PKG_VERSION").into(),
            enabled: self.cfg.enabled,
            paused: self.is_paused(now),
            pause_remaining_secs: self
                .paused_until
                .filter(|t| t.is_finite() && *t > now)
                .map(|t| (t - now) as u64),
            state: self.state_label(now),
            idle_secs: power::idle_secs() as u64,
            panel_hours_since_rest: (self.panel_on_secs / 3600.0) as f32,
            monitors: self
                .screens
                .iter()
                .map(|s| {
                    let n = s.composed.len().max(1) as f32;
                    ipc::MonitorStatus {
                        id: s.d.id.clone(),
                        name: s.d.name.clone(),
                        width: s.d.width(),
                        height: s.d.height(),
                        hdr: s.d.hdr,
                        capturing: s.model.capturing,
                        enabled: s.enabled,
                        dimmed_fraction: s.composed.iter().filter(|&&a| a > 0.02).count() as f32 / n,
                        max_dim: s.composed.iter().copied().fold(0.0, f32::max),
                        static_fraction: s.model.static_fraction(60.0),
                        ddc_supported: support.get(&s.d.gdi_name).copied(),
                    }
                })
                .collect(),
        };
        if let Ok(text) = serde_json::to_string_pretty(&st) {
            let _ = util::write_atomic(&ipc::status_path(), text.as_bytes());
        }
        if util::debug_enabled() {
            for s in &self.screens {
                let path = util::data_dir().join(format!("mask-{}.bmp", s.d.id));
                let _ = util::write_atomic(&path, &mask_bmp(&s.composed, s.d.geom.gw, s.d.geom.gh, 4));
            }
        }
    }

    fn shutdown(&mut self) {
        if self.shut_down {
            return;
        }
        self.shut_down = true;
        log!("shutting down");
        self.save_ledgers();
        self.save_state();
        for s in &mut self.screens {
            s.overlay = None;
        }
        self.color.fx = None;
        ddc::restore_now();
        self.tray.remove();
    }
}

/// Debug view of a dim mask as a 24-bit BMP: white = untouched, darker = dimmed.
fn mask_bmp(alpha: &[f32], gw: usize, gh: usize, scale: usize) -> Vec<u8> {
    let (w, h) = (gw * scale, gh * scale);
    let row = (w * 3).div_ceil(4) * 4;
    let size = 54 + row * h;
    let mut out = Vec::with_capacity(size);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&(size as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&(h as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&24u16.to_le_bytes());
    out.extend_from_slice(&[0u8; 24]);
    for y in (0..h).rev() {
        let start = out.len();
        for x in 0..w {
            let a = alpha.get((y / scale) * gw + x / scale).copied().unwrap_or(0.0);
            let v = ((1.0 - a.clamp(0.0, 1.0)) * 255.0) as u8;
            out.extend_from_slice(&[v, v, v]);
        }
        out.resize(start + row, 0);
    }
    out
}

/// Cursor position (physical px) and whether Alt is held.
fn pointer() -> (POINT, bool) {
    let mut p = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut p);
    }
    // Test hook: pretend the pointer is elsewhere (debug runs only).
    if util::debug_enabled() {
        if let Some((x, y)) = std::env::var("WANELIGHT_DEBUG_POINTER").ok().and_then(|v| {
            let (x, y) = v.split_once(',')?;
            Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
        }) {
            p = POINT { x, y };
        }
    }
    let alt = unsafe { GetAsyncKeyState(VK_MENU.0 as i32) } as u16 & 0x8000 != 0;
    (p, alt)
}

fn distance_to_rect(p: POINT, r: &RECT) -> f32 {
    let dx = (r.left - p.x).max(0).max(p.x - r.right) as f32;
    let dy = (r.top - p.y).max(0).max(p.y - r.bottom) as f32;
    (dx * dx + dy * dy).sqrt()
}

/// Edge bands of the foreground window if it (nearly) fills this monitor.
/// Updates `reveal` with the time the cursor was last near each band.
fn chrome_ctx(
    c: &crate::config::Chrome,
    snap: &winmap::Snapshot,
    s: &Screen,
    cursor: POINT,
    alt: bool,
    now: f64,
    reveal: &mut [f64; 4],
) -> Option<(model::ChromeCtx, [RECT; 4])> {
    let r = snap.fg_rect?;
    let m = s.d.rect;
    let ix = RECT { left: r.left.max(m.left), top: r.top.max(m.top), right: r.right.min(m.right), bottom: r.bottom.min(m.bottom) };
    let area = |r: &RECT| (r.right - r.left).max(0) as f64 * (r.bottom - r.top).max(0) as f64;
    if area(&ix) < 0.9 * area(&m) {
        return None;
    }
    let (x0, y0, x1, y1) = winmap::cell_range(&ix, &m, &s.d.geom);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    let (w, h) = ((x1 - x0) as f32, (y1 - y0) as f32);
    let depth = [(h * 0.12).ceil() as usize, (h * 0.08).ceil() as usize, (w * 0.20).ceil() as usize, (w * 0.20).ceil() as usize];
    let cell = capture::CELL;
    let px = |cx: usize| m.left + s.d.geom.ox + cell * cx as i32;
    let py = |cy: usize| m.top + s.d.geom.oy + cell * cy as i32;
    let zones = [
        RECT { left: px(x0), top: py(y0), right: px(x1), bottom: py(y0 + depth[0]) },
        RECT { left: px(x0), top: py(y1 - depth[1]), right: px(x1), bottom: py(y1) },
        RECT { left: px(x0), top: py(y0), right: px(x0 + depth[2]), bottom: py(y1) },
        RECT { left: px(x1 - depth[3]), top: py(y0), right: px(x1), bottom: py(y1) },
    ];
    let revealed = reveal_flags(&zones, cursor, alt, now, reveal, c);
    let ctx = model::ChromeCtx { rect: (x0, y0, x1, y1), depth, revealed, dim: c.max_dim, after_secs: c.after_secs as f32 };
    Some((ctx, zones))
}

/// Which chrome bands are lit: the cursor is near them, was recently, or Alt is held.
fn reveal_flags(
    zones: &[RECT; 4],
    cursor: POINT,
    alt: bool,
    now: f64,
    reveal: &mut [f64; 4],
    c: &crate::config::Chrome,
) -> [bool; 4] {
    let mut revealed = [alt; 4];
    for (z, zr) in zones.iter().enumerate() {
        if distance_to_rect(cursor, zr) <= c.reveal_px as f32 {
            reveal[z] = now;
        }
        revealed[z] |= now - reveal[z] < c.hold_secs as f64;
    }
    revealed
}

fn parse_hotkey(s: &str) -> Option<(HOT_KEY_MODIFIERS, u32)> {
    let mut mods = HOT_KEY_MODIFIERS(0);
    let mut vk = None;
    for part in s.split('+').map(|p| p.trim().to_ascii_lowercase()) {
        match part.as_str() {
            "ctrl" | "control" => mods |= MOD_CONTROL,
            "alt" => mods |= MOD_ALT,
            "shift" => mods |= MOD_SHIFT,
            "win" | "super" => mods |= MOD_WIN,
            "" => {}
            k if k.len() == 1 => {
                let c = k.chars().next()?.to_ascii_uppercase();
                if !c.is_ascii_alphanumeric() {
                    return None;
                }
                vk = Some(c as u32);
            }
            k if k.starts_with('f') => {
                let n: u32 = k[1..].parse().ok()?;
                if !(1..=24).contains(&n) {
                    return None;
                }
                vk = Some(VK_F1.0 as u32 + n - 1);
            }
            _ => return None,
        }
    }
    if mods.0 == 0 {
        return None;
    }
    Some((mods, vk?))
}

unsafe extern "system" fn win_event_proc(
    _hook: HWINEVENTHOOK,
    event: u32,
    _hwnd: HWND,
    _id_object: i32,
    _id_child: i32,
    _thread: u32,
    _time: u32,
) {
    const EVENT_SYSTEM_FOREGROUND: u32 = 0x0003;
    const EVENT_SYSTEM_MOVESIZEEND: u32 = 0x000B;
    const EVENT_SYSTEM_MINIMIZESTART: u32 = 0x0016;
    const EVENT_SYSTEM_MINIMIZEEND: u32 = 0x0017;
    if matches!(
        event,
        EVENT_SYSTEM_FOREGROUND | EVENT_SYSTEM_MOVESIZEEND | EVENT_SYSTEM_MINIMIZESTART | EVENT_SYSTEM_MINIMIZEEND
    ) {
        let hwnd = HWND(AGENT_HWND.load(Ordering::Relaxed) as _);
        unsafe {
            let _ = PostMessageW(Some(hwnd), WM_APP_FOCUS, WPARAM(0), LPARAM(0));
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_TIMER => {
                match wp.0 {
                    TIMER_TICK => with_agent(|a| a.tick()),
                    TIMER_FAST => with_agent(|a| a.fast_tick()),
                    _ => None,
                };
                LRESULT(0)
            }
            tray::WM_TRAY => {
                match (lp.0 & 0xFFFF) as u32 {
                    WM_LBUTTONUP => crate::ui::launch("overview"),
                    WM_RBUTTONUP | WM_CONTEXTMENU => {
                        let now = util::now();
                        let info = with_agent(|a| {
                            (a.state_label(now), a.is_paused(now), a.cfg.torch.enabled, a.cfg.blacks.enabled)
                        });
                        let (line, paused, torch, blacks) = info.unwrap_or_default();
                        // No borrow is held here: the menu runs a nested message loop.
                        let cmd = tray::show_menu(hwnd, &line, paused, torch, blacks);
                        if cmd != 0 {
                            with_agent(|a| a.on_menu(cmd));
                        }
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_HOTKEY => {
                if wp.0 as i32 == HOTKEY_TORCH {
                    with_agent(|a| a.toggle_torch());
                } else if wp.0 as i32 == HOTKEY_BLACKS {
                    with_agent(|a| a.toggle_blacks());
                } else {
                    with_agent(|a| a.toggle_pause());
                }
                LRESULT(0)
            }
            WM_APP_FOCUS => {
                with_agent(|a| a.on_focus_change());
                LRESULT(0)
            }
            WM_APP_PANEL => {
                with_agent(|a| a.on_panel_answers());
                LRESULT(0)
            }
            WM_INPUT => {
                // Keyboard activity only; the key itself is never read.
                with_agent(|a| a.panel.key_pending = true);
                DefWindowProcW(hwnd, msg, wp, lp)
            }
            WM_DISPLAYCHANGE => {
                log!("display configuration changed");
                with_agent(|a| a.rebuild_pending = true);
                LRESULT(0)
            }
            WM_SETTINGCHANGE => {
                if lp.0 != 0 {
                    let s = PCWSTR(lp.0 as *const u16).to_string().unwrap_or_default();
                    if s == "ImmersiveColorSet" {
                        with_agent(|a| a.tray.refresh_theme());
                    }
                }
                LRESULT(0)
            }
            WM_POWERBROADCAST => {
                const PBT_APMRESUMEAUTOMATIC: usize = 0x12;
                if wp.0 == PBT_POWERSETTINGCHANGE as usize && lp.0 != 0 {
                    let setting = &*(lp.0 as *const POWERBROADCAST_SETTING);
                    if setting.PowerSetting == GUID_CONSOLE_DISPLAY_STATE && setting.DataLength >= 1 {
                        let state = setting.Data[0];
                        with_agent(|a| a.on_display_state(state));
                    }
                } else if wp.0 == PBT_APMRESUMEAUTOMATIC {
                    with_agent(|a| {
                        for s in &mut a.screens {
                            s.d.capture.reset();
                        }
                    });
                }
                LRESULT(1)
            }
            WM_QUERYENDSESSION => LRESULT(1),
            WM_ENDSESSION => {
                if wp.0 != 0 {
                    with_agent(|a| a.shutdown());
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                with_agent(|a| a.shutdown());
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ if msg != 0 && msg == CMD_MSG.load(Ordering::Relaxed) => {
                with_agent(|a| a.on_command(wp.0, lp.0));
                LRESULT(0)
            }
            _ if msg != 0 && msg == TASKBAR_CREATED.load(Ordering::Relaxed) => {
                with_agent(|a| a.tray.add());
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

pub fn run(opts: Options) -> i32 {
    util::init_log("agent");
    unsafe {
        let name = wide(&format!("Local\\Wanelight.Agent{}", util::instance_suffix()));
        let _mutex = CreateMutexW(None, false, PCWSTR(name.as_ptr()));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            // Already running: a second launch opens the settings window instead.
            crate::ui::launch("overview");
            return 0;
        }
        log!("Wanelight {} starting", env!("CARGO_PKG_VERSION"));
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);

        std::panic::set_hook(Box::new(|info| {
            crate::util::log_line(&format!("PANIC: {info}"));
            ddc::restore_now();
        }));

        let Ok(hinst) = GetModuleHandleW(None) else { return 1 };
        let class = wide(&ipc::agent_class());
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            lpszClassName: PCWSTR(class.as_ptr()),
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let Ok(hwnd) = CreateWindowExW(
            WS_EX_TOOLWINDOW,
            PCWSTR(class.as_ptr()),
            w!("Wanelight"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hinst.into()),
            None,
        ) else {
            log!("cannot create agent window");
            return 1;
        };
        AGENT_HWND.store(hwnd.0 as isize, Ordering::Relaxed);
        CMD_MSG.store(ipc::command_message_id(), Ordering::Relaxed);
        TASKBAR_CREATED.store(RegisterWindowMessageW(w!("TaskbarCreated")), Ordering::Relaxed);

        let agent = Agent::new(hwnd, opts);
        AGENT.with(|a| *a.borrow_mut() = Some(agent));

        let _ = RegisterPowerSettingNotification(
            HANDLE(hwnd.0),
            &GUID_CONSOLE_DISPLAY_STATE,
            windows::Win32::UI::WindowsAndMessaging::DEVICE_NOTIFY_WINDOW_HANDLE,
        );
        let hook = SetWinEventHook(0x0003, 0x0017, None, Some(win_event_proc), 0, 0, 0x0002);
        SetTimer(Some(hwnd), TIMER_TICK, 1000, None);
        with_agent(|a| a.tick());

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        if !hook.is_invalid() {
            let _ = windows::Win32::UI::Accessibility::UnhookWinEvent(hook);
        }
        with_agent(|a| a.shutdown());
        AGENT.with(|a| a.borrow_mut().take());
        log!("stopped");
    }
    0
}
