//! Settings window. Runs as a separate process (`wanelight --ui <tab>`) so the
//! always-on agent never loads a GUI toolkit. Settings are saved to the config
//! file as they change; the agent picks them up within a second.

mod heatmap;

use std::sync::Arc;

use eframe::egui::{self, Align, Align2, Color32, FontId, Key, Layout, RichText, Sense, Stroke, StrokeKind, pos2, vec2};
use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, HWND, LPARAM};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, FindWindowW, GW_OWNER, GWL_EXSTYLE, GetWindow, GetWindowLongW, GetWindowTextLengthW,
    GetWindowThreadProcessId, IsWindowVisible, SW_RESTORE, SetForegroundWindow, ShowWindow, WS_EX_TOOLWINDOW,
};
use windows::core::{BOOL, PCWSTR, w};

use crate::config::{Config, MORE_DIMMING, StaticDimming, TorchMode};
use crate::ipc::{self, Status};
use crate::{autostart, hardening, icon_art, util};

const TITLE: &str = "Wanelight Settings";
const AMBER: Color32 = Color32::from_rgb(0xf5, 0xc0, 0x6a);
const NAV_BG: Color32 = Color32::from_rgb(0x16, 0x16, 0x16);
const PAGE_BG: Color32 = Color32::from_rgb(0x1c, 0x1c, 0x1c);
const CARD_BG: Color32 = Color32::from_rgb(0x24, 0x24, 0x24);
const CARD_LINE: Color32 = Color32::from_rgb(0x31, 0x31, 0x31);
const LABEL_W: f32 = 190.0;

pub fn launch(tab: &str) {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe).args(["--ui", tab]).spawn();
    }
}

fn request_path() -> std::path::PathBuf {
    util::data_dir().join("ui-request.txt")
}

/// Asks an open settings window to close. Called by the agent when it exits.
pub fn close() {
    let _ = std::fs::write(request_path(), "quit");
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tab {
    Home,
    Torch,
    Blacks,
    Away,
    Apps,
    More,
    Wear,
    Windows,
}

impl Tab {
    const ALL: [Tab; 8] = [Tab::Home, Tab::Torch, Tab::Blacks, Tab::Away, Tab::Apps, Tab::More, Tab::Wear, Tab::Windows];
    fn parse(s: &str) -> Tab {
        match s.trim() {
            "torch" | "focus" | "extra" => Tab::Torch,
            "blacks" => Tab::Blacks,
            "away" => Tab::Away,
            "apps" => Tab::Apps,
            "more" | "automatic" | "protection" if MORE_DIMMING => Tab::More,
            "wear" | "heatmap" => Tab::Wear,
            "windows" | "tweaks" => Tab::Windows,
            _ => Tab::Home,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Tab::Home => "Home",
            Tab::Torch => "Torch",
            Tab::Blacks => "Deeper blacks",
            Tab::Away => "Away and rest",
            Tab::Apps => "Apps",
            Tab::More => "More dimming",
            Tab::Wear => "Wear map",
            Tab::Windows => "Windows setup",
        }
    }
    fn intro(self) -> &'static str {
        match self {
            Tab::Home => "",
            Tab::Torch => "Only what you're working on stays lit, and everything else dims.",
            Tab::Blacks => "Dark grays in every app become true black, so those pixels switch off. Whites and colours stay the same.",
            Tab::Away => "What happens when you step away, and giving the panel time to recover.",
            Tab::Apps => "Apps that need special treatment.",
            Tab::More => "Optional dimming that works on its own in the background, without a shortcut.",
            Tab::Wear => {
                "How much light each part of the screen has given off, in hours at full white. Bright, sharp shapes are where burn-in would show first."
            }
            Tab::Windows => "One-time Windows settings that reduce burn-in. Turn one off to put your previous setting back.",
        }
    }
    /// Pages whose header carries the feature's own switch.
    fn has_switch(self) -> bool {
        matches!(self, Tab::Torch | Tab::Blacks)
    }
}

struct Preset {
    name: &'static str,
    blurb: &'static str,
    background: f32,
    background_after: u32,
    foreground: f32,
    foreground_after: u32,
    other: f32,
}

const PRESETS: [Preset; 3] = [
    Preset {
        name: "Gentle",
        blurb: "Up to 15%, after 5 minutes. Very hard to notice.",
        background: 0.15,
        background_after: 300,
        foreground: 0.05,
        foreground_after: 1200,
        other: 0.20,
    },
    Preset {
        name: "Balanced",
        blurb: "Up to 25%, after 3 minutes. Recommended.",
        background: 0.25,
        background_after: 180,
        foreground: 0.10,
        foreground_after: 900,
        other: 0.35,
    },
    Preset {
        name: "Strong",
        blurb: "Up to 40%, after 2 minutes. You may notice it on a still screen.",
        background: 0.40,
        background_after: 120,
        foreground: 0.15,
        foreground_after: 600,
        other: 0.50,
    },
];

fn preset_of(s: &StaticDimming) -> Option<usize> {
    let near = |a: f32, b: f32| (a - b).abs() < 0.005;
    PRESETS.iter().position(|p| {
        near(s.background_max_dim, p.background)
            && s.background_after_secs == p.background_after
            && near(s.foreground_max_dim, p.foreground)
            && s.foreground_after_secs == p.foreground_after
            && near(s.neglected_monitor_dim, p.other)
            && near(s.high_risk_app_dim, p.other)
    })
}

fn apply_preset(s: &mut StaticDimming, p: &Preset) {
    s.background_max_dim = p.background;
    s.background_after_secs = p.background_after;
    s.foreground_max_dim = p.foreground;
    s.foreground_after_secs = p.foreground_after;
    s.neglected_monitor_dim = p.other;
    s.high_risk_app_dim = p.other;
}

pub fn run(tab: &str) -> i32 {
    util::init_log("ui");
    unsafe {
        let name = util::wide(&format!("Local\\Wanelight.UI{}", util::instance_suffix()));
        let _mutex = CreateMutexW(None, false, PCWSTR(name.as_ptr()));
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let _ = std::fs::write(request_path(), tab);
            if let Ok(h) = FindWindowW(PCWSTR::null(), w!("Wanelight Settings")) {
                let _ = ShowWindow(h, SW_RESTORE);
                let _ = SetForegroundWindow(h);
            }
            return 0;
        }
        std::mem::forget(_mutex);
    }
    // A leftover request (for example a quit sent after the window had closed).
    let _ = std::fs::remove_file(request_path());
    let icon = icon_art::app_icon_rgba(64);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(TITLE)
            .with_inner_size([900.0, 660.0])
            .with_min_inner_size([720.0, 500.0])
            .with_icon(Arc::new(egui::IconData { rgba: icon, width: 64, height: 64 })),
        ..Default::default()
    };
    let start = Tab::parse(tab);
    match eframe::run_native(TITLE, options, Box::new(move |cc| Ok(Box::new(App::new(cc, start))))) {
        Ok(()) => 0,
        Err(e) => {
            crate::log!("ui: {e}");
            1
        }
    }
}

