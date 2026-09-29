//! Click-through dimming overlay for one monitor.
//!
//! The mask is a tiny texture (one texel per 16x16 cell) that DirectComposition
//! scales up with bilinear filtering, so an update uploads ~100 KB instead of a
//! full-resolution bitmap. The window only covers the bounding box of the
//! dimmed cells and is hidden entirely when nothing is dimmed, so fullscreen
//! apps elsewhere keep direct scan-out. It is excluded from screen capture, so
//! our own sampling (and the user's screenshots) never see it.
//!
//! Spooky mode's cat is a visual underneath the mask, so it is dimmed (and
//! capture-excluded) along with everything else. Its fireflies make their own
//! light, so they sit above the mask. A window the cat scratches "shakes": a
//! copy of it from the last captured frame is shown jittering under the cat;
//! the real window is never touched.
//!
//! The window never covers the whole monitor: Windows treats any visible
//! topmost window that does as a fullscreen app, which stops the auto-hide
//! taskbar from appearing and can switch on "do not disturb".

use std::sync::{Once, OnceLock};

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F;
use windows::Win32::Graphics::DirectComposition::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{Interface, Result, w};
use windows_numerics::Matrix3x2;

use super::capture::{CELL, GridGeom, Gpu};
use super::critter::{self, MAX_FLIES, Scene};
use crate::log;

const CLASS: windows::core::PCWSTR = w!("WanelightOverlay");
/// Pixel rows left uncovered at the bottom of the monitor.
const EDGE_GAP: i32 = 1;

unsafe extern "system" fn overlay_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

fn register_class() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe {
        let hinst = GetModuleHandleW(None).unwrap_or_default();
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(overlay_proc),
            hInstance: hinst.into(),
            lpszClassName: CLASS,
            ..Default::default()
        };
        RegisterClassExW(&wc);
    });
}

/// The cat sprite sheet, decoded once to premultiplied BGRA.
struct Atlas {
    pixels: Vec<u8>,
    width: u32,
    height: u32,
}

fn atlas() -> Option<&'static Atlas> {
    static ATLAS: OnceLock<Option<Atlas>> = OnceLock::new();
    ATLAS
        .get_or_init(|| match decode_png(include_bytes!("../../assets/cat.png")) {
            Ok(a) => Some(a),
            Err(e) => {
                log!("overlay: cannot decode the cat sprites: {}", e.message());
                None
            }
        })
        .as_ref()
}

fn decode_png(bytes: &[u8]) -> Result<Atlas> {
    unsafe {
        let factory: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
        let stream = factory.CreateStream()?;
        stream.InitializeFromMemory(bytes)?;
        let decoder = factory.CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnDemand)?;
        let conv = factory.CreateFormatConverter()?;
        conv.Initialize(&decoder.GetFrame(0)?, &GUID_WICPixelFormat32bppPBGRA, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom)?;
        let (mut width, mut height) = (0, 0);
        conv.GetSize(&mut width, &mut height)?;
        let mut pixels = vec![0u8; (width * height * 4) as usize];
        conv.CopyPixels(std::ptr::null(), width * 4, &mut pixels)?;
        Ok(Atlas { pixels, width, height })
    }
}

/// One sprite from the atlas: its own small swap chain showing one frame.
struct Sprite {
    visual: IDCompositionVisual,
    swap: IDXGISwapChain1,
    frame: Option<usize>,
}

/// A copy of a scratched window, made when a scratch starts.
struct Shake {
    visual: IDCompositionVisual,
    swap: Option<IDXGISwapChain1>,
    /// Scratch id of the copy, and its rect relative to the monitor.
    id: Option<u32>,
    at: RECT,
}

/// Spooky mode's cat and fireflies.
struct Sprites {
    device: ID3D11Device,
    factory: IDXGIFactory2,
    atlas: ID3D11Texture2D,
    /// Clips the cat while it hides behind a window.
    cat_box: IDCompositionVisual,
    cat: Sprite,
    flies: [Sprite; MAX_FLIES],
    shake: Shake,
    scene: Scene,
}

/// Everything, for clips that show all.
const NO_CLIP: D2D_RECT_F = D2D_RECT_F { left: -1.0e7, top: -1.0e7, right: 1.0e7, bottom: 1.0e7 };

