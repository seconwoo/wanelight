//! Screen sampling: DXGI Desktop Duplication per output, reduced on the GPU to a
//! coarse grid of 16x16-pixel cells. For every cell we get the mean and peak
//! brightness (1.0 = SDR white, HDR aware) and how many pixels changed since
//! the previous sample. Only ~100 KB per 4K monitor ever reaches the CPU.

use std::rc::Rc;

use windows::Win32::Devices::Display::*;
use windows::Win32::Foundation::{E_ACCESSDENIED, E_FAIL, ERROR_SUCCESS, HMODULE, LUID, RECT};
use windows::Win32::Graphics::Direct3D::Fxc::*;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;

use windows::core::{Error, Interface, Result, s};

use crate::log;
use crate::util::{fnv1a, from_wide};

/// Cell edge in physical pixels.
pub const CELL: i32 = 16;

const SHADER: &str = r#"
cbuffer Params : register(b0)
{
    uint2 size;
    uint isHdr;
    uint hasPrev;
    float sdrWhite;
    float changeEps;
    float2 pad;
};

Texture2D<float4> Cur : register(t0);
Texture2D<float4> Prev : register(t1);
RWTexture2D<float4> Cells : register(u0);

groupshared float gSum[256];
groupshared float gMax[256];
groupshared float gChg[256];

float SrgbToLinear(float c)
{
    return c <= 0.04045 ? c / 12.92 : pow(abs((c + 0.055) / 1.055), 2.4);
}

[numthreads(16, 16, 1)]
void main(uint3 gid : SV_GroupID, uint3 tid : SV_GroupThreadID, uint gi : SV_GroupIndex)
{
    uint2 p = gid.xy * 16 + tid.xy;
    float b = 0.0;
    float chg = 0.0;
    if (p.x < size.x && p.y < size.y)
    {
        float3 c = Cur.Load(int3(p, 0)).rgb;
        float3 l;
        if (isHdr != 0)
            l = max(c, 0.0) / sdrWhite;
        else
            l = float3(SrgbToLinear(c.r), SrgbToLinear(c.g), SrgbToLinear(c.b));
        b = dot(l, float3(0.2126, 0.7152, 0.0722));
        if (hasPrev != 0)
        {
            float3 d = abs(c - Prev.Load(int3(p, 0)).rgb);
            float eps = (isHdr != 0) ? changeEps * max(1.0, max(c.r, max(c.g, c.b))) : changeEps;
            chg = (max(d.r, max(d.g, d.b)) > eps) ? 1.0 : 0.0;
        }
    }
    gSum[gi] = b;
    gMax[gi] = b;
    gChg[gi] = chg;
    GroupMemoryBarrierWithGroupSync();
    [unroll]
    for (uint s = 128; s > 0; s >>= 1)
    {
        if (gi < s)
        {
            gSum[gi] += gSum[gi + s];
            gMax[gi] = max(gMax[gi], gMax[gi + s]);
            gChg[gi] += gChg[gi + s];
        }
        GroupMemoryBarrierWithGroupSync();
    }
    if (gi == 0)
    {
        uint w = min(16u, size.x - gid.x * 16);
        uint h = min(16u, size.y - gid.y * 16);
        float n = (float)max(1u, w * h);
        Cells[gid.xy] = float4(gSum[0] / n, gMax[0], gChg[0], n);
    }
}
"#;

#[repr(C)]
#[derive(Clone, Copy)]
struct Params {
    size: [u32; 2],
    is_hdr: u32,
    has_prev: u32,
    sdr_white: f32,
    change_eps: f32,
    _pad: [f32; 2],
}

/// Per-cell statistics for one sample.
#[derive(Clone, Copy, Default, Debug)]
pub struct Cell {
    pub mean: f32,
    pub max: f32,
    /// Number of pixels that changed since the previous sample.
    pub changed: f32,
    /// Number of pixels in the cell (smaller at the right/bottom edge).
    pub n: f32,
}

pub enum Sample {
    /// Capture is not possible right now (secure desktop, mode change, ...).
    Unavailable,
    /// Nothing on the output changed since the last sample.
    NoChange,
    /// `exact` is false when there was no previous frame to diff against.
    Frame { cells: Vec<Cell>, exact: bool },
}

/// Grid layout in desktop orientation. `ox`/`oy` (<= 0) shift the grid so it
/// lines up with the physical panel when the output is rotated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GridGeom {
    pub gw: usize,
    pub gh: usize,
    pub ox: i32,
    pub oy: i32,
}