struct App {
    tab: Tab,
    cfg: Config,
    saved: Config,
    status: Option<Status>,
    agent_running: bool,
    last_poll: f64,
    autostart: bool,
    heat: heatmap::HeatmapView,
    tweak_state: Vec<bool>,
    tweak_backup: Vec<bool>,
    tweak_msg: Option<String>,
    new_excluded: String,
    new_high_risk: String,
    open_apps: Vec<String>,
    recording: Option<&'static str>,
}

impl App {
    fn new(cc: &eframe::CreationContext, tab: Tab) -> Self {
        let mut visuals = egui::Visuals::dark();
        visuals.selection.bg_fill = Color32::from_rgb(0x6b, 0x4f, 0x1d);
        visuals.hyperlink_color = AMBER;
        visuals.panel_fill = PAGE_BG;
        visuals.weak_text_color = Some(Color32::from_gray(0x92));
        cc.egui_ctx.set_theme(egui::Theme::Dark);
        cc.egui_ctx.set_visuals_of(egui::Theme::Dark, visuals);
        cc.egui_ctx.style_mut_of(egui::Theme::Dark, |s| {
            use egui::TextStyle;
            s.text_styles.insert(TextStyle::Body, FontId::proportional(14.0));
            s.text_styles.insert(TextStyle::Button, FontId::proportional(14.0));
            s.text_styles.insert(TextStyle::Small, FontId::proportional(12.0));
            s.text_styles.insert(TextStyle::Heading, FontId::proportional(24.0));
            s.spacing.item_spacing = vec2(8.0, 7.0);
            s.spacing.button_padding = vec2(10.0, 4.0);
            s.spacing.interact_size.y = 24.0;
            s.spacing.slider_width = 240.0;
        });
        let cfg = Config::load();
        let mut app = App {
            tab,
            saved: cfg.clone(),
            cfg,
            status: None,
            agent_running: false,
            last_poll: f64::NEG_INFINITY,
            autostart: autostart::is_enabled(),
            heat: heatmap::HeatmapView::default(),
            tweak_state: Vec::new(),
            tweak_backup: Vec::new(),
            tweak_msg: None,
            new_excluded: String::new(),
            new_high_risk: String::new(),
            open_apps: Vec::new(),
            recording: None,
        };
        app.refresh_tweaks();
        app
    }

    fn refresh_tweaks(&mut self) {
        self.tweak_state = hardening::ITEMS.iter().map(|i| hardening::is_applied(i.key)).collect();
        self.tweak_backup = hardening::ITEMS.iter().map(|i| hardening::has_backup(i.key)).collect();
    }

    fn poll(&mut self, ctx: &egui::Context) {
        let now = util::now();
        if now - self.last_poll < 1.0 {
            return;
        }
        self.last_poll = now;
        self.agent_running = ipc::send_command(ipc::CMD_WRITE_STATUS, 0);
        // The agent answers asynchronously; this reads last second's answer.
        self.status = if self.agent_running { ipc::read_status() } else { None };
        if let Ok(t) = std::fs::read_to_string(request_path()) {
            let _ = std::fs::remove_file(request_path());
            if t.trim() == "quit" {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            } else {
                self.tab = Tab::parse(&t);
            }
        }
        if self.tab == Tab::Apps {
            self.open_apps = open_apps();
        }
        // Pick up edits made elsewhere (another window, a text editor).
        let disk = Config::load();
        if disk != self.saved {
            self.cfg = disk.clone();
            self.saved = disk;
        }
    }

