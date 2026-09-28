//! Per-monitor static-content model and the dimming policy built on it.

use super::capture::{Cell, Sample};
use super::winmap::{Kind, OWNER_NONE, Snapshot};
use crate::config::Config;

#[derive(Clone, Copy, Default)]
pub struct CellState {
    /// Seconds since the cell's content last changed.
    pub static_secs: f32,
    pub mean: f32,
    pub max: f32,
    pub has_data: bool,
}

pub struct Model {
    pub cells: Vec<CellState>,
    /// Last time a meaningful part of the screen changed.
    pub last_activity: f64,
    pub capturing: bool,
}

impl Model {
    pub fn new(len: usize, now: f64) -> Self {
        Self { cells: vec![CellState::default(); len], last_activity: now, capturing: false }
    }

    pub fn apply(&mut self, sample: &Sample, dt: f32, now: f64) {
        match sample {
            Sample::Unavailable => self.capturing = false,
            Sample::NoChange => {
                self.capturing = true;
                for c in &mut self.cells {
                    c.static_secs += dt;
                }
            }
            Sample::Frame { cells, exact } => {
                self.capturing = true;
                let mut changed = 0usize;
                for (st, s) in self.cells.iter_mut().zip(cells.iter()) {
                    if cell_changed(st, s, *exact) {
                        st.static_secs = 0.0;
                        changed += 1;
                    } else {
                        st.static_secs += dt;
                    }
                    st.mean = s.mean;
                    st.max = s.max;
                    st.has_data = true;
                }
                // A clock tick or a blinking caret is not "activity"; a video or scrolling is.
                let significant = (self.cells.len() / 500).max(3);
                if changed >= significant {
                    self.last_activity = now;
                }
            }
        }
    }

    /// Fraction of cells unchanged for at least `secs`.
    pub fn static_fraction(&self, secs: f32) -> f32 {
        if self.cells.is_empty() {
            return 0.0;
        }
        let n = self.cells.iter().filter(|c| c.has_data && c.static_secs >= secs).count();
        n as f32 / self.cells.len() as f32
    }
}

fn cell_changed(st: &CellState, s: &Cell, exact: bool) -> bool {
    if exact {
        s.changed >= (0.015 * s.n).max(4.0)
    } else {
        // No previous frame to diff against (fresh duplication): compare stats.
        st.has_data && ((s.mean - st.mean).abs() > 0.003 || (s.max - st.max).abs() > 0.02)
    }
}

fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Inputs to the per-cell policy for one monitor.
pub struct PolicyCtx<'a> {
    pub cfg: &'a Config,
    pub snap: &'a Snapshot,
    pub owners: &'a [u16],
    /// Per window index: is it a "high risk" app (chat, trading, ...)?
    pub high_risk: &'a [bool],
    pub neglected: bool,
    /// Static dimming is suspended on this monitor (paused, excluded app, ...).
    pub suspended: bool,
}

/// Computes the dim each cell should settle at (0 = untouched).
pub fn targets(model: &Model, p: &PolicyCtx, out: &mut [f32]) {
    let s = &p.cfg.static_dimming;
    if p.suspended || !s.enabled {
        out.fill(0.0);
        return;
    }
    for (i, (st, t)) in model.cells.iter().zip(out.iter_mut()).enumerate() {
        *t = 0.0;
        if !st.has_data {
            continue;
        }
        // Dim only what is actually lit: dark areas barely wear.
        let weight = smoothstep(s.min_brightness, s.min_brightness + 0.35, st.max);
        if weight <= 0.0 {
            continue;
        }
        let owner = p.owners.get(i).copied().unwrap_or(OWNER_NONE);
        let win = (owner != OWNER_NONE).then(|| &p.snap.wins[owner as usize]);
        let in_foreground = win.map(|w| w.root == p.snap.fg_root && w.kind == Kind::Normal).unwrap_or(false);
        let (after, cap) = if in_foreground {
            (s.foreground_after_secs, s.foreground_max_dim)
        } else {
            let mut cap = s.background_max_dim;
            if p.neglected {
                cap = cap.max(s.neglected_monitor_dim);
            }
            if owner != OWNER_NONE && p.high_risk.get(owner as usize).copied().unwrap_or(false) {
                cap = cap.max(s.high_risk_app_dim);
            }
            (s.background_after_secs, cap)
        };
        if st.static_secs >= after as f32 {
            *t = cap * weight;
        }
    }
}

/// Moves `cur` toward `target`: slowly when dimming, quickly when releasing.
/// Returns true while a release is still animating.
pub fn ramp(cur: &mut [f32], target: &[f32], dt: f32, up_per_sec: f32, down_per_sec: f32) -> bool {
    let mut animating = false;
    for (c, &t) in cur.iter_mut().zip(target.iter()) {
        if t > *c {
            *c = (*c + up_per_sec * dt).min(t);
        } else if t < *c {
            *c = (*c - down_per_sec * dt).max(t);
            if *c > t {
                animating = true;
            }
        }
    }
    animating
}

/// Feathers the dim mask so no hard edges appear: each cell becomes the max of
/// itself and a 3x3 blur, then the global "away" dim is composited on top.
pub fn compose(cur: &[f32], gw: usize, gh: usize, away: f32, out: &mut Vec<f32>) {
    out.clear();
    out.resize(cur.len(), 0.0);
    const K: [f32; 3] = [1.0, 2.0, 1.0];
    for y in 0..gh {
        for x in 0..gw {
            let mut acc = 0.0;
            let mut wsum = 0.0;
            for (dy, ky) in K.iter().enumerate() {
                let yy = y as isize + dy as isize - 1;
                if yy < 0 || yy >= gh as isize {
                    continue;
                }
                for (dx, kx) in K.iter().enumerate() {
                    let xx = x as isize + dx as isize - 1;
                    if xx < 0 || xx >= gw as isize {
                        continue;
                    }
                    let w = ky * kx;
                    acc += cur[yy as usize * gw + xx as usize] * w;
                    wsum += w;
                }
            }
            let v = cur[y * gw + x].max(acc / wsum);
            out[y * gw + x] = 1.0 - (1.0 - v) * (1.0 - away);
        }
    }
}
