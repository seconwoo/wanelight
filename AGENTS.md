# AGENTS.md

Guidance for coding agents working on Wanelight, a Windows tray app that prevents OLED burn-in by dimming bright, unchanging screen areas with a click-through overlay. See README.md for what the app does from a user's point of view.

## Build and run

- Toolchain: Rust MSVC 1.92+ (developed on 1.94) and the Windows SDK (`rc.exe`, used by `build.rs` to embed the icon and the PerMonitorV2 manifest).
- `cargo build --release` produces a single file, `target\release\wanelight.exe`.
- Whenever you build, also run `cargo build --release` so `target\release` always has the latest code, alongside any test build in `target/test`. If the build can't replace the exe because the agent is running, say so. Don't stop the agent yourself.
- Keep `eframe` pinned to 0.35. Version 0.36 needs rustc 1.95.
- New Win32 APIs usually need an extra `windows` feature in `Cargo.toml`. If a type is "not found", check the feature list there first.
- Release builds use the `windows` subsystem, so there is no console. Commands that print output call `util::attach_console()`.

## Testing without disturbing the user's instance

The user normally has an agent running from `target\release`. Don't kill it while testing unless they ask. Test with the copy in `target/test`, not the one in `target\release`.

- Build test copies into a separate target dir: `cargo build --release --target-dir target/test`.
- Run the test copy with `WANELIGHT_DATA_DIR=<scratch dir>`. That separates its config, logs and ledger, and adds a suffix to the single-instance mutex and agent window class, so it runs next to the real one. Without the variable, launching a second copy just opens the settings window of the one already running.
- Set `WANELIGHT_DEBUG=1` to enable logging (`<data dir>\agent.log`). With debugging on, `--status` also writes `mask-<monitor>.bmp` showing what the overlay is dimming.
- `WANELIGHT_DEBUG_POINTER=x,y` pins the pointer position used by the focus modes, for repeatable tests.
- Self-checks:
  - `--selftest` covers capture, the shader and capture exclusion, plus the check that the overlay doesn't break the auto-hide taskbar.
  - `--selftest --map` is read-only and prints which parts of the screen are changing.
  - `--panel-probe [--brief]` prints the panels panel mode would pick.
  - `--test-surface [secs]` shows a white square that you can watch dim.
- Spooky mode's cat is drawn inside the overlay, so it is capture-excluded too. To grab it on screen, start the test copy with `--no-capture-exclusion` and pin the pointer with `WANELIGHT_DEBUG_POINTER`. With debugging on, the log records the cat's state changes (`cat: Hunt -> Wiggle`). `WANELIGHT_DEBUG_CAT=windows` leaves out fireflies and torch visits so the cat only plays with windows; `climb`, `hide` or `scratch` makes it pick that game every time. With `--no-capture-exclusion` the capture also sees the overlay, so a scratched window's shaking copy looks dimmed twice; that's expected.
- Live screen content confuses before/after comparisons. Use `--test-surface` or another controlled window, not whatever is on screen.
- Stop test instances when you're done: `WANELIGHT_DATA_DIR=<same dir> wanelight.exe --quit`.

## Architecture

One exe, several modes selected by argument (`src/main.rs`). With no argument it runs the tray agent. `--ui` runs the egui settings window as a separate process. The two talk through window messages to the agent (`ipc.rs`) and through files in the data dir (`config.toml`, `status.json`).

```
capture.rs  DXGI Desktop Duplication + HLSL compute shader -> per 16x16 cell mean/max/changed
model.rs    per-cell static time, dimming targets (static, chrome, torch), eased ramps, blur
winmap.rs   z-ordered window map: foreground, taskbar, per-app rules
panels.rs   UI Automation on a background MTA thread, used by torch panel mode
overlay.rs  DirectComposition overlay at grid resolution, scaled x16, capture-excluded; cat sprite layer under the mask
spook.rs    spooky mode: torch flicker, schedules the cat's visits
critter.rs  spooky mode's cat and fireflies: behavior, gait, hunting, window play
color.rs    full-screen color matrix (Magnification API) for deeper blacks
mod.rs      agent loop, tiers (away, display off, pixel refresh), input tracking, tray, IPC
ddc.rs      DDC/CI brightness worker with a crash-safe restore file
```