    fn home(&mut self, ui: &mut egui::Ui) {
        if !self.agent_running {
            card(ui, |ui| {
                ui.horizontal(|ui| {
                    dot(ui, Color32::from_gray(0x70));
                    ui.label(RichText::new("Wanelight isn't running").size(20.0).strong());
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.button("Start Wanelight").clicked()
                            && let Ok(exe) = std::env::current_exe()
                        {
                            let _ = std::process::Command::new(exe).spawn();
                        }
                    });
                });
                ui.label(RichText::new("Nothing is being protected right now.").weak());
            });
        } else if let Some(st) = &self.status {
            let any_on = self.cfg.torch.enabled || self.cfg.blacks.enabled || self.cfg.static_dimming.enabled || self.cfg.chrome.enabled;
            let active = self.cfg.enabled && !st.paused && any_on;
            card(ui, |ui| {
                ui.horizontal(|ui| {
                    dot(ui, if active { AMBER } else { Color32::from_gray(0x70) });
                    let state = if self.cfg.enabled && !st.paused && !any_on {
                        "Ready".to_string()
                    } else {
                        st.state.replace(" · torch mode", "").replace(" · deeper blacks", "")
                    };
                    ui.label(RichText::new(state).size(20.0).strong());
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if !self.cfg.enabled {
                            if ui.button("Turn on").clicked() {
                                self.cfg.enabled = true;
                            }
                        } else if st.paused {
                            if ui.button("Resume").clicked() {
                                ipc::send_command(ipc::CMD_RESUME, 0);
                            }
                        } else {
                            ui.menu_button("Pause  ⏷", |ui| {
                                if ui.button("For 1 hour").clicked() {
                                    ipc::send_command(ipc::CMD_PAUSE, 60);
                                }
                                if ui.button("Until I resume").clicked() {
                                    ipc::send_command(ipc::CMD_PAUSE, 0);
                                }
                                ui.separator();
                                if ui.button("Turn off").on_hover_text("Stays off until you turn it back on, even after a restart.").clicked() {
                                    self.cfg.enabled = false;
                                }
                            });
                        }
                    });
                });
                let detail = if !self.cfg.enabled {
                    "Nothing is dimmed until you turn it back on.".to_string()
                } else if st.paused {
                    "Nothing is dimmed while paused.".to_string()
                } else if !any_on {
                    "Torch and Deeper blacks are off. Turn one on below, or use its shortcut.".to_string()
                } else {
                    let (mut area, mut dimmed) = (0.0, 0.0);
                    for m in st.monitors.iter().filter(|m| m.enabled) {
                        let a = (m.width * m.height) as f32;
                        area += a;
                        dimmed += a * m.dimmed_fraction;
                    }
                    let pct = if area > 0.0 { dimmed / area * 100.0 } else { 0.0 };
                    if pct < 0.5 {
                        "Nothing needs dimming right now.".to_string()
                    } else {
                        format!("Dimming {pct:.0}% of your screen right now.")
                    }
                };
                ui.label(RichText::new(detail).weak());
                ui.add_space(4.0);
                row(ui, "Pause shortcut", |ui| shortcut(ui, "pause", &mut self.cfg.hotkey, &mut self.recording));
            });
        }

        ui.add_space(6.0);
        let mut goto = None;
        ui.columns(2, |cols| {
            let t = &mut self.cfg.torch;
            primary(
                &mut cols[0],
                "Torch",
                "Only what you're using stays lit.",
                &mut t.enabled,
                "torch",
                &mut t.hotkey,
                &mut self.recording,
                |ui| {
                    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 84.0), Sense::hover());
                    paint_torch(ui.painter(), rect, t.mode);
                },
                || goto = Some(Tab::Torch),
            );
            let b = &mut self.cfg.blacks;
            primary(
                &mut cols[1],
                "Deeper blacks",
                "Dark grays become true black.",
                &mut b.enabled,
                "blacks",
                &mut b.hotkey,
                &mut self.recording,
                |ui| {
                    let w = ui.available_width();
                    gray_strip(ui, b.level, false, w, 38.0);
                    gray_strip(ui, b.level, true, w, 38.0);
                },
                || goto = Some(Tab::Blacks),
            );
        });
        if let Some(t) = goto {
            self.tab = t;
        }

        section(ui, "Displays");
        let monitors = self.status.as_ref().map(|s| s.monitors.clone()).unwrap_or_default();
        if monitors.is_empty() {
            ui.label(RichText::new("Display details appear here while Wanelight is running.").weak());
        }
        for m in monitors {
            card(ui, |ui| {
                let entry = self.cfg.monitors.entry(m.id.clone()).or_default();
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&m.name).size(16.0).strong());
                    ui.label(RichText::new(format!("{} × {}{}", m.width, m.height, if m.hdr { "  HDR" } else { "" })).weak());
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        toggle(ui, &mut entry.enabled).on_hover_text("Protect this display");
                    });
                });
                let line = if !entry.enabled {
                    "Not protected.".to_string()
                } else if !m.capturing {
                    "Can't see this screen right now (locked, protected video or a graphics limit). Away and rest still work.".to_string()
                } else if m.dimmed_fraction < 0.005 {
                    "Nothing dimmed right now.".to_string()
                } else {
                    format!("Dimming {:.0}% of this screen, by up to {:.0}%.", m.dimmed_fraction * 100.0, m.max_dim * 100.0)
                };
                ui.label(RichText::new(line).weak());
                let ddc = match m.ddc_supported {
                    Some(true) => "Brightness control (DDC/CI): supported",
                    Some(false) => "Brightness control (DDC/CI): not supported",
                    None => "Brightness control (DDC/CI): not checked yet",
                };
                ui.label(
                    RichText::new(format!("{:.0}% of the screen has been still for over a minute · {ddc}", m.static_fraction * 100.0))
                        .weak()
                        .small(),
                );
            });
        }
        if let Some(st) = &self.status
            && self.cfg.refresh.enabled
        {
            card(ui, |ui| {
                let goal = self.cfg.refresh.hours_between;
                row(ui, "Panel rest", |ui| {
                    let (rect, _) = ui.allocate_exact_size(vec2(240.0, 6.0), Sense::hover());
                    ui.painter().rect_filled(rect, 3.0, Color32::from_gray(0x38));
                    let done = (st.panel_hours_since_rest / goal).clamp(0.0, 1.0);
                    if done > 0.0 {
                        let fill = egui::Rect::from_min_size(rect.min, vec2((rect.width() * done).max(6.0), rect.height()));
                        ui.painter().rect_filled(fill, 3.0, AMBER);
                    }
                    ui.label(format!("{:.1} of {goal:.0} h", st.panel_hours_since_rest));
                });
                ui.label(RichText::new("Hours on since the panel last rested. It rests at a quiet moment once the bar is full.").weak());
            });
        }

        section(ui, "Startup");
        card(ui, |ui| {
            let mut auto = self.autostart;
            if switch(ui, &mut auto, "Start with Windows").changed() && autostart::set(auto) {
                self.autostart = auto;
            }
        });
    }

    fn more(&mut self, ui: &mut egui::Ui) {
        let s = &mut self.cfg.static_dimming;
        let fullscreen = &mut self.cfg.dim_fullscreen_apps;
        let mut enabled = s.enabled;
        feature(
            ui,
            "Dim still areas",
            "Bright areas that haven't changed for a while fade down. The window you're using is dimmed least and last.",
            Some(&mut enabled),
            |_| {},
            |ui| {
                let current = preset_of(s);
                row(ui, "Strength", |ui| {
                    let names: Vec<_> = PRESETS.iter().enumerate().map(|(i, p)| (i, p.name)).collect();
                    if let Some(i) = segmented(ui, current, &names) {
                        apply_preset(s, &PRESETS[i]);
                    }
                });
                let blurb = match preset_of(s) {
                    Some(i) => PRESETS[i].blurb,
                    None => "Your own mix, set below.",
                };
                row(ui, "", |ui| ui.label(RichText::new(blurb).weak()));
                group(ui, "Everywhere except the window you're using", "The taskbar, side panels, other windows and the desktop.");
                pct(ui, "Dim by up to", &mut s.background_max_dim, 0.0..=0.6);
                mins(ui, "Start after", &mut s.background_after_secs, 1..=30);
                group(ui, "The window you're using", "Only its still toolbars and sidebars, and much later.");
                pct(ui, "Dim by up to", &mut s.foreground_max_dim, 0.0..=0.3);
                mins(ui, "Start after", &mut s.foreground_after_secs, 5..=60);
                group(ui, "Displays you're not using", "With more than one display: one that hasn't had the pointer or focus for a while.");
                pct(ui, "Dim by up to", &mut s.neglected_monitor_dim, 0.0..=0.6);
                mins(ui, "Counts as unused after", &mut s.neglected_after_secs, 1..=60);
                group(ui, "Feel", "");
                row(ui, "Fade-in speed", |ui| {
                    ui.add(slider(&mut s.fade_in_percent_per_minute, 5.0..=120.0).custom_formatter(|v, _| format!("{v:.0}% a minute")))
                        .on_hover_text("20% a minute or slower is below what most people notice.");
                });
                row(ui, "Ignore if darker than", |ui| {
                    ui.add(slider(&mut s.min_brightness, 0.0..=0.5).custom_formatter(|v, _| format!("{:.0}% of white", v * 100.0)));
                });
                ui.add_space(4.0);
                switch(ui, fullscreen, "Also in full-screen games and videos");
                indented(ui, |ui| {
                    ui.label(
                        RichText::new("Dims still scores, maps and logos. Moving pictures are never dimmed. Turn off for competitive games: on some graphics cards an overlay adds a frame of delay.")
                            .weak(),
                    )
                });
            },
        );
        s.enabled = enabled;

        let c = &mut self.cfg.chrome;
        feature(
            ui,
            "Hide toolbars until needed",
            "In a window that fills the screen, still toolbars, tabs, sidebars and status bars dim. Point at one, or hold Alt, and it lights up at once.",
            Some(&mut c.enabled),
            |_| {},
            |ui| {
                pct(ui, "Dim them by", &mut c.max_dim, 0.1..=0.9);
                row(ui, "Start after", |ui| {
                    ui.add(slider(&mut c.after_secs, 10..=600).custom_formatter(|v, _| {
                        if v < 60.0 { format!("{v:.0} s") } else { format!("{:.1} min", v / 60.0) }
                    }));
                });
                row(ui, "Light up within", |ui| {
                    ui.add(slider(&mut c.reveal_px, 20..=400).custom_formatter(|v, _| format!("{v:.0} px of the pointer")));
                });
                row(ui, "Stay lit for", |ui| {
                    ui.add(slider(&mut c.hold_secs, 0.5..=15.0).custom_formatter(|v, _| format!("{v:.1} s")));
                });
                row(ui, "Fade over", |ui| {
                    ui.add(slider(&mut c.fade_secs, 0.5..=10.0).custom_formatter(|v, _| format!("{v:.1} s")));
                });
            },
        );

    }

    fn torch(&mut self, ui: &mut egui::Ui) {
        let t = &mut self.cfg.torch;
        card(ui, |ui| {
            ui.label(RichText::new("What stays lit").size(16.0).strong());
            ui.add_space(4.0);
            torch_tiles(ui, &mut t.mode);
            let what = match t.mode {
                TorchMode::Window => "The window you're using, plus a little around the pointer.",
                TorchMode::Spotlight => "A circle around the pointer.",
                TorchMode::Panel => "The part of the app under the pointer, or the box you're typing in.",
            };
            ui.label(what);
            if t.mode == TorchMode::Panel {
                ui.label(
                    RichText::new(
                        "Panels are found with Windows accessibility, which reads layout only, never text. \
                         Browsers and Electron apps do a little extra work to support it, and VS Code may ask \
                         whether you use a screen reader. Apps without it light up as a whole window.",
                    )
                    .weak()
                    .small(),
                );
            }
        });
        card(ui, |ui| {
            ui.label(RichText::new("Strength").size(16.0).strong());
            ui.add_space(4.0);
            pct(ui, "Dim the rest by", &mut t.dim, 0.2..=0.95);
            if t.mode == TorchMode::Spotlight {
                row(ui, "Circle size", |ui| {
                    ui.add(slider(&mut t.spotlight_radius_px, 100..=1500).custom_formatter(|v, _| format!("{v:.0} px")));
                });
            }
        });
        card(ui, |ui| {
            switch(ui, &mut t.spooky, "Spooky mode");
            ui.label(RichText::new("Bonus animations in the dark. The rest dims almost to black.").weak());
            if t.spooky {
                ui.add_space(4.0);
                pct(ui, "Dim the rest by", &mut t.spooky_dim, 0.5..=0.98);
            }
        });
        let note = if self.cfg.static_dimming.enabled || self.cfg.chrome.enabled {
            "Turn it on and off any time with the shortcut or from the tray icon. While it's on, it replaces the dimming under More dimming."
        } else {
            "Turn it on and off any time with the shortcut or from the tray icon."
        };
        ui.label(RichText::new(note).weak());
    }

    fn blacks(&mut self, ui: &mut egui::Ui) {
        let b = &mut self.cfg.blacks;
        card(ui, |ui| {
            ui.label(RichText::new("How dark counts as black").size(16.0).strong());
            ui.add_space(4.0);
            row(ui, "Make black up to", |ui| {
                ui.add(slider(&mut b.level, 0.0..=0.35).custom_formatter(|v, _| {
                    let g = (v * 255.0).round() as u8;
                    format!("#{g:02X}{g:02X}{g:02X}")
                }));
            });
            ui.add_space(4.0);
            row(ui, "Before", |ui| gray_strip(ui, b.level, false, 340.0, 26.0));
            row(ui, "After", |ui| gray_strip(ui, b.level, true, 340.0, 26.0));
            ui.label(RichText::new("Grays up to the level turn black. Lighter shades are stretched slightly so they stay distinct.").weak());
        });
        ui.label(
            RichText::new("Turn it on and off any time with the shortcut or from the tray icon. It doesn't work while Windows Magnifier or colour filters are on.")
                .weak(),
        );
    }

    fn away(&mut self, ui: &mut egui::Ui) {
        let a = &mut self.cfg.away;
        feature(
            ui,
            "Fade when you step away",
            "After no input with nothing moving on screen, so videos and games never trigger it. Any input brings the screen straight back.",
            Some(&mut a.enabled),
            |_| {},
            |ui| {
                mins(ui, "After", &mut a.dim_after_secs, 1..=60);
                pct(ui, "Fade by", &mut a.dim_amount, 0.2..=0.95);
                row(ui, "Fade over", |ui| {
                    ui.add(slider(&mut a.fade_secs, 5.0..=120.0).custom_formatter(|v, _| format!("{v:.0} s")));
                });
            },
        );

        let mut off = a.display_off_after_secs > 0;
        let mut off_min = if off { a.display_off_after_secs / 60 } else { 20 };
        let audio = &mut a.respect_audio;
        feature(
            ui,
            "Turn displays off",
            "When you've been away longer, the displays go to sleep.",
            Some(&mut off),
            |_| {},
            |ui| {
                row(ui, "After", |ui| {
                    ui.add(slider(&mut off_min, 1..=120).custom_formatter(|v, _| format!("{v:.0} min")));
                });
                switch(ui, audio, "Not while audio is playing");
            },
        );
        let off_secs = if off { off_min.max(1) * 60 } else { 0 };
        if off_secs != a.display_off_after_secs {
            a.display_off_after_secs = off_secs;
        }

        let support = self.status.as_ref().and_then(|s| {
            let known: Vec<bool> = s.monitors.iter().filter_map(|m| m.ddc_supported).collect();
            if known.is_empty() {
                None
            } else if known.iter().all(|&k| k) {
                Some("Your displays support this.")
            } else if known.iter().any(|&k| k) {
                Some("Some of your displays support this.")
            } else {
                Some("Your displays don't support this.")
            }
        });
        let d = &mut self.cfg.ddc;
        feature(
            ui,
            "Lower monitor brightness",
            "While you're away, also turns down the monitor's own brightness. This works in exclusive full-screen games and on the lock screen. Some monitors show a message when it changes.",
            Some(&mut d.enabled),
            |_| {},
            |ui| {
                row(ui, "Brightness while away", |ui| {
                    ui.add(slider(&mut d.away_brightness_percent, 0..=90).custom_formatter(|v, _| format!("{v:.0}% of normal")));
                });
                if let Some(s) = support {
                    ui.label(RichText::new(s).weak());
                }
            },
        );

        let r = &mut self.cfg.refresh;
        feature(
            ui,
            "Panel rest",
            "OLED monitors run a short care cycle when they go to sleep after hours of use. Wanelight turns the display off at a quiet moment so it can.",
            Some(&mut r.enabled),
            |_| {},
            |ui| {
                row(ui, "Every", |ui| {
                    ui.add(slider(&mut r.hours_between, 1.0..=12.0).custom_formatter(|v, _| format!("{v:.1} h of use")));
                });
                row(ui, "Rest for", |ui| {
                    ui.add(slider(&mut r.rest_minutes, 1..=60).custom_formatter(|v, _| format!("{v:.0} min")));
                });
                switch(ui, &mut r.remind, "Remind me if it goes far too long without a rest");
            },
        );
    }

    fn apps(&mut self, ui: &mut egui::Ui) {
        let open = &self.open_apps;
        let list = &mut self.cfg.apps.excluded;
        let input = &mut self.new_excluded;
        feature(
            ui,
            "Never dim these apps",
            "While one of these is in front, nothing is dimmed or recoloured. Useful for photo and video work.",
            None,
            |_| {},
            |ui| {
                app_list(ui, "excluded", list, input, open);
                suggestions(
                    ui,
                    list,
                    &[
                        ("Video players", &["vlc.exe", "mpc-hc64.exe", "mpv.exe", "potplayermini64.exe"]),
                        ("Photo and video editors", &["photoshop.exe", "lightroom.exe", "resolve.exe", "afterfx.exe", "premiere pro.exe"]),
                    ],
                );
            },
        );

        if !MORE_DIMMING {
            return;
        }
        let dim = &mut self.cfg.static_dimming.high_risk_app_dim;
        let list = &mut self.cfg.apps.high_risk;
        let input = &mut self.new_high_risk;
        feature(
            ui,
            "Apps left open all day",
            "Still apps you keep open for hours, like chat or trading tools, are dimmed more while they're not in front.",
            None,
            |_| {},
            |ui| {
                app_list(ui, "high_risk", list, input, open);
                suggestions(
                    ui,
                    list,
                    &[
                        ("Chat", &["discord.exe", "slack.exe", "ms-teams.exe", "telegram.exe", "whatsapp.exe"]),
                        ("Trading", &["tradingview.exe", "tws.exe", "thinkorswim.exe"]),
                        ("Monitoring", &["taskmgr.exe", "hwinfo64.exe", "msiafterburner.exe"]),
                    ],
                );
                ui.add_space(4.0);
                pct(ui, "Dim them by up to", dim, 0.0..=0.6);
            },
        );
    }

    fn windows(&mut self, ui: &mut egui::Ui) {
        let mut action: Option<(usize, bool)> = None;
        for (i, item) in hardening::ITEMS.iter().enumerate() {
            let applied = self.tweak_state.get(i).copied().unwrap_or(false);
            let has_backup = self.tweak_backup.get(i).copied().unwrap_or(false);
            card(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(item.title).size(15.0).strong());
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let mut on = applied;
                        let r = toggle(ui, &mut on);
                        let r = if applied && !has_backup {
                            r.on_hover_text("This was already set before Wanelight. Change it in Windows Settings.")
                        } else {
                            r
                        };
                        if r.changed() {
                            action = Some((i, on));
                        }
                    });
                });
                ui.label(RichText::new(item.detail).weak());
            });
        }
        if let Some((i, apply)) = action {
            let key = hardening::ITEMS[i].key;
            let has_backup = self.tweak_backup.get(i).copied().unwrap_or(false);
            self.tweak_msg = if apply {
                hardening::apply(key).err()
            } else if has_backup {
                hardening::revert(key).err()
            } else {
                Some("This was already set before Wanelight, so there's nothing to put back. Change it in Windows Settings.".into())
            };
            self.refresh_tweaks();
        }
        if let Some(msg) = &self.tweak_msg {
            ui.colored_label(AMBER, msg);
        }
    }

    fn nav(&mut self, ui: &mut egui::Ui) {
        ui.add_space(14.0);
        ui.horizontal(|ui| {
            ui.add_space(12.0);
            ui.label(RichText::new("Wanelight").size(18.0).strong());
        });
        ui.add_space(12.0);
        let first_secondary = if MORE_DIMMING { Tab::More } else { Tab::Wear };
        for tab in Tab::ALL.into_iter().filter(|&t| t != Tab::More || MORE_DIMMING) {
            if tab == first_secondary {
                ui.add_space(6.0);
                let y = ui.cursor().top();
                let x = ui.max_rect().x_range();
                ui.painter().hline((x.min + 12.0)..=(x.max - 12.0), y, Stroke::new(1.0, CARD_LINE));
                ui.add_space(8.0);
            }
            let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 32.0), Sense::click());
            let selected = self.tab == tab;
            let rect = rect.shrink2(vec2(6.0, 1.0));
            if selected {
                ui.painter().rect_filled(rect, 6.0, Color32::from_rgb(0x2c, 0x26, 0x1c));
                let bar = egui::Rect::from_min_size(rect.min + vec2(0.0, 8.0), vec2(3.0, rect.height() - 16.0));
                ui.painter().rect_filled(bar, 1.5, AMBER);
            } else if resp.hovered() {
                ui.painter().rect_filled(rect, 6.0, Color32::from_gray(0x22));
            }
            let color = if selected { Color32::from_gray(0xf0) } else { Color32::from_gray(0xb4) };
            ui.painter().text(rect.left_center() + vec2(14.0, 0.0), Align2::LEFT_CENTER, tab.label(), FontId::proportional(14.0), color);
            if resp.clicked() {
                self.tab = tab;
                if tab == Tab::Windows {
                    self.refresh_tweaks();
                }
                if tab == Tab::Apps {
                    self.open_apps = open_apps();
                }
            }
        }
        ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.add_space(12.0);
                ui.label(RichText::new(format!("v{}", env!("CARGO_PKG_VERSION"))).weak().small());
            });
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.add_space(8.0);
                if ui.button("Exit Wanelight").on_hover_text("Stops protecting the screen and closes this window.").clicked() {
                    ipc::send_command(ipc::CMD_QUIT, 0);
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
        });
    }
}

