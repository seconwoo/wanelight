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

/// A container chain: nodes innermost first, with the UIA elements behind them.
pub struct Chain {
    pub nodes: Vec<Node>,
    elements: Vec<IUIAutomationElement>,
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
    fn chain_from(&self, el: IUIAutomationElement, max_depth: usize) -> Chain {
        let (mut nodes, mut elements) = (Vec::new(), Vec::new());
        let mut cur = Some(el);
        while let Some(el) = cur {
            if nodes.len() >= max_depth {
                break;
            }
            nodes.push(self.node(&el));
            cur = unsafe { self.walker.GetParentElementBuildCache(&el, &self.cache) }.ok();
            elements.push(el);
        }
        // The last element reached is the desktop root; drop it.
        if nodes.len() > 1 {
            nodes.pop();
            elements.pop();
        }
        Chain { nodes, elements }
    }

    pub fn chain_at(&self, pt: POINT) -> Result<Chain> {
        let el = unsafe { self.uia.ElementFromPointBuildCache(pt, &self.cache)? };
        Ok(self.chain_from(el, 80))
    }

    pub fn focused_chain(&self) -> Result<Chain> {
        let el = unsafe { self.uia.GetFocusedElementBuildCache(&self.cache)? };
        Ok(self.chain_from(el, 80))
    }

    /// Bounding rects of an element's direct children (at most 64).
    fn children_rects(&self, el: &IUIAutomationElement) -> Vec<RECT> {
        unsafe {
            let Ok(cond) = self.uia.CreateTrueCondition() else { return Vec::new() };
            let Ok(arr) = el.FindAllBuildCache(TreeScope_Children, &cond, &self.cache) else { return Vec::new() };
            let n = arr.Length().unwrap_or(0).clamp(0, 64);
            (0..n).filter_map(|i| arr.GetElement(i).ok()?.CachedBoundingRectangle().ok()).collect()
        }
    }
}

fn intersect(a: &RECT, b: &RECT) -> RECT {
    RECT { left: a.left.max(b.left), top: a.top.max(b.top), right: a.right.min(b.right), bottom: a.bottom.min(b.bottom) }
}

fn contains(r: &RECT, pt: POINT) -> bool {
    pt.x >= r.left && pt.x < r.right && pt.y >= r.top && pt.y < r.bottom
}

fn center(r: &RECT) -> POINT {
    POINT { x: (r.left + r.right) / 2, y: (r.top + r.bottom) / 2 }
}

fn width(r: &RECT) -> f64 {
    (r.right - r.left).max(0) as f64
}

fn height(r: &RECT) -> f64 {
    (r.bottom - r.top).max(0) as f64
}

/// One level of the container chain: its visible rect (clipped to the window)
/// and the raw chain indices of its innermost and outermost element.
#[derive(Clone, Copy)]
struct Level {
    rect: RECT,
    outer: usize,
}

/// Chain elements that really contain `pt`, innermost first, never shrinking
/// going outward, with same-size wrappers merged into one level. Ancestors
/// are not trusted to enclose their children: web layouts often report stale
/// or narrower bounds for wrappers, so those are skipped rather than used.
fn levels(chain: &Chain, window: &RECT, pt: POINT) -> Vec<Level> {
    let mut out: Vec<Level> = Vec::new();
    for (i, n) in chain.nodes.iter().enumerate() {
        let r = intersect(&n.rect, window);
        if !contains(&r, pt) {
            continue;
        }
        match out.last_mut() {
            Some(last) if area(&r) < area(&last.rect) => {}
            Some(last) if area(&r) <= area(&last.rect) * 1.05 => {
                last.rect = r;
                last.outer = i;
            }
            _ => out.push(Level { rect: r, outer: i }),
        }
    }
    out
}

struct Columns<'a> {
    uia: &'a Uia,
    chain: &'a Chain,
    window: RECT,
    levels: Vec<Level>,
    children: std::collections::HashMap<usize, Vec<RECT>>,
}