const HIDDEN: Matrix3x2 = Matrix3x2 { M11: 0.0, M12: 0.0, M21: 0.0, M22: 0.0, M31: 0.0, M32: 0.0 };

pub struct Overlay {
    hwnd: HWND,
    dcomp: IDCompositionDevice,
    _target: IDCompositionTarget,
    _root: IDCompositionVisual,
    visual: IDCompositionVisual,
    sprites: Option<Sprites>,
    swap: IDXGISwapChain1,
    context: ID3D11DeviceContext,
    geom: GridGeom,
    mon: RECT,
    visible: bool,
    win_rect: RECT,
    pixels: Vec<u8>,
    uploaded: Vec<u8>,
}

impl Overlay {
    pub fn new(gpu: &Gpu, mon: RECT, geom: GridGeom, exclude_from_capture: bool) -> Result<Overlay> {
        register_class();
        unsafe {
            let hinst = GetModuleHandleW(None)?;
            let hwnd = CreateWindowExW(
                WS_EX_NOREDIRECTIONBITMAP
                    | WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_TOPMOST
                    | WS_EX_TOOLWINDOW
                    | WS_EX_NOACTIVATE,
                CLASS,
                w!(""),
                WS_POPUP,
                // Sized properly on first show; never monitor-sized (see module docs).
                mon.left,
                mon.top,
                1,
                1,
                None,
                None,
                Some(hinst.into()),
                None,
            )?;
            SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA)?;
            if exclude_from_capture
                && let Err(e) = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE) {
                    log!("overlay: cannot exclude from capture: {}", e.message());
                }
            let dxgi_device: IDXGIDevice = gpu.device.cast()?;
            let dcomp: IDCompositionDevice = DCompositionCreateDevice(&dxgi_device)?;
            let target = dcomp.CreateTargetForHwnd(hwnd, true)?;
            let root = dcomp.CreateVisual()?;
            let visual = dcomp.CreateVisual()?;
            let factory: IDXGIFactory2 = dxgi_device.GetAdapter()?.GetParent()?;
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: geom.gw as u32,
                Height: geom.gh as u32,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                Stereo: false.into(),
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
                AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
                Flags: 0,
            };
            let swap = factory.CreateSwapChainForComposition(&gpu.device, &desc, None)?;
            visual.SetContent(&swap)?;
            visual.SetBitmapInterpolationMode(DCOMPOSITION_BITMAP_INTERPOLATION_MODE_LINEAR)?;
            visual.SetBorderMode(DCOMPOSITION_BORDER_MODE_SOFT)?;
            let sprites = Sprites::new(gpu, &dcomp, &factory).unwrap_or_else(|e| {
                log!("overlay: no spooky sprites: {}", e.message());
                None
            });
            // Back to front: a shaking window copy, the cat, the mask (so both are
            // dimmed with everything else), then the glowing fireflies.
            match &sprites {
                Some(sp) => {
                    root.AddVisual(&sp.shake.visual, false, None)?;
                    root.AddVisual(&sp.cat_box, true, &sp.shake.visual)?;
                    root.AddVisual(&visual, true, &sp.cat_box)?;
                    for f in &sp.flies {
                        root.AddVisual(&f.visual, true, &visual)?;
                    }
                }
                None => root.AddVisual(&visual, false, None)?,
            }
            target.SetRoot(&root)?;
            dcomp.Commit()?;
            Ok(Overlay {
                hwnd,
                dcomp,
                _target: target,
                _root: root,
                visual,
                sprites,
                swap,
                context: gpu.context.clone(),
                geom,
                mon,
                visible: false,
                win_rect: RECT::default(),
                pixels: vec![0; geom.len() * 4],
                uploaded: Vec::new(),
            })
        }
    }

    /// Shows the mask (per-cell dim, 0..1). Hides the window when all zero.
    pub fn show(&mut self, alpha: &[f32]) -> Result<()> {
        let (gw, gh) = (self.geom.gw, self.geom.gh);
        let (mut x0, mut y0, mut x1, mut y1) = (usize::MAX, usize::MAX, 0usize, 0usize);
        for y in 0..gh {
            for x in 0..gw {
                let i = y * gw + x;
                let a = (alpha.get(i).copied().unwrap_or(0.0).clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
                self.pixels[i * 4 + 3] = a;
                if a > 0 {
                    x0 = x0.min(x);
                    y0 = y0.min(y);
                    x1 = x1.max(x);
                    y1 = y1.max(y);
                }
            }
        }
        if x0 == usize::MAX {
            self.hide();
            return Ok(());
        }
        unsafe {
            if self.pixels != self.uploaded {
                let back: ID3D11Texture2D = self.swap.GetBuffer(0)?;
                self.context.UpdateSubresource(&back, 0, None, self.pixels.as_ptr() as _, (gw * 4) as u32, 0);
                self.swap.Present(0, DXGI_PRESENT(0)).ok()?;
                self.uploaded.clone_from(&self.pixels);
            }
            // Cover the dimmed cells plus one cell of bilinear falloff on each side.
            let cell_x = |cx: isize| self.mon.left + self.geom.ox + CELL * cx as i32;
            let cell_y = |cy: isize| self.mon.top + self.geom.oy + CELL * cy as i32;
            let rect = RECT {
                left: cell_x(x0 as isize - 1).max(self.mon.left),
                top: cell_y(y0 as isize - 1).max(self.mon.top),
                right: cell_x(x1 as isize + 2).min(self.mon.right),
                // Stay one row short of the bottom edge: a monitor-sized window
                // counts as a fullscreen app, and the auto-hide taskbar watches this row.
                bottom: cell_y(y1 as isize + 2).min(self.mon.bottom - EDGE_GAP),
            };
            if rect.bottom <= rect.top || rect.right <= rect.left {
                self.hide();
                return Ok(());
            }
            if rect != self.win_rect || !self.visible {
                let m = Matrix3x2 {
                    M11: CELL as f32,
                    M12: 0.0,
                    M21: 0.0,
                    M22: CELL as f32,
                    M31: (self.mon.left + self.geom.ox - rect.left) as f32,
                    M32: (self.mon.top + self.geom.oy - rect.top) as f32,
                };
                self.visual.SetTransform2(&m)?;
                self.win_rect = rect;
                self.place_sprites()?;
                self.dcomp.Commit()?;
                SetWindowPos(
                    self.hwnd,
                    Some(HWND_TOPMOST),
                    rect.left,
                    rect.top,
                    rect.right - rect.left,
                    rect.bottom - rect.top,
                    SWP_NOACTIVATE | SWP_SHOWWINDOW | SWP_NOOWNERZORDER,
                )?;
                self.win_rect = rect;
                self.visible = true;
            }
        }
        Ok(())
    }

    /// Shows spooky mode's cat and fireflies. Only swaps sprites whose frame
    /// changed; otherwise just moves the visuals. `desktop` is the monitor's last
    /// captured frame, used to copy a window the cat starts scratching.
    pub fn set_scene(&mut self, scene: &Scene, desktop: Option<&ID3D11Texture2D>) -> Result<()> {
        let Some(sp) = &mut self.sprites else { return Ok(()) };
        if sp.scene == *scene {
            return Ok(());
        }
        sp.scene = *scene;
        match (scene.shake, desktop) {
            (Some(sh), Some(tex)) if sp.shake.id != Some(sh.id) => {
                sp.shake.id = Some(sh.id);
                if let Err(e) = sp.shake.copy(&sp.device, &sp.factory, &self.context, tex, sh.rect, self.mon) {
                    log!("overlay: cannot copy the scratched window: {}", e.message());
                    sp.shake.drop_copy();
                }
            }
            (None, _) if sp.shake.id.is_some() => sp.shake.drop_copy(),
            _ => {}
        }
        if let Some(d) = scene.cat {
            sp.cat.show(&self.context, &sp.atlas, d.frame, (0, 0), (critter::FRAME_W, critter::FRAME_H))?;
        }
        for (sprite, fly) in sp.flies.iter_mut().zip(&scene.flies) {
            if let Some(f) = fly {
                sprite.show(&self.context, &sp.atlas, f.frame, critter::FLY_AT, (critter::FLY, critter::FLY))?;
            }
        }
        self.place_sprites()?;
        unsafe { self.dcomp.Commit() }
    }

    /// Positions the sprites relative to the window; a zero scale hides one.
    fn place_sprites(&self) -> Result<()> {
        let Some(sp) = &self.sprites else { return Ok(()) };
        let (wx, wy) = (self.win_rect.left as f32, self.win_rect.top as f32);
        let cat = match sp.scene.cat {
            Some(d) => {
                let sx = if d.flip { -d.scale } else { d.scale };
                Matrix3x2 {
                    M11: sx,
                    M12: 0.0,
                    M21: 0.0,
                    M22: d.scale,
                    M31: d.x - wx - sx * critter::ANCHOR.0,
                    M32: d.y - wy - d.scale * critter::ANCHOR.1,
                }
            }
            None => HIDDEN,
        };
        let clip = match sp.scene.cat.and_then(|d| d.clip) {
            Some((lo, hi)) => D2D_RECT_F { left: (lo - wx).max(-1.0e7), top: -1.0e7, right: (hi - wx).min(1.0e7), bottom: 1.0e7 },
            None => NO_CLIP,
        };
        let shake = match (sp.scene.shake, &sp.shake.swap) {
            (Some(sh), Some(_)) => Matrix3x2 {
                M11: 1.0,
                M12: 0.0,
                M21: 0.0,
                M22: 1.0,
                M31: (self.mon.left + sp.shake.at.left) as f32 - wx + sh.dx,
                M32: (self.mon.top + sp.shake.at.top) as f32 - wy + sh.dy,
            },
            _ => HIDDEN,
        };
        unsafe {
            sp.cat_box.SetClip2(&clip)?;
            sp.shake.visual.SetTransform2(&shake)?;
            sp.cat.visual.SetTransform2(&cat)?;
            for (sprite, fly) in sp.flies.iter().zip(&sp.scene.flies) {
                let m = match fly {
                    Some(f) => {
                        let half = f.scale * critter::FLY as f32 / 2.0;
                        Matrix3x2 { M11: f.scale, M12: 0.0, M21: 0.0, M22: f.scale, M31: f.x - wx - half, M32: f.y - wy - half }
                    }
                    None => HIDDEN,
                };
                sprite.visual.SetTransform2(&m)?;
            }
        }
        Ok(())
    }

    pub fn hide(&mut self) {
        if self.visible {
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
            self.visible = false;
        }
    }

    /// Other topmost windows (e.g. a clicked taskbar) can rise above us.
    pub fn keep_on_top(&self) {
        if self.visible {
            unsafe {
                let _ = SetWindowPos(
                    self.hwnd,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
                );
            }
        }
    }
}

