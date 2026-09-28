//! Settings window. Runs as a separate process (`wanelight --ui <tab>`) so the
//! always-on agent never loads a GUI toolkit. Settings are saved to the config
//! file as they change; the agent picks them up within a second.

mod heatmap;

use std::sync::Arc;

use eframe::egui::{self, Color32, RichText};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, SW_RESTORE, SetForegroundWindow, ShowWindow};
use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
use windows::core::{PCWSTR, w};

use crate::config::{Config, TorchMode};
use crate::ipc::{self, Status};
use crate::{autostart, hardening, icon_art, util};

const TITLE: &str = "Wanelight Settings";
const AMBER: Color32 = Color32::from_rgb(0xf5, 0xc0, 0x6a);

pub fn launch(tab: &str) {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::process::Command::new(exe).args(["--ui", tab]).spawn();
    }
}

fn request_path() -> std::path::PathBuf {
    util::data_dir().join("ui-request.txt")
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tab {
    Overview,
    Heatmap,
    Protection,
    Focus,
    Away,
    Apps,
    Tweaks,
}

impl Tab {
    const ALL: [Tab; 7] = [Tab::Overview, Tab::Heatmap, Tab::Protection, Tab::Focus, Tab::Away, Tab::Apps, Tab::Tweaks];
    fn parse(s: &str) -> Tab {
        match s.trim() {
            "heatmap" => Tab::Heatmap,
            "protection" => Tab::Protection,
            "focus" => Tab::Focus,
            "away" => Tab::Away,
            "apps" => Tab::Apps,
            "tweaks" => Tab::Tweaks,
            _ => Tab::Overview,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Tab::Overview => "Overview",
            Tab::Heatmap => "Wear heatmap",
            Tab::Protection => "Static dimming",
            Tab::Focus => "Focus modes",
            Tab::Away => "Away & power",
            Tab::Apps => "Apps",
            Tab::Tweaks => "Windows tweaks",
        }
    }
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
    let icon = icon_art::app_icon_rgba(64);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(TITLE)
            .with_inner_size([860.0, 620.0])
            .with_min_inner_size([680.0, 480.0])
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
}

impl App {
    fn new(cc: &eframe::CreationContext, tab: Tab) -> Self {
        let mut visuals = egui::Visuals::dark();
        visuals.selection.bg_fill = Color32::from_rgb(0x6b, 0x4f, 0x1d);
        visuals.hyperlink_color = AMBER;
        cc.egui_ctx.set_theme(egui::Theme::Dark);
        cc.egui_ctx.set_visuals_of(egui::Theme::Dark, visuals);
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
        };
        app.refresh_tweaks();
        app
    }

    fn refresh_tweaks(&mut self) {
        self.tweak_state = hardening::ITEMS.iter().map(|i| hardening::is_applied(i.key)).collect();
        self.tweak_backup = hardening::ITEMS.iter().map(|i| hardening::has_backup(i.key)).collect();
    }

    fn poll(&mut self) {
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
            self.tab = Tab::parse(&t);
        }
        // Pick up edits made elsewhere (another window, a text editor).
        let disk = Config::load();
        if disk != self.saved {
            self.cfg = disk.clone();
            self.saved = disk;
        }
    }

    fn overview(&mut self, ui: &mut egui::Ui) {
        ui.heading("Wanelight");
        ui.label("Dims only what stays still: static, bright areas fade down slowly and come back the moment they change.");
        ui.add_space(8.0);
        if !self.agent_running {
            ui.colored_label(AMBER, "Wanelight isn't running, so nothing is being protected.");
            if ui.button("Start Wanelight").clicked()
                && let Ok(exe) = std::env::current_exe() {
                    let _ = std::process::Command::new(exe).spawn();
                }
        } else if let Some(st) = &self.status {
            ui.label(RichText::new(&st.state).size(20.0).color(AMBER));
            ui.horizontal(|ui| {
                if st.paused {
                    if ui.button("Resume").clicked() {
                        ipc::send_command(ipc::CMD_RESUME, 0);
                    }
                } else {
                    if ui.button("Pause for 1 hour").clicked() {
                        ipc::send_command(ipc::CMD_PAUSE, 60);
                    }
                    if ui.button("Pause until resumed").clicked() {
                        ipc::send_command(ipc::CMD_PAUSE, 0);
                    }
                }
            });
            ui.add_space(4.0);
            let next = self.cfg.refresh.hours_between;
            ui.label(format!(
                "Panel on for {:.1} h since its last rest{}.",
                st.panel_hours_since_rest,
                if self.cfg.refresh.enabled {
                    format!(" (Wanelight lets it rest at a quiet moment after {next:.0} h)")
                } else {
                    String::new()
                }
            ));
        }
        ui.add_space(8.0);
        ui.checkbox(&mut self.cfg.enabled, "Protection enabled");
        let mut auto = self.autostart;
        if ui.checkbox(&mut auto, "Start with Windows").changed() && autostart::set(auto) {
            self.autostart = auto;
        }
        if !self.cfg.hotkey.is_empty() {
            ui.label(RichText::new(format!("Pause / resume anytime with {}", self.cfg.hotkey)).weak());
        }
        ui.add_space(12.0);
        ui.label(RichText::new("Displays").strong());
        let monitors = self.status.as_ref().map(|s| s.monitors.clone()).unwrap_or_default();
        if monitors.is_empty() {
            ui.label(RichText::new("No display information yet.").weak());
        }
        for m in monitors {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&m.name).strong());
                    ui.label(RichText::new(format!("{}x{}{}", m.width, m.height, if m.hdr { " HDR" } else { "" })).weak());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let entry = self.cfg.monitors.entry(m.id.clone()).or_default();
                        ui.checkbox(&mut entry.enabled, "Protect");
                    });
                });
                if !m.capturing {
                    ui.colored_label(AMBER, "Screen sampling unavailable (locked, protected content or GPU limits); away and power features still work.");
                }
                ui.label(format!(
                    "{:.0}% of the screen has been static for over a minute · {:.0}% currently dimmed (strongest {:.0}%)",
                    m.static_fraction * 100.0,
                    m.dimmed_fraction * 100.0,
                    m.max_dim * 100.0
                ));
                let ddc = match m.ddc_supported {
                    Some(true) => "supports DDC/CI brightness",
                    Some(false) => "no DDC/CI brightness control",
                    None => "DDC/CI not checked yet",
                };
                ui.label(RichText::new(ddc).weak());
            });
        }
    }

    fn protection(&mut self, ui: &mut egui::Ui) {
        let s = &mut self.cfg.static_dimming;
        ui.heading("Static dimming");
        ui.label("Areas that haven't changed for a while are darkened so gradually you won't notice. Anything that changes, or the window you switch to, brightens back within a fraction of a second.");
        ui.add_space(8.0);
        ui.checkbox(&mut s.enabled, "Dim static areas");
        ui.add_enabled_ui(s.enabled, |ui| {
            section(ui, "Everywhere except the window you're using", "Taskbar, side panels, other windows, the desktop.");
            percent(ui, &mut s.background_max_dim, 0.0..=0.6, "Maximum dim");
            minutes(ui, &mut s.background_after_secs, 1..=30, "Start after unchanged for");
            section(ui, "Inside the window you're using", "Kept light: toolbars and sidebars only, and much later.");
            percent(ui, &mut s.foreground_max_dim, 0.0..=0.3, "Maximum dim");
            minutes(ui, &mut s.foreground_after_secs, 5..=60, "Start after unchanged for");
            section(ui, "Unattended monitors", "With several displays: one that has had neither the cursor nor focus.");
            percent(ui, &mut s.neglected_monitor_dim, 0.0..=0.6, "Maximum dim");
            minutes(ui, &mut s.neglected_after_secs, 1..=60, "Counts as unattended after");
            section(ui, "High-risk apps", "Apps listed under Apps, e.g. chat or trading tools left open all day.");
            percent(ui, &mut s.high_risk_app_dim, 0.0..=0.6, "Maximum dim");
            section(ui, "Feel", "");
            ui.add(
                slider(&mut s.fade_in_percent_per_minute, 5.0..=120.0)
                    .text("Fade-in speed")
                    .custom_formatter(|v, _| format!("{v:.0}% per minute")),
            );
            ui.label(RichText::new("Around 20% per minute or less is below what most people can notice.").weak());
            ui.add(
                slider(&mut s.min_brightness, 0.0..=0.5)
                    .text("Ignore areas darker than")
                    .custom_formatter(|v, _| format!("{:.0}% of white", v * 100.0)),
            );
        });
        ui.add_space(8.0);
        ui.checkbox(&mut self.cfg.dim_fullscreen_apps, "Also dim static HUDs and logos in fullscreen games and videos");
        ui.label(RichText::new("Turn off for competitive gaming: on some GPUs any overlay above a game adds a frame of latency.").weak());
    }

    fn focus(&mut self, ui: &mut egui::Ui) {
        ui.heading("Focus modes");
        ui.label("Stronger, visible protection for people who live in one app all day. All are off until you turn them on.");

        let c = &mut self.cfg.chrome;
        section(
            ui,
            "Chrome hover-reveal",
            "When a window fills the screen, its unchanging toolbars, tab strips, sidebars and status bar dim hard. Move the pointer toward one, or hold Alt, and it lights up at once.",
        );
        ui.checkbox(&mut c.enabled, "Dim the toolbars and sidebars of full-screen windows");
        ui.add_enabled_ui(c.enabled, |ui| {
            percent(ui, &mut c.max_dim, 0.1..=0.9, "Dim to");
            ui.add(slider(&mut c.after_secs, 10..=600).text("After unchanged for").custom_formatter(|v, _| {
                if v < 60.0 { format!("{v:.0} s") } else { format!("{:.1} min", v / 60.0) }
            }));
            ui.add(
                slider(&mut c.reveal_px, 20..=400)
                    .text("Light up when the pointer is within")
                    .custom_formatter(|v, _| format!("{v:.0} px")),
            );
            ui.add(slider(&mut c.hold_secs, 0.5..=15.0).text("Stay lit for").custom_formatter(|v, _| format!("{v:.1} s")));
            ui.add(slider(&mut c.fade_secs, 0.5..=10.0).text("Fade over").custom_formatter(|v, _| format!("{v:.1} s")));
        });

        let hotkey = self.cfg.torch.hotkey.clone();
        let t = &mut self.cfg.torch;
        let detail = if hotkey.is_empty() {
            "Only what you're focused on stays lit; everything else dims. Also in the tray menu.".to_string()
        } else {
            format!("Only what you're focused on stays lit; everything else dims. Toggle anytime with {hotkey} or from the tray menu.")
        };
        section(ui, "Torch mode", &detail);
        ui.checkbox(&mut t.enabled, "Torch mode");
        ui.add_enabled_ui(t.enabled, |ui| {
            ui.radio_value(&mut t.mode, TorchMode::Window, "Light the window I'm using, plus the area around the pointer");
            ui.radio_value(&mut t.mode, TorchMode::Spotlight, "Light only a circle around the pointer");
            ui.radio_value(&mut t.mode, TorchMode::Panel, "Light only the panel I'm pointing at, or the text box I'm typing in");
            if t.mode == TorchMode::Panel {
                ui.label(
                    RichText::new(
                        "Panels are found through Windows accessibility (UI Automation), reading layout only, never text. \
                         Chromium and Electron apps turn on their accessibility support when asked, which costs them a \
                         little extra work; VS Code may ask whether you use a screen reader. Apps that expose no layout \
                         light up as a whole window.",
                    )
                    .weak(),
                );
            }
            percent(ui, &mut t.dim, 0.2..=0.95, "Dim everything else by");
            ui.label(RichText::new("While torch mode is on it replaces static and chrome dimming, so the lit area stays evenly bright.").weak());
            if t.mode == TorchMode::Spotlight {
                ui.add(
                    slider(&mut t.spotlight_radius_px, 100..=1500)
                        .text("Spotlight radius")
                        .custom_formatter(|v, _| format!("{v:.0} px")),
                );
            }
        });

        let b = &mut self.cfg.blacks;
        section(
            ui,
            "Deeper blacks",
            "Near-black grays in every app become true black, so those OLED pixels switch off. Whites stay as they are. Also in the tray menu.",
        );
        ui.checkbox(&mut b.enabled, "Deeper blacks");
        ui.add_enabled_ui(b.enabled, |ui| {
            ui.add(slider(&mut b.level, 0.0..=0.35).text("Black up to").custom_formatter(|v, _| {
                let g = (v * 255.0).round() as u8;
                format!("#{g:02X}{g:02X}{g:02X}")
            }));
        });
        ui.horizontal(|ui| {
            ui.label("Hotkey");
            ui.add(egui::TextEdit::singleline(&mut b.hotkey).hint_text("none, e.g. Ctrl+Alt+Shift+B").desired_width(200.0));
        });
        ui.label(RichText::new("Doesn't work while Windows Magnifier or colour filters are on.").weak());
    }

    fn away(&mut self, ui: &mut egui::Ui) {
        ui.heading("Away & power");
        let a = &mut self.cfg.away;
        section(ui, "When you step away", "Needs both no input and a still screen, so videos and games are never interrupted.");
        ui.checkbox(&mut a.enabled, "Fade the screen down");
        ui.add_enabled_ui(a.enabled, |ui| {
            minutes(ui, &mut a.dim_after_secs, 1..=60, "After no input for");
            percent(ui, &mut a.dim_amount, 0.2..=0.95, "Fade by");
            ui.add(slider(&mut a.fade_secs, 5.0..=120.0).text("over").custom_formatter(|v, _| format!("{v:.0} s")));
        });
        ui.add_space(4.0);
        let mut off_min = a.display_off_after_secs / 60;
        let r = ui.add(
            slider(&mut off_min, 0..=120)
                .text("Turn displays off after")
                .custom_formatter(|v, _| if v == 0.0 { "never".into() } else { format!("{v:.0} min") }),
        );
        if r.changed() {
            a.display_off_after_secs = off_min * 60;
        }
        ui.checkbox(&mut a.respect_audio, "Keep displays on while audio is playing");

        let d = &mut self.cfg.ddc;
        section(ui, "Monitor brightness (DDC/CI)", "Also lowers the monitor's own brightness while away. Covers exclusive-fullscreen games and the lock screen. Some monitors flash an on-screen message when this happens.");
        ui.checkbox(&mut d.enabled, "Lower monitor brightness while away");
        ui.add_enabled_ui(d.enabled, |ui| {
            ui.add(slider(&mut d.away_brightness_percent, 0..=90).text("Brightness while away").custom_formatter(|v, _| format!("{v:.0}% of normal")));
        });

        let r = &mut self.cfg.refresh;
        section(ui, "Pixel refresh helper", "OLED monitors run a short compensation cycle when they go to standby after hours of use. Wanelight turns the display off at a quiet moment so that cycle can run.");
        ui.checkbox(&mut r.enabled, "Let the panel rest after long sessions");
        ui.add_enabled_ui(r.enabled, |ui| {
            ui.add(slider(&mut r.hours_between, 1.0..=12.0).text("Every").custom_formatter(|v, _| format!("{v:.1} h of use")));
            ui.add(slider(&mut r.rest_minutes, 1..=60).text("A rest is").custom_formatter(|v, _| format!("{v:.0} min off")));
            ui.checkbox(&mut r.remind, "Remind me if the panel goes far too long without a rest");
        });
    }

    fn apps(&mut self, ui: &mut egui::Ui) {
        ui.heading("Apps");
        section(ui, "Never dim or recolour while these are in front", "For apps where you want exact colours (photo or video editing).");
        app_list(ui, "excluded", &mut self.cfg.apps.excluded, &mut self.new_excluded);
        ui.horizontal(|ui| {
            ui.label("Add common:");
            if ui.button("Video players").clicked() {
                add_all(&mut self.cfg.apps.excluded, &["vlc.exe", "mpc-hc64.exe", "mpv.exe", "potplayermini64.exe"]);
            }
            if ui.button("Photo & video editors").clicked() {
                add_all(&mut self.cfg.apps.excluded, &["photoshop.exe", "lightroom.exe", "resolve.exe", "afterfx.exe", "premiere pro.exe"]);
            }
        });
        section(ui, "High-risk apps", "Static apps often left open for hours get the stronger high-risk dim when not in front.");
        app_list(ui, "high_risk", &mut self.cfg.apps.high_risk, &mut self.new_high_risk);
        ui.horizontal(|ui| {
            ui.label("Add common:");
            if ui.button("Chat").clicked() {
                add_all(&mut self.cfg.apps.high_risk, &["discord.exe", "slack.exe", "ms-teams.exe", "telegram.exe", "whatsapp.exe"]);
            }
            if ui.button("Trading").clicked() {
                add_all(&mut self.cfg.apps.high_risk, &["tradingview.exe", "tws.exe", "thinkorswim.exe"]);
            }
            if ui.button("Monitoring").clicked() {
                add_all(&mut self.cfg.apps.high_risk, &["taskmgr.exe", "hwinfo64.exe", "msiafterburner.exe"]);
            }
        });
    }

    fn tweaks(&mut self, ui: &mut egui::Ui) {
        ui.heading("Windows tweaks");
        ui.label("One-time settings that reduce burn-in. Each can be undone here; your previous value is kept.");
        ui.add_space(8.0);
        let mut action: Option<(usize, bool)> = None;
        for (i, item) in hardening::ITEMS.iter().enumerate() {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(RichText::new(item.title).strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let applied = self.tweak_state.get(i).copied().unwrap_or(false);
                        let has_backup = self.tweak_backup.get(i).copied().unwrap_or(false);
                        if has_backup && ui.button("Undo").clicked() {
                            action = Some((i, false));
                        }
                        if applied {
                            ui.label(RichText::new("✔ on").color(AMBER));
                        } else if ui.button("Apply").clicked() {
                            action = Some((i, true));
                        }
                    });
                });
                ui.label(RichText::new(item.detail).weak());
            });
        }
        if let Some((i, apply)) = action {
            let key = hardening::ITEMS[i].key;
            let r = if apply { hardening::apply(key) } else { hardening::revert(key) };
            self.tweak_msg = r.err();
            self.refresh_tweaks();
        }
        if let Some(msg) = &self.tweak_msg {
            ui.colored_label(AMBER, msg);
        }
    }
}

