//! User configuration, stored as TOML in `%APPDATA%\Wanelight\config.toml`.
//!
//! The agent re-reads the file whenever its modification time changes, so the
//! settings window only has to save it.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::SystemTime;

use crate::util;

/// Still-area dimming and toolbar hiding aren't ready: while this is false
/// they stay off and out of the settings window, whatever the file says.
pub const MORE_DIMMING: bool = false;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Config {
    /// Master switch for all dimming.
    pub enabled: bool,
    /// Global pause/resume hotkey, e.g. "Ctrl+Alt+Shift+W". Empty disables it.
    pub hotkey: String,
    /// Dim static HUDs/logos while a fullscreen app (game, video) is in front.
    /// Turn off for competitive gaming: an overlay above a game can cost a
    /// frame of latency on some GPUs.
    pub dim_fullscreen_apps: bool,
    pub static_dimming: StaticDimming,
    pub chrome: Chrome,
    pub torch: Torch,
    pub blacks: Blacks,
    pub away: Away,
    pub ddc: Ddc,
    pub refresh: Refresh,
    pub apps: Apps,
    /// Per-monitor preferences keyed by monitor id.
    pub monitors: BTreeMap<String, MonitorPrefs>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct StaticDimming {
    pub enabled: bool,
    /// Maximum dim (0..1) for static areas outside the window you are using.
    pub background_max_dim: f32,
    /// Seconds an area must stay unchanged before it starts dimming.
    pub background_after_secs: u32,
    /// Maximum dim for static areas inside the foreground window.
    pub foreground_max_dim: f32,
    pub foreground_after_secs: u32,
    /// Extra dimming for a monitor that has had neither focus nor the cursor.
    pub neglected_monitor_dim: f32,
    pub neglected_after_secs: u32,
    /// Dim for static areas of "high risk" apps (chat, trading, monitoring).
    pub high_risk_app_dim: f32,
    /// How fast dimming fades in, in percent per minute. Low values are imperceptible.
    pub fade_in_percent_per_minute: f32,
    /// Areas whose brightest pixel is below this (0..1 of SDR white) are left alone.
    pub min_brightness: f32,
}

/// Hard-dims the static edges (toolbars, tab strips, sidebars, status bars)
/// of a maximized or fullscreen foreground window; they light up again when
/// the cursor approaches or Alt is held.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Chrome {
    pub enabled: bool,
    pub max_dim: f32,
    /// Seconds an edge area must stay unchanged before it dims.
    pub after_secs: u32,
    /// Cursor distance (px) at which a dimmed band lights up.
    pub reveal_px: u32,
    /// Seconds a band stays lit after the cursor leaves.
    pub hold_secs: f32,
    /// Roughly how long dimming takes to settle, in seconds.
    pub fade_secs: f32,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TorchMode {
    /// The window you're using plus a small halo around the cursor stay lit.
    Window,
    /// Only a circle around the cursor stays lit.
    Spotlight,
    /// Only the app panel under the pointer (sidebar, main area, side pane)
    /// stays lit, or the text box you are typing in. Uses UI Automation.
    Panel,
}

/// Aggressive mode: everything except the focus area is dimmed.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Torch {
    pub enabled: bool,
    pub mode: TorchMode,
    pub dim: f32,
    /// Lit radius around the cursor in spotlight mode (px).
    pub spotlight_radius_px: u32,
    /// Toggle hotkey. Empty disables it.
    pub hotkey: String,
    /// Bonus animations in the dark: a flickering torch and a little visitor.
    pub spooky: bool,
}

/// Near-black grays become true black, through a full-screen color matrix
/// (the same mechanism as Windows' Color filters).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Blacks {
    pub enabled: bool,
    /// Grays at or below this level (0..1, as in #RRGGBB) become black.
    pub level: f32,
    /// Toggle hotkey. Empty disables it.
    pub hotkey: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Away {
    pub enabled: bool,
    /// No input for this long AND a static screen => fade the screen down.
    pub dim_after_secs: u32,
    pub dim_amount: f32,
    pub fade_secs: f32,
    /// Turn displays off after this long away (0 disables).
    pub display_off_after_secs: u32,
    /// Don't turn displays off while audio is playing.
    pub respect_audio: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Ddc {
    /// Lower the monitor's real backlight/brightness over DDC/CI while away.
    /// Off by default: some monitors flash an OSD when brightness changes.
    pub enabled: bool,
    /// Brightness while away, as a percentage of the original value.
    pub away_brightness_percent: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Refresh {
    /// Let the panel run its compensation ("pixel refresh") cycle by turning the
    /// display off at a quiet moment after long sessions.
    pub enabled: bool,
    pub hours_between: f32,
    /// Display-off time that counts as a completed rest.
    pub rest_minutes: u32,
    /// Show a tray balloon if the panel has gone far too long without a rest.
    pub remind: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
#[serde(default)]
pub struct Apps {
    /// Executable names (e.g. "vlc.exe"): no dimming while one is in front.
    pub excluded: Vec<String>,
    /// Executable names whose static areas get the stronger high-risk dim.
    pub high_risk: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct MonitorPrefs {
    pub enabled: bool,
    /// Informational: the monitor's friendly name when it was last seen.
    pub name: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: true,
            hotkey: "Ctrl+Alt+Shift+W".into(),
            dim_fullscreen_apps: true,
            static_dimming: StaticDimming::default(),
            chrome: Chrome::default(),
            torch: Torch::default(),
            blacks: Blacks::default(),
            away: Away::default(),
            ddc: Ddc::default(),
            refresh: Refresh::default(),
            apps: Apps::default(),
            monitors: BTreeMap::new(),
        }
    }
}

impl Default for StaticDimming {
    fn default() -> Self {
        Self {
            enabled: true,
            background_max_dim: 0.25,
            background_after_secs: 180,
            foreground_max_dim: 0.10,
            foreground_after_secs: 900,
            neglected_monitor_dim: 0.35,
            neglected_after_secs: 300,
            high_risk_app_dim: 0.35,
            fade_in_percent_per_minute: 20.0,
            min_brightness: 0.12,
        }
    }
}

impl Default for Chrome {
    fn default() -> Self {
        Self { enabled: false, max_dim: 0.5, after_secs: 60, reveal_px: 100, hold_secs: 3.0, fade_secs: 2.0 }
    }
}

impl Default for Torch {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: TorchMode::Window,
            dim: 0.6,
            spotlight_radius_px: 400,
            hotkey: "Ctrl+Alt+Shift+T".into(),
            spooky: false,
        }
    }
}