impl GridGeom {
    pub fn new(width: i32, height: i32, rotation: DXGI_MODE_ROTATION) -> Self {
        let swap = rotation == DXGI_MODE_ROTATION_ROTATE90 || rotation == DXGI_MODE_ROTATION_ROTATE270;
        let (wn, hn) = if swap { (height, width) } else { (width, height) };
        let (gwn, ghn) = ((wn + CELL - 1) / CELL, (hn + CELL - 1) / CELL);
        let (gw, gh) = if swap { (ghn, gwn) } else { (gwn, ghn) };
        let flip_x = rotation == DXGI_MODE_ROTATION_ROTATE90 || rotation == DXGI_MODE_ROTATION_ROTATE180;
        let flip_y = rotation == DXGI_MODE_ROTATION_ROTATE180 || rotation == DXGI_MODE_ROTATION_ROTATE270;
        GridGeom {
            gw: gw.max(1) as usize,
            gh: gh.max(1) as usize,
            ox: if flip_x { width - gw * CELL } else { 0 },
            oy: if flip_y { height - gh * CELL } else { 0 },
        }
    }

    pub fn len(&self) -> usize {
        self.gw * self.gh
    }
}

pub struct Gpu {
    pub luid: LUID,
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    cs: Option<ID3D11ComputeShader>,
    cbuf: Option<ID3D11Buffer>,
}

impl Gpu {
    pub fn new(adapter: Option<&IDXGIAdapter1>, shader: Option<&[u8]>) -> Result<Gpu> {
        unsafe {
            let mut device = None;
            let mut context = None;
            let mut level = D3D_FEATURE_LEVEL::default();
            let levels = [
                D3D_FEATURE_LEVEL_11_1,
                D3D_FEATURE_LEVEL_11_0,
                D3D_FEATURE_LEVEL_10_1,
                D3D_FEATURE_LEVEL_10_0,
            ];
            let driver = if adapter.is_some() { D3D_DRIVER_TYPE_UNKNOWN } else { D3D_DRIVER_TYPE_HARDWARE };
            let adapter_base: Option<IDXGIAdapter> = adapter.map(|a| a.cast()).transpose()?;
            D3D11CreateDevice(
                adapter_base.as_ref(),
                driver,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                Some(&mut level),
                Some(&mut context),
            )?;
            let device: ID3D11Device = device.ok_or_else(|| Error::from(E_FAIL))?;
            let context = context.ok_or_else(|| Error::from(E_FAIL))?;
            let luid = match adapter {
                Some(a) => a.GetDesc1()?.AdapterLuid,
                None => LUID::default(),
            };
            let (mut cs, mut cbuf) = (None, None);
            if level.0 >= D3D_FEATURE_LEVEL_11_0.0 {
                if let Some(code) = shader {
                    device.CreateComputeShader(code, None, Some(&mut cs))?;
                    let desc = D3D11_BUFFER_DESC {
                        ByteWidth: std::mem::size_of::<Params>() as u32,
                        Usage: D3D11_USAGE_DEFAULT,
                        BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                        ..Default::default()
                    };
                    device.CreateBuffer(&desc, None, Some(&mut cbuf))?;
                }
            } else {
                log!("capture: adapter feature level {:#x} too low for compute shaders", level.0);
            }
            Ok(Gpu { luid, device, context, cs, cbuf })
        }
    }

    pub fn can_capture(&self) -> bool {
        self.cs.is_some()
    }
}

pub fn compile_shader() -> Result<Vec<u8>> {
    unsafe {
        let mut code: Option<ID3DBlob> = None;
        let mut errors: Option<ID3DBlob> = None;
        let r = D3DCompile(
            SHADER.as_ptr() as _,
            SHADER.len(),
            s!("wanelight_cells.hlsl"),
            None,
            None,
            s!("main"),
            s!("cs_5_0"),
            D3DCOMPILE_OPTIMIZATION_LEVEL3,
            0,
            &mut code,
            Some(&mut errors),
        );
        if let Err(e) = r {
            if let Some(err) = errors {
                let msg = std::slice::from_raw_parts(err.GetBufferPointer() as *const u8, err.GetBufferSize());
                log!("capture: shader compile failed: {}", String::from_utf8_lossy(msg));
            }
            return Err(e);
        }
        let code = code.ok_or_else(|| Error::from(E_FAIL))?;
        Ok(std::slice::from_raw_parts(code.GetBufferPointer() as *const u8, code.GetBufferSize()).to_vec())
    }
}