impl Columns<'_> {
    /// Is level `k` one column of a side-by-side layout? Its parent must be
    /// nearly the same height and leave room beside it, and the parent must
    /// really have another child next to it (this rejects stale bounds).
    fn is_column(&mut self, k: usize) -> bool {
        let Some(outer) = self.levels.get(k + 1).map(|l| l.rect) else { return false };
        let inner = self.levels[k].rect;
        if height(&inner) < 0.8 * height(&outer) || width(&inner) > width(&outer) - 100.0 {
            return false;
        }
        // The sibling may hang off any of the same-size wrappers that make up
        // the parent level (e.g. a sidebar outside a full-width `main`).
        let (uia, window) = (self.uia, self.window);
        for raw in self.levels[k].outer + 1..=self.levels[k + 1].outer {
            let Some(el) = self.chain.elements.get(raw) else { break };
            let kids = self
                .children
                .entry(raw)
                .or_insert_with(|| uia.children_rects(el).iter().map(|r| intersect(r, &window)).collect());
            let found = kids.iter().any(|s| {
                let beside = s.right <= inner.left + 8 || s.left >= inner.right - 8;
                let overlap = (s.bottom.min(inner.bottom) - s.top.max(inner.top)).max(0) as f64;
                beside && width(s) >= 60.0 && overlap >= 0.5 * height(s).min(height(&inner))
            });
            if found {
                return true;
            }
        }
        false
    }
}

/// The panel to light for a pointer position: the outermost column of a
/// side-by-side layout under the point (sidebar, main area, side pane) that is
/// not itself split into columns. So 3 columns closing to 2 simply widens the
/// panel. Without a column layout, the largest container under 75 % of the
/// window is used.
pub fn choose_panel(uia: &Uia, chain: &Chain, window: &RECT, pt: POINT) -> Option<RECT> {
    let wa = area(window).max(1.0);
    let mut c = Columns { uia, chain, window: *window, levels: levels(chain, window, pt), children: Default::default() };
    let mut best = None;
    for k in 0..c.levels.len() {
        let r = c.levels[k].rect;
        if width(&r) < 160.0 || height(&r) < 120.0 || area(&r) > 0.95 * wa {
            continue;
        }
        // Check the cheap geometric condition first; siblings only when needed.
        if c.is_column(k) && !(k > 0 && c.is_column(k - 1)) {
            best = Some(r);
        }
    }
    best.or_else(|| {
        c.levels
            .iter()
            .map(|l| l.rect)
            .take_while(|r| area(r) <= 0.75 * wa)
            .filter(|r| width(r) >= 160.0 && height(r) >= 120.0)
            .last()
    })
}

/// The input area to light while typing: the largest container around the
/// keyboard focus that is still small (the text box with its buttons). Falls
/// back to the surrounding panel when the focus is tiny (e.g. an editor's
/// hidden text field).
pub fn choose_input(uia: &Uia, chain: &Chain, window: &RECT) -> Option<RECT> {
    let pt = center(&chain.nodes.first()?.rect);
    let wa = area(window).max(1.0);
    let wh = height(window).max(1.0);
    let mut best: Option<RECT> = None;
    for l in levels(chain, window, pt) {
        if area(&l.rect) > 0.08 * wa || height(&l.rect) > 0.35 * wh {
            break;
        }
        best = Some(l.rect);
    }
    match best {
        Some(r) if width(&r) >= 120.0 && height(&r) >= 16.0 => Some(r),
        _ => choose_panel(uia, chain, window, pt),
    }
}

/// True when only the app's outer shell is exposed (e.g. Chromium before its
/// accessibility tree is built): worth asking again shortly.
pub fn shell_only(chain: &Chain, window: &RECT) -> bool {
    let wa = area(window).max(1.0);
    chain.nodes.first().is_some_and(|n| area(&intersect(&n.rect, window)) > 0.9 * wa)
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
                        Ok(c) => {
                            if crate::util::debug_enabled() {
                                let sizes: Vec<String> =
                                    c.nodes.iter().take(8).map(|n| format!("{}x{}", n.rect.right - n.rect.left, n.rect.bottom - n.rect.top)).collect();
                                crate::log!(
                                    "panels: at ({x},{y}) window {:?} chain {}",
                                    (window.left, window.top, window.right, window.bottom),
                                    sizes.join(" < ")
                                );
                            }
                            (choose_panel(&uia, &c, &window, pt), shell_only(&c, &window))
                        }
                        Err(e) => {
                            crate::log!("panels: lookup at ({x},{y}) failed: {}", e.message());
                            (None, false)
                        }
                    };
                    batch.push(Answer { query: q, rect, retry });
                }
                if focus {
                    let window = window_rect(unsafe { GetForegroundWindow() });
                    let (rect, retry) = match uia.focused_chain() {
                        Ok(c) => (choose_input(&uia, &c, &window), shell_only(&c, &window)),
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