fn dot(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(14.0, 14.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), 5.0, color);
}

fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let r = egui::Frame::new()
        .fill(CARD_BG)
        .stroke(Stroke::new(1.0, CARD_LINE))
        .corner_radius(10.0)
        .inner_margin(egui::Margin::symmetric(18, 14))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner;
    ui.add_space(6.0);
    r
}

/// A card for one feature: title, a line of explanation, an optional on/off
/// switch, and controls that only show while it's on.
fn feature(
    ui: &mut egui::Ui,
    title: &str,
    detail: &str,
    on: Option<&mut bool>,
    right: impl FnOnce(&mut egui::Ui),
    body: impl FnOnce(&mut egui::Ui),
) {
    card(ui, |ui| {
        let show = ui
            .horizontal(|ui| {
                ui.label(RichText::new(title).size(16.0).strong());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let show = match on {
                        Some(on) => {
                            toggle(ui, on);
                            *on
                        }
                        None => true,
                    };
                    right(ui);
                    show
                })
                .inner
            })
            .inner;
        ui.label(RichText::new(detail).weak());
        if show {
            ui.add_space(6.0);
            body(ui);
        }
    });
}

fn section(ui: &mut egui::Ui, title: &str) {
    ui.add_space(12.0);
    ui.label(RichText::new(title).strong().color(Color32::from_gray(0xc8)));
    ui.add_space(2.0);
}

