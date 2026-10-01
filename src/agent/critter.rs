//! Spooky mode's visitor: a little shadow cat that pads about in the dark,
//! hunts the fireflies drifting there, plays with the windows on screen
//! (walks along their top edges, leaps between them, hides behind them and
//! scratches them), comes to the edge of the torch when the pointer rests,
//! and reaches into the light with the tentacles hidden in its mouth. It
//! bolts when the light lands on it or the pointer lunges at it.
//!
//! The sprites come from art/cat/cat.html (baked by art/bake.ps1), which also
//! generates cat_frames.rs: clip ranges and where the mouth and tentacle tip
//! are in every frame, so a caught firefly follows the drawing exactly.

use windows::Win32::Foundation::RECT;

use super::capture::{CELL, GridGeom};
use super::cat_frames as cf;
use crate::{log, util};

/// Sprite frame size (px).
pub const FRAME_W: u32 = 288;
pub const FRAME_H: u32 = 192;
/// Feet position inside a frame (px). Frames face right.
pub const ANCHOR: (f32, f32) = (100.0, 184.0);
/// Firefly frames: a FLY x FLY square at FLY_AT inside their cell.
pub const FLY: u32 = 32;
pub const FLY_AT: (u32, u32) = (128, 80);
pub const MAX_FLIES: usize = 3;

/// Reach frame where the grabbing tentacle is fully out and takes hold.
const GRAB_FRAME: usize = 7;
/// Pounce frame where the jaws snap shut.
const SNAP_FRAME: usize = 6;

/// Test hook (debug runs only): `WANELIGHT_DEBUG_CAT` set to `windows` leaves
/// out the fireflies and the torch visits, so the cat only plays with windows;
/// `climb`, `hide` or `scratch` also makes it pick that game every time.
fn debug_cat() -> &'static str {
    static MODE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    MODE.get_or_init(|| if util::debug_enabled() { std::env::var("WANELIGHT_DEBUG_CAT").unwrap_or_default() } else { String::new() })
}

fn windows_only() -> bool {
    matches!(debug_cat(), "windows" | "climb" | "hide" | "scratch")
}

