//! UI Automation helpers: the container chain under a point, or around the
//! keyboard focus, as rectangles plus control type and landmark role. Used to
//! find the app panel (sidebar, main area, side pane) or text box to light.
//!
//! Only geometry and roles are read, never names or text.

use windows::Win32::Foundation::{POINT, RECT};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::System::Variant::VT_I4;
use windows::Win32::UI::Accessibility::*;
use windows::core::{Interface, Result};

#[derive(Clone, Debug)]
pub struct Node {
    pub rect: RECT,
    pub control_type: i32,
    pub landmark: i32,
    pub aria_role: String,
}

pub struct Uia {
    uia: IUIAutomation,
    walker: IUIAutomationTreeWalker,
    cache: IUIAutomationCacheRequest,
}

pub fn area(r: &RECT) -> f64 {
    (r.right - r.left).max(0) as f64 * (r.bottom - r.top).max(0) as f64
}

pub fn control_type_name(id: i32) -> &'static str {
    match id {
        50000 => "Button",
        50004 => "Edit",
        50007 => "ListItem",
        50008 => "List",
        50018 => "Tab",
        50019 => "TabItem",
        50020 => "Text",
        50023 => "Tree",
        50025 => "Custom",
        50026 => "Group",
        50030 => "Document",
        50032 => "Window",
        50033 => "Pane",
        50037 => "TitleBar",
        _ => "Other",
    }
}

pub fn landmark_name(id: i32) -> &'static str {
    match id {
        80000 => "custom",
        80001 => "form",
        80002 => "main",
        80003 => "navigation",
        80004 => "search",
        _ => "",
    }
}

impl Uia {
    /// Must be called on a thread with COM initialised.
    pub fn new() -> Result<Uia> {
        unsafe {
            let uia: IUIAutomation = CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)?;
            // A hung app must not stall us for long.
            if let Ok(u2) = uia.cast::<IUIAutomation2>() {
                let _ = u2.SetConnectionTimeout(800);
                let _ = u2.SetTransactionTimeout(800);
            }
            let walker = uia.RawViewWalker()?;
            let cache = uia.CreateCacheRequest()?;
            for p in [
                UIA_BoundingRectanglePropertyId,
                UIA_ControlTypePropertyId,
                UIA_LandmarkTypePropertyId,
                UIA_AriaRolePropertyId,
            ] {
                cache.AddProperty(p)?;
            }
            Ok(Uia { uia, walker, cache })
        }
    }

    fn node(&self, el: &IUIAutomationElement) -> Node {
        unsafe {
            let landmark = el
                .GetCachedPropertyValue(UIA_LandmarkTypePropertyId)
                .ok()
                .and_then(|v| (v.Anonymous.Anonymous.vt == VT_I4).then(|| v.Anonymous.Anonymous.Anonymous.lVal))
                .unwrap_or(0);
            Node {
                rect: el.CachedBoundingRectangle().unwrap_or_default(),
                control_type: el.CachedControlType().map(|c| c.0).unwrap_or(0),
                landmark,
                aria_role: el.CachedAriaRole().map(|b| b.to_string()).unwrap_or_default(),
            }
        }
    }

    /// The element and its ancestors, innermost first, up to (not including) the desktop.
    fn chain_from(&self, el: IUIAutomationElement, max_depth: usize) -> Vec<Node> {
        let mut out = Vec::new();
        let mut cur = Some(el);
        while let Some(el) = cur {
            if out.len() >= max_depth {
                break;
            }
            out.push(self.node(&el));
            cur = unsafe { self.walker.GetParentElementBuildCache(&el, &self.cache) }.ok();
        }
        // The last element reached is the desktop root; drop it.
        if out.len() > 1 {
            out.pop();
        }
        out
    }

    pub fn chain_at(&self, pt: POINT) -> Result<Vec<Node>> {
        let el = unsafe { self.uia.ElementFromPointBuildCache(pt, &self.cache)? };
        Ok(self.chain_from(el, 80))
    }

    pub fn focused_chain(&self) -> Result<Vec<Node>> {
        let el = unsafe { self.uia.GetFocusedElementBuildCache(&self.cache)? };
        Ok(self.chain_from(el, 80))
    }
}

fn intersect(a: &RECT, b: &RECT) -> RECT {
    RECT { left: a.left.max(b.left), top: a.top.max(b.top), right: a.right.min(b.right), bottom: a.bottom.min(b.bottom) }
}

/// Visible part of each chain element: clipped to all of its ancestors and the window.
/// The result is nested, so areas never shrink going outward.
fn clip_chain(chain: &[Node], window: &RECT) -> Vec<RECT> {
    let mut out = vec![RECT::default(); chain.len()];
    let mut outer = *window;
    for (i, n) in chain.iter().enumerate().rev() {
        outer = intersect(&n.rect, &outer);
        out[i] = outer;
    }
    out
}

/// The panel to light for a pointer position: the largest container under the
/// point that is still clearly smaller than its window (sidebar, main area,
/// side pane).
pub fn choose_panel(chain: &[Node], window: &RECT) -> Option<RECT> {
    let wa = area(window).max(1.0);
    let mut best = None;
    for r in clip_chain(chain, window) {
        if area(&r) > 0.75 * wa {
            break;
        }
        if r.right - r.left >= 160 && r.bottom - r.top >= 120 {
            best = Some(r);
        }
    }
    best
}

