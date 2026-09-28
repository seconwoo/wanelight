# Wanelight

Non-intrusive OLED burn-in prevention for Windows 10/11.

Burn-in comes from bright pixels that stay unchanged for hours. Wanelight watches for exactly
that and dims only those areas. It ignores whether you are touching the mouse, so a movie
never dims and a game with a moving picture is left alone. Changes fade in slower than you
can notice and release within a fraction of a second.

## What it does

| Feature | Behaviour |
|---|---|
| **Static dimming** | Samples each monitor about once a second (DXGI Desktop Duplication, reduced on the GPU to 16×16-pixel cells). Bright areas unchanged for 3 minutes fade down by up to 25% (taskbar, sidebars, logos, HUDs). The window you are using is protected: at most 10%, and only after 15 minutes. |
| **Unattended monitors** | A second monitor that hasn't had the cursor or focus for 5 minutes gets stronger dimming of its static areas. |
| **High-risk apps** | Chat, trading and monitoring apps (your list) get stronger dimming when they aren't in front. |
| **Away** | No input *and* a still screen for 5 minutes fades the screen down; any input restores it instantly. After 20 minutes the displays are turned off, unless audio is playing. |
| **DDC/CI brightness** | Optional (off by default): also lowers the monitor's own brightness while you're away. The original brightness is restored on exit, and after a crash on the next start. |
| **Pixel-refresh helper** | Counts panel-on hours. After 4 hours it turns the display off at a quiet moment so the panel can run its compensation cycle. |
| **Wear heatmap** | Keeps a per-cell ledger of light emitted and light avoided by dimming, and shows it in the settings window. |
| **Windows tweaks** | One-click, reversible OLED-friendly settings: dark mode, black desktop, hidden icons, no accent colour, taskbar auto-hide, display timeout. |

It is designed to be non-disruptive:

- The overlay is click-through and never takes focus.
- It is hidden entirely when nothing is dimmed, and otherwise covers only the dimmed region.
- It is excluded from screen capture, so screenshots and streams never show it, and Wanelight's own sampling isn't fooled by it.
- Presentation mode and apps you exclude pause everything.
- Pause or resume anytime with **Ctrl+Alt+Shift+W** or the tray menu.

Measured on a 3440×1440 display: about 3.5 ms per sample, around 0.3% of one CPU core, and about 50 MB of RAM.

## Build

Requires the Rust MSVC toolchain (1.92+) and the Windows SDK (for `rc.exe`).

```bash
cargo build --release
```

The result is a single file, `target\release\wanelight.exe`.

## Use

```bash
wanelight.exe
```

This starts the tray agent; running it again opens the settings window. Other flags:

| Flag | Purpose |
|---|---|
| `--ui [tab]` | Open the settings window (`overview`, `heatmap`, `protection`, `away`, `apps`, `tweaks`) |
| `--status` | Print the running agent's status as JSON |
| `--quit` | Stop the running agent |
| `--selftest` | Check capture, the GPU shader and capture exclusion (briefly dims a small square) |
| `--selftest --map` | Read-only: print a map of which parts of the screen are changing |
| `--test-surface [secs]` | Show a white test square to watch dimming happen |

Settings, logs and wear data live in `%APPDATA%\Wanelight`, or in `WANELIGHT_DATA_DIR` if that is set.
`config.toml` can be edited by hand; the agent reloads it within a second.

## Layout

```
src/agent/capture.rs   Desktop Duplication + compute shader -> per-cell stats
src/agent/model.rs     static-time model, dimming policy, ramps, feathering
src/agent/winmap.rs    z-ordered window map (foreground, taskbar, per-app rules)
src/agent/overlay.rs   DirectComposition click-through overlay (capture-excluded)
src/agent/ddc.rs       DDC/CI worker thread with crash-safe restore
src/agent/mod.rs       tray agent, tiers, power/away logic
src/ui/                settings window (egui, separate process)
src/hardening.rs       reversible Windows tweaks
```