/// A sub-heading inside a card.
fn group(ui: &mut egui::Ui, title: &str, detail: &str) {
    ui.add_space(8.0);
    ui.label(RichText::new(title).strong());
    if !detail.is_empty() {
        ui.label(RichText::new(detail).weak().small());
    }
}

fn indented<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.horizontal(|ui| {
        ui.add_space(46.0);
        ui.vertical(add).inner
    })
    .inner
}

/// A labelled line: the label in a fixed column, then the control.
fn row<R>(ui: &mut egui::Ui, label: &str, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.horizontal(|ui| {
        let h = ui.spacing().interact_size.y;
        ui.allocate_ui_with_layout(vec2(LABEL_W, h), Layout::left_to_right(Align::Center), |ui| {
            ui.set_min_width(LABEL_W);
            ui.label(RichText::new(label).color(Color32::from_gray(0xc8)));
        });
        add(ui)
    })
    .inner
}

/// Sliders never rewrite a value just by being shown; only user edits are clamped.
fn slider<'a, N: egui::emath::Numeric>(v: &'a mut N, range: std::ops::RangeInclusive<N>) -> egui::Slider<'a> {
    egui::Slider::new(v, range).clamping(egui::SliderClamping::Edits)
}

fn pct(ui: &mut egui::Ui, label: &str, v: &mut f32, range: std::ops::RangeInclusive<f32>) {
    row(ui, label, |ui| ui.add(slider(v, range).custom_formatter(|v, _| format!("{:.0}%", v * 100.0))));
}