impl Sprite {
    fn new(gpu: &Gpu, dcomp: &IDCompositionDevice, factory: &IDXGIFactory2, width: u32, height: u32) -> Result<Sprite> {
        unsafe {
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: width,
                Height: height,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                Stereo: false.into(),
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
                AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
                Flags: 0,
            };
            let swap = factory.CreateSwapChainForComposition(&gpu.device, &desc, None)?;
            let visual = dcomp.CreateVisual()?;
            visual.SetContent(&swap)?;
            visual.SetBitmapInterpolationMode(DCOMPOSITION_BITMAP_INTERPOLATION_MODE_LINEAR)?;
            visual.SetTransform2(&HIDDEN)?;
            Ok(Sprite { visual, swap, frame: None })
        }
    }

    /// Copies atlas frame `frame` (the `size` box at `at` inside its cell) into the swap chain.
    fn show(&mut self, ctx: &ID3D11DeviceContext, atlas: &ID3D11Texture2D, frame: usize, at: (u32, u32), size: (u32, u32)) -> Result<()> {
        if self.frame == Some(frame) {
            return Ok(());
        }
        let (col, row) = ((frame % critter::COLS) as u32, (frame / critter::COLS) as u32);
        let (x, y) = (col * critter::FRAME_W + at.0, row * critter::FRAME_H + at.1);
        let src = D3D11_BOX { left: x, top: y, front: 0, right: x + size.0, bottom: y + size.1, back: 1 };
        unsafe {
            let back: ID3D11Texture2D = self.swap.GetBuffer(0)?;
            ctx.CopySubresourceRegion(&back, 0, 0, 0, 0, atlas, 0, Some(&src));
            self.swap.Present(0, DXGI_PRESENT(0)).ok()?;
        }
        self.frame = Some(frame);
        Ok(())
    }
}

