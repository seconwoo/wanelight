//! Full-screen color matrix through the Magnification API (the same mechanism
//! as Windows' Color filters). DWM applies it to every monitor on the GPU, in
//! linear light, and Desktop Duplication captures the result. Windows resets
//! it when the process exits, even after a crash.
//! Matrices are 5x5, row-vector convention: out = [r g b a 1] * M.

use windows::Win32::UI::Magnification::{MAGCOLOREFFECT, MagInitialize, MagSetFullscreenColorEffect, MagUninitialize};

pub type Matrix = [f32; 25];

pub const IDENTITY: Matrix = [
    1.0, 0.0, 0.0, 0.0, 0.0, //
    0.0, 1.0, 0.0, 0.0, 0.0, //
    0.0, 0.0, 1.0, 0.0, 0.0, //
    0.0, 0.0, 0.0, 1.0, 0.0, //
    0.0, 0.0, 0.0, 0.0, 1.0,
];

/// Time constant for changes (turning on and off, slider moves).
const EASE_SECS: f32 = 0.08;

pub fn srgb_to_linear(v: f32) -> f32 {
    if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
}

/// Maps `t` (linear) and below to black and keeps white: out = (in - t) / (1 - t).
pub fn crush(t: f32) -> Matrix {
    let t = t.clamp(0.0, 0.9);
    let s = 1.0 / (1.0 - t);
    let mut m = IDENTITY;
    for i in 0..3 {
        m[i * 5 + i] = s;
        m[20 + i] = -t * s;
    }
    m
}

/// Eases the shown matrix toward a target.
pub struct Fader {
    cur: Matrix,
}

impl Default for Fader {
    fn default() -> Self {
        Fader { cur: IDENTITY }
    }
}

impl Fader {
    /// Returns the matrix to show and whether it is still moving.
    pub fn step(&mut self, target: &Matrix, dt: f32) -> (Matrix, bool) {
        let k = 1.0 - (-dt / EASE_SECS).exp();
        for (c, t) in self.cur.iter_mut().zip(target) {
            *c += (t - *c) * k;
        }
        if self.cur.iter().zip(target).all(|(c, t)| (c - t).abs() < 2e-3) {
            self.cur = *target;
        }
        (self.cur, self.cur != *target)
    }
}

/// Owns the full-screen color effect and resets it on drop.
pub struct ColorEffect {
    current: Matrix,
}

impl ColorEffect {
    pub fn new() -> Option<ColorEffect> {
        unsafe { MagInitialize().as_bool().then_some(ColorEffect { current: IDENTITY }) }
    }

    pub fn set(&mut self, m: &Matrix) -> bool {
        if *m == self.current {
            return true;
        }
        let ok = unsafe { MagSetFullscreenColorEffect(&MAGCOLOREFFECT { transform: *m }).as_bool() };
        if ok {
            self.current = *m;
        }
        ok
    }
}

impl Drop for ColorEffect {
    fn drop(&mut self) {
        self.set(&IDENTITY);
        unsafe {
            let _ = MagUninitialize();
        }
    }
}