/// A point of frame `frame` relative to the feet (px at scale 1, facing right).
fn offset(p: (f32, f32)) -> (f32, f32) {
    (p.0 - ANCHOR.0, p.1 - ANCHOR.1)
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct CritterDraw {
    /// Feet position (physical px, virtual screen).
    pub x: f32,
    pub y: f32,
    pub scale: f32,
    pub frame: usize,
    pub flip: bool,
    /// Only this horizontal range (screen px) shows, while hiding behind a window.
    pub clip: Option<(f32, f32)>,
}

/// A window the cat is scratching: a copy of it shown offset by (dx, dy).
/// A new `id` means a new scratch, so the copy is taken again.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct ShakeDraw {
    pub rect: RECT,
    pub dx: f32,
    pub dy: f32,
    pub id: u32,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct FlyDraw {
    /// Centre (physical px, virtual screen).
    pub x: f32,
    pub y: f32,
    pub scale: f32,
    pub frame: usize,
}

/// Everything spooky mode draws on one monitor.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct Scene {
    pub cat: Option<CritterDraw>,
    pub flies: [Option<FlyDraw>; MAX_FLIES],
    pub shake: Option<ShakeDraw>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Clip {
    Walk,
    Run,
    Sit,
    Groom,
    Stare,
    Reach,
    Startle,
    Stalk,
    Wiggle,
    Pounce,
    Chew,
    ChewSit,
    Jump,
    Scratch,
}

impl Clip {
    /// First frame, frame count, frames per second, loops.
    fn spec(self) -> (usize, usize, f64, bool) {
        let ((first, n), fps, looped) = match self {
            Clip::Walk => (cf::WALK, 12.0, true),
            Clip::Run => (cf::RUN, 16.0, true),
            Clip::Sit => (cf::SIT, 12.0, true),
            Clip::Groom => (cf::GROOM, 12.0, true),
            Clip::Stare => (cf::STARE, 10.0, true),
            Clip::Reach => (cf::REACH, 12.0, false),
            Clip::Startle => (cf::STARTLE, 12.0, false),
            Clip::Stalk => (cf::STALK, 10.0, true),
            Clip::Wiggle => (cf::WIGGLE, 14.0, true),
            Clip::Pounce => (cf::POUNCE, cf::POUNCE.1 as f64 / POUNCE_SECS, false),
            Clip::Chew => (cf::CHEW, 10.0, true),
            Clip::ChewSit => (cf::CHEWSIT, 10.0, true),
            // Jumps are timed per leap; see `jump_to`.
            Clip::Jump => (cf::JUMP, 12.0, false),
            Clip::Scratch => (cf::SCRATCH, 12.0, true),
        };
        (first, n, fps, looped)
    }

    fn secs(self) -> f64 {
        let (_, n, fps, _) = self.spec();
        n as f64 / fps
    }

    /// Distance covered per leg cycle (px at scale 1), so feet don't slide.
    fn stride(self) -> Option<f32> {
        match self {
            Clip::Walk => Some(53.0),
            Clip::Run => Some(170.0),
            Clip::Stalk => Some(30.0),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum State {
    /// Strolling to a spot in the dark.
    Wander,
    Idle,
    /// Heading for the edge of the light.
    Approach,
    Stare,
    Reach,
    /// Closing in on a firefly: trotting, then stalking low.
    Hunt,
    Wiggle,
    Pounce,
    /// Grabbing a firefly with the tentacles.
    Snatch,
    Eat,
    /// Walking to where it will spring up onto a window's top edge.
    Climb,
    Jump,
    /// Walking behind a window, out of sight.
    Sneak,
    Hidden,
    /// Looking out from behind a window's edge.
    Peek,
    /// Walking to a window's side edge to scratch it.
    ToScratch,
    Scratch,
    Startle,
    /// Running into the shadows, or off screen.
    Flee,
    /// Walking off screen at the end of a visit.
    Leave,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Attack {
    Pounce,
    Snatch,
}

/// Where the cat's feet are: anywhere on the screen, or on a window's top edge.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Ground {
    Floor,
    /// Top edge at `y`, walkable from `x0` to `x1`.
    Ledge { y: f32, x0: f32, x1: f32 },
}

struct Jump {
    from: (f32, f32),
    to: (f32, f32),
    start: f64,
    secs: f64,
    height: f32,
    land: Ground,
    then: State,
}

/// A window the cat hides behind, and which side it went in from (-1 left, 1 right).
#[derive(Clone, Copy)]
struct Cover {
    rect: RECT,
    side: f32,
    through: bool,
}

/// What the cat can see of one monitor.
pub struct Env<'a> {
    pub mon: RECT,
    pub geom: GridGeom,
    /// Dim target per cell; the torch's light is where it is low.
    pub target: &'a [f32],
    pub dim: f32,
    pub pointer: (f32, f32),
    /// Windows on this monitor, top of the z-order first, and whether the cat
    /// may play with each (big ones and the taskbar only get in the way).
    pub wins: &'a [(RECT, bool)],
}

impl Env<'_> {
    /// Whether a window above window `i` in the z-order covers (x, y).
    fn covered(&self, i: usize, x: f32, y: f32) -> bool {
        self.wins[..i].iter().any(|(r, _)| x >= r.left as f32 && x < r.right as f32 && y >= r.top as f32 && y < r.bottom as f32)
    }

    fn dark(&self, x: f32, y: f32) -> bool {
        self.dark_to(x, y, 0.5)
    }

    /// Whether (x, y) is dimmed to at least `level` of the full dim.
    fn dark_to(&self, x: f32, y: f32, level: f32) -> bool {
        let cx = ((x - (self.mon.left + self.geom.ox) as f32) / CELL as f32).floor();
        let cy = ((y - (self.mon.top + self.geom.oy) as f32) / CELL as f32).floor();
        if cx < 0.0 || cy < 0.0 || cx >= self.geom.gw as f32 || cy >= self.geom.gh as f32 {
            return true;
        }
        self.target.get(cy as usize * self.geom.gw + cx as usize).is_some_and(|&v| v >= level * self.dim)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Grip {
    Free,
    /// Hovering, waiting for the cat to strike.
    Held,
    /// Stuck to the grabbing tentacle's tip.
    Tip,
    /// In the cat's mouth.
    Mouth,
}

/// A firefly drifting in the dark, blinking now and then.
struct Fly {
    pos: (f32, f32),
    heading: f32,
    seed: f32,
    /// Darting away after a near miss, until then.
    dart_until: f64,
    grip: Grip,
    /// Being swallowed since then.
    gulp: Option<f64>,
    gone: bool,
}

const GULP_SECS: f64 = 0.18;

impl Fly {
    fn step(&mut self, now: f64, dt: f32, env: &Env, s: f32, away_from: (f32, f32)) {
        if matches!(self.grip, Grip::Tip | Grip::Mouth) {
            return;
        }
        let t = now as f32;
        let k = self.seed;
        if self.grip == Grip::Held {
            // Hovers in place with a tiny bob, as if it has noticed nothing.
            self.pos.1 += 6.0 * s * (t * 6.0 + k).cos() * dt;
            return;
        }
        // Lazy loops: the heading drifts on two slow waves; speed swells and dips.
        self.heading += (1.3 * (t * 1.7 + k).sin() + 0.9 * (t * 0.63 + 3.0 * k).sin()) * dt;
        let mut speed = (38.0 + 30.0 * (t * 0.9 + 2.0 * k).sin()).max(6.0) * s;
        if now < self.dart_until {
            self.heading = (self.pos.1 - away_from.1).atan2(self.pos.0 - away_from.0) + 0.6 * (t * 5.0 + k).sin();
            speed = 300.0 * s;
        }
        let next = (self.pos.0 + self.heading.cos() * speed * dt, self.pos.1 + self.heading.sin() * speed * dt);
        let m = env.mon;
        let inset = 80.0 * s;
        let inside = next.0 > m.left as f32 + inset
            && next.0 < m.right as f32 - inset
            && next.1 > m.top as f32 + inset
            && next.1 < m.bottom as f32 - inset;
        if !inside {
            let c = ((m.left + m.right) as f32 / 2.0, (m.top + m.bottom) as f32 / 2.0);
            self.heading = (c.1 - self.pos.1).atan2(c.0 - self.pos.0);
        } else if !env.dark_to(next.0, next.1, 0.8) {
            // Fireflies keep to the dark.
            self.heading += std::f32::consts::PI * 0.6;
        } else {
            self.pos = next;
        }
    }

    fn draw(&self, now: f64, s: f32) -> FlyDraw {
        let level = if matches!(self.grip, Grip::Tip | Grip::Mouth) {
            // Caught: it flares and flickers as it struggles.
            2 + ((now * 14.0) as usize & 1)
        } else {
            // A slow blink: glow for a moment every couple of seconds.
            let cycle = 2.2 + 0.8 * self.seed.sin().abs() as f64;
            let ph = ((now + self.seed as f64 * 7.0) % cycle) / cycle;
            let glow = (1.0 - ((ph - 0.3) / 0.22).powi(2)).max(0.15);
            ((glow * 3.0).round() as usize).min(3)
        };
        let shrink = self.gulp.map_or(1.0, |t| (1.0 - (now - t) / GULP_SECS).clamp(0.0, 1.0) as f32);
        let flap = ((now * 24.0) as usize) & 1;
        FlyDraw { x: self.pos.0, y: self.pos.1, scale: s * shrink, frame: cf::FLY.0 + flap * 4 + level }
    }
}

struct Pounce {
    from: (f32, f32),
    to: (f32, f32),
    start: f64,
}

pub struct Critter {
    pos: (f32, f32),
    left: bool,
    clip: Clip,
    clip_start: f64,
    state: State,
    until: f64,
    goal: (f32, f32),
    born: f64,
    last: f64,
    rng: u64,
    scale: f32,
    ptr: (f32, f32),
    ptr_vel: (f32, f32),
    ptr_moved: f64,
    /// Current speed and leg phase (cycles), for gait that speeds up and slows down.
    v: f32,
    phase: f32,
    cruise: f32,
    cruise_until: f64,
    flies: Vec<Fly>,
    flies_spawned: usize,
    next_fly: f64,
    prey: Option<usize>,
    attack: Attack,
    /// The firefly will get away from this attack (decided when it starts).
    dodge: bool,
    dodged: bool,
    pounce: Option<Pounce>,
    misses: u32,
    hunt_after: f64,
    ground: Ground,
    jump: Option<Jump>,
    /// Where to leap once the cat reaches its launch spot.
    leap: Option<((f32, f32), Ground)>,
    cover: Option<Cover>,
    scratch: Option<(RECT, f64)>,
    scratches: u32,
    ledge_checked: f64,
    /// Reaches into the light this visit to the rim, and when the light is interesting again.
    reaches: u32,
    rim_after: f64,
    /// The cat heads off after this many seconds.
    visit: f64,
    /// Startled again this soon after the last time, it bolts off screen.
    startled: f64,
    bolt: bool,
    done: bool,
}

const WALK: f32 = 80.0;
const TROT: f32 = 140.0;
const STALK: f32 = 32.0;
const RUN: f32 = 460.0;
/// Acceleration and braking (px/s² at scale 1): cats get going and stop quickly.
const ACCEL: f32 = 420.0;
const BRAKE: f32 = 520.0;
const POUNCE_SECS: f64 = 0.55;
/// Horizontal distance of a pounce (px at scale 1).
const POUNCE_REACH: f32 = 150.0;
/// A visit ends this long after it was meant to, however busy the cat is.
const OVERTIME_SECS: f64 = 80.0;
/// How far the cat can leap between windows (px at scale 1).
const LEAP_UP: f32 = 450.0;
const LEAP_ACROSS: f32 = 700.0;
/// How much a scratched window shakes (px at scale 1).
const SHAKE: f32 = 3.0;
/// The pointer counts as resting after this long.
const REST_SECS: f64 = 3.0;
const FLIES_PER_VISIT: usize = 4;

impl Critter {
    /// Walks in from the side farther from the pointer, if that side is dark.
    pub fn enter(now: f64, env: &Env, seed: u64, visit: f64) -> Option<Critter> {
        let m = env.mon;
        let scale = ((m.bottom - m.top) as f32 / 1440.0).clamp(0.5, 2.0);
        let mut c = Critter {
            pos: (0.0, 0.0),
            left: false,
            clip: Clip::Walk,
            clip_start: now,
            state: State::Wander,
            until: 0.0,
            goal: (0.0, 0.0),
            born: now,
            last: now,
            rng: seed | 1,
            scale,
            ptr: env.pointer,
            ptr_vel: (0.0, 0.0),
            ptr_moved: now,
            v: 0.0,
            phase: 0.0,
            cruise: WALK,
            cruise_until: now + 2.0,
            flies: Vec::new(),
            flies_spawned: 0,
            next_fly: now + 2.5,
            prey: None,
            attack: Attack::Pounce,
            dodge: false,
            dodged: false,
            pounce: None,
            misses: 0,
            hunt_after: now,
            ground: Ground::Floor,
            jump: None,
            leap: None,
            cover: None,
            scratch: None,
            scratches: 0,
            ledge_checked: now,
            reaches: 0,
            rim_after: now,
            visit,
            startled: f64::NEG_INFINITY,
            bolt: false,
            done: false,
        };
        let from_left = env.pointer.0 > (m.left + m.right) as f32 / 2.0;
        for side in [from_left, !from_left] {
            for _ in 0..8 {
                let (lo, hi) = (m.top as f32 + 0.45 * (m.bottom - m.top) as f32, m.bottom as f32 - 8.0 * scale);
                let y = lo + (hi - lo) * c.unit();
                let inside = if side { m.left as f32 + 260.0 * scale } else { m.right as f32 - 260.0 * scale };
                if env.dark(inside, y - 60.0 * scale) {
                    c.pos = (if side { m.left as f32 - 150.0 * scale } else { m.right as f32 + 150.0 * scale }, y);
                    c.goal = (inside, y);
                    c.v = WALK * scale;
                    return Some(c);
                }
            }
        }
        None
    }

    fn unit(&mut self) -> f32 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        (self.rng % 10_000) as f32 / 10_000.0
    }

    fn set(&mut self, state: State, now: f64, secs: f64) {
        if state != self.state {
            log!("cat: {:?} -> {:?} at {:.0},{:.0}", self.state, state, self.pos.0, self.pos.1);
        }
        self.state = state;
        self.until = now + secs;
    }

    /// Enters `state` for `min` plus up to `spread` seconds.
    fn set_random(&mut self, state: State, now: f64, min: f64, spread: f64) {
        let r = self.unit() as f64;
        self.set(state, now, min + spread * r);
    }

    fn play(&mut self, clip: Clip, now: f64) {
        if self.clip != clip {
            self.clip = clip;
            self.clip_start = now;
        }
    }

    /// Frame of the current clip (0-based within the clip).
    fn clip_frame(&self, now: f64) -> usize {
        let (_, n, fps, looped) = self.clip.spec();
        if self.clip.stride().is_some() {
            // Legs follow the ground covered; standing still shows the neutral frame.
            if self.v < 2.0 { 0 } else { (self.phase.rem_euclid(1.0) * n as f32) as usize % n }
        } else {
            let k = ((now - self.clip_start) * fps) as usize;
            if looped { k % n } else { k.min(n - 1) }
        }
    }

    fn dir(&self) -> f32 {
        if self.left { -1.0 } else { 1.0 }
    }

    /// Screen position of a point given relative to the feet (px at scale 1, facing right).
    fn at(&self, off: (f32, f32), lift: f32) -> (f32, f32) {
        (self.pos.0 + self.dir() * off.0 * self.scale, self.pos.1 - lift + off.1 * self.scale)
    }

    fn torso(&self) -> (f32, f32) {
        (self.pos.0, self.pos.1 - 60.0 * self.scale)
    }

    fn feet_bounds(&self, env: &Env, p: (f32, f32)) -> (f32, f32) {
        let (m, s) = (env.mon, self.scale);
        (
            p.0.clamp(m.left as f32 + 60.0 * s, m.right as f32 - 60.0 * s),
            p.1.clamp(m.top as f32 + 170.0 * s, m.bottom as f32 - 6.0 * s),
        )
    }

    /// Where to sit so the mouth is just outside the light, level with the
    /// pointer, on the cat's side of it.
    fn rim_seat(&self, env: &Env) -> Option<(f32, f32)> {
        let (px, py) = env.pointer;
        let s = self.scale;
        let m = env.mon;
        let mouth = offset(cf::MOUTH[cf::SIT.0]);
        let near = if self.pos.0 < px { -1.0 } else { 1.0 };
        for side in [near, -near] {
            let mut x = px;
            while x > m.left as f32 && x < m.right as f32 {
                x += side * 8.0;
                // Sit past the torch's soft edge, so the whole cat stays in the dark.
                if env.dark_to(x, py, 0.9) {
                    let mouth_x = x + side * 18.0 * s;
                    // Facing the light: the mouth is ahead of the feet.
                    let feet = (mouth_x + side * mouth.0 * s, py - mouth.1 * s);
                    let lo = m.left as f32 + 20.0 * s;
                    let hi = m.right as f32 - 20.0 * s;
                    if feet.0 > lo && feet.0 < hi && feet.1 < m.bottom as f32 && env.dark(feet.0, feet.1 - 60.0 * s) {
                        return Some(feet);
                    }
                    break;
                }
            }
        }
        None
    }

    /// A random dark spot within reach, with a dark path to it.
    fn wander_goal(&mut self, env: &Env) -> Option<(f32, f32)> {
        let s = self.scale;
        for _ in 0..12 {
            let a = self.unit() * std::f32::consts::TAU;
            let r = (150.0 + 350.0 * self.unit()) * s;
            let g = self.feet_bounds(env, (self.pos.0 + r * a.cos(), self.pos.1 + 0.5 * r * a.sin()));
            let clear = (1..=4).all(|k| {
                let f = k as f32 / 4.0;
                env.dark(self.pos.0 + (g.0 - self.pos.0) * f, self.pos.1 + (g.1 - self.pos.1) * f - 60.0 * s)
            });
            if clear {
                return Some(g);
            }
        }
        None
    }

    fn exit_goal(&self, env: &Env, away_from: f32) -> (f32, f32) {
        let m = env.mon;
        let x = if self.pos.0 < away_from { m.left as f32 - 200.0 * self.scale } else { m.right as f32 + 200.0 * self.scale };
        (x, self.pos.1)
    }

    /// Where to run when startled: a dark spot well away from the pointer,
    /// with a dark path to it. Off screen if there's none, or if it was
    /// startled again soon after the last time.
    fn flee_goal(&mut self, env: &Env) -> (f32, f32) {
        let (px, py) = env.pointer;
        if self.bolt {
            return self.exit_goal(env, px);
        }
        let (m, s) = (env.mon, self.scale);
        let from = (self.pos.0 - px).hypot(self.pos.1 - py);
        let mut best: Option<((f32, f32), f32)> = None;
        for _ in 0..24 {
            let x = m.left as f32 + (m.right - m.left) as f32 * self.unit();
            let y = m.top as f32 + (m.bottom - m.top) as f32 * self.unit();
            let g = self.feet_bounds(env, (x, y));
            let d = (g.0 - px).hypot(g.1 - py);
            if d < (500.0 * s).max(from) || best.is_some_and(|(_, b)| d <= b) || !env.dark_to(g.0, g.1 - 60.0 * s, 0.9) {
                continue;
            }
            let clear = (1..=6).all(|k| {
                let f = k as f32 / 6.0;
                env.dark(self.pos.0 + (g.0 - self.pos.0) * f, self.pos.1 + (g.1 - self.pos.1) * f - 60.0 * s)
            });
            if clear {
                best = Some((g, d));
            }
        }
        best.map_or_else(|| self.exit_goal(env, px), |(g, _)| g)
    }

    /// Walkable top edges of windows: the stretch of each top edge that isn't
    /// under a window higher in the z-order and has dark room above it.
    fn ledges(&self, env: &Env) -> Vec<(f32, f32, f32)> {
        let (m, s) = (env.mon, self.scale);
        let mut out = Vec::new();
        for (i, (r, play)) in env.wins.iter().enumerate() {
            let y = r.top as f32;
            if !play || y < m.top as f32 + 170.0 * s || y > m.bottom as f32 - 20.0 * s {
                continue;
            }
            let (mut x0, mut x1) = (r.left.max(m.left) as f32, r.right.min(m.right) as f32);
            // Trim away windows above it in the z-order that cover the edge or the space over it.
            for (h, _) in &env.wins[..i] {
                if (h.top as f32) < y + 2.0 && (h.bottom as f32) > y - 150.0 * s && (h.right as f32) > x0 && (h.left as f32) < x1 {
                    let (hl, hr) = (h.left as f32, h.right as f32);
                    if hl - x0 > x1 - hr {
                        x1 = x1.min(hl);
                    } else {
                        x0 = x0.max(hr);
                    }
                }
            }
            let (x0, x1) = (x0 + 40.0 * s, x1 - 40.0 * s);
            if x1 - x0 < 200.0 * s {
                continue;
            }
            let dark = [x0, (x0 + x1) / 2.0, x1].iter().all(|&x| env.dark_to(x, y - 60.0 * s, 0.9));
            if dark {
                out.push((y, x0, x1));
            }
        }
        out
    }

    /// The ledge under the cat still exists (windows move and close); follows small moves.
    fn check_ledge(&mut self, env: &Env) -> bool {
        let Ground::Ledge { y, .. } = self.ground else { return true };
        let x = self.pos.0;
        let found = self
            .ledges(env)
            .into_iter()
            .filter(|&(ly, x0, x1)| (ly - y).abs() < 40.0 * self.scale && x >= x0 - 40.0 * self.scale && x <= x1 + 40.0 * self.scale)
            .min_by(|a, b| (a.0 - y).abs().total_cmp(&(b.0 - y).abs()));
        match found {
            Some((ly, x0, x1)) => {
                self.ground = Ground::Ledge { y: ly, x0, x1 };
                self.pos = (x.clamp(x0, x1), ly);
                self.goal.1 = ly;
                true
            }
            None => false,
        }
    }

    /// Starts a leap to `to`, landing on `land`, then carrying on as `then`.
    fn jump_to(&mut self, now: f64, to: (f32, f32), land: Ground, then: State) {
        let dist = (to.0 - self.pos.0).hypot(to.1 - self.pos.1);
        let up = (self.pos.1 - to.1).max(0.0);
        let secs = 0.45 + 0.0005 * dist as f64;
        let height = 0.5 * up + 50.0 * self.scale + 0.1 * dist;
        if (to.0 - self.pos.0).abs() > 2.0 {
            self.left = to.0 < self.pos.0;
        }
        self.v = 0.0;
        self.jump = Some(Jump { from: self.pos, to, start: now, secs, height, land, then });
        self.clip = Clip::Jump;
        self.clip_start = now;
        self.set(State::Jump, now, secs);
    }

    /// A dark landing spot below, for hopping down off a ledge.
    fn floor_below(&mut self, env: &Env) -> (f32, f32) {
        let (m, s) = (env.mon, self.scale);
        for _ in 0..8 {
            let dx = self.dir() * (60.0 + 160.0 * self.unit()) * s;
            let dy = (220.0 + 300.0 * self.unit()) * s;
            let p = self.feet_bounds(env, (self.pos.0 + dx, self.pos.1 + dy));
            if p.1 > self.pos.1 + 60.0 * s && env.dark(p.0, p.1 - 60.0 * s) {
                return p;
            }
        }
        self.feet_bounds(env, (self.pos.0, m.bottom as f32))
    }

    /// Picks a ledge to leap onto: from the floor, via a launch spot below it;
    /// from a ledge, straight across to another one within reach.
    fn pick_ledge(&mut self, env: &Env) -> bool {
        let s = self.scale;
        let mut ledges = self.ledges(env);
        if let Ground::Ledge { y, .. } = self.ground {
            ledges.retain(|l| (l.0 - y).abs() > 8.0);
        }
        if ledges.is_empty() {
            return false;
        }
        let start = (self.unit() * ledges.len() as f32) as usize;
        for k in 0..ledges.len() {
            let (y, x0, x1) = ledges[(start + k) % ledges.len()];
            let land = Ground::Ledge { y, x0, x1 };
            match self.ground {
                Ground::Floor => {
                    let x = x0 + (x1 - x0) * (0.2 + 0.6 * self.unit());
                    let side = if self.pos.0 < x { -1.0 } else { 1.0 };
                    let launch = self.feet_bounds(env, (x + side * 180.0 * s, y + 260.0 * s));
                    let rise = launch.1 - y;
                    if rise > 80.0 * s && rise < LEAP_UP * s && env.dark(launch.0, launch.1 - 60.0 * s) {
                        self.goal = launch;
                        self.leap = Some(((x, y), land));
                        return true;
                    }
                }
                Ground::Ledge { .. } => {
                    let x = (self.pos.0 + self.dir() * 300.0 * s).clamp(x0, x1);
                    let (dx, dy) = ((x - self.pos.0).abs(), y - self.pos.1);
                    if dx > 100.0 * s && dx < LEAP_ACROSS * s && dy > -LEAP_UP * s && dy < 650.0 * s {
                        self.leap = Some(((x, y), land));
                        return true;
                    }
                }
            }
        }
        false
    }

    /// A window to slip behind: tall and wide enough to hide the cat, with room
    /// beside it in the dark. Sets the entry spot as the goal.
    fn pick_cover(&mut self, env: &Env) -> bool {
        let (m, s) = (env.mon, self.scale);
        let n = env.wins.len();
        if n == 0 {
            return false;
        }
        let start = (self.unit() * n as f32) as usize;
        for k in 0..n {
            let i = (start + k) % n;
            let (r, play) = env.wins[i];
            let (lo, hi) = (r.top as f32 + 175.0 * s, (r.bottom as f32 - 8.0 * s).min(m.bottom as f32 - 6.0 * s));
            if !play || hi <= lo || ((r.right - r.left) as f32) < 360.0 * s {
                continue;
            }
            let y = lo + (hi - lo) * self.unit();
            let side = if self.pos.0 < (r.left + r.right) as f32 / 2.0 { -1.0 } else { 1.0 };
            let edge = if side < 0.0 { r.left as f32 } else { r.right as f32 };
            let entry = (edge + side * 130.0 * s, y);
            let room = entry.0 > m.left as f32 + 60.0 * s && entry.0 < m.right as f32 - 60.0 * s;
            // The edge it disappears behind must be in view.
            let seen = !env.covered(i, edge - side, y - 10.0 * s) && !env.covered(i, edge - side, y - 120.0 * s);
            if room && seen && env.dark(entry.0, y - 60.0 * s) && env.dark(edge + side * 40.0 * s, y - 100.0 * s) {
                self.goal = entry;
                self.cover = Some(Cover { rect: r, side, through: self.unit() < 0.4 });
                return true;
            }
        }
        false
    }

    /// A window side edge to scratch, standing up beside it in the dark.
    fn pick_scratch(&mut self, env: &Env) -> bool {
        let (m, s) = (env.mon, self.scale);
        let reach = (cf::SCRATCH_X - ANCHOR.0) * s;
        let n = env.wins.len();
        if n == 0 {
            return false;
        }
        let start = (self.unit() * n as f32) as usize;
        for k in 0..n {
            let i = (start + k) % n;
            let (r, play) = env.wins[i];
            let (lo, hi) = (r.top as f32 + 130.0 * s, (r.bottom as f32 + 40.0 * s).min(m.bottom as f32 - 6.0 * s));
            if !play || hi <= lo {
                continue;
            }
            let y = lo + (hi - lo) * self.unit();
            let side = if self.pos.0 < (r.left + r.right) as f32 / 2.0 { -1.0 } else { 1.0 };
            let edge = if side < 0.0 { r.left as f32 } else { r.right as f32 };
            let feet = (edge + side * reach, y);
            let room = feet.0 > m.left as f32 + 80.0 * s && feet.0 < m.right as f32 - 80.0 * s;
            // The claws must land on a part of the edge that's in view.
            let seen = !env.covered(i, edge - side, y - 110.0 * s) && !env.covered(i, edge - side, y - 70.0 * s);
            if room && seen && env.dark(feet.0, y - 60.0 * s) {
                self.goal = feet;
                self.scratch = Some((r, 0.0));
                return true;
            }
        }
        false
    }

    /// Moves toward the goal, speeding up toward `top` and braking to arrive.
    /// Advances the leg cycle by the distance covered. True on arrival.
    fn drive(&mut self, dt: f32, top: f32) -> bool {
        let s = self.scale;
        let (dx, dy) = (self.goal.0 - self.pos.0, self.goal.1 - self.pos.1);
        let d = (dx * dx + dy * dy).sqrt();
        let want = (top * s).min((2.0 * BRAKE * s * d).sqrt());
        self.v = if want > self.v { (self.v + ACCEL * s * dt).min(want) } else { (self.v - BRAKE * s * dt).max(want) };
        if dx.abs() > 2.0 {
            self.left = dx < 0.0;
        }
        let step = self.v * dt;
        if d <= step.max(1.0) {
            self.pos = self.goal;
            self.phase += d / (self.stride() * s);
            self.v = 0.0;
            return true;
        }
        self.pos = (self.pos.0 + dx / d * step, self.pos.1 + dy / d * step);
        self.phase += step / (self.stride() * s);
        false
    }

    fn stride(&self) -> f32 {
        self.clip.stride().unwrap_or(53.0)
    }

    /// Walk or run to match the current speed.
    fn gait(&mut self, now: f64) {
        let run = self.v > 230.0 * self.scale;
        self.play(if run { Clip::Run } else { Clip::Walk }, now);
    }

    /// Picks how fast to stroll next: mostly walking, sometimes ambling or
    /// trotting, now and then stopping to look around or dashing off.
    fn vary_cruise(&mut self, now: f64) {
        if now < self.cruise_until {
            return;
        }
        let r = self.unit();
        let (speed, secs) = match r {
            r if r < 0.12 => (0.0, 0.5 + 1.0 * self.unit()),
            r if r < 0.18 => (320.0, 0.5 + 0.3 * self.unit()),
            r if r < 0.40 => (45.0, 1.5 + 2.0 * self.unit()),
            r if r < 0.75 => (WALK, 1.5 + 2.0 * self.unit()),
            _ => (TROT, 1.0 + 1.5 * self.unit()),
        };
        self.cruise = speed;
        self.cruise_until = now + secs as f64;
    }

    fn nearest_fly(&self, max: f32) -> Option<usize> {
        let t = self.torso();
        self.flies
            .iter()
            .enumerate()
            .filter(|(_, f)| f.grip == Grip::Free && !f.gone && f.gulp.is_none())
            .map(|(i, f)| (i, (f.pos.0 - t.0).hypot(f.pos.1 - t.1)))
            .filter(|&(_, d)| d < max)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(i, _)| i)
    }

    fn prey_pos(&self) -> Option<(f32, f32)> {
        self.prey.and_then(|i| self.flies.get(i)).filter(|f| !f.gone && f.gulp.is_none()).map(|f| f.pos)
    }

    fn set_grip(&mut self, grip: Grip) {
        if let Some(f) = self.prey.and_then(|i| self.flies.get_mut(i)) {
            f.grip = grip;
        }
    }

    fn spawn_fly(&mut self, now: f64, env: &Env) {
        if windows_only() {
            return;
        }
        let (m, s) = (env.mon, self.scale);
        for _ in 0..10 {
            let p = (
                m.left as f32 + (m.right - m.left) as f32 * (0.1 + 0.8 * self.unit()),
                m.top as f32 + (m.bottom - m.top) as f32 * (0.2 + 0.7 * self.unit()),
            );
            let far = (p.0 - self.pos.0).hypot(p.1 - self.pos.1) > 300.0 * s;
            if far && env.dark_to(p.0, p.1, 0.8) {
                let heading = self.unit() * std::f32::consts::TAU;
                let seed = self.unit() * 100.0;
                self.flies.push(Fly { pos: p, heading, seed, dart_until: 0.0, grip: Grip::Free, gulp: None, gone: false });
                self.flies_spawned += 1;
                self.next_fly = now + 5.0 + 6.0 * self.unit() as f64;
                return;
            }
        }
        self.next_fly = now + 2.0;
    }

    /// The firefly slips away just before the strike lands.
    fn dodge_now(&mut self, now: f64) {
        self.dodged = true;
        let cat = self.torso();
        if let Some(f) = self.prey.and_then(|i| self.flies.get_mut(i)) {
            f.grip = Grip::Free;
            f.dart_until = now + 1.2;
            f.heading = (f.pos.1 - cat.1).atan2(f.pos.0 - cat.0) - 0.8;
        }
    }

    /// After a miss the cat sits a moment; three misses and it gives up for a while.
    fn missed(&mut self, now: f64) {
        self.misses += 1;
        if self.misses >= 3 {
            self.misses = 0;
            self.hunt_after = now + 12.0;
        }
        if self.prey.and_then(|i| self.flies.get(i)).is_some_and(|f| f.grip == Grip::Held) {
            self.set_grip(Grip::Free);
        }
        self.prey = None;
        self.set(State::Idle, now, 1.2);
        self.play(Clip::Sit, now);
    }

    /// Picks the next strike and whether the firefly will escape it.
    fn plan_attack(&mut self) {
        self.attack = if self.unit() < 0.6 { Attack::Pounce } else { Attack::Snatch };
        self.dodge = self.unit() < 0.3;
        self.dodged = false;
    }

    /// Where the feet go so the strike meets the firefly at `f`.
    fn strike_seat(&self, env: &Env, f: (f32, f32)) -> (f32, f32) {
        let s = self.scale;
        let side = if self.pos.0 < f.0 { -1.0 } else { 1.0 };
        match self.attack {
            // Crouch a leap away; the landing puts the snapping jaws on it.
            Attack::Pounce => {
                let jaw = offset(cf::MOUTH[cf::POUNCE.0 + SNAP_FRAME]);
                self.feet_bounds(env, (f.0 + side * (jaw.0 + POUNCE_REACH) * s, f.1 - jaw.1 * s))
            }
            // Sit where the grabbing tentacle, fully out, touches it.
            Attack::Snatch => {
                let tip = offset(cf::TIP[cf::REACH.0 + GRAB_FRAME]);
                self.feet_bounds(env, (f.0 + side * tip.0 * s, f.1 - tip.1 * s))
            }
        }
    }

    pub fn step(&mut self, now: f64, env: &Env) -> Option<Scene> {
        if self.done {
            return None;
        }
        let dt = (now - self.last).clamp(0.0, 0.1) as f32;
        self.last = now;
        let s = self.scale;

        // Follow the pointer: resting, or lunging at the cat.
        let (px, py) = env.pointer;
        if dt > 0.0 {
            let v = ((px - self.ptr.0) / dt, (py - self.ptr.1) / dt);
            self.ptr_vel = (0.7 * self.ptr_vel.0 + 0.3 * v.0, 0.7 * self.ptr_vel.1 + 0.3 * v.1);
        }
        if (px - self.ptr.0).abs() + (py - self.ptr.1).abs() > 2.0 {
            self.ptr_moved = now;
        }
        self.ptr = (px, py);
        let resting = now - self.ptr_moved >= REST_SECS;

        // Fireflies.
        if self.flies_spawned < FLIES_PER_VISIT
            && now >= self.next_fly
            && self.flies.iter().filter(|f| f.grip == Grip::Free && !f.gone).count() < 2
            && !matches!(self.state, State::Leave | State::Flee)
        {
            self.spawn_fly(now, env);
        }
        let cat = self.torso();
        for f in self.flies.iter_mut().filter(|f| !f.gone) {
            f.step(now, dt, env, s, cat);
            if f.gulp.is_some_and(|t| now - t >= GULP_SECS) {
                f.gone = true;
            }
        }

        let on_screen = self.pos.0 > env.mon.left as f32 && self.pos.0 < env.mon.right as f32;
        let can_startle = match self.state {
            State::Startle | State::Flee | State::Pounce | State::Jump | State::Hidden | State::Sneak => false,
            State::Leave | State::Wander => on_screen,
            _ => true,
        };
        if can_startle {
            let t = self.torso();
            let (dx, dy) = (t.0 - px, t.1 - py);
            let dist = (dx * dx + dy * dy).sqrt().max(1.0);
            let speed = (self.ptr_vel.0 * self.ptr_vel.0 + self.ptr_vel.1 * self.ptr_vel.1).sqrt();
            let toward = (self.ptr_vel.0 * dx + self.ptr_vel.1 * dy) / dist;
            let lunge = speed > 1500.0 * s && dist < 500.0 * s && toward > 0.6 * speed;
            if !env.dark(t.0, t.1) || lunge {
                self.left = px < self.pos.0;
                // A firefly already caught stays caught; one merely waiting flies off.
                if self.prey.and_then(|i| self.flies.get(i)).is_some_and(|f| f.grip == Grip::Held) {
                    self.set_grip(Grip::Free);
                }
                if self.prey.and_then(|i| self.flies.get(i)).is_some_and(|f| f.grip == Grip::Free) {
                    self.prey = None;
                }
                self.v = 0.0;
                self.bolt = now - self.startled < 20.0;
                self.startled = now;
                self.set(State::Startle, now, Clip::Startle.secs());
            }
        }
        if now - self.born > self.visit + OVERTIME_SECS {
            self.done = true;
            return None;
        }

        let recheck = matches!(self.ground, Ground::Ledge { .. }) && now - self.ledge_checked > 0.3;
        if recheck {
            self.ledge_checked = now;
        }
        if recheck && self.state != State::Jump && !self.check_ledge(env) {
            // The window under it moved away or closed: drop to the floor.
            let to = self.floor_below(env);
            self.jump_to(now, to, Ground::Floor, State::Idle);
        }

        let mut lift = 0.0;
        match self.state {
            State::Wander => {
                self.vary_cruise(now);
                let arrived = self.drive(dt, self.cruise);
                self.gait(now);
                if let Some(c) = self.cover
                    && ((c.side < 0.0 && self.pos.0 > c.rect.right as f32) || (c.side > 0.0 && self.pos.0 < c.rect.left as f32))
                {
                    // Out the far side: it shows again.
                    self.cover = None;
                }
                if arrived {
                    self.cover = None;
                    let groom = self.unit() < 0.3;
                    self.set_random(State::Idle, now, 2.0, 4.0);
                    self.play(if groom { Clip::Groom } else { Clip::Sit }, now);
                } else if self.ground == Ground::Floor && self.cover.is_none() && now >= self.hunt_after && self.nearest_fly(900.0 * s).is_some() {
                    self.set(State::Idle, now, 0.0);
                }
            }
            State::Idle if matches!(self.ground, Ground::Ledge { .. }) => {
                self.v = 0.0;
                let fly = now >= self.hunt_after && self.nearest_fly(1200.0 * s).is_some();
                let rim = resting && !windows_only() && now >= self.rim_after && self.rim_seat(env).is_some();
                if fly || rim || now - self.born > self.visit {
                    // Hop down to hunt, visit the light or leave.
                    let to = self.floor_below(env);
                    self.jump_to(now, to, Ground::Floor, State::Idle);
                } else if now >= self.until {
                    let r = self.unit();
                    if r < 0.45 {
                        if let Ground::Ledge { y, x0, x1 } = self.ground {
                            self.goal = (x0 + (x1 - x0) * self.unit(), y);
                            self.cruise_until = 0.0;
                            self.set(State::Wander, now, 0.0);
                        }
                    } else if r < 0.75 && self.pick_ledge(env) {
                        if let Some((to, land)) = self.leap.take() {
                            self.jump_to(now, to, land, State::Idle);
                        }
                    } else {
                        let to = self.floor_below(env);
                        self.jump_to(now, to, Ground::Floor, State::Idle);
                    }
                }
            }
            State::Idle => {
                self.v = 0.0;
                let fly = if now >= self.hunt_after { self.nearest_fly(1200.0 * s) } else { None };
                if now - self.born > self.visit {
                    self.goal = self.exit_goal(env, px);
                    self.set(State::Leave, now, 0.0);
                } else if let Some(f) = fly {
                    self.prey = Some(f);
                    self.plan_attack();
                    self.set(State::Hunt, now, 0.0);
                } else if resting && !windows_only() && now >= self.rim_after && let Some(g) = self.rim_seat(env) {
                    self.goal = g;
                    self.reaches = 0;
                    self.set(State::Approach, now, 0.0);
                } else if now >= self.until {
                    // Play with the windows now and then; otherwise stroll.
                    let r = match debug_cat() {
                        "climb" => 0.1,
                        "hide" => 0.4,
                        "scratch" => 0.5,
                        _ => self.unit(),
                    };
                    if r < 0.3 && self.pick_ledge(env) {
                        self.cruise_until = 0.0;
                        self.set(State::Climb, now, 0.0);
                    } else if r < 0.45 && self.pick_cover(env) {
                        self.cruise_until = 0.0;
                        self.set(State::Sneak, now, 0.0);
                    } else if r < 0.6 && self.scratches < 3 && self.pick_scratch(env) {
                        self.cruise_until = 0.0;
                        self.set(State::ToScratch, now, 0.0);
                    } else {
                        match self.wander_goal(env) {
                            Some(g) => {
                                self.goal = g;
                                self.cruise_until = 0.0;
                                self.set(State::Wander, now, 0.0);
                            }
                            None => self.until = now + 2.0,
                        }
                    }
                }
            }
            State::Climb => {
                self.vary_cruise(now);
                let arrived = self.drive(dt, self.cruise.max(WALK));
                self.gait(now);
                if arrived {
                    match self.leap.take() {
                        Some((to, land)) => self.jump_to(now, to, land, State::Idle),
                        None => self.set(State::Idle, now, 1.0),
                    }
                }
            }
            State::Jump => {
                self.play(Clip::Jump, now);
                if let Some(j) = &self.jump {
                    let u = (((now - j.start) / j.secs) as f32).clamp(0.0, 1.0);
                    let e = u * u * (3.0 - 2.0 * u);
                    self.pos = (j.from.0 + (j.to.0 - j.from.0) * e, j.from.1 + (j.to.1 - j.from.1) * e);
                    lift = j.height * (std::f32::consts::PI * u).sin();
                }
                if now >= self.until
                    && let Some(j) = self.jump.take()
                {
                    self.pos = j.to;
                    self.ground = j.land;
                    match j.then {
                        State::Flee => {
                            self.goal = self.flee_goal(env);
                            self.set(State::Flee, now, 0.0);
                        }
                        then => {
                            self.set_random(then, now, 0.6, 1.5);
                            self.play(Clip::Sit, now);
                        }
                    }
                }
            }
            State::Sneak => {
                // Walk to the entry spot, then in behind the window until fully hidden.
                let Some(c) = self.cover else {
                    self.set(State::Idle, now, 1.0);
                    return Some(self.scene(now, 0.0));
                };
                let edge = if c.side < 0.0 { c.rect.left as f32 } else { c.rect.right as f32 };
                let inside = edge - c.side * 120.0 * s;
                let arrived = self.drive(dt, if (self.goal.0 - inside).abs() < 1.0 { 55.0 } else { WALK });
                self.gait(now);
                if arrived {
                    if (self.goal.0 - inside).abs() < 1.0 {
                        self.set_random(State::Hidden, now, 1.5, 2.0);
                        self.play(Clip::Sit, now);
                    } else {
                        self.goal = (inside, self.pos.1);
                    }
                }
            }
            State::Hidden => {
                let Some(c) = self.cover else {
                    self.set(State::Idle, now, 1.0);
                    return Some(self.scene(now, 0.0));
                };
                if now >= self.until {
                    if c.through {
                        // Slip along behind it and come out the far side.
                        let far = if c.side < 0.0 { c.rect.right as f32 + 160.0 * s } else { c.rect.left as f32 - 160.0 * s };
                        self.goal = self.feet_bounds(env, (far, self.pos.1));
                        self.cruise_until = 0.0;
                        self.set(State::Wander, now, 0.0);
                    } else {
                        // Creep up to the edge until the face shows.
                        let edge = if c.side < 0.0 { c.rect.left as f32 } else { c.rect.right as f32 };
                        self.goal = (edge - c.side * 22.0 * s, self.pos.1);
                        // No deadline while it creeps up; the look-out gets one on arrival.
                        self.set(State::Peek, now, f64::INFINITY);
                    }
                }
            }
            State::Peek => {
                let Some(c) = self.cover else {
                    self.set(State::Idle, now, 1.0);
                    return Some(self.scene(now, 0.0));
                };
                if self.until.is_infinite() {
                    // Still creeping up to the edge.
                    self.play(Clip::Stalk, now);
                    if self.drive(dt, STALK) {
                        self.left = c.side < 0.0;
                        self.set_random(State::Peek, now, 2.0, 1.5);
                    }
                } else {
                    self.left = c.side < 0.0;
                    self.play(Clip::Stare, now);
                    if now >= self.until {
                        // Out it comes.
                        let edge = if c.side < 0.0 { c.rect.left as f32 } else { c.rect.right as f32 };
                        self.goal = self.feet_bounds(env, (edge + c.side * 170.0 * s, self.pos.1));
                        self.cover = None;
                        self.cruise_until = 0.0;
                        self.set(State::Wander, now, 0.0);
                    }
                }
            }
            State::ToScratch => {
                self.vary_cruise(now);
                let arrived = self.drive(dt, self.cruise.max(WALK));
                self.gait(now);
                if arrived {
                    if let Some((r, _)) = self.scratch {
                        self.left = self.pos.0 > (r.left + r.right) as f32 / 2.0;
                        self.scratch = Some((r, now));
                        self.scratches += 1;
                    }
                    self.set_random(State::Scratch, now, 2.0, 1.5);
                }
            }
            State::Scratch => {
                self.play(Clip::Scratch, now);
                if now >= self.until {
                    self.scratch = None;
                    let groom = self.unit() < 0.5;
                    self.set_random(State::Idle, now, 1.5, 2.0);
                    self.play(if groom { Clip::Groom } else { Clip::Sit }, now);
                }
            }
            State::Approach => match self.rim_seat(env).filter(|_| resting) {
                None => {
                    self.set(State::Idle, now, 1.0);
                    self.play(Clip::Sit, now);
                }
                Some(g) => {
                    self.goal = g;
                    // Trot over from afar, then slow to a careful walk near the light.
                    let far = (g.0 - self.pos.0).hypot(g.1 - self.pos.1) > 500.0 * s;
                    let arrived = self.drive(dt, if far { TROT } else { 70.0 });
                    self.gait(now);
                    if arrived {
                        self.set_random(State::Stare, now, 2.0, 2.0);
                    }
                }
            },
            State::Stare | State::Reach => {
                self.left = px < self.pos.0;
                let moved = self.rim_seat(env).is_none_or(|g| (g.0 - self.pos.0).abs() + (g.1 - self.pos.1).abs() > 40.0 * s);
                let fly = if now >= self.hunt_after { self.nearest_fly(700.0 * s) } else { None };
                if !resting || moved || (fly.is_some() && self.state == State::Stare) {
                    self.set(State::Idle, now, 1.5);
                    self.play(Clip::Sit, now);
                } else if now >= self.until {
                    if self.state == State::Stare && self.reaches >= 2 + (self.unit() * 3.0) as u32 {
                        // Enough of the light for now: it wanders off for a while.
                        self.rim_after = now + 25.0 + 20.0 * self.unit() as f64;
                        self.set(State::Idle, now, 0.5);
                        self.play(Clip::Sit, now);
                    } else if self.state == State::Stare {
                        self.reaches += 1;
                        self.set(State::Reach, now, Clip::Reach.secs());
                    } else {
                        self.set_random(State::Stare, now, 3.0, 3.0);
                    }
                }
                if self.state == State::Stare {
                    self.play(Clip::Stare, now);
                } else if self.state == State::Reach {
                    self.play(Clip::Reach, now);
                }
            }
            State::Hunt => {
                let Some(f) = self.prey_pos() else {
                    self.prey = None;
                    self.set(State::Idle, now, 0.5);
                    return Some(self.scene(now, 0.0));
                };
                self.goal = self.strike_seat(env, f);
                let d = (self.goal.0 - self.pos.0).hypot(self.goal.1 - self.pos.1);
                if d > 260.0 * s {
                    self.drive(dt, TROT);
                    self.gait(now);
                } else {
                    self.play(Clip::Stalk, now);
                    if self.drive(dt, STALK) || d < 6.0 * s {
                        self.v = 0.0;
                        self.left = f.0 < self.pos.0;
                        self.set_grip(Grip::Held);
                        match self.attack {
                            Attack::Pounce => self.set_random(State::Wiggle, now, 0.5, 0.7),
                            Attack::Snatch => self.set(State::Snatch, now, Clip::Reach.secs()),
                        }
                    }
                }
            }
            State::Wiggle => {
                self.play(Clip::Wiggle, now);
                match self.prey_pos() {
                    None => self.set(State::Idle, now, 0.5),
                    Some(f) if now >= self.until => {
                        // Leap so the jaws snap shut where the firefly hovers.
                        self.left = f.0 < self.pos.0;
                        let jaw = offset(cf::MOUTH[cf::POUNCE.0 + SNAP_FRAME]);
                        let to = self.feet_bounds(env, (f.0 - self.dir() * jaw.0 * s, f.1 - jaw.1 * s));
                        self.pounce = Some(Pounce { from: self.pos, to, start: now });
                        self.set(State::Pounce, now, POUNCE_SECS);
                    }
                    Some(_) => {}
                }
            }
            State::Pounce => {
                self.play(Clip::Pounce, now);
                let u = self.pounce.as_ref().map_or(1.0, |p| ((now - p.start) / POUNCE_SECS).clamp(0.0, 1.0) as f32);
                if let Some(p) = &self.pounce {
                    let e = u * u * (3.0 - 2.0 * u);
                    self.pos = (p.from.0 + (p.to.0 - p.from.0) * e, p.from.1 + (p.to.1 - p.from.1) * e);
                    let dist = (p.to.0 - p.from.0).hypot(p.to.1 - p.from.1);
                    lift = (40.0 * s + 0.25 * dist) * (std::f32::consts::PI * u).sin();
                }
                if self.dodge && !self.dodged && u >= 0.3 {
                    self.dodge_now(now);
                }
                if now >= self.until {
                    self.pounce = None;
                    if self.dodge {
                        self.missed(now);
                    } else {
                        self.set(State::Eat, now, 2.0 * Clip::Chew.secs());
                    }
                }
            }
            State::Snatch => {
                self.play(Clip::Reach, now);
                if self.dodge && !self.dodged && self.clip_frame(now) >= GRAB_FRAME - 2 {
                    self.dodge_now(now);
                }
                if now >= self.until {
                    if self.dodge {
                        self.missed(now);
                    } else {
                        self.set(State::Eat, now, 2.0 * Clip::ChewSit.secs());
                    }
                }
            }
            State::Eat => {
                let sitting = self.clip == Clip::Reach || self.clip == Clip::ChewSit;
                self.play(if sitting { Clip::ChewSit } else { Clip::Chew }, now);
                if now >= self.until {
                    self.misses = 0;
                    self.prey = None;
                    // Full for a bit: time for other games before the next hunt.
                    self.hunt_after = now + 10.0 + 10.0 * self.unit() as f64;
                    self.set_random(State::Idle, now, 1.0, 2.0);
                    self.play(Clip::Sit, now);
                }
            }
            State::Startle => {
                self.play(Clip::Startle, now);
                self.cover = None;
                self.scratch = None;
                if now >= self.until {
                    if self.ground == Ground::Floor {
                        self.goal = self.flee_goal(env);
                        self.set(State::Flee, now, 0.0);
                    } else {
                        let to = self.floor_below(env);
                        self.jump_to(now, to, Ground::Floor, State::Flee);
                    }
                }
            }
            State::Flee | State::Leave => {
                let run = self.state == State::Flee;
                let arrived = self.drive(dt, if run { RUN } else { WALK });
                self.gait(now);
                let hid = self.pos.0 > env.mon.left as f32 && self.pos.0 < env.mon.right as f32;
                if arrived && run && hid {
                    // Safe in the shadows: it crouches, and keeps away from the light for a while.
                    self.rim_after = now + 10.0 + 10.0 * self.unit() as f64;
                    self.hunt_after = self.hunt_after.max(now + 3.0);
                    self.set_random(State::Idle, now, 3.0, 4.0);
                    self.play(Clip::Sit, now);
                } else if arrived {
                    self.done = true;
                    return None;
                }
            }
        }
        self.follow_prey(now, dt, lift);
        Some(self.scene(now, lift))
    }

    /// Keeps the prey on the tentacle tip or in the mouth exactly where the
    /// current frame draws them, and swallows it when the jaws close.
    fn follow_prey(&mut self, now: f64, dt: f32, lift: f32) {
        let Some(i) = self.prey else { return };
        let (first, _, _, _) = self.clip.spec();
        let k = self.clip_frame(now);
        let frame = first + k;
        let mouth = self.at(offset(cf::MOUTH[frame]), lift);
        let tip = cf::TIP[frame];
        let tip = (tip != (0.0, 0.0)).then(|| self.at(offset(tip), lift));
        let (clip, dodge) = (self.clip, self.dodge);
        let Some(f) = self.flies.get_mut(i).filter(|f| !f.gone) else { return };
        let pull = |f: &mut Fly, to: (f32, f32), rate: f32| {
            let a = (rate * dt).min(1.0);
            f.pos = (f.pos.0 + (to.0 - f.pos.0) * a, f.pos.1 + (to.1 - f.pos.1) * a);
        };
        match (clip, f.grip) {
            // The tentacle closes in, and the tip takes hold at full stretch.
            (Clip::Reach, Grip::Held) if !dodge => {
                if let Some(t) = tip
                    && k + 2 >= GRAB_FRAME
                {
                    pull(f, t, 18.0);
                    if k >= GRAB_FRAME {
                        f.grip = Grip::Tip;
                    }
                }
            }
            // Riding the tip as it curls and pulls back; in the mouth once it's in.
            (_, Grip::Tip) => match tip {
                Some(t) => f.pos = t,
                None => {
                    f.grip = Grip::Mouth;
                    f.pos = mouth;
                    f.gulp.get_or_insert(now);
                }
            },
            // Mid-leap the open jaws close over it.
            (Clip::Pounce, Grip::Held) if !dodge && k >= 3 => {
                pull(f, mouth, 20.0);
                if k >= SNAP_FRAME - 1 {
                    f.grip = Grip::Mouth;
                }
            }
            (_, Grip::Mouth) => {
                f.pos = mouth;
                if clip != Clip::Pounce || k >= SNAP_FRAME {
                    f.gulp.get_or_insert(now);
                }
            }
            _ => {}
        }
    }

    fn scene(&self, now: f64, lift: f32) -> Scene {
        let (first, _, _, _) = self.clip.spec();
        let frame = first + self.clip_frame(now);
        // Behind a window only the part beyond its edge shows.
        let clip = self.cover.map(|c| {
            let behind_left = self.pos.0 < (c.rect.left + c.rect.right) as f32 / 2.0;
            if behind_left { (f32::MIN, c.rect.left as f32) } else { (c.rect.right as f32, f32::MAX) }
        });
        // A scratched window trembles, in bursts that follow the paw strokes.
        let shake = self.scratch.filter(|_| self.state == State::Scratch).map(|(rect, t0)| {
            let t = (now - t0) as f32;
            let tau = std::f32::consts::TAU;
            let fade = (t / 0.15).min(1.0) * (((self.until - now) as f32) / 0.15).clamp(0.0, 1.0);
            let a = SHAKE * self.scale * fade * (0.55 + 0.45 * (tau * 2.0 * t).sin());
            ShakeDraw { rect, dx: a * (tau * 16.0 * t).sin(), dy: 0.4 * a * (tau * 11.0 * t + 1.0).sin(), id: self.scratches }
        });
        let mut scene = Scene {
            cat: Some(CritterDraw { x: self.pos.0, y: self.pos.1 - lift, scale: self.scale, frame, flip: self.left, clip }),
            flies: [None; MAX_FLIES],
            shake,
        };
        // Swallowed fireflies stay in the list (the cat tracks its prey by index) but aren't drawn.
        for (slot, f) in scene.flies.iter_mut().zip(self.flies.iter().filter(|f| !f.gone)) {
            *slot = Some(f.draw(now, self.scale));
        }
        scene
    }
}