struct FrameSet {
    w: u32,
    h: u32,
    format: DXGI_FORMAT,
    tex: [ID3D11Texture2D; 2],
    srv: [ID3D11ShaderResourceView; 2],
    /// Index of the most recent frame.
    cur: usize,
    has_prev: bool,
    out: ID3D11Texture2D,
    uav: ID3D11UnorderedAccessView,
    staging: ID3D11Texture2D,
    gwn: u32,
    ghn: u32,
}

impl FrameSet {
    fn new(gpu: &Gpu, w: u32, h: u32, format: DXGI_FORMAT) -> Result<FrameSet> {
        unsafe {
            let dev = &gpu.device;
            let mk_frame = || -> Result<(ID3D11Texture2D, ID3D11ShaderResourceView)> {
                let desc = D3D11_TEXTURE2D_DESC {
                    Width: w,
                    Height: h,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: format,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                    CPUAccessFlags: 0,
                    MiscFlags: 0,
                };
                let mut tex = None;
                dev.CreateTexture2D(&desc, None, Some(&mut tex))?;
                let tex = tex.ok_or_else(|| Error::from(E_FAIL))?;
                let mut srv = None;
                dev.CreateShaderResourceView(&tex, None, Some(&mut srv))?;
                Ok((tex, srv.ok_or_else(|| Error::from(E_FAIL))?))
            };
            let (t0, s0) = mk_frame()?;
            let (t1, s1) = mk_frame()?;
            let (gwn, ghn) = (w.div_ceil(CELL as u32), h.div_ceil(CELL as u32));
            let mut out_desc = D3D11_TEXTURE2D_DESC {
                Width: gwn,
                Height: ghn,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_R32G32B32A32_FLOAT,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_UNORDERED_ACCESS.0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut out = None;
            dev.CreateTexture2D(&out_desc, None, Some(&mut out))?;
            let out = out.ok_or_else(|| Error::from(E_FAIL))?;
            let mut uav = None;
            dev.CreateUnorderedAccessView(&out, None, Some(&mut uav))?;
            out_desc.Usage = D3D11_USAGE_STAGING;
            out_desc.BindFlags = 0;
            out_desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            let mut staging = None;
            dev.CreateTexture2D(&out_desc, None, Some(&mut staging))?;
            Ok(FrameSet {
                w,
                h,
                format,
                tex: [t0, t1],
                srv: [s0, s1],
                cur: 0,
                has_prev: false,
                out,
                uav: uav.ok_or_else(|| Error::from(E_FAIL))?,
                staging: staging.ok_or_else(|| Error::from(E_FAIL))?,
                gwn,
                ghn,
            })
        }
    }
}

pub struct OutputCapture {
    output: IDXGIOutput,
    dupl: Option<IDXGIOutputDuplication>,
    rotation: DXGI_MODE_ROTATION,
    frames: Option<FrameSet>,
    retry_at: f64,
    failures: u32,
    label: String,
}

impl OutputCapture {
    fn new(output: IDXGIOutput, label: String) -> Self {
        Self {
            output,
            dupl: None,
            rotation: DXGI_MODE_ROTATION_IDENTITY,
            frames: None,
            retry_at: 0.0,
            failures: 0,
            label,
        }
    }

    fn duplicate(&self, gpu: &Gpu) -> Result<IDXGIOutputDuplication> {
        unsafe {
            if let Ok(o5) = self.output.cast::<IDXGIOutput5>() {
                let formats = [DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_FORMAT_B8G8R8A8_UNORM];
                match o5.DuplicateOutput1(&gpu.device, 0, &formats) {
                    Ok(d) => return Ok(d),
                    Err(e) if e.code() == E_ACCESSDENIED => return Err(e),
                    Err(_) => {}
                }
            }
            let o1: IDXGIOutput1 = self.output.cast()?;
            o1.DuplicateOutput(&gpu.device)
        }
    }

    /// The last captured frame (8-bit BGRA or FP16 scRGB), if it is in monitor
    /// orientation so it can be copied straight into a sprite. Used by spooky mode.
    pub fn latest_frame(&self) -> Option<&ID3D11Texture2D> {
        let f = self.frames.as_ref().filter(|f| f.has_prev)?;
        (self.rotation == DXGI_MODE_ROTATION_IDENTITY).then(|| &f.tex[f.cur])
    }