fn mins(ui: &mut egui::Ui, label: &str, secs: &mut u32, range: std::ops::RangeInclusive<u32>) {
    let mut m = *secs as f32 / 60.0;
    let r = row(ui, label, |ui| {
        ui.add(
            slider(&mut m, *range.start() as f32..=*range.end() as f32)
                .step_by(1.0)
                .custom_formatter(|v, _| if v < 1.0 { format!("{:.0} s", v * 60.0) } else { format!("{v:.0} min") }),
        )
    });
    if r.changed() {
        *secs = (m.round().max(1.0) * 60.0) as u32;
    }
}

/// An on/off switch.
fn toggle(ui: &mut egui::Ui, on: &mut bool) -> egui::Response {
    let (rect, mut resp) = ui.allocate_exact_size(vec2(38.0, 22.0), Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::Checkbox, ui.is_enabled(), *on, ""));
    let t = ui.ctx().animate_bool_responsive(resp.id, *on);
    let off_fill = if resp.hovered() { Color32::from_gray(0x5a) } else { Color32::from_gray(0x48) };
    let r = rect.height() / 2.0;
    ui.painter().rect_filled(rect, r, off_fill.lerp_to_gamma(AMBER, t));
    let x = egui::lerp((rect.left() + r)..=(rect.right() - r), t);
    let knob = Color32::from_gray(0xe0).lerp_to_gamma(Color32::from_rgb(0x24, 0x1c, 0x0e), t);
    ui.painter().circle_filled(pos2(x, rect.center().y), r - 4.0, knob);
    resp
}

/// A switch with a clickable label to its right.
fn switch(ui: &mut egui::Ui, on: &mut bool, label: &str) -> egui::Response {
    ui.horizontal(|ui| {
        let mut r = toggle(ui, on);
        ui.add_space(4.0);
        if ui.add(egui::Label::new(label).sense(Sense::click())).clicked() {
            *on = !*on;
            r.mark_changed();
        }
        r
    })
    .inner
}