impl Default for Blacks {
    fn default() -> Self {
        Self { enabled: false, level: 0.15, hotkey: String::new() }
    }
}

impl Default for Away {
    fn default() -> Self {
        Self {
            enabled: true,
            dim_after_secs: 300,
            dim_amount: 0.70,
            fade_secs: 45.0,
            display_off_after_secs: 1200,
            respect_audio: true,
        }
    }
}

impl Default for Ddc {
    fn default() -> Self {
        Self { enabled: false, away_brightness_percent: 30 }
    }
}

impl Default for Refresh {
    fn default() -> Self {
        Self { enabled: true, hours_between: 4.0, rest_minutes: 10, remind: false }
    }
}

impl Default for MonitorPrefs {
    fn default() -> Self {
        Self { enabled: true, name: String::new() }
    }
}

impl Config {
    pub fn path() -> PathBuf {
        util::data_dir().join("config.toml")
    }

    /// Loads the config, writing defaults if the file does not exist yet.
    /// A malformed file is left untouched and defaults are used.
    pub fn load() -> Config {
        let path = Self::path();
        match std::fs::read_to_string(&path) {
            Ok(text) => match toml::from_str::<Config>(&text) {
                Ok(cfg) => cfg.sanitized(),
                Err(e) => {
                    crate::log!("config: parse error, using defaults: {e}");
                    Config::default().sanitized()
                }
            },
            Err(_) => {
                let cfg = Config::default().sanitized();
                let _ = cfg.save();
                cfg
            }
        }
    }

    pub fn save(&self) -> std::io::Result<()> {
        let text = toml::to_string_pretty(self).map_err(std::io::Error::other)?;
        util::write_atomic(&Self::path(), text.as_bytes())
    }

    pub fn modified() -> Option<SystemTime> {
        std::fs::metadata(Self::path()).and_then(|m| m.modified()).ok()
    }

    /// Clamps values so a hand-edited file can't produce nonsense.
    pub fn sanitized(mut self) -> Config {
        if !MORE_DIMMING {
            self.static_dimming.enabled = false;
            self.chrome.enabled = false;
        }
        let s = &mut self.static_dimming;
        for v in [
            &mut s.background_max_dim,
            &mut s.foreground_max_dim,
            &mut s.neglected_monitor_dim,
            &mut s.high_risk_app_dim,
        ] {
            *v = v.clamp(0.0, 0.8);
        }
        s.fade_in_percent_per_minute = s.fade_in_percent_per_minute.clamp(1.0, 600.0);
        s.min_brightness = s.min_brightness.clamp(0.0, 1.0);
        s.background_after_secs = s.background_after_secs.max(10);
        s.foreground_after_secs = s.foreground_after_secs.max(10);
        s.neglected_after_secs = s.neglected_after_secs.max(10);
        let c = &mut self.chrome;
        c.max_dim = c.max_dim.clamp(0.0, 0.9);
        c.after_secs = c.after_secs.max(5);
        c.reveal_px = c.reveal_px.clamp(0, 1000);
        c.hold_secs = c.hold_secs.clamp(0.0, 60.0);
        c.fade_secs = c.fade_secs.clamp(0.2, 30.0);
        let t = &mut self.torch;
        t.dim = t.dim.clamp(0.0, 0.95);
        t.spotlight_radius_px = t.spotlight_radius_px.clamp(50, 3000);
        self.blacks.level = self.blacks.level.clamp(0.0, 0.35);
        let a = &mut self.away;
        a.dim_amount = a.dim_amount.clamp(0.0, 0.95);
        a.fade_secs = a.fade_secs.clamp(1.0, 600.0);
        a.dim_after_secs = a.dim_after_secs.max(30);
        self.ddc.away_brightness_percent = self.ddc.away_brightness_percent.clamp(0, 100);
        self.refresh.hours_between = self.refresh.hours_between.clamp(0.5, 48.0);
        self.refresh.rest_minutes = self.refresh.rest_minutes.clamp(1, 240);
        for list in [&mut self.apps.excluded, &mut self.apps.high_risk] {
            for name in list.iter_mut() {
                *name = name.trim().to_ascii_lowercase();
            }
            list.retain(|n| !n.is_empty());
        }
        self
    }

    pub fn monitor_enabled(&self, id: &str) -> bool {
        self.monitors.get(id).map(|m| m.enabled).unwrap_or(true)
    }
}