impl Shake {
    /// Copies `rect` (screen px) from the captured frame into a fresh swap chain.
    fn copy(
        &mut self,
        device: &ID3D11Device,
        factory: &IDXGIFactory2,
        ctx: &ID3D11DeviceContext,
        tex: &ID3D11Texture2D,
        rect: RECT,
        mon: RECT,
    ) -> Result<()> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { tex.GetDesc(&mut desc) };
        let at = RECT {
            left: (rect.left - mon.left).max(0),
            top: (rect.top - mon.top).max(0),
            right: (rect.right - mon.left).min(desc.Width as i32),
            bottom: (rect.bottom - mon.top).min(desc.Height as i32),
        };
        if at.right - at.left < 8 || at.bottom - at.top < 8 {
            self.drop_copy();
            return Ok(());
        }
        let (w, h) = ((at.right - at.left) as u32, (at.bottom - at.top) as u32);
        // Same format as the capture: FP16 frames are scRGB, which composition shows as is.
        let format = desc.Format;
        if format != DXGI_FORMAT_B8G8R8A8_UNORM && format != DXGI_FORMAT_R16G16B16A16_FLOAT {
            self.drop_copy();
            return Ok(());
        }
        unsafe {
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: w,
                Height: h,
                Format: format,
                Stereo: false.into(),
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
                AlphaMode: DXGI_ALPHA_MODE_IGNORE,
                Flags: 0,
            };
            let swap = factory.CreateSwapChainForComposition(device, &desc, None)?;
            if format == DXGI_FORMAT_R16G16B16A16_FLOAT {
                // Captured FP16 frames are linear scRGB; say so, or they show far too dark.
                swap.cast::<IDXGISwapChain3>()?.SetColorSpace1(DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709)?;
            }
            let back: ID3D11Texture2D = swap.GetBuffer(0)?;
            let src = D3D11_BOX { left: at.left as u32, top: at.top as u32, front: 0, right: at.right as u32, bottom: at.bottom as u32, back: 1 };
            ctx.CopySubresourceRegion(&back, 0, 0, 0, 0, tex, 0, Some(&src));
            swap.Present(0, DXGI_PRESENT(0)).ok()?;
            self.visual.SetContent(&swap)?;
            self.swap = Some(swap);
        }
        self.at = at;
        Ok(())
    }

    /// Frees the copy once the scratching stops.
    fn drop_copy(&mut self) {
        self.id = None;
        if self.swap.take().is_some() {
            unsafe {
                let _ = self.visual.SetContent(None::<&windows::core::IUnknown>);
                let _ = self.visual.SetTransform2(&HIDDEN);
            }
        }
    }
}

