//! Wear heatmap: accumulated light output per 16x16-pixel cell.

use eframe::egui::{self, Color32, ColorImage, RichText, TextureHandle, TextureOptions, pos2, vec2};

use crate::ipc;
use crate::ledger::Ledger;
use crate::util;

/// Single-hue sequential ramp (blue 700 -> 100). On a dark surface the low end
/// recedes into the background and heavy wear reads as the brightest.
const RAMP: [u32; 13] = [
    0x0d366b, 0x104281, 0x184f95, 0x1c5cab, 0x256abf, 0x2a78d6, 0x3987e5, 0x5598e7, 0x6da7ec, 0x86b6ef, 0x9ec5f4,
    0xb7d3f6, 0xcde2fb,
];

fn ramp(t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0) * (RAMP.len() - 1) as f32;
    let i = (t as usize).min(RAMP.len() - 2);
    let f = t - i as f32;
    let c = |v: u32, s: u32| ((v >> s) & 0xff) as f32;
    let mix = |s| (c(RAMP[i], s) + (c(RAMP[i + 1], s) - c(RAMP[i], s)) * f).round() as u8;
    Color32::from_rgb(mix(16), mix(8), mix(0))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Metric {
    Emitted,
    Avoided,
}

pub struct HeatmapView {
    ledgers: Vec<(String, Ledger)>,
    selected: usize,
    metric: Metric,
    texture: Option<TextureHandle>,
    texture_key: Option<(usize, Metric, f64)>,
    scale: f32,
    last_load: f64,
    confirm_reset: bool,
}

impl Default for HeatmapView {
    fn default() -> Self {
        Self {
            ledgers: Vec::new(),
            selected: 0,
            metric: Metric::Emitted,
            texture: None,
            texture_key: None,
            scale: 1.0,
            last_load: f64::NEG_INFINITY,
            confirm_reset: false,
        }
    }
}

fn hours(white_secs: f32) -> String {
    let h = white_secs / 3600.0;
    if h >= 10.0 { format!("{h:.0} h") } else if h >= 0.1 { format!("{h:.1} h") } else { format!("{:.0} min", white_secs / 60.0) }
}

impl HeatmapView {
    fn reload(&mut self, agent_running: bool) {
        let now = util::now();
        if now - self.last_load < 10.0 {
            return;
        }
        if agent_running {
            ipc::send_command(ipc::CMD_FLUSH_LEDGER, 0);
            // Give the agent a moment to write; the next reload picks it up otherwise.
            std::thread::sleep(std::time::Duration::from_millis(150));
        }
        self.last_load = now;
        self.ledgers = Ledger::load_all();
        self.texture_key = None;
        if self.selected >= self.ledgers.len() {
            self.selected = 0;
        }
    }

    fn values(&self) -> Option<&[f32]> {
        let (_, l) = self.ledgers.get(self.selected)?;
        Some(match self.metric {
            Metric::Emitted => &l.emitted,
            Metric::Avoided => &l.avoided,
        })
    }