    /// Drops the duplication so the next sample starts fresh (e.g. after sleep).
    pub fn reset(&mut self) {
        self.dupl = None;
        if let Some(f) = &mut self.frames {
            f.has_prev = false;
        }
    }

    pub fn sample(&mut self, gpu: &Gpu, sdr_white: f32, now: f64) -> Sample {
        if !gpu.can_capture() {
            return Sample::Unavailable;
        }
        if self.dupl.is_none() {
            if now < self.retry_at {
                return Sample::Unavailable;
            }
            match self.duplicate(gpu) {
                Ok(d) => {
                    self.rotation = unsafe { d.GetDesc() }.Rotation;
                    self.dupl = Some(d);
                    if self.failures > 0 {
                        log!("capture: {} resumed", self.label);
                    }
                    self.failures = 0;
                    if let Some(f) = &mut self.frames {
                        f.has_prev = false;
                    }
                }
                Err(e) => {
                    self.failures += 1;
                    self.retry_at = now + (2.0 * self.failures as f64).min(10.0);
                    if self.failures <= 2 || self.failures.is_multiple_of(60) {
                        log!("capture: cannot duplicate {} ({}): {}", self.label, self.failures, e.message());
                    }
                    return Sample::Unavailable;
                }
            }
        }
        let Some(dupl) = self.dupl.clone() else { return Sample::Unavailable };
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        if let Err(e) = unsafe { dupl.AcquireNextFrame(0, &mut info, &mut resource) } {
            if e.code() == DXGI_ERROR_WAIT_TIMEOUT {
                return Sample::NoChange;
            }
            if e.code() != DXGI_ERROR_ACCESS_LOST {
                log!("capture: AcquireNextFrame on {}: {}", self.label, e.message());
            }
            self.dupl = None;
            self.retry_at = now + 1.0;
            return Sample::Unavailable;
        }
        let copied = if info.LastPresentTime != 0 {
            resource
                .and_then(|r| r.cast::<ID3D11Texture2D>().ok())
                .map(|tex| self.copy_frame(gpu, &tex))
        } else {
            None
        };
        unsafe {
            let _ = dupl.ReleaseFrame();
        }
        match copied {
            None => Sample::NoChange,
            Some(Err(e)) => {
                log!("capture: copy failed on {}: {}", self.label, e.message());
                self.frames = None;
                Sample::Unavailable
            }
            Some(Ok(())) => match self.reduce(gpu, sdr_white) {
                Ok(s) => s,
                Err(e) => {
                    log!("capture: reduce failed on {}: {}", self.label, e.message());
                    self.frames = None;
                    Sample::Unavailable
                }
            },
        }
    }

    /// Copies the acquired frame into our own texture (slot `1 - cur`).
    fn copy_frame(&mut self, gpu: &Gpu, src: &ID3D11Texture2D) -> Result<()> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { src.GetDesc(&mut desc) };
        let fits = self
            .frames
            .as_ref()
            .map(|f| f.w == desc.Width && f.h == desc.Height && f.format == desc.Format)
            .unwrap_or(false);
        if !fits {
            self.frames = Some(FrameSet::new(gpu, desc.Width, desc.Height, desc.Format)?);
        }
        let Some(f) = self.frames.as_mut() else { return Err(Error::from(E_FAIL)) };
        let next = 1 - f.cur;
        unsafe { gpu.context.CopyResource(&f.tex[next], src) };
        Ok(())
    }