/// The input area to light while typing: the largest container around the
/// keyboard focus that is still small (the text box with its buttons). Falls
/// back to the surrounding panel when the focus is tiny (e.g. an editor's
/// hidden text field).
pub fn choose_input(chain: &[Node], window: &RECT) -> Option<RECT> {
    let wa = area(window).max(1.0);
    let wh = (window.bottom - window.top).max(1) as f64;
    let mut best: Option<RECT> = None;
    for r in clip_chain(chain, window) {
        if area(&r) > 0.08 * wa || (r.bottom - r.top) as f64 > 0.35 * wh {
            break;
        }
        best = Some(r);
    }
    match best {
        Some(r) if r.right - r.left >= 120 && r.bottom - r.top >= 16 => Some(r),
        _ => choose_panel(chain, window),
    }
}

/// True when only the app's outer shell is exposed (e.g. Chromium before its
/// accessibility tree is built): worth asking again shortly.
pub fn shell_only(chain: &[Node], window: &RECT) -> bool {
    let wa = area(window).max(1.0);
    clip_chain(chain, window).first().is_some_and(|r| area(r) > 0.9 * wa)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Query {
    /// Panel under a screen point.
    Point(i32, i32),
    /// Input area around the keyboard focus.
    Focus,
}

#[derive(Clone, Copy, Debug)]
pub struct Answer {
    pub query: Query,
    pub rect: Option<RECT>,
    pub retry: bool,
}

/// Runs UI Automation lookups on a background thread (they are cross-process
/// calls and a busy app can be slow to answer). Newer queries replace older
/// ones; answers are collected by the agent after it is notified.
pub struct PanelWorker {
    tx: std::sync::mpsc::Sender<Query>,
    answers: std::sync::Arc<std::sync::Mutex<Vec<Answer>>>,
}

fn window_rect(hwnd: windows::Win32::Foundation::HWND) -> RECT {
    use windows::Win32::Graphics::Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
    let mut r = RECT::default();
    unsafe {
        if DwmGetWindowAttribute(hwnd, DWMWA_EXTENDED_FRAME_BOUNDS, &mut r as *mut RECT as _, std::mem::size_of::<RECT>() as u32)
            .is_err()
        {
            let _ = windows::Win32::UI::WindowsAndMessaging::GetWindowRect(hwnd, &mut r);
        }
    }
    r
}

impl PanelWorker {
    /// `notify` receives a posted message (`msg`) whenever answers are ready.
    pub fn start(notify: isize, msg: u32) -> Self {
        use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
        use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
        use windows::Win32::UI::WindowsAndMessaging::{GA_ROOT, GetAncestor, GetForegroundWindow, PostMessageW, WindowFromPoint};

        let (tx, rx) = std::sync::mpsc::channel::<Query>();
        let answers: std::sync::Arc<std::sync::Mutex<Vec<Answer>>> = Default::default();
        let out = answers.clone();
        let _ = std::thread::Builder::new().name("panels".into()).spawn(move || {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            }
            let uia = match Uia::new() {
                Ok(u) => u,
                Err(e) => {
                    crate::log!("panels: UI Automation unavailable: {}", e.message());
                    return;
                }
            };
            while let Ok(first) = rx.recv() {
                // Coalesce: only the newest point and one focus lookup matter.
                let (mut point, mut focus) = (None, false);
                for q in std::iter::once(first).chain(rx.try_iter()) {
                    match q {
                        Query::Point(..) => point = Some(q),
                        Query::Focus => focus = true,
                    }
                }
                let mut batch = Vec::new();
                if let Some(q @ Query::Point(x, y)) = point {
                    let pt = POINT { x, y };
                    let root = unsafe { GetAncestor(WindowFromPoint(pt), GA_ROOT) };
                    let window = window_rect(root);
                    let (rect, retry) = match uia.chain_at(pt) {
                        Ok(c) => (choose_panel(&c, &window), shell_only(&c, &window)),
                        Err(_) => (None, false),
                    };
                    batch.push(Answer { query: q, rect, retry });
                }
                if focus {
                    let window = window_rect(unsafe { GetForegroundWindow() });
                    let (rect, retry) = match uia.focused_chain() {
                        Ok(c) => (choose_input(&c, &window), shell_only(&c, &window)),
                        Err(_) => (None, false),
                    };
                    batch.push(Answer { query: Query::Focus, rect, retry });
                }
                out.lock().unwrap_or_else(|e| e.into_inner()).extend(batch);
                unsafe {
                    let _ = PostMessageW(Some(HWND(notify as _)), msg, WPARAM(0), LPARAM(0));
                }
            }
        });
        PanelWorker { tx, answers }
    }

    pub fn ask(&self, q: Query) {
        let _ = self.tx.send(q);
    }

    pub fn take_answers(&self) -> Vec<Answer> {
        std::mem::take(&mut *self.answers.lock().unwrap_or_else(|e| e.into_inner()))
    }
}