    pub fn show(&mut self, ui: &mut egui::Ui, agent_running: bool) {
        self.reload(agent_running);
        if self.ledgers.is_empty() {
            ui.label(RichText::new("No data yet. Wanelight records wear while it runs, so check back in a few minutes.").weak());
            return;
        }
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("heat_monitor")
                .selected_text(self.ledgers[self.selected].1.name.clone())
                .show_ui(ui, |ui| {
                    for (i, (_, l)) in self.ledgers.iter().enumerate() {
                        ui.selectable_value(&mut self.selected, i, &l.name);
                    }
                });
            ui.add_space(8.0);
            let current = if self.metric == Metric::Emitted { 0 } else { 1 };
            match super::segmented(ui, Some(current), &[(0, "Light given off"), (1, "Light saved by dimming")]) {
                Some(0) => self.metric = Metric::Emitted,
                Some(_) => self.metric = Metric::Avoided,
                None => {}
            }
        });
        let (id, ledger) = &self.ledgers[self.selected];
        let (gw, gh) = (ledger.gw, ledger.gh);
        let total_e: f64 = ledger.emitted.iter().map(|&v| v as f64).sum();
        let total_a: f64 = ledger.avoided.iter().map(|&v| v as f64).sum();
        ui.label(format!(
            "Tracked for {:.1} h. Dimming saved {:.1}% of the light this screen would have given off.",
            ledger.seconds / 3600.0,
            if total_e + total_a > 0.0 { total_a / (total_e + total_a) * 100.0 } else { 0.0 }
        ));
        let id = id.clone();

        let key = (self.selected, self.metric, ledger.seconds);
        if self.texture_key != Some(key) {
            let vals = self.values().unwrap_or(&[]).to_vec();
            // Scale to the 99th percentile so one stray hot cell doesn't wash out the rest.
            let mut sorted: Vec<f32> = vals.iter().copied().filter(|v| *v > 0.0).collect();
            sorted.sort_by(|a, b| a.total_cmp(b));
            self.scale = sorted.get(sorted.len() * 99 / 100).copied().unwrap_or(1.0).max(1e-3);
            let pixels: Vec<Color32> = vals.iter().map(|v| ramp(v / self.scale)).collect();
            let image = ColorImage { size: [gw, gh], pixels, source_size: vec2(gw as f32, gh as f32) };
            self.texture = Some(ui.ctx().load_texture("wear", image, TextureOptions::NEAREST));
            self.texture_key = Some(key);
        }
        let Some(tex) = &self.texture else { return };
        let width = ui.available_width().min(1200.0);
        let size = vec2(width, width * gh as f32 / gw as f32);
        let resp = ui.add(egui::Image::from_texture((tex.id(), size)).sense(egui::Sense::hover()));
        if let Some(p) = resp.hover_pos() {
            let rel = (p - resp.rect.min) / resp.rect.size();
            let (cx, cy) = ((rel.x * gw as f32) as usize, (rel.y * gh as f32) as usize);
            if let Some(v) = self.values().and_then(|v| v.get(cy.min(gh - 1) * gw + cx.min(gw - 1))) {
                let label = match self.metric {
                    Metric::Emitted => format!("{} at full white", hours(*v)),
                    Metric::Avoided => format!("{} at full white saved", hours(*v)),
                };
                resp.on_hover_text_at_pointer(label);
            }
        }

        // Legend: the ramp with its end values.
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("0").weak());
            let (rect, _) = ui.allocate_exact_size(vec2(220.0, 12.0), egui::Sense::hover());
            let steps = 44;
            for i in 0..steps {
                let x0 = rect.left() + rect.width() * i as f32 / steps as f32;
                let x1 = rect.left() + rect.width() * (i + 1) as f32 / steps as f32;
                ui.painter().rect_filled(
                    egui::Rect::from_min_max(pos2(x0, rect.top()), pos2(x1 + 0.5, rect.bottom())),
                    0.0,
                    ramp((i as f32 + 0.5) / steps as f32),
                );
            }
            ui.label(RichText::new(format!("{}+ at full white", hours(self.scale))).weak());
        });

        ui.add_space(12.0);
        if self.confirm_reset {
            ui.horizontal(|ui| {
                ui.label("Clear the wear history for this display?");
                if ui.button("Clear").clicked() {
                    if agent_running {
                        ipc::send_command(ipc::CMD_RESET_LEDGER, 0);
                    } else {
                        let _ = std::fs::remove_file(Ledger::path(&id));
                    }
                    self.confirm_reset = false;
                    self.last_load = f64::NEG_INFINITY;
                }
                if ui.button("Cancel").clicked() {
                    self.confirm_reset = false;
                }
            });
        } else if ui.button("Clear history…").clicked() {
            self.confirm_reset = true;
        }
    }
}
