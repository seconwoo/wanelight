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

/// How a cell animates toward its target.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum Motion {
    /// Static dimming: creeps in linearly, below the threshold of notice.
    #[default]
    Gentle,
    /// Chrome bands: a deliberate, smooth fade over about `fade_secs`.
    Chrome,
    /// Torch mode: quick but eased.
    Torch,
}

/// Edge bands of a maximized/fullscreen foreground window, in cell units.
pub struct ChromeCtx {
    /// Window cell range [x0, x1) x [y0, y1).
    pub rect: (usize, usize, usize, usize),
    /// Band depth in cells: top, bottom, left, right.
    pub depth: [usize; 4],
    pub revealed: [bool; 4],
    pub dim: f32,
    pub after_secs: f32,
}

impl ChromeCtx {
    /// Which band (0 top, 1 bottom, 2 left, 3 right) a cell falls in, if any.
    pub fn zone(&self, x: usize, y: usize) -> Option<usize> {
        let (x0, y0, x1, y1) = self.rect;
        if x < x0 || x >= x1 || y < y0 || y >= y1 {
            return None;
        }
        if y < y0 + self.depth[0] {
            Some(0)
        } else if y + self.depth[1] >= y1 {
            Some(1)
        } else if x < x0 + self.depth[2] {
            Some(2)
        } else if x + self.depth[3] >= x1 {
            Some(3)
        } else {
            None
        }
    }
}

pub struct TorchCtx {
    pub dim: f32,
    /// Keep the foreground window lit.
    pub lit_foreground: bool,
    /// Keep this rectangle lit: [x0, y0, x1, y1] in cell units.
    pub lit_rect: Option<[f32; 4]>,
    /// Lit circle around the cursor: centre (cells), radius and feather (cells).
    pub halo: Option<(f32, f32, f32, f32)>,
}

/// Inputs to the per-cell policy for one monitor.
pub struct PolicyCtx<'a> {
    pub cfg: &'a Config,
    pub snap: &'a Snapshot,
    pub owners: &'a [u16],
    pub gw: usize,
    /// Per window index: is it a "high risk" app (chat, trading, ...)?
    pub high_risk: &'a [bool],
    pub neglected: bool,
    /// All dimming is suspended on this monitor (paused, excluded app, ...).
    pub suspended: bool,
    pub chrome: Option<ChromeCtx>,
    pub torch: Option<TorchCtx>,
}

/// Computes the dim each cell should settle at (0 = untouched) and how it moves there.
pub fn targets(model: &Model, p: &PolicyCtx, out: &mut [f32], motion: &mut [Motion]) {
    out.fill(0.0);
    motion.fill(Motion::Gentle);
    if p.suspended {
        return;
    }
    let s = &p.cfg.static_dimming;
    for (i, (st, (t, m))) in model.cells.iter().zip(out.iter_mut().zip(motion.iter_mut())).enumerate() {
        let owner = p.owners.get(i).copied().unwrap_or(OWNER_NONE);
        let win = (owner != OWNER_NONE).then(|| &p.snap.wins[owner as usize]);
        let in_foreground = win.is_some_and(|w| w.root == p.snap.fg_root && w.kind == Kind::Normal);
        let (x, y) = (i % p.gw, i / p.gw);
        if let Some(tc) = &p.torch {
            // Torch mode owns the picture while it is on: the focus stays evenly
            // lit and everything else is dimmed to exactly the torch level, so
            // static dimming and chrome can't leave patches in either.
            let (cx, cy) = (x as f32 + 0.5, y as f32 + 0.5);
            let mut lit: f32 = if tc.lit_foreground && in_foreground { 1.0 } else { 0.0 };
            if let Some([x0, y0, x1, y1]) = tc.lit_rect {
                let dx = (x0 - cx).max(0.0).max(cx - x1);
                let dy = (y0 - cy).max(0.0).max(cy - y1);
                lit = lit.max(1.0 - smoothstep(0.0, 0.75, (dx * dx + dy * dy).sqrt()));
            }
            if let Some((hx, hy, radius, feather)) = tc.halo {
                let dist = ((cx - hx).powi(2) + (cy - hy).powi(2)).sqrt();
                lit = lit.max(1.0 - smoothstep(radius, radius + feather, dist));
            }
            *t = tc.dim * (1.0 - lit);
            *m = Motion::Torch;
            continue;
        }

        // Dim only what is actually lit: dark areas barely wear.
        let weight = if st.has_data { smoothstep(s.min_brightness, s.min_brightness + 0.35, st.max) } else { 0.0 };

        if s.enabled && weight > 0.0 {
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

        if let Some(c) = p.chrome.as_ref().filter(|_| in_foreground) {
            if let Some(z) = c.zone(x, y) {
                if c.revealed[z] {
                    // Reaching for the toolbar: show it at full brightness.
                    *t = 0.0;
                    *m = Motion::Chrome;
                } else if st.static_secs >= c.after_secs && c.dim * weight > *t {
                    *t = c.dim * weight;
                    *m = Motion::Chrome;
                }
            }
        }
    }
}

/// Time constants (seconds) for eased motion.
pub struct Easing {
    pub release: f32,
    pub chrome_in: f32,
    pub torch_in: f32,
}

/// Moves `cur` toward `target`. Gentle dimming creeps in linearly at
/// `gentle_up` per second; everything else eases out exponentially, which
/// looks smooth at any frame rate. Returns (anything changed, anything still easing).
pub fn ramp(cur: &mut [f32], target: &[f32], motion: &[Motion], dt: f32, gentle_up: f32, e: &Easing) -> (bool, bool) {
    let ease = |c: f32, t: f32, tau: f32| c + (t - c) * (1.0 - (-dt / tau.max(0.001)).exp());
    let mut animating = false;
    let mut changed = false;
    for ((c, &t), &m) in cur.iter_mut().zip(target.iter()).zip(motion.iter()) {
        if *c == t {
            continue;
        }
        changed = true;
        if (t - *c).abs() < 0.002 {
            *c = t;
            continue;
        }
        if t > *c {
            match m {
                Motion::Gentle => *c = (*c + gentle_up * dt).min(t),
                Motion::Chrome => {
                    *c = ease(*c, t, e.chrome_in);
                    animating = true;
                }
                Motion::Torch => {
                    *c = ease(*c, t, e.torch_in);
                    animating = true;
                }
            }
        } else {
            *c = ease(*c, t, e.release);
            animating = true;
        }
    }
    (changed, animating)
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
