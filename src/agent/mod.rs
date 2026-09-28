//! The background agent: a hidden window that owns the tray icon, runs a 1 Hz
//! control loop (sample screens -> update model -> choose dims -> animate), and
//! a 30 Hz loop only while something is animating or the screen is resting.

mod capture;
mod ddc;
mod model;
mod overlay;
mod power;
pub mod selftest;
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
use windows::Win32::UI::Shell::{QUNS_PRESENTATION_MODE, SHQueryUserNotificationState};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

use crate::config::{Config, MonitorPrefs};
use crate::ipc;
use crate::ledger::Ledger;
use crate::log;
use crate::util::{self, wide};
use capture::Gpu;

const TIMER_TICK: usize = 1;
const TIMER_FAST: usize = 2;
const WM_APP_FOCUS: u32 = WM_APP + 2;
const HOTKEY_ID: i32 = 1;
/// How fast dimming releases when content changes or focus moves (per second).
const RELEASE_PER_SEC: f32 = 1.2;

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
    fast_timer: bool,
    rebuild_pending: bool,
    last_ledger_save: f64,
    last_state_save: f64,
    reminder_shown: bool,
    shut_down: bool,
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
            fast_timer: false,
            rebuild_pending: false,
            last_ledger_save: now,
            last_state_save: now,
            reminder_shown: false,
            shut_down: false,
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
        match capture::enumerate() {
            Ok(e) => {
                self.overlay_gpu = e.gpus.first().cloned().or_else(|| Gpu::new(None, None).ok().map(Rc::new));
                for d in e.displays {
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
        unsafe {
            let _ = UnregisterHotKey(Some(self.hwnd), HOTKEY_ID);
        }
        if self.cfg.hotkey.trim().is_empty() {
            return;
        }
        match parse_hotkey(&self.cfg.hotkey) {
            Some((mods, vk)) => {
                if let Err(e) = unsafe { RegisterHotKey(Some(self.hwnd), HOTKEY_ID, mods | MOD_NOREPEAT, vk) } {
                    log!("hotkey {} unavailable: {}", self.cfg.hotkey, e.message());
                }
            }
            None => log!("hotkey \"{}\" not understood", self.cfg.hotkey),
        }
    }

    fn reload_config(&mut self) {
        let new = Config::load();
        if new == self.cfg {
            return;
        }
        log!("config reloaded");
        let hotkey_changed = new.hotkey != self.cfg.hotkey;
        self.cfg = new;
        if hotkey_changed {
            self.register_hotkey();
        }
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

        if self.display_on {
            for s in &mut self.screens {
                let sample = s.d.sample(now);
                s.model.apply(&sample, dt, now);
            }
            self.panel_on_secs += dt as f64;
        }

        self.update_away(now, idle);
        self.compute_targets(now);
        let animating = self.animate();
        self.accumulate_ledgers(dt);
        self.set_fast_timer(animating || self.needs_input_watch());
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
        let animating = self.animate();
        self.set_fast_timer(animating || self.needs_input_watch());
    }

    fn needs_input_watch(&self) -> bool {
        self.away_target > 0.0 || self.away_cur > 0.0 || self.ddc_dimmed
    }

    fn set_fast_timer(&mut self, on: bool) {
        if on == self.fast_timer {
            return;
        }
        self.fast_timer = on;
        unsafe {
            if on {
                SetTimer(Some(self.hwnd), TIMER_FAST, 33, None);
            } else {
                let _ = KillTimer(Some(self.hwnd), TIMER_FAST);
            }
        }
    }

    fn kick_animation(&mut self) {
        let animating = self.animate();
        self.set_fast_timer(animating || self.needs_input_watch());
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
        for s in &mut self.screens {
            let suspended = !self.cfg.enabled
                || !s.enabled
                || paused
                || self.excluded_fg
                || self.presentation
                || (s.fullscreen && !self.cfg.dim_fullscreen_apps);
            let neglected =
                multi && now - s.last_attention >= self.cfg.static_dimming.neglected_after_secs as f64;
            let ctx = model::PolicyCtx {
                cfg: &self.cfg,
                snap: &self.snap,
                owners: &s.owners,
                high_risk: &self.high_risk,
                neglected,
                suspended,
            };
            model::targets(&s.model, &ctx, &mut s.target);
        }
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
        let mut animating = self.away_cur < self.away_target;
        for s in &mut self.screens {
            let before = s.cur.clone();
            animating |= model::ramp(&mut s.cur, &s.target, dt, up, RELEASE_PER_SEC);
            let away = if s.enabled { self.away_cur } else { 0.0 };
            if before == s.cur && away == s.composed_away && s.composed.len() == s.cur.len() {
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

    fn toggle_pause(&mut self) {
        if self.is_paused(util::now()) { self.resume() } else { self.pause(None) }
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
        format!("Protecting {} display{}", n, if n == 1 { "" } else { "s" })
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
        ddc::restore_now();
        self.tray.remove();
    }
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
                        let info = with_agent(|a| (a.state_label(now), a.is_paused(now)));
                        let (line, paused) = info.unwrap_or_default();
                        // No borrow is held here: the menu runs a nested message loop.
                        let cmd = tray::show_menu(hwnd, &line, paused);
                        if cmd != 0 {
                            with_agent(|a| a.on_menu(cmd));
                        }
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_HOTKEY => {
                with_agent(|a| a.toggle_pause());
                LRESULT(0)
            }
            WM_APP_FOCUS => {
                with_agent(|a| a.on_focus_change());
                LRESULT(0)
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
        let name = wide("Local\\Wanelight.Agent");
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
        let class = wide(ipc::AGENT_CLASS);
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
