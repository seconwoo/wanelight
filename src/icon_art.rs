// Procedural icon art (a waning crescent). Dependency-free so build.rs can
// `include!` it to generate the .ico embedded in the executable.

/// Straight-alpha RGBA pixels, row-major, top row first.
pub type Rgba = Vec<u8>;

fn inside_circle(x: f32, y: f32, cx: f32, cy: f32, r: f32) -> bool {
    let (dx, dy) = (x - cx, y - cy);
    dx * dx + dy * dy <= r * r
}

/// Crescent in unit coordinates: a disc with an offset disc cut out of it.
fn crescent(u: f32, v: f32, moon_r: f32) -> bool {
    inside_circle(u, v, 0.5, 0.5, moon_r)
        && !inside_circle(u, v, 0.5 + moon_r * 0.52, 0.5 - moon_r * 0.34, moon_r * 0.86)
}

/// Supersampled coverage (0..1) of `shape` over pixel (px, py).
fn coverage(size: u32, px: u32, py: u32, shape: &dyn Fn(f32, f32) -> bool) -> f32 {
    const N: u32 = 4;
    let mut hits = 0;
    for sy in 0..N {
        for sx in 0..N {
            let u = (px as f32 + (sx as f32 + 0.5) / N as f32) / size as f32;
            let v = (py as f32 + (sy as f32 + 0.5) / N as f32) / size as f32;
            if shape(u, v) {
                hits += 1;
            }
        }
    }
    hits as f32 / (N * N) as f32
}

/// Monochrome tray icon matching the taskbar theme.
pub fn tray_icon_rgba(size: u32, light_taskbar: bool, paused: bool) -> Rgba {
    let rgb: [u8; 3] = if light_taskbar { [0x1c, 0x1c, 0x1c] } else { [0xf5, 0xf5, 0xf5] };
    let opacity = if paused { 0.42 } else { 1.0 };
    let mut out = vec![0u8; (size * size * 4) as usize];
    let shape = |u: f32, v: f32| crescent(u, v, 0.44);
    for py in 0..size {
        for px in 0..size {
            let a = coverage(size, px, py, &shape) * opacity;
            let i = ((py * size + px) * 4) as usize;
            out[i..i + 3].copy_from_slice(&rgb);
            out[i + 3] = (a * 255.0).round() as u8;
        }
    }
    out
}

fn inside_rounded_square(u: f32, v: f32, radius: f32) -> bool {
    let (m, r) = (0.04, radius);
    if u < m || v < m || u > 1.0 - m || v > 1.0 - m {
        return false;
    }
    let cx = u.clamp(m + r, 1.0 - m - r);
    let cy = v.clamp(m + r, 1.0 - m - r);
    inside_circle(u, v, cx, cy, r)
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// Full-colour application icon: amber crescent on a dark rounded tile.
pub fn app_icon_rgba(size: u32) -> Rgba {
    let mut out = vec![0u8; (size * size * 4) as usize];
    let tile = |u: f32, v: f32| inside_rounded_square(u, v, 0.2);
    let moon = |u: f32, v: f32| crescent(u, v, 0.33);
    for py in 0..size {
        for px in 0..size {
            let (u, v) = ((px as f32 + 0.5) / size as f32, (py as f32 + 0.5) / size as f32);
            let bg_a = coverage(size, px, py, &tile);
            let fg_a = coverage(size, px, py, &moon);
            // Tile: deep blue-grey, slightly lighter at the top.
            let bg = [lerp(0.13, 0.07, v), lerp(0.15, 0.08, v), lerp(0.20, 0.11, v)];
            // Moon: warm amber fading to a softer orange toward the lit edge.
            let t = (u * 0.6 + v * 0.4).clamp(0.0, 1.0);
            let fg = [lerp(1.0, 0.98, t), lerp(0.84, 0.62, t), lerp(0.48, 0.30, t)];
            let rgb: Vec<f32> = (0..3).map(|c| lerp(bg[c], fg[c], fg_a)).collect();
            let a = bg_a.max(fg_a);
            let i = ((py * size + px) * 4) as usize;
            for c in 0..3 {
                out[i + c] = (rgb[c].clamp(0.0, 1.0) * 255.0).round() as u8;
            }
            out[i + 3] = (a * 255.0).round() as u8;
        }
    }
    out
}