    fn reduce(&mut self, gpu: &Gpu, sdr_white: f32) -> Result<Sample> {
        let (Some(cs), Some(cbuf)) = (gpu.cs.as_ref(), gpu.cbuf.as_ref()) else {
            return Ok(Sample::Unavailable);
        };
        let Some(f) = self.frames.as_mut() else { return Ok(Sample::Unavailable) };
        let next = 1 - f.cur;
        let is_hdr = f.format == DXGI_FORMAT_R16G16B16A16_FLOAT;
        let params = Params {
            size: [f.w, f.h],
            is_hdr: is_hdr as u32,
            has_prev: f.has_prev as u32,
            sdr_white: if is_hdr { sdr_white.max(0.5) } else { 1.0 },
            change_eps: if is_hdr { 0.004 } else { 1.5 / 255.0 },
            _pad: [0.0; 2],
        };
        let ctx = &gpu.context;
        let mut native = vec![Cell::default(); (f.gwn * f.ghn) as usize];
        unsafe {
            ctx.UpdateSubresource(cbuf, 0, None, &params as *const Params as _, 0, 0);
            ctx.CSSetShader(cs, None);
            ctx.CSSetConstantBuffers(0, Some(&[Some(cbuf.clone())]));
            ctx.CSSetShaderResources(0, Some(&[Some(f.srv[next].clone()), Some(f.srv[f.cur].clone())]));
            let uav = Some(f.uav.clone());
            ctx.CSSetUnorderedAccessViews(0, 1, Some(&uav as *const _), None);
            ctx.Dispatch(f.gwn, f.ghn, 1);
            ctx.CSSetShaderResources(0, Some(&[None, None]));
            let none: Option<ID3D11UnorderedAccessView> = None;
            ctx.CSSetUnorderedAccessViews(0, 1, Some(&none as *const _), None);
            ctx.CopyResource(&f.staging, &f.out);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            ctx.Map(&f.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            for y in 0..f.ghn as usize {
                let row = (mapped.pData as *const u8).add(y * mapped.RowPitch as usize) as *const [f32; 4];
                for x in 0..f.gwn as usize {
                    let v = *row.add(x);
                    native[y * f.gwn as usize + x] = Cell { mean: v[0], max: v[1], changed: v[2], n: v[3] };
                }
            }
            ctx.Unmap(&f.staging, 0);
        }
        let exact = f.has_prev;
        f.cur = next;
        f.has_prev = true;
        let cells = rotate_to_desktop(&native, f.gwn as usize, f.ghn as usize, self.rotation);
        Ok(Sample::Frame { cells, exact })
    }
}

/// Maps a grid in the panel's native orientation to desktop orientation.
fn rotate_to_desktop(native: &[Cell], gwn: usize, ghn: usize, rot: DXGI_MODE_ROTATION) -> Vec<Cell> {
    let mut out = vec![Cell::default(); native.len()];
    for yn in 0..ghn {
        for xn in 0..gwn {
            let c = native[yn * gwn + xn];
            let (xd, yd, gw) = match rot {
                DXGI_MODE_ROTATION_ROTATE90 => (ghn - 1 - yn, xn, ghn),
                DXGI_MODE_ROTATION_ROTATE180 => (gwn - 1 - xn, ghn - 1 - yn, gwn),
                DXGI_MODE_ROTATION_ROTATE270 => (yn, gwn - 1 - xn, ghn),
                _ => (xn, yn, gwn),
            };
            out[yd * gw + xd] = c;
        }
    }
    out
}

/// A monitor as seen by the capture engine.
pub struct Display {
    pub id: String,
    pub name: String,
    pub gdi_name: String,
    pub rect: RECT,
    pub hdr: bool,
    /// scRGB value of SDR white (1.0 = 80 nits). Used to normalise HDR frames.
    pub sdr_white: f32,
    pub geom: GridGeom,
    pub gpu: Rc<Gpu>,
    pub capture: OutputCapture,
}

impl Display {
    pub fn width(&self) -> i32 {
        self.rect.right - self.rect.left
    }
    pub fn height(&self) -> i32 {
        self.rect.bottom - self.rect.top
    }
    pub fn sample(&mut self, now: f64) -> Sample {
        let gpu = self.gpu.clone();
        let s = self.capture.sample(&gpu, self.sdr_white, now);
        if let Sample::Frame { cells, .. } = &s
            && cells.len() != self.geom.len() {
                // Output mode changed under us; a WM_DISPLAYCHANGE rebuild will follow.
                return Sample::Unavailable;
            }
        s
    }
}

struct DisplayNames {
    gdi: String,
    friendly: String,
    device_path: String,
    sdr_white: f32,
}

fn query_display_names() -> Vec<DisplayNames> {
    let mut out = Vec::new();
    unsafe {
        let (mut np, mut nm) = (0u32, 0u32);
        if GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm) != ERROR_SUCCESS {
            return out;
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
        if QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS, &mut np, paths.as_mut_ptr(), &mut nm, modes.as_mut_ptr(), None)
            != ERROR_SUCCESS
        {
            return out;
        }
        paths.truncate(np as usize);
        for p in &paths {
            let mut src = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
            src.header = DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                size: std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                adapterId: p.sourceInfo.adapterId,
                id: p.sourceInfo.id,
            };
            if DisplayConfigGetDeviceInfo(&mut src.header) != 0 {
                continue;
            }
            let mut tgt = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
            tgt.header = DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
                size: std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32,
                adapterId: p.targetInfo.adapterId,
                id: p.targetInfo.id,
            };
            let (friendly, device_path) = if DisplayConfigGetDeviceInfo(&mut tgt.header) == 0 {
                (from_wide(&tgt.monitorFriendlyDeviceName), from_wide(&tgt.monitorDevicePath))
            } else {
                (String::new(), String::new())
            };
            let mut white = DISPLAYCONFIG_SDR_WHITE_LEVEL::default();
            white.header = DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL,
                size: std::mem::size_of::<DISPLAYCONFIG_SDR_WHITE_LEVEL>() as u32,
                adapterId: p.targetInfo.adapterId,
                id: p.targetInfo.id,
            };
            let sdr_white = if DisplayConfigGetDeviceInfo(&mut white.header) == 0 && white.SDRWhiteLevel > 0 {
                white.SDRWhiteLevel as f32 / 1000.0
            } else {
                1.0
            };
            out.push(DisplayNames { gdi: from_wide(&src.viewGdiDeviceName), friendly, device_path, sdr_white });
        }
    }
    out
}