fn section(ui: &mut egui::Ui, title: &str, detail: &str) {
    ui.add_space(10.0);
    ui.label(RichText::new(title).strong());
    if !detail.is_empty() {
        ui.label(RichText::new(detail).weak());
    }
}

/// Sliders never rewrite a value just by being shown; only user edits are clamped.
fn slider<'a, N: egui::emath::Numeric>(v: &'a mut N, range: std::ops::RangeInclusive<N>) -> egui::Slider<'a> {
    egui::Slider::new(v, range).clamping(egui::SliderClamping::Edits)
}

fn percent(ui: &mut egui::Ui, v: &mut f32, range: std::ops::RangeInclusive<f32>, text: &str) {
    ui.add(slider(v, range).text(text).custom_formatter(|v, _| format!("{:.0}%", v * 100.0)));
}

fn minutes(ui: &mut egui::Ui, secs: &mut u32, range: std::ops::RangeInclusive<u32>, text: &str) {
    let mut m = *secs as f32 / 60.0;
    let r = ui.add(
        slider(&mut m, *range.start() as f32..=*range.end() as f32)
            .step_by(1.0)
            .text(text)
            .custom_formatter(|v, _| if v < 1.0 { format!("{:.0} s", v * 60.0) } else { format!("{v:.0} min") }),
    );
    if r.changed() {
        *secs = (m.round().max(1.0) * 60.0) as u32;
    }
}