## Invariants: don't break these

- **The overlay never covers a whole monitor.** A full-monitor topmost window counts as a full-screen app, which breaks the auto-hide taskbar. `overlay.rs` keeps an `EDGE_GAP` of 1 px, and `--selftest` checks this.
- **The overlay is excluded from capture** (`WDA_EXCLUDEFROMCAPTURE`). Otherwise the agent would sample its own dimming and feed back on itself.
- **The overlay is click-through, never activates, and is hidden when nothing is dimmed.** It is sized to the bounding box of dimmed cells.
- **Content changes drive dimming, not input idleness.** Video and moving games must never dim. Away mode needs no input *and* a still screen.
- **Release is fast and fade-in is slow.** Keep the easing in `model.rs::ramp` and the `Motion` kinds; don't add abrupt jumps.
- **Torch mode overrides static and chrome dimming** and doesn't stack with them. Stacking caused uneven patches.
- **Panel mode reads layout only**: bounding rects, control types, landmarks. It never reads text or key values. Raw keyboard input is used only to know that typing happened.
- **Panel mode must follow layout changes.** Chromium reports stale ancestor bounds, so don't clip a panel to its ancestors. Requery after clicks, large redraws and returning from typing. The retry cap for apps without an accessibility tree must stay.
- **The CPU budget is under 1% of a core.** Don't clone per frame, recompute only when inputs change, and keep the frame timer adaptive (16 ms while animating, 33 ms while watching, stopped otherwise).
- **Hardening tweaks must stay reversible.** Record the previous values before changing anything, and never apply tweaks to the user's system as part of testing.
- **Spooky sprites cost nothing while no cat is on screen.** The overlay creates them when a visit starts and frees them when it ends, and decodes frames one at a time into a small cache. Never decode the whole sheet or keep it on the GPU.
- **Restore DDC brightness on exit and after a crash.**
- **Spooky mode never touches other windows.** The cat walks on, hides behind and scratches windows only in the overlay; a scratched window's shake is a copy from the last captured frame. It never moves, clicks or reads other windows.
- **The color matrix is not excluded from capture.** Desktop Duplication sees it, so the model measures what the panel shows. Never make the matrix depend on captured brightness, or it feeds back on itself. Windows resets the matrix when the process exits, even on a crash.

## Conventions

- Match the surrounding code: small modules, few comments, plain `windows` crate calls inside `unsafe` blocks, and no new dependencies without a good reason.
- New config fields go in `config.rs` with serde defaults and a clamp in `sanitized()`. Settings sliders use `SliderClamping::Edits`, and the UI writes config only when a value changes.
- The settings UI is always dark.
- The README demos in `docs/*.svg` are hand-written, self-animating SVGs (SMIL, no scripts). `heatmap.svg` is generated. The demo dimming is deliberately stronger than the defaults. To check a change, render frames in headless Edge using `pauseAnimations()` and `setCurrentTime()`.
- The spooky cat is drawn in code in `art/cat/cat.html` (open it in a browser to preview). `art/bake.ps1` bakes it with headless Edge into `assets/cat.bin` (every frame trimmed to what's drawn, as its own PNG), writes `art/cat/sheet.png` for review, and generates `src/agent/cat_frames.rs` (clip ranges, where each frame's PNG is, and the mouth and tentacle-tip position in every frame, which keep caught fireflies on the drawing). Clips in `DRAFTS` are previewed but not baked; move them to `CLIPS` when the agent uses them. Rebake after any art change; don't edit the generated file.
- User-facing text (README, UI strings) uses plain, short sentences.
- Commit messages have an imperative subject line and a short body. Commit on `master` when asked. Don't push unless asked.