/// Stable id from the monitor's device path: "<EDID product>-<hash>".
fn monitor_id(device_path: &str, gdi: &str) -> String {
    if device_path.is_empty() {
        return format!("gdi-{:08x}", fnv1a(gdi.as_bytes()) as u32);
    }
    let product = device_path
        .split('#')
        .nth(1)
        .filter(|s| !s.is_empty() && s.len() <= 16)
        .unwrap_or("MON");
    let clean: String = product.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    format!("{}-{:08x}", clean, fnv1a(device_path.to_ascii_lowercase().as_bytes()) as u32)
}

/// True for an id made from the GDI name because the device path was missing.
pub fn is_fallback_id(id: &str) -> bool {
    id.starts_with("gdi-")
}

pub struct Enumeration {
    pub displays: Vec<Display>,
    pub gpus: Vec<Rc<Gpu>>,
}

pub fn enumerate() -> Result<Enumeration> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
    let names = query_display_names();
    let shader = match compile_shader() {
        Ok(s) => Some(s),
        Err(e) => {
            log!("capture: shader unavailable ({}); static detection disabled", e.message());
            None
        }
    };
    let mut displays = Vec::new();
    let mut gpus = Vec::new();
    let mut ai = 0;
    while let Ok(adapter) = unsafe { factory.EnumAdapters1(ai) } {
        ai += 1;
        let mut outputs = Vec::new();
        while let Ok(o) = unsafe { adapter.EnumOutputs(outputs.len() as u32) } {
            outputs.push(o);
        }
        if outputs.is_empty() {
            continue;
        }
        let gpu = match Gpu::new(Some(&adapter), shader.as_deref()) {
            Ok(g) => Rc::new(g),
            Err(e) => {
                log!("capture: cannot create device on adapter {}: {}", ai - 1, e.message());
                continue;
            }
        };
        for output in outputs {
            let Ok(desc) = (unsafe { output.GetDesc() }) else { continue };
            if !desc.AttachedToDesktop.as_bool() {
                continue;
            }
            let gdi = from_wide(&desc.DeviceName);
            let hdr = output
                .cast::<IDXGIOutput6>()
                .ok()
                .and_then(|o6| unsafe { o6.GetDesc1() }.ok())
                .map(|d| d.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020)
                .unwrap_or(false);
            let info = names.iter().find(|n| n.gdi.eq_ignore_ascii_case(&gdi));
            let id = monitor_id(info.map(|n| n.device_path.as_str()).unwrap_or(""), &gdi);
            let name = info
                .map(|n| n.friendly.clone())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| gdi.trim_start_matches("\\\\.\\").to_string());
            let sdr_white = if hdr { info.map(|n| n.sdr_white).unwrap_or(2.5) } else { 1.0 };
            let rect = desc.DesktopCoordinates;
            let geom = GridGeom::new(rect.right - rect.left, rect.bottom - rect.top, desc.Rotation);
            displays.push(Display {
                id,
                name: name.clone(),
                gdi_name: gdi,
                rect,
                hdr,
                sdr_white,
                geom,
                gpu: gpu.clone(),
                capture: OutputCapture::new(output, name),
            });
        }
        gpus.push(gpu);
    }
    Ok(Enumeration { displays, gpus })
}
