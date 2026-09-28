//! Click-through dimming overlay for one monitor.
//!
//! The mask is a tiny texture (one texel per 16x16 cell) that DirectComposition
//! scales up with bilinear filtering, so an update uploads ~100 KB instead of a
//! full-resolution bitmap. The window only covers the bounding box of the
//! dimmed cells and is hidden entirely when nothing is dimmed, so fullscreen
//! apps elsewhere keep direct scan-out. It is excluded from screen capture, so
//! our own sampling (and the user's screenshots) never see it.

use std::sync::Once;

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Direct3D11::{ID3D11DeviceContext, ID3D11Texture2D};
use windows::Win32::Graphics::DirectComposition::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{Interface, Result, w};
use windows_numerics::Matrix3x2;

use super::capture::{CELL, GridGeom, Gpu};
use crate::log;

const CLASS: windows::core::PCWSTR = w!("WanelightOverlay");

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

pub struct Overlay {
    hwnd: HWND,
    dcomp: IDCompositionDevice,
    _target: IDCompositionTarget,
    visual: IDCompositionVisual,
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
                mon.left,
                mon.top,
                mon.right - mon.left,
                mon.bottom - mon.top,
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
            target.SetRoot(&visual)?;
            dcomp.Commit()?;
            Ok(Overlay {
                hwnd,
                dcomp,
                _target: target,
                visual,
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
                bottom: cell_y(y1 as isize + 2).min(self.mon.bottom),
            };
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

impl Drop for Overlay {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}