impl Sprites {
    fn new(gpu: &Gpu, dcomp: &IDCompositionDevice, factory: &IDXGIFactory2) -> Result<Option<Sprites>> {
        let Some(a) = atlas() else { return Ok(None) };
        let atlas = unsafe {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: a.width,
                Height: a.height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_IMMUTABLE,
                BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let init = D3D11_SUBRESOURCE_DATA { pSysMem: a.pixels.as_ptr() as _, SysMemPitch: a.width * 4, SysMemSlicePitch: 0 };
            let mut atlas = None;
            gpu.device.CreateTexture2D(&desc, Some(&init), Some(&mut atlas))?;
            let Some(atlas) = atlas else { return Ok(None) };
            atlas
        };
        let cat = Sprite::new(gpu, dcomp, factory, critter::FRAME_W, critter::FRAME_H)?;
        let fly = || Sprite::new(gpu, dcomp, factory, critter::FLY, critter::FLY);
        let flies = [fly()?, fly()?, fly()?];
        let (cat_box, shake) = unsafe {
            let cat_box = dcomp.CreateVisual()?;
            cat_box.AddVisual(&cat.visual, false, None)?;
            let shake = dcomp.CreateVisual()?;
            shake.SetTransform2(&HIDDEN)?;
            (cat_box, shake)
        };
        let shake = Shake { visual: shake, swap: None, id: None, at: RECT::default() };
        Ok(Some(Sprites {
            device: gpu.device.clone(),
            factory: factory.clone(),
            atlas,
            cat_box,
            cat,
            flies,
            shake,
            scene: Scene::default(),
        }))
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}