fn add_all(list: &mut Vec<String>, names: &[&str]) {
    for n in names {
        if !list.iter().any(|x| x == n) {
            list.push(n.to_string());
        }
    }
}

fn app_list(ui: &mut egui::Ui, id: &str, list: &mut Vec<String>, input: &mut String) {
    let mut remove = None;
    ui.horizontal_wrapped(|ui| {
        for (i, name) in list.iter().enumerate() {
            if ui.button(format!("{name}  ✕")).on_hover_text("Remove").clicked() {
                remove = Some(i);
            }
        }
        if list.is_empty() {
            ui.label(RichText::new("None").weak());
        }
    });
    if let Some(i) = remove {
        list.remove(i);
    }
    ui.horizontal(|ui| {
        let edit = ui.add(egui::TextEdit::singleline(input).id_salt(id).hint_text("program.exe").desired_width(200.0));
        let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
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

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.poll();
        egui::Panel::left("nav").resizable(false).exact_size(170.0).show(ui, |ui| {
            ui.add_space(10.0);
            for tab in Tab::ALL {
                if ui.selectable_label(self.tab == tab, tab.label()).clicked() {
                    self.tab = tab;
                    if tab == Tab::Tweaks {
                        self.refresh_tweaks();
                    }
                }
                ui.add_space(2.0);
            }
            ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
                ui.add_space(8.0);
                ui.label(RichText::new(format!("v{}", env!("CARGO_PKG_VERSION"))).weak().small());
            });
        });
        egui::CentralPanel::default_margins().show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| match self.tab {
                Tab::Overview => self.overview(ui),
                Tab::Heatmap => self.heat.show(ui, self.agent_running),
                Tab::Protection => self.protection(ui),
                Tab::Focus => self.focus(ui),
                Tab::Away => self.away(ui),
                Tab::Apps => self.apps(ui),
                Tab::Tweaks => self.tweaks(ui),
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