/// A row of mutually exclusive buttons; returns the one clicked.
fn segmented(ui: &mut egui::Ui, current: Option<usize>, options: &[(usize, &str)]) -> Option<usize> {
    let mut picked = None;
    egui::Frame::new().fill(Color32::from_gray(0x1a)).corner_radius(7.0).inner_margin(3).show(ui, |ui| {
        ui.spacing_mut().item_spacing.x = 3.0;
        ui.horizontal(|ui| {
            for &(v, name) in options {
                let sel = current == Some(v);
                let text = RichText::new(name).color(if sel { Color32::from_gray(0x10) } else { Color32::from_gray(0xc0) });
                let b = egui::Button::new(text)
                    .fill(if sel { AMBER } else { Color32::TRANSPARENT })
                    .stroke(Stroke::NONE)
                    .corner_radius(5.0)
                    .min_size(vec2(84.0, 24.0));
                if ui.add(b).clicked() {
                    picked = Some(v);
                }
            }
        });
    });
    picked
}

/// A large card for a main feature, used on Home.
#[allow(clippy::too_many_arguments)]
fn primary(
    ui: &mut egui::Ui,
    title: &str,
    detail: &str,
    on: &mut bool,
    id: &'static str,
    hotkey: &mut String,
    recording: &mut Option<&'static str>,
    preview: impl FnOnce(&mut egui::Ui),
    open: impl FnOnce(),
) {
    card(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new(title).size(18.0).strong());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| toggle(ui, on));
        });
        ui.label(RichText::new(detail).weak());
        ui.add_space(6.0);
        preview(ui);
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            shortcut(ui, id, hotkey, recording);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.link("Settings ›").clicked() {
                    open();
                }
            });
        });
    });
}

/// Grays from black to #606060, as they look with or without deeper blacks.
fn gray_strip(ui: &mut egui::Ui, level: f32, crushed: bool, width: f32, height: f32) {
    use egui::ecolor::{gamma_u8_from_linear_f32, linear_f32_from_gamma_u8};
    const STEPS: usize = 13;
    let (rect, _) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
    let t = linear_f32_from_gamma_u8((level * 255.0).round() as u8).min(0.9);
    let w = rect.width() / STEPS as f32;
    for i in 0..STEPS {
        let g = (i * 8) as u8;
        let shown = if crushed { gamma_u8_from_linear_f32(((linear_f32_from_gamma_u8(g) - t) / (1.0 - t)).max(0.0)) } else { g };
        let r = egui::Rect::from_min_size(rect.min + vec2(i as f32 * w, 0.0), vec2(w + 0.5, rect.height()));
        ui.painter().rect_filled(r, 0.0, Color32::from_gray(shown));
    }
    ui.painter().rect_stroke(rect, 3.0, Stroke::new(1.0, Color32::from_gray(0x3c)), StrokeKind::Outside);
}

/// A small picture of what a torch style keeps lit.
fn paint_torch(p: &egui::Painter, screen: egui::Rect, mode: TorchMode) {
    let dim = Color32::from_gray(0x3a);
    let dim_win = Color32::from_gray(0x4a);
    let lit = Color32::from_rgb(0xec, 0xe6, 0xda);
    let lit_line = Color32::from_gray(0xb8);
    let at = |x: f32, y: f32, w: f32, h: f32| {
        egui::Rect::from_min_size(screen.min + vec2(x * screen.width(), y * screen.height()), vec2(w * screen.width(), h * screen.height()))
    };
    p.rect_filled(screen, 3.0, dim);
    match mode {
        TorchMode::Window => {
            p.rect_filled(at(0.55, 0.08, 0.4, 0.5), 2.0, dim_win);
            p.rect_filled(at(0.06, 0.18, 0.56, 0.66), 2.0, lit);
            p.rect_filled(at(0.06, 0.18, 0.56, 0.12), 2.0, lit_line);
        }
        TorchMode::Spotlight => {
            p.rect_filled(at(0.08, 0.12, 0.84, 0.76), 2.0, dim_win);
            p.circle_filled(at(0.5, 0.5, 0.0, 0.0).min, screen.height() * 0.28, lit);
        }
        TorchMode::Panel => {
            p.rect_filled(at(0.0, 0.0, 0.24, 1.0), 0.0, dim_win);
            p.rect_filled(at(0.26, 0.0, 0.5, 1.0), 0.0, lit);
            p.rect_filled(at(0.78, 0.0, 0.22, 1.0), 0.0, dim_win);
            for i in 0..3 {
                p.rect_filled(at(0.3, 0.14 + i as f32 * 0.2, 0.3, 0.08), 1.0, lit_line);
            }
        }
    }
}

/// Pictures of the three torch styles, clickable.
fn torch_tiles(ui: &mut egui::Ui, mode: &mut TorchMode) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 10.0;
        for (m, name) in [(TorchMode::Window, "Window"), (TorchMode::Spotlight, "Spotlight"), (TorchMode::Panel, "Panel")] {
            let (rect, resp) = ui.allocate_exact_size(vec2(140.0, 104.0), Sense::click());
            if resp.clicked() {
                *mode = m;
            }
            let sel = *mode == m;
            let p = ui.painter();
            p.rect_filled(rect, 8.0, if resp.hovered() && !sel { Color32::from_gray(0x2a) } else { Color32::from_gray(0x1e) });
            p.rect_stroke(
                rect,
                8.0,
                if sel { Stroke::new(2.0, AMBER) } else { Stroke::new(1.0, CARD_LINE) },
                StrokeKind::Inside,
            );
            let screen = egui::Rect::from_min_size(rect.min + vec2(14.0, 12.0), vec2(112.0, 62.0));
            paint_torch(p, screen, m);
            let color = if sel { Color32::from_gray(0xf0) } else { Color32::from_gray(0xa8) };
            p.text(pos2(rect.center().x, rect.bottom() - 15.0), Align2::CENTER_CENTER, name, FontId::proportional(14.0), color);
        }
    });
}

