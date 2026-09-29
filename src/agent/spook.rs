//! Spooky mode, bonus animations for torch mode: now and then the torch
//! flickers like a failing bulb, and when it steadies a little shadow cat
//! pays a visit (see critter.rs). The overlay is excluded from capture, so
//! screenshots never show any of it.

use super::critter::{Critter, Env, Scene};

/// Torch dim while spooky mode is on.
pub const DIM: f32 = 0.95;

const FIRST_AFTER: f64 = 4.0;
/// The cat walks in this long after the light steadies.
const CAT_DELAY: f64 = 0.4;
/// A show that stops being drawn (its screen went away) is dropped after this.
const STALL_SECS: f64 = 150.0;
/// Seconds per brightness wobble while the bulb is failing.
const JITTER_SECS: f64 = 0.045;

struct Show {
    screen: usize,
    start: f64,
    /// Blackouts [from, to) in seconds since `start`.
    blackouts: Vec<(f64, f64)>,
    steady_at: f64,
    seed: u64,
    cat: Option<Critter>,
    cat_tried: bool,
}

pub struct Spook {
    rng: u64,
    next_at: f64,
    show: Option<Show>,
}

impl Spook {
    pub fn new(now: f64) -> Self {
        Self { rng: now.to_bits() ^ 0x9E37_79B9_7F4A_7C15 | 1, next_at: now + FIRST_AFTER, show: None }
    }

    fn rand(&mut self) -> u64 {
        self.rng = xorshift(self.rng);
        self.rng
    }

    fn unit(&mut self) -> f64 {
        (self.rand() % 10_000) as f64 / 10_000.0
    }

    /// The torch is flickering, so the mask must be recomputed every frame.
    pub fn flickering(&self, now: f64) -> bool {
        self.show.as_ref().is_some_and(|sh| now - sh.start < sh.steady_at + 0.1)
    }

    /// Whether a new show should start. Drops one that stalled.
    pub fn due(&mut self, now: f64) -> bool {
        if self.show.as_ref().is_some_and(|sh| now - sh.start > STALL_SECS) {
            self.cancel(now);
        }
        self.show.is_none() && now >= self.next_at
    }

    pub fn cancel(&mut self, now: f64) {
        if self.show.take().is_some() {
            self.schedule(now);
        }
    }

    fn schedule(&mut self, now: f64) {
        self.next_at = now + 60.0 + 120.0 * self.unit();
    }

    /// Starts a show: a few quick blackouts, the last one longer, like a bulb about to die.
    pub fn start(&mut self, now: f64, screen: usize) {
        let mut blackouts = Vec::new();
        let mut t = 0.1;
        let n = 3 + self.rand() % 3;
        for k in 0..n {
            let len = if k + 1 == n { 0.3 + 0.2 * self.unit() } else { 0.04 + 0.1 * self.unit() };
            blackouts.push((t, t + len));
            t += len + 0.06 + 0.22 * self.unit();
        }
        let seed = self.rand();
        self.show = Some(Show { screen, start: now, blackouts, steady_at: t, seed, cat: None, cat_tried: false });
    }

    /// Torch brightness (1 = normal, 0 = out) and whether it should snap there
    /// instead of easing.
    pub fn flicker(&self, now: f64, screen: usize) -> (f32, bool) {
        let Some(sh) = self.show.as_ref().filter(|sh| sh.screen == screen) else { return (1.0, false) };
        let e = now - sh.start;
        if e >= sh.steady_at {
            return (1.0, false);
        }
        if sh.blackouts.iter().any(|&(a, b)| e >= a && e < b) {
            return (0.0, true);
        }
        // Between blackouts the bulb buzzes: a quick uneven wobble.
        let step = (e / JITTER_SECS) as u64;
        let noise = (xorshift(sh.seed ^ step.wrapping_mul(0x2545_F491_4F6C_DD1D) | 1) % 1000) as f32 / 1000.0;
        (0.6 + 0.4 * noise, true)
    }

    /// Advances the cat and its fireflies on `screen` and returns what to draw, if anything.
    pub fn cat(&mut self, now: f64, screen: usize, env: &Env) -> Option<Scene> {
        let sh = self.show.as_mut().filter(|sh| sh.screen == screen)?;
        if now - sh.start < sh.steady_at + CAT_DELAY {
            return None;
        }
        if !sh.cat_tried {
            sh.cat_tried = true;
            sh.cat = Critter::enter(now, env, sh.seed);
        }
        let draw = sh.cat.as_mut().and_then(|c| c.step(now, env));
        if draw.is_none() {
            self.show = None;
            self.schedule(now);
        }
        draw
    }
}

fn xorshift(mut x: u64) -> u64 {
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    x
}