/// Shows a shortcut; click it, then press the new keys.
fn shortcut(ui: &mut egui::Ui, id: &'static str, value: &mut String, recording: &mut Option<&'static str>) {
    if *recording == Some(id) {
        let r = ui.add(egui::Button::new(RichText::new("Press keys…").color(AMBER).small()).stroke(Stroke::new(1.0, AMBER)));
        let events = ui.input(|i| i.events.clone());
        for e in events {
            let egui::Event::Key { key, pressed: true, modifiers, .. } = e else { continue };
            match key {
                Key::Escape => *recording = None,
                Key::Backspace | Key::Delete => {
                    value.clear();
                    *recording = None;
                }
                k => {
                    let name = k.name();
                    let usable = (name.len() == 1 && name.chars().all(|c| c.is_ascii_alphanumeric()))
                        || (name.len() > 1 && name.starts_with('F') && name[1..].chars().all(|c| c.is_ascii_digit()));
                    if usable && (modifiers.ctrl || modifiers.alt || modifiers.shift) {
                        let mut s = String::new();
                        for (held, part) in [(modifiers.ctrl, "Ctrl+"), (modifiers.alt, "Alt+"), (modifiers.shift, "Shift+")] {
                            if held {
                                s.push_str(part);
                            }
                        }
                        s.push_str(name);
                        *value = s;
                        *recording = None;
                    }
                }
            }
        }
        if r.clicked_elsewhere() {
            *recording = None;
        }
        r.on_hover_text("Hold Ctrl, Alt or Shift and press a letter, number or F-key. Backspace removes the shortcut, Esc cancels.");
    } else {
        let text = if value.is_empty() { "Add shortcut".to_string() } else { value.replace('+', " + ") };
        let b = egui::Button::new(RichText::new(text).small().color(Color32::from_gray(0xb0)))
            .fill(Color32::from_gray(0x1a))
            .stroke(Stroke::new(1.0, Color32::from_gray(0x3c)));
        if ui.add(b).on_hover_text("Keyboard shortcut. Click to change it.").clicked() {
            *recording = Some(id);
        }
    }
}

fn add_all(list: &mut Vec<String>, names: &[&str]) {
    for n in names {
        if !list.iter().any(|x| x == n) {
            list.push(n.to_string());
        }
    }
}

fn suggestions(ui: &mut egui::Ui, list: &mut Vec<String>, groups: &[(&str, &[&str])]) {
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new("Suggestions:").weak());
        for (name, apps) in groups {
            if ui.small_button(format!("+ {name}")).on_hover_text(apps.join(", ")).clicked() {
                add_all(list, apps);
            }
        }
    });
}

fn app_list(ui: &mut egui::Ui, id: &str, list: &mut Vec<String>, input: &mut String, open: &[String]) {
    let mut remove = None;
    ui.horizontal_wrapped(|ui| {
        for (i, name) in list.iter().enumerate() {
            let b = egui::Button::new(format!("{name}   ×")).fill(Color32::from_gray(0x30)).corner_radius(12.0);
            if ui.add(b).on_hover_text("Remove").clicked() {
                remove = Some(i);
            }
        }
        if list.is_empty() {
            ui.label(RichText::new("No apps yet.").weak());
        }
    });
    if let Some(i) = remove {
        list.remove(i);
    }
    ui.add_space(2.0);
    ui.horizontal(|ui| {
        ui.menu_button("Add an open app  ⏷", |ui| {
            let choices: Vec<_> = open.iter().filter(|n| !list.contains(n)).collect();
            if choices.is_empty() {
                ui.label(RichText::new("No other apps are open.").weak());
            }
            for name in choices {
                if ui.button(name).clicked() {
                    add_all(list, &[name]);
                }
            }
        });
        ui.label(RichText::new("or type a name").weak());
        let edit = ui.add(egui::TextEdit::singleline(input).id_salt(id).hint_text("app.exe").desired_width(160.0));
        let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
        if (ui.button("Add").clicked() || enter) && !input.trim().is_empty() {
            let mut name = input.trim().to_ascii_lowercase();
            if !name.ends_with(".exe") {
                name.push_str(".exe");
            }
            add_all(list, &[&name]);
            input.clear();
        }
    });
}

/// Executable names of apps with a visible window, for the app lists.
fn open_apps() -> Vec<String> {
    unsafe extern "system" fn collect(h: HWND, lp: LPARAM) -> BOOL {
        unsafe {
            let pids = &mut *(lp.0 as *mut Vec<u32>);
            let tool = GetWindowLongW(h, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW.0 != 0;
            if IsWindowVisible(h).as_bool() && !tool && GetWindowTextLengthW(h) > 0 && GetWindow(h, GW_OWNER).is_err() {
                let mut pid = 0;
                GetWindowThreadProcessId(h, Some(&mut pid));
                pids.push(pid);
            }
            BOOL(1)
        }
    }
    let mut pids: Vec<u32> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(collect), LPARAM(&mut pids as *mut _ as isize));
    }
    let me = std::process::id();
    let mut names: Vec<String> = pids
        .into_iter()
        .filter(|&p| p != me)
        .filter_map(util::process_name)
        .filter(|n| !n.is_empty() && n != "wanelight.exe" && n != "applicationframehost.exe")
        .collect();
    names.sort();
    names.dedup();
    names
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll(ui.ctx());
        egui::Panel::left("nav")
            .resizable(false)
            .exact_size(196.0)
            .frame(egui::Frame::new().fill(NAV_BG))
            .show(ui, |ui| self.nav(ui));
        egui::CentralPanel::default().frame(egui::Frame::new().fill(PAGE_BG).inner_margin(egui::Margin::symmetric(28, 20))).show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                ui.set_max_width(ui.available_width().min(780.0));
                if self.tab.has_switch() {
                    let (on, id, hotkey) = match self.tab {
                        Tab::Torch => (&mut self.cfg.torch.enabled, "torch", &mut self.cfg.torch.hotkey),
                        _ => (&mut self.cfg.blacks.enabled, "blacks", &mut self.cfg.blacks.hotkey),
                    };
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(self.tab.label()).heading().strong());
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            toggle(ui, on);
                            shortcut(ui, id, hotkey, &mut self.recording);
                        });
                    });
                    ui.label(RichText::new(self.tab.intro()).weak());
                    ui.add_space(12.0);
                } else if self.tab != Tab::Home {
                    ui.label(RichText::new(self.tab.label()).heading().strong());
                    ui.label(RichText::new(self.tab.intro()).weak());
                    ui.add_space(12.0);
                }
                match self.tab {
                    Tab::Home => self.home(ui),
                    Tab::Torch => self.torch(ui),
                    Tab::Blacks => self.blacks(ui),
                    Tab::More => self.more(ui),
                    Tab::Away => self.away(ui),
                    Tab::Apps => self.apps(ui),
                    Tab::Wear => self.heat.show(ui, self.agent_running),
                    Tab::Windows => self.windows(ui),
                }
                ui.add_space(12.0);
            });
        });
        if self.cfg != self.saved {
            let cfg = self.cfg.clone().sanitized();
            if cfg.save().is_ok() {
                self.saved = self.cfg.clone();
            }
        }
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(1000));
    }
}
