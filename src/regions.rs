//! 自由形状点透选区（「点透洞」）+ 真·透明洞覆盖层。
//!
//! 语义：窗口整体仍然可操作；每个多边形选区可以单独设定
//!   - 正常（洞）：圈内点击穿透、圈内视觉透出下层；
//!   - 反向（保留区）：圈内保留可点，圈外整片成为洞；
//! 每个选区还有 0–100% 的视觉透明度。
//!
//! 输入穿透：不裁剪窗口，低级鼠标钩子按坐标临时给目标加 WS_EX_TRANSPARENT，
//! 洞内点击自然落到下层，出洞恢复。
//! 视觉透明：一个置顶、点击穿透的分层覆盖层，在洞区域贴上「下层画面快照」，
//! 按选区透明度混合；快照在应用/刷新时截取，下层变了需要刷新。

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use std::sync::{Mutex, Once};
use std::thread;

use windows::core::w;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

const WM_EDITOR_OPEN: u32 = WM_APP + 0x51;
const WM_VISUAL_REFRESH: u32 = WM_APP + 0x52;
const WM_VISUAL_HIDE: u32 = WM_APP + 0x53;
const WM_VISUAL_MASK: u32 = WM_APP + 0x54;

#[derive(Clone, Debug)]
pub struct Region {
    /// 归一化客户区坐标
    pub pts: Vec<(f32, f32)>,
    /// false = 圈内是洞；true = 圈内保留、圈外是洞（反向选择）
    pub invert: bool,
    /// 视觉洞的透明度 0..=100（不影响点击穿透）
    pub opacity: u8,
    /// 图层"眼睛"：停用后这个圈既不点透也不视觉透
    pub enabled: bool,
}

static REGIONS: Mutex<Vec<(isize, Vec<Region>)>> = Mutex::new(Vec::new());
static HOLE_STYLES: Mutex<Vec<isize>> = Mutex::new(Vec::new());
/// 这些窗口的 LAYERED 是我们在洞内临时补的，离开洞时要撤掉。
static HOLE_LAYERED: Mutex<Vec<isize>> = Mutex::new(Vec::new());

static INIT: Once = Once::new();
static EDITOR_HWND: AtomicIsize = AtomicIsize::new(0);
static PENDING: Mutex<Option<isize>> = Mutex::new(None);

static VISUAL_HWND: AtomicIsize = AtomicIsize::new(0);
static VISUAL_REQ: Mutex<Option<isize>> = Mutex::new(None);
static VISUAL_HIDE_REQ: AtomicBool = AtomicBool::new(false);

pub fn init() {
    INIT.call_once(|| {
        let _ = thread::Builder::new()
            .name("tg-regions".into())
            .spawn(|| unsafe { editor_thread() });
        let _ = thread::Builder::new()
            .name("tg-holes".into())
            .spawn(|| unsafe { visual_thread() });
    });
}

/// 全屏覆盖层编辑器（2.19.0 改回「窗口上直接画」：矩形/椭圆/自由 + 确定）。
pub fn open_editor(hwnd: isize) {
    open_editor_at(hwnd, None);
}

pub fn open_editor_at(hwnd: isize, _selected: Option<usize>) {
    if hwnd == 0 {
        return;
    }
    open_editor_post(hwnd);
}

fn open_editor_post(hwnd: isize) {
    if let Ok(mut p) = PENDING.lock() {
        *p = Some(hwnd);
    }
    let h = EDITOR_HWND.load(Ordering::Relaxed);
    if h != 0 {
        unsafe {
            let _ = PostMessageW(HWND(h as *mut _), WM_EDITOR_OPEN, WPARAM(0), LPARAM(0));
        }
    }
}

pub fn refresh_visual(hwnd: isize) {
    if hwnd == 0 {
        return;
    }
    if let Ok(mut p) = VISUAL_REQ.lock() {
        *p = Some(hwnd);
    }
    let h = VISUAL_HWND.load(Ordering::Relaxed);
    if h != 0 {
        unsafe {
            let _ = PostMessageW(HWND(h as *mut _), WM_VISUAL_REFRESH, WPARAM(0), LPARAM(0));
        }
    }
}

pub fn hide_visual() {
    crate::gpu_hole::hide();
    VISUAL_HIDE_REQ.store(true, Ordering::Relaxed);
    let h = VISUAL_HWND.load(Ordering::Relaxed);
    if h != 0 {
        unsafe {
            let _ = PostMessageW(HWND(h as *mut _), WM_VISUAL_HIDE, WPARAM(0), LPARAM(0));
        }
    }
}

static VISUAL_SEQ: AtomicU32 = AtomicU32::new(0);

/// 同步版刷新：等视觉线程完成一次抓取再返回（最多等 2 秒）。
/// 编辑结束用它，保证「藏主界面 → 抓下层 → 恢复主界面」的顺序，洞里的文字才不会和窗口本身错位。
pub fn refresh_visual_wait(hwnd: isize) {
    if hwnd == 0 {
        return;
    }
    let seq = VISUAL_SEQ.load(Ordering::Relaxed);
    refresh_visual(hwnd);
    if VISUAL_HWND.load(Ordering::Relaxed) == 0 {
        return;
    }
    for _ in 0..400 {
        if VISUAL_SEQ.load(Ordering::Relaxed) != seq {
            return;
        }
        thread::sleep(std::time::Duration::from_millis(5));
    }
}

pub fn has_any() -> bool {
    REGIONS.lock().map(|v| !v.is_empty()).unwrap_or(false)
}

pub fn count(hwnd: isize) -> usize {
    REGIONS
        .lock()
        .ok()
        .and_then(|v| {
            v.iter()
                .find(|(h, _)| *h == hwnd)
                .map(|(_, r)| r.len())
        })
        .unwrap_or(0)
}

pub fn clear(hwnd: isize) {
    if let Ok(mut v) = REGIONS.lock() {
        v.retain(|(h, _)| *h != hwnd);
    }
    clear_hole_style(hwnd);
    hide_visual();
}

/// 目标窗口关掉后清掉它的选区和视觉洞，别让选区 UI 一直挂着。
pub fn prune() {
    let mut removed = false;
    if let Ok(mut v) = REGIONS.lock() {
        let before = v.len();
        v.retain(|(h, _)| unsafe { IsWindow(HWND(*h as *mut _)).as_bool() });
        removed = v.len() != before;
    }
    if let Ok(mut styles) = HOLE_STYLES.lock() {
        styles.retain(|h| unsafe { IsWindow(HWND(*h as *mut _)).as_bool() });
    }
    if removed {
        hide_visual();
    }
}

/// 编辑结束用：等透明洞真正抓完下层快照再返回（否则主界面可能被拍进洞里）
pub fn set_regions(hwnd: isize, regions: Vec<Region>) {
    apply_regions(hwnd, regions, true);
}

fn apply_regions(hwnd: isize, regions: Vec<Region>, wait: bool) {
    let empty = regions.is_empty();
    if let Ok(mut v) = REGIONS.lock() {
        v.retain(|(h, _)| *h != hwnd);
        if !regions.is_empty() {
            v.push((hwnd, regions));
        }
    }
    clear_hole_style(hwnd);
    // 即使当前透明度是 0（只点透、不改外观）也抓一张下层快照存着：
    // 之后在列表里把透明度滑块拉起来就能立刻看到视觉洞，不用重新抓屏。
    if empty {
        hide_visual();
    } else if wait {
        refresh_visual_wait(hwnd);
    } else {
        refresh_visual(hwnd);
    }
}

/// 所有有蒙版的窗口（hwnd, 数量），面板用来显示"其他窗口的蒙版"。
pub fn mask_windows() -> Vec<(isize, usize)> {
    REGIONS
        .lock()
        .map(|v| v.iter().filter(|(_, r)| !r.is_empty()).map(|(h, r)| (*h, r.len())).collect())
        .unwrap_or_default()
}

pub fn snapshot_all() -> Vec<(isize, Vec<Region>)> {
    REGIONS.lock().map(|v| v.clone()).unwrap_or_default()
}

pub fn update_region(
    hwnd: isize,
    idx: usize,
    invert: Option<bool>,
    opacity: Option<u8>,
    enabled: Option<bool>,
) {
    if let Ok(mut v) = REGIONS.lock() {
        if let Some((_, regions)) = v.iter_mut().find(|(h, _)| *h == hwnd) {
            if let Some(r) = regions.get_mut(idx) {
                if let Some(i) = invert {
                    r.invert = i;
                }
                if let Some(o) = opacity {
                    r.opacity = o.min(100);
                }
                if let Some(e) = enabled {
                    r.enabled = e;
                }
            }
        }
    }
    clear_hole_style(hwnd);
    // 透明度归 0 时只重画 mask（全透明 + follow_visual 会把覆盖层藏起来），
    // 保留已抓的下层快照，之后拉回透明度不用重抓。
    refresh_visual_mask();
}

pub fn remove_region(hwnd: isize, idx: usize) {
    let mut empty = false;
    if let Ok(mut v) = REGIONS.lock() {
        if let Some((_, regions)) = v.iter_mut().find(|(h, _)| *h == hwnd) {
            if idx < regions.len() {
                regions.remove(idx);
            }
            empty = regions.is_empty();
        }
        if empty {
            v.retain(|(h, _)| *h != hwnd);
        }
    }
    clear_hole_style(hwnd);
    if empty {
        hide_visual();
    } else {
        refresh_visual_mask();
    }
}

fn refresh_visual_mask() {
    let h = VISUAL_HWND.load(Ordering::Relaxed);
    if h != 0 {
        unsafe {
            let _ = PostMessageW(HWND(h as *mut _), WM_VISUAL_MASK, WPARAM(0), LPARAM(0));
        }
    }
}

fn clear_hole_style(hwnd: isize) {
    let mut hit = false;
    if let Ok(mut styles) = HOLE_STYLES.lock() {
        if let Some(pos) = styles.iter().position(|h| *h == hwnd) {
            styles.remove(pos);
            hit = true;
        }
    }
    let added_layered = HOLE_LAYERED
        .lock()
        .map(|mut v| {
            if let Some(pos) = v.iter().position(|h| *h == hwnd) {
                v.remove(pos);
                true
            } else {
                false
            }
        })
        .unwrap_or(false);
    if hit && !crate::window_passthrough_active(hwnd) {
        unsafe {
            crate::remove_hole_passthrough(HWND(hwnd as *mut _), added_layered);
        }
    }
}

pub fn enforce_at(x: i32, y: i32) {
    let has_any = REGIONS.lock().map(|v| !v.is_empty()).unwrap_or(false);
    if !has_any {
        return;
    }
    let Ok(map) = REGIONS.lock() else { return };
    let Ok(mut styles) = HOLE_STYLES.lock() else { return };

    // 取走「LAYERED 是我们加的」标记
    let take_layered = |h: isize| -> bool {
        HOLE_LAYERED
            .lock()
            .map(|mut v| {
                if let Some(pos) = v.iter().position(|x| *x == h) {
                    v.remove(pos);
                    true
                } else {
                    false
                }
            })
            .unwrap_or(false)
    };

    for (hwnd_val, regions) in map.iter() {
        let hwnd = HWND(*hwnd_val as *mut _);
        let tracked = styles.contains(hwnd_val);

        if crate::selector::is_session_window(*hwnd_val)
            || crate::window_passthrough_active(*hwnd_val)
        {
            if tracked {
                styles.retain(|h| h != hwnd_val);
            }
            let added = take_layered(*hwnd_val);
            if tracked || added {
                unsafe {
                    crate::remove_hole_passthrough(hwnd, added);
                }
            }
            continue;
        }

        let valid = unsafe {
            IsWindow(hwnd).as_bool() && IsWindowVisible(hwnd).as_bool() && !IsIconic(hwnd).as_bool()
        };
        if !valid {
            if tracked {
                styles.retain(|h| h != hwnd_val);
            }
            let added = take_layered(*hwnd_val);
            if tracked || added {
                unsafe {
                    crate::remove_hole_passthrough(hwnd, added);
                }
            }
            continue;
        }

        let Some((cx, cy, w, h)) = (unsafe { client_screen_rect(hwnd) }) else {
            if tracked {
                styles.retain(|h| h != hwnd_val);
            }
            let added = take_layered(*hwnd_val);
            if tracked || added {
                unsafe {
                    crate::remove_hole_passthrough(hwnd, added);
                }
            }
            continue;
        };
        let inside_rect = x >= cx && y >= cy && x < cx + w && y < cy + h;
        let inside = inside_rect && {
            let nx = (x - cx) as f32 / w as f32;
            let ny = (y - cy) as f32 / h as f32;
            regions.iter().any(|r| region_contains(r, nx, ny))
        };

        if inside {
            if !tracked {
                // WS_EX_TRANSPARENT 只有在 layered 窗口上才真的让鼠标穿过去
                let added = unsafe { crate::add_hole_passthrough(hwnd) };
                if added {
                    if let Ok(mut v) = HOLE_LAYERED.lock() {
                        v.push(*hwnd_val);
                    }
                }
                styles.push(*hwnd_val);
            }
        } else if tracked {
            styles.retain(|h| h != hwnd_val);
            let added = take_layered(*hwnd_val);
            unsafe {
                crate::remove_hole_passthrough(hwnd, added);
            }
        }
    }
}

/// 鼠标点是不是落在某个蒙版的洞里——洞里点击是"穿到下层"的，不应该把控制目标切过去。
pub fn hole_at(x: i32, y: i32) -> bool {
    let Ok(map) = REGIONS.lock() else { return false };
    for (hwnd_val, regions) in map.iter() {
        let hwnd = HWND(*hwnd_val as *mut _);
        let Some((cx, cy, w, h)) = (unsafe { client_screen_rect(hwnd) }) else {
            continue;
        };
        if w <= 0 || h <= 0 || x < cx || y < cy || x >= cx + w || y >= cy + h {
            continue;
        }
        let nx = (x - cx) as f32 / w as f32;
        let ny = (y - cy) as f32 / h as f32;
        if regions.iter().any(|r| region_contains(r, nx, ny)) {
            return true;
        }
    }
    false
}

fn region_contains(r: &Region, x: f32, y: f32) -> bool {
    if !r.enabled {
        return false;
    }
    let inside = point_in_poly(&r.pts, x, y);
    if r.invert {
        !inside
    } else {
        inside
    }
}

fn point_in_poly(poly: &[(f32, f32)], x: f32, y: f32) -> bool {
    if poly.len() < 3 {
        return false;
    }
    let mut inside = false;
    let mut j = poly.len() - 1;
    for i in 0..poly.len() {
        let (xi, yi) = poly[i];
        let (xj, yj) = poly[j];
        if (yi > y) != (yj > y) && x < (xj - xi) * (y - yi) / (yj - yi) + xi {
            inside = !inside;
        }
        j = i;
    }
    inside
}

unsafe fn client_screen_rect(hwnd: HWND) -> Option<(i32, i32, i32, i32)> {
    let mut rc = RECT::default();
    if GetClientRect(hwnd, &mut rc).is_err() {
        return None;
    }
    let w = rc.right - rc.left;
    let h = rc.bottom - rc.top;
    if w <= 0 || h <= 0 {
        return None;
    }
    let mut pt = POINT::default();
    if !ClientToScreen(hwnd, &mut pt).as_bool() {
        return None;
    }
    Some((pt.x, pt.y, w, h))
}

// ---------------------------------------------------------------------------
// 套索编辑器
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// 选区编辑器：窗口上直接画（画图式工具 + 拖出封闭图形 + 确定）
//   左键拖动画一个封闭图形（矩形 / 椭圆 / 自由），点「确定」生成一个蒙版。
//   框内 = 点透的洞（绿色高亮），框外仍可操作；Enter 确定、Esc 取消、Backspace 重画。
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tool {
    Rect,
    Ellipse,
    Free,
}

impl Tool {
    fn index(self) -> usize {
        match self {
            Tool::Rect => 0,
            Tool::Ellipse => 1,
            Tool::Free => 2,
        }
    }
}

struct EditState {
    active: bool,
    hwnd: isize,
    screen: (i32, i32, i32, i32),
    /// 已有蒙版（只做参考描边，不在编辑器里改）
    regions: Vec<Region>,
    tool: Tool,
    dragging: bool,
    drag_from: (i32, i32),
    drag_to: (i32, i32),
    /// 自由工具正在采集的点（像素）
    free: Vec<(i32, i32)>,
    /// 已经画好的封闭图形（归一化）；点「确定」才写入
    pending: Option<Vec<(f32, f32)>>,
    /// 悬停的按钮：0 无、1..3 工具、4 确定、5 取消、6 重画
    hover: u8,
    was_minimized: bool,
    prev_above: isize,
    topmost: bool,
    main_was_visible: bool,
}

impl Default for EditState {
    fn default() -> Self {
        Self {
            active: false,
            hwnd: 0,
            screen: (0, 0, 0, 0),
            regions: Vec::new(),
            tool: Tool::Rect,
            dragging: false,
            drag_from: (0, 0),
            drag_to: (0, 0),
            free: Vec::new(),
            pending: None,
            hover: 0,
            was_minimized: false,
            prev_above: 0,
            topmost: false,
            main_was_visible: false,
        }
    }
}

thread_local! {
    static ES: RefCell<EditState> = RefCell::new(EditState::default());
}

struct BarLayout {
    tools: [(i32, i32, i32, i32); 3],
    ok: (i32, i32, i32, i32),
    cancel: (i32, i32, i32, i32),
    redraw: (i32, i32, i32, i32),
    height: i32,
}

/// 顶部工具栏布局。窗口太窄时把动作按钮放到第二行。
fn bar_layout(w: i32) -> BarLayout {
    let tw = 64;
    let gap = 6;
    let tools = [
        (8, 8, 8 + tw, 40),
        (8 + tw + gap, 8, 8 + 2 * tw + gap, 40),
        (8 + 2 * (tw + gap), 8, 8 + 3 * tw + 2 * gap, 40),
    ];
    let aw = 76;
    let (y0, y1, height) = if w >= 560 { (8, 40, 52) } else { (46, 78, 88) };
    let ok = (w - 8 - aw, y0, w - 8, y1);
    let cancel = (ok.0 - gap - aw, y0, ok.0 - gap, y1);
    let redraw = (cancel.0 - gap - aw, y0, cancel.0 - gap, y1);
    BarLayout {
        tools,
        ok,
        cancel,
        redraw,
        height,
    }
}

fn in_rect(x: i32, y: i32, r: (i32, i32, i32, i32)) -> bool {
    x >= r.0 && x < r.2 && y >= r.1 && y < r.3
}

/// 命中返回：1..3 工具，4 确定，5 取消，6 重画，0 无
fn hit_bar(x: i32, y: i32, w: i32) -> u8 {
    let bar = bar_layout(w);
    if y >= bar.height {
        return 0;
    }
    for (i, r) in bar.tools.iter().enumerate() {
        if in_rect(x, y, *r) {
            return 1 + i as u8;
        }
    }
    if in_rect(x, y, bar.ok) {
        return 4;
    }
    if in_rect(x, y, bar.cancel) {
        return 5;
    }
    if in_rect(x, y, bar.redraw) {
        return 6;
    }
    0
}

fn normalize(x: i32, y: i32, w: i32, h: i32) -> (f32, f32) {
    let ww = w.max(1) as f32;
    let hh = h.max(1) as f32;
    (
        x.clamp(0, w.max(1)) as f32 / ww,
        y.clamp(0, h.max(1)) as f32 / hh,
    )
}

fn rect_norm(x0: i32, y0: i32, x1: i32, y1: i32, w: i32, h: i32) -> Vec<(f32, f32)> {
    let (xa, xb) = (x0.min(x1), x0.max(x1));
    let (ya, yb) = (y0.min(y1), y0.max(y1));
    vec![
        normalize(xa, ya, w, h),
        normalize(xb, ya, w, h),
        normalize(xb, yb, w, h),
        normalize(xa, yb, w, h),
    ]
}

fn ellipse_norm(x0: i32, y0: i32, x1: i32, y1: i32, w: i32, h: i32) -> Vec<(f32, f32)> {
    let (xa, xb) = (x0.min(x1), x0.max(x1));
    let (ya, yb) = (y0.min(y1), y0.max(y1));
    let cx = (xa + xb) as f32 / 2.0;
    let cy = (ya + yb) as f32 / 2.0;
    let rx = ((xb - xa) as f32 / 2.0).max(1.0);
    let ry = ((yb - ya) as f32 / 2.0).max(1.0);
    let n = 48;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let a = 2.0 * std::f32::consts::PI * i as f32 / n as f32;
        out.push(normalize(
            (cx + rx * a.cos()) as i32,
            (cy + ry * a.sin()) as i32,
            w,
            h,
        ));
    }
    out
}

unsafe fn editor_thread() {
    let hinst = match GetModuleHandleW(None) {
        Ok(h) => h,
        Err(_) => return,
    };
    let class_w: Vec<u16> = "TransGlassRegionEditor"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let wc = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(editor_proc),
        hInstance: HINSTANCE(hinst.0),
        hCursor: LoadCursorW(None, IDC_CROSS).unwrap_or_default(),
        lpszClassName: windows::core::PCWSTR(class_w.as_ptr()),
        ..Default::default()
    };
    let _ = RegisterClassW(&wc);
    let hwnd = match CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
        windows::core::PCWSTR(class_w.as_ptr()),
        w!("TransGlassRegionEditor"),
        WS_POPUP,
        0,
        0,
        1,
        1,
        None,
        None,
        HINSTANCE(hinst.0),
        None,
    ) {
        Ok(h) => h,
        Err(_) => return,
    };
    EDITOR_HWND.store(hwnd.0 as isize, Ordering::Relaxed);

    let mut msg = MSG::default();
    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
}

unsafe extern "system" fn editor_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        WM_EDITOR_OPEN => {
            begin_edit(hwnd);
            LRESULT(0)
        }
        WM_SETCURSOR => {
            let _ = SetCursor(LoadCursorW(None, IDC_CROSS).unwrap_or_default());
            LRESULT(1)
        }
        WM_LBUTTONDOWN => {
            on_down(l);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            on_move(l);
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            on_up();
            LRESULT(0)
        }
        WM_KEYDOWN => {
            on_key(w.0 as u32);
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, w, l),
    }
}

unsafe fn begin_edit(overlay: HWND) {
    let Some(hwnd_val) = PENDING.lock().ok().and_then(|mut p| p.take()) else {
        return;
    };
    if ES.with(|e| e.borrow().active) {
        return;
    }
    let hwnd = HWND(hwnd_val as *mut _);
    if !IsWindow(hwnd).as_bool() || !IsWindowVisible(hwnd).as_bool() {
        return;
    }
    hide_visual();

    let was_minimized = IsIconic(hwnd).as_bool();
    if was_minimized {
        let _ = ShowWindow(hwnd, SW_RESTORE);
        thread::sleep(std::time::Duration::from_millis(90));
    }
    let Some((cx, cy, w, h)) = client_screen_rect(hwnd) else {
        if was_minimized {
            let _ = ShowWindow(hwnd, SW_MINIMIZE);
        }
        return;
    };
    let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
    let topmost = (ex & WS_EX_TOPMOST.0) != 0;
    let prev_above = prev_visible_same_band(hwnd, topmost);
    let regions = REGIONS
        .lock()
        .ok()
        .and_then(|v| v.iter().find(|(h, _)| *h == hwnd_val).map(|(_, r)| r.clone()))
        .unwrap_or_default();

    // 主界面先藏起来：画布干净，下层快照也不会把主界面拍进去（否则洞里的文字会和窗口本身错位）
    let main_was_visible = crate::hide_root_for_overlay();
    let _ = SetWindowPos(
        hwnd,
        HWND_TOP,
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
    );

    ES.with(|e| {
        *e.borrow_mut() = EditState {
            active: true,
            hwnd: hwnd_val,
            screen: (cx, cy, w, h),
            regions,
            tool: Tool::Rect,
            dragging: false,
            drag_from: (0, 0),
            drag_to: (0, 0),
            free: Vec::new(),
            pending: None,
            hover: 0,
            was_minimized,
            prev_above,
            topmost,
            main_was_visible,
        };
    });
    let _ = SetWindowPos(overlay, HWND_TOPMOST, cx, cy, w, h, SWP_NOACTIVATE);
    draw_editor(overlay);
    let _ = ShowWindow(overlay, SW_SHOW);
    focus_editor(overlay);
}

unsafe fn end_edit(overlay: HWND, save: bool) {
    let state = ES.with(|e| std::mem::take(&mut *e.borrow_mut()));
    if !state.active {
        return;
    }
    let _ = ShowWindow(overlay, SW_HIDE);
    let _ = ReleaseCapture();
    if save {
        // 同步等透明洞抓完下层快照，再恢复主界面，避免主界面出现在洞里
        set_regions(state.hwnd, state.regions.clone());
        // 让面板切到这扇窗口，刚画的蒙版立刻能在列表里看到/调
        crate::set_target_window(state.hwnd);
    }
    clear_hole_style(state.hwnd);

    let hwnd = HWND(state.hwnd as *mut _);
    if IsWindow(hwnd).as_bool() {
        let target = if state.prev_above != 0
            && IsWindow(HWND(state.prev_above as *mut _)).as_bool()
        {
            HWND(state.prev_above as *mut _)
        } else if state.topmost {
            HWND_TOPMOST
        } else {
            HWND_TOP
        };
        let _ = SetWindowPos(
            hwnd,
            target,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
        if state.was_minimized {
            let _ = ShowWindow(hwnd, SW_MINIMIZE);
        }
    }
    if state.main_was_visible {
        crate::show_root_after_overlay();
    }
}

unsafe fn focus_editor(hwnd: HWND) {
    let fg = GetForegroundWindow();
    if fg == hwnd {
        let _ = SetFocus(hwnd);
        return;
    }
    let fg_tid = if fg.0.is_null() {
        0
    } else {
        GetWindowThreadProcessId(fg, None)
    };
    let my_tid = GetCurrentThreadId();
    let attached = fg_tid != 0 && fg_tid != my_tid;
    if attached {
        let _ = AttachThreadInput(my_tid, fg_tid, true);
    }
    let _ = SetForegroundWindow(hwnd);
    let _ = SetFocus(hwnd);
    if attached {
        let _ = AttachThreadInput(my_tid, fg_tid, false);
    }
}

unsafe fn prev_visible_same_band(hwnd: HWND, topmost: bool) -> isize {
    let mut above = GetWindow(hwnd, GW_HWNDPREV).ok();
    while let Some(h) = above {
        if IsWindowVisible(h).as_bool() {
            let ex = GetWindowLongW(h, GWL_EXSTYLE) as u32;
            if ((ex & WS_EX_TOPMOST.0) != 0) == topmost {
                break;
            }
        }
        above = GetWindow(h, GW_HWNDPREV).ok();
    }
    above.map(|h| h.0 as isize).unwrap_or(0)
}

unsafe fn on_down(l: LPARAM) {
    let (x, y) = mouse_xy(l);
    let (w, h) = ES.with(|e| {
        let st = e.borrow();
        (st.screen.2, st.screen.3)
    });
    if w <= 0 || h <= 0 {
        return;
    }
    let overlay = HWND(EDITOR_HWND.load(Ordering::Relaxed) as *mut _);
    let bar = bar_layout(w);
    if y < bar.height {
        match hit_bar(x, y, w) {
            1 => {
                ES.with(|e| e.borrow_mut().tool = Tool::Rect);
                redraw();
            }
            2 => {
                ES.with(|e| e.borrow_mut().tool = Tool::Ellipse);
                redraw();
            }
            3 => {
                ES.with(|e| e.borrow_mut().tool = Tool::Free);
                redraw();
            }
            4 => confirm_edit(overlay),
            5 => end_edit(overlay, false),
            6 => {
                ES.with(|e| {
                    let mut st = e.borrow_mut();
                    st.pending = None;
                    st.free.clear();
                });
                redraw();
            }
            _ => {}
        }
        return;
    }
    let _ = SetCapture(overlay);
    ES.with(|e| {
        let mut st = e.borrow_mut();
        st.dragging = true;
        st.drag_from = (x, y);
        st.drag_to = (x, y);
        st.pending = None;
        st.free.clear();
        st.free.push((x, y));
    });
    redraw();
}

unsafe fn on_move(l: LPARAM) {
    let (x, y) = mouse_xy(l);
    let (w, h, dragging, tool) = ES.with(|e| {
        let st = e.borrow();
        (st.screen.2, st.screen.3, st.dragging, st.tool)
    });
    if w <= 0 || h <= 0 {
        return;
    }
    if !dragging {
        let hv = if y < bar_layout(w).height {
            hit_bar(x, y, w)
        } else {
            0
        };
        let changed = ES.with(|e| {
            let mut st = e.borrow_mut();
            if st.hover != hv {
                st.hover = hv;
                true
            } else {
                false
            }
        });
        if changed {
            redraw();
        }
        return;
    }
    let changed = ES.with(|e| {
        let mut st = e.borrow_mut();
        match tool {
            Tool::Free => {
                if let Some((lx, ly)) = st.free.last() {
                    if (x - lx).abs() + (y - ly).abs() < 2 {
                        return false;
                    }
                }
                st.free.push((x, y));
            }
            _ => {
                if st.drag_to == (x, y) {
                    return false;
                }
                st.drag_to = (x, y);
            }
        }
        true
    });
    if changed {
        redraw();
    }
}

unsafe fn on_up() {
    let _ = ReleaseCapture();
    ES.with(|e| {
        let mut st = e.borrow_mut();
        if !st.dragging {
            return;
        }
        st.dragging = false;
        let (w, h) = (st.screen.2, st.screen.3);
        if w <= 0 || h <= 0 {
            return;
        }
        let (x0, y0) = st.drag_from;
        let (x1, y1) = st.drag_to;
        let shape = match st.tool {
            Tool::Rect => {
                if (x1 - x0).abs() >= 4 && (y1 - y0).abs() >= 4 {
                    Some(rect_norm(x0, y0, x1, y1, w, h))
                } else {
                    None
                }
            }
            Tool::Ellipse => {
                if (x1 - x0).abs() >= 4 && (y1 - y0).abs() >= 4 {
                    Some(ellipse_norm(x0, y0, x1, y1, w, h))
                } else {
                    None
                }
            }
            Tool::Free => {
                let pts = std::mem::take(&mut st.free);
                if pts.len() >= 3 {
                    Some(
                        pts.iter()
                            .map(|(x, y)| normalize(*x, *y, w, h))
                            .collect(),
                    )
                } else {
                    None
                }
            }
        };
        st.pending = shape;
    });
    redraw();
}

unsafe fn on_key(vk: u32) {
    let overlay = HWND(EDITOR_HWND.load(Ordering::Relaxed) as *mut _);
    if vk == VK_RETURN.0 as u32 {
        confirm_edit(overlay);
    } else if vk == VK_ESCAPE.0 as u32 {
        end_edit(overlay, false);
    } else if vk == VK_BACK.0 as u32 || vk == VK_DELETE.0 as u32 {
        ES.with(|e| {
            let mut st = e.borrow_mut();
            st.pending = None;
            st.free.clear();
        });
        redraw();
    }
}

/// 把当前画好的图形写成一个蒙版并关闭编辑器
unsafe fn confirm_edit(overlay: HWND) {
    let ok = ES.with(|e| {
        let mut st = e.borrow_mut();
        let Some(pts) = st.pending.take() else {
            return false;
        };
        if pts.len() < 3 {
            return false;
        }
        st.regions.push(Region {
            pts,
            invert: false,
            // 新蒙版默认只做「点击穿透」，不改变窗口外观：
            // 视觉洞贴的是下层快照，默认打开会让人看到别的窗口文字叠在本窗口文字上，像"文字错位"。
            // 想要视觉洞到列表里把透明度拉起来即可（会有提示）。
            // 新蒙版默认给一个看得见的半透明，用户才能立刻确认洞的位置和效果
            opacity: 40,
            enabled: true,
        });
        true
    });
    if ok {
        end_edit(overlay, true);
    }
}

unsafe fn redraw() {
    let h = EDITOR_HWND.load(Ordering::Relaxed);
    if h != 0 {
        draw_editor(HWND(h as *mut _));
    }
}

fn mouse_xy(l: LPARAM) -> (i32, i32) {
    let x = (l.0 & 0xFFFF) as u16 as i16 as i32;
    let y = ((l.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
    (x, y)
}

unsafe fn draw_bar_button(
    buf: &mut [u8],
    mem: HDC,
    w: i32,
    h: i32,
    r: (i32, i32, i32, i32),
    label: &str,
    selected: bool,
    primary: bool,
    enabled: bool,
    hover: bool,
    font: HFONT,
) {
    let (x0, y0, x1, y1) = (r.0, r.1, r.2.min(w), r.3.min(h));
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    let bg = if !enabled {
        (236, 238, 240)
    } else if primary {
        (47, 194, 124)
    } else if selected {
        // 选中的工具用浅绿底 + 深绿字，白字在绿底上太小会糊
        (226, 245, 236)
    } else if hover {
        (236, 245, 240)
    } else {
        (255, 255, 255)
    };
    fill_rect(buf, w, x0, y0, x1, y1, bg, 255);
    let edge = if primary {
        (39, 166, 105)
    } else if selected {
        (47, 194, 124)
    } else {
        (210, 214, 220)
    };
    fill_rect(buf, w, x0, y0, x1, y0 + 1, edge, 255);
    fill_rect(buf, w, x0, y1 - 1, x1, y1, edge, 255);
    fill_rect(buf, w, x0, y0, x0 + 1, y1, edge, 255);
    fill_rect(buf, w, x1 - 1, y0, x1, y1, edge, 255);
    let color = if !enabled {
        (150, 154, 160)
    } else if primary {
        (255, 255, 255)
    } else if selected {
        (24, 120, 78)
    } else {
        (40, 44, 50)
    };
    let mut rc = RECT {
        left: x0,
        top: y0,
        right: x1,
        bottom: y1,
    };
    draw_text(
        mem,
        label,
        &mut rc,
        color,
        font,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
    );
}

unsafe fn draw_editor(overlay: HWND) {
    let (cx, cy, w, h) = ES.with(|e| e.borrow().screen);
    if w <= 0 || h <= 0 {
        return;
    }
    let screen = GetDC(None);
    let mem = CreateCompatibleDC(screen);
    let mut bi: BITMAPINFO = std::mem::zeroed();
    bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    bi.bmiHeader.biWidth = w;
    bi.bmiHeader.biHeight = -h;
    bi.bmiHeader.biPlanes = 1;
    bi.bmiHeader.biBitCount = 32;
    bi.bmiHeader.biCompression = BI_RGB.0;
    let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
    let Ok(dib) = CreateDIBSection(mem, &bi, DIB_RGB_COLORS, &mut bits, None, 0) else {
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        return;
    };
    let old = SelectObject(mem, HGDIOBJ(dib.0));
    let buf = std::slice::from_raw_parts_mut(bits as *mut u8, (w * h * 4) as usize);

    // 整块压暗；框内再擦亮，形成微信截图那种“选区亮、周围暗”的效果
    for px in buf.chunks_exact_mut(4) {
        px[0] = 0;
        px[1] = 0;
        px[2] = 0;
        px[3] = 118;
    }

    let (tool, pending, free, dragging, from, to, hover, regions) = ES.with(|e| {
        let st = e.borrow();
        (
            st.tool,
            st.pending.clone(),
            st.free.clone(),
            st.dragging,
            st.drag_from,
            st.drag_to,
            st.hover,
            st.regions.clone(),
        )
    });

    // 已有蒙版：细灰框参考
    for r in &regions {
        let pts: Vec<(i32, i32)> = r
            .pts
            .iter()
            .map(|(x, y)| ((x * w as f32) as i32, (y * h as f32) as i32))
            .collect();
        stroke_poly(buf, w, h, &pts, (128, 130, 136), 1);
    }

    // 当前形状：已画好的 pending，或正在拖的实时预览
    let live: Option<Vec<(f32, f32)>> = if let Some(p) = &pending {
        Some(p.clone())
    } else if dragging {
        match tool {
            Tool::Rect => Some(rect_norm(from.0, from.1, to.0, to.1, w, h)),
            Tool::Ellipse => Some(ellipse_norm(from.0, from.1, to.0, to.1, w, h)),
            Tool::Free => {
                if free.len() >= 2 {
                    Some(
                        free.iter()
                            .map(|(x, y)| normalize(*x, *y, w, h))
                            .collect(),
                    )
                } else {
                    None
                }
            }
        }
    } else {
        None
    };
    if let Some(shape) = &live {
        let pts: Vec<(i32, i32)> = shape
            .iter()
            .map(|(x, y)| ((x * w as f32) as i32, (y * h as f32) as i32))
            .collect();
        if pts.len() >= 3 {
            fill_poly(buf, w, h, &pts, (0, 0, 0), 0);
            fill_poly(buf, w, h, &pts, (47, 194, 124), 44);
        }
        stroke_poly(buf, w, h, &pts, (47, 194, 124), 2);
    }

    // 顶部工具栏
    let bar = bar_layout(w);
    fill_rect(buf, w, 0, 0, w, bar.height, (250, 251, 252), 255);
    fill_rect(
        buf,
        w,
        0,
        bar.height - 1,
        w,
        bar.height,
        (214, 218, 224),
        255,
    );

    // 13px 粗体 CJK 在 GDI 里会糊成空心块，用常规字重
    let font = create_font(13, 400);
    let font_small = create_font(12, 400);
    for (i, r) in bar.tools.iter().enumerate() {
        draw_bar_button(
            buf,
            mem,
            w,
            h,
            *r,
            ["矩形", "椭圆", "自由"][i],
            tool.index() == i,
            false,
            true,
            hover == 1 + i as u8,
            font,
        );
    }
    let has_pending = pending.is_some();
    draw_bar_button(
        buf, mem, w, h, bar.redraw, "重画", false, false, true, hover == 6, font,
    );
    draw_bar_button(
        buf, mem, w, h, bar.cancel, "取消", false, false, true, hover == 5, font,
    );
    draw_bar_button(
        buf,
        mem,
        w,
        h,
        bar.ok,
        "确定",
        false,
        true,
        has_pending,
        hover == 4,
        font,
    );

    if bar.redraw.0 - 232 > 100 {
        let mut rc = RECT {
            left: 224,
            top: 8,
            right: bar.redraw.0 - 8,
            bottom: 40,
        };
        draw_text(
            mem,
            "框内 = 点透（洞） · 框外仍可操作 · Enter 确定 · Esc 取消",
            &mut rc,
            (95, 100, 106),
            font_small,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER,
        );
    }

    let _ = SelectObject(mem, GetStockObject(DEFAULT_GUI_FONT));
    let _ = DeleteObject(HGDIOBJ(font.0));
    let _ = DeleteObject(HGDIOBJ(font_small.0));

    // GDI 画文字不写 alpha（文字像素 alpha 会变成 0，分层窗口里就是"透明的字"）。
    // 工具栏是整块不透明底，直接把这行 alpha 补满。
    for y in 0..bar.height.min(h) {
        let row = (y * w) as usize;
        for x in 0..w {
            buf[(row + x as usize) * 4 + 3] = 255;
        }
    }

    let pos = POINT { x: cx, y: cy };
    let size = SIZE { cx: w, cy: h };
    let src = POINT { x: 0, y: 0 };
    let blend = BLENDFUNCTION {
        BlendOp: 0,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: 1,
    };
    let _ = UpdateLayeredWindow(
        overlay,
        screen,
        Some(&pos),
        Some(&size),
        mem,
        Some(&src),
        COLORREF(0),
        Some(&blend),
        ULW_ALPHA,
    );

    SelectObject(mem, old);
    let _ = DeleteObject(HGDIOBJ(dib.0));
    let _ = DeleteDC(mem);
    ReleaseDC(None, screen);
}

// ---------------------------------------------------------------------------
// 真·透明洞覆盖层（下层快照 + 洞形状 alpha）
// ---------------------------------------------------------------------------

struct VisualState {
    target: isize,
    /// 整块虚拟屏幕的下层快照（RGBA，alpha=255）。拖动窗口时从这里裁，不用重抓 → 不闪。
    snapshot: Option<Vec<u8>>,
    /// snapshot 对应的虚拟屏幕原点和尺寸
    origin: (i32, i32),
    w: i32,
    h: i32,
    /// 目标窗口自己的客户区快照（PrintWindow，1:1，客户区尺寸）。洞里按透明度与下层混合，
    /// 这样"透明"作用在窗口自己的画面上，而不是把下层图片直接盖上去。
    win_snap: Option<Vec<u8>>,
    win_w: i32,
    win_h: i32,
    /// 目标窗口客户区（当前屏幕坐标 + 尺寸）
    rect: (i32, i32, i32, i32),
    regions: Vec<Region>,
}

impl Default for VisualState {
    fn default() -> Self {
        Self {
            target: 0,
            snapshot: None,
            origin: (0, 0),
            w: 0,
            h: 0,
            win_snap: None,
            win_w: 0,
            win_h: 0,
            rect: (0, 0, 0, 0),
            regions: Vec::new(),
        }
    }
}

thread_local! {
    static VS: RefCell<VisualState> = RefCell::new(VisualState::default());
}

unsafe fn visual_thread() {
    let hinst = match GetModuleHandleW(None) {
        Ok(h) => h,
        Err(_) => return,
    };
    let class_w: Vec<u16> = "TransGlassHole"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let wc = WNDCLASSW {
        lpfnWndProc: Some(visual_proc),
        hInstance: HINSTANCE(hinst.0),
        lpszClassName: windows::core::PCWSTR(class_w.as_ptr()),
        ..Default::default()
    };
    let _ = RegisterClassW(&wc);
    let hwnd = match CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
        windows::core::PCWSTR(class_w.as_ptr()),
        w!("TransGlassHole"),
        WS_POPUP,
        0,
        0,
        1,
        1,
        None,
        None,
        HINSTANCE(hinst.0),
        None,
    ) {
        Ok(h) => h,
        Err(_) => return,
    };
    VISUAL_HWND.store(hwnd.0 as isize, Ordering::Relaxed);
    // 50ms：拖动窗口时洞跟得更紧（内容按当前 rect 从整屏快照重裁，不重抓，所以不会闪）
    SetTimer(hwnd, 1, 16, None);

    let mut msg = MSG::default();
    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
}

unsafe extern "system" fn visual_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        WM_VISUAL_REFRESH => {
            VISUAL_HIDE_REQ.store(false, Ordering::Relaxed);
            let target = VISUAL_REQ.lock().ok().and_then(|mut p| p.take()).unwrap_or(0);
            if target != 0 {
                capture_visual(hwnd, target);
            }
            VISUAL_SEQ.fetch_add(1, Ordering::Relaxed);
            LRESULT(0)
        }
        WM_VISUAL_MASK => {
            let (target, w, h, has_snapshot) = VS.with(|v| {
                let st = v.borrow();
                (st.target, st.w, st.h, st.snapshot.is_some())
            });
            if target != 0 && w > 0 && h > 0 && has_snapshot {
                if let Ok(map) = REGIONS.lock() {
                    if let Some((_, regions)) = map.iter().find(|(x, _)| *x == target) {
                        let regions = regions.clone();
                        VS.with(|v| v.borrow_mut().regions = regions);
                    }
                }
                draw_visual(hwnd);
                follow_visual(hwnd);
            }
            LRESULT(0)
        }
        WM_VISUAL_HIDE => {
            VISUAL_HIDE_REQ.store(false, Ordering::Relaxed);
            VS.with(|v| *v.borrow_mut() = VisualState::default());
            let _ = ShowWindow(hwnd, SW_HIDE);
            LRESULT(0)
        }
        WM_TIMER => {
            follow_visual(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, w, l),
    }
}

unsafe fn capture_visual(overlay: HWND, target: isize) {
    let regions = REGIONS
        .lock()
        .ok()
        .and_then(|v| {
            v.iter()
                .find(|(h, _)| *h == target)
                .map(|(_, r)| r.clone())
        })
        .unwrap_or_default();
    if regions.is_empty() {
        VS.with(|v| *v.borrow_mut() = VisualState::default());
        let _ = ShowWindow(overlay, SW_HIDE);
        return;
    }
    let hwnd = HWND(target as *mut _);
    if !IsWindow(hwnd).as_bool() || !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
        let _ = ShowWindow(overlay, SW_HIDE);
        return;
    }
    let Some((cx, cy, w, h)) = client_screen_rect(hwnd) else {
        let _ = ShowWindow(overlay, SW_HIDE);
        return;
    };
    let was_foreground = GetForegroundWindow() == hwnd;

    // 先抓窗口自己的客户区画面（1:1）。优先从屏幕截取（Chromium/KOOK 的 PrintWindow 经常是黑帧），
    // PrintWindow 兜底；此时目标窗口还没被任何样式/挖洞影响，截到的就是它真实的画面。
    let win_snap = capture_screen_client(cx, cy, w, h).or_else(|| capture_window_client(hwnd, w, h));

    // 主界面绝不能出现在洞里：抓下层快照前无条件藏起来（目标 hwnd 可能和面板里的不是同一个，
    // 用"矩形相交"判断会漏掉，结果面板被拍进洞里、洞上浮出一块自己的 UI）。
    let root = crate::main_window_handle();
    let root_hwnd = HWND(root as *mut _);
    let mut main_hidden = false;
    let mut main_was_foreground = false;
    if root != 0 && IsWindowVisible(root_hwnd).as_bool() && !IsIconic(root_hwnd).as_bool() {
        main_was_foreground = GetForegroundWindow() == root_hwnd;
        let _ = ShowWindow(root_hwnd, SW_HIDE);
        main_hidden = true;
    }

    // 抓整块虚拟屏幕，不只抓客户区：之后窗口拖动/缩放时直接从这张图里裁，
    // 不用再藏目标窗口重抓 → 松手不闪、洞里的内容也不会错位。
    let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
    let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
    let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN);
    let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN);
    if vw <= 0 || vh <= 0 {
        let _ = ShowWindow(overlay, SW_HIDE);
        return;
    }

    let _ = ShowWindow(overlay, SW_HIDE);
    let _ = ShowWindow(hwnd, SW_HIDE);
    thread::sleep(std::time::Duration::from_millis(80));

    let screen = GetDC(None);
    let mem = CreateCompatibleDC(screen);
    let mut bi: BITMAPINFO = std::mem::zeroed();
    bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    bi.bmiHeader.biWidth = vw;
    bi.bmiHeader.biHeight = -vh;
    bi.bmiHeader.biPlanes = 1;
    bi.bmiHeader.biBitCount = 32;
    bi.bmiHeader.biCompression = BI_RGB.0;
    let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
    let dib = CreateDIBSection(mem, &bi, DIB_RGB_COLORS, &mut bits, None, 0);

    let mut snapshot = None;
    if let Ok(dib) = dib {
        let old = SelectObject(mem, HGDIOBJ(dib.0));
        if BitBlt(mem, 0, 0, vw, vh, screen, vx, vy, SRCCOPY).is_ok() {
            let buf = std::slice::from_raw_parts(bits as *const u8, (vw * vh * 4) as usize);
            let mut v = buf.to_vec();
            for px in v.chunks_exact_mut(4) {
                px[3] = 255;
            }
            snapshot = Some(v);
        }
        SelectObject(mem, old);
        let _ = DeleteObject(HGDIOBJ(dib.0));
    }
    let _ = DeleteDC(mem);
    ReleaseDC(None, screen);

    let _ = ShowWindow(hwnd, SW_SHOW);
    if was_foreground {
        let _ = SetForegroundWindow(hwnd);
    }
    if main_hidden {
        let _ = ShowWindow(root_hwnd, SW_SHOW);
        if main_was_foreground {
            let _ = SetForegroundWindow(root_hwnd);
        }
    }

    let Some(snapshot) = snapshot else {
        let _ = ShowWindow(overlay, SW_HIDE);
        return;
    };
    VS.with(|v| {
        *v.borrow_mut() = VisualState {
            target,
            snapshot: Some(snapshot),
            origin: (vx, vy),
            w: vw,
            h: vh,
            win_snap,
            win_w: w,
            win_h: h,
            rect: (cx, cy, w, h),
            regions,
        };
    });
    draw_visual(overlay);
    follow_visual(overlay);
}

unsafe fn follow_visual(overlay: HWND) {
    let (target, rect, has_snapshot, has_visual) = VS.with(|v| {
        let st = v.borrow();
        (
            st.target,
            st.rect,
            st.snapshot.is_some(),
            st.regions.iter().any(|r| r.enabled && r.opacity > 0),
        )
    });
    if target == 0 || !has_snapshot || !has_visual {
        let _ = ShowWindow(overlay, SW_HIDE);
        crate::gpu_hole::hide();
        return;
    }
    // GPU v2：每个矩形洞一个小窗口 + DWM 缩略图（缩略图被小窗口边界自然裁住）。
    // 非矩形或没有合适的下层窗口时自动回退 CPU。
    {
        let mut rect = None;
        let mut op = 0u8;
        let screen_rect = {
            let hw = HWND(target as *mut _);
            client_screen_rect(hw)
        };
        let (gcx, gcy, gcw, gch) = match screen_rect {
            Some(v) => v,
            None => {
                let _ = ShowWindow(overlay, SW_HIDE);
                return;
            }
        };
        VS.with(|v| {
            let st = v.borrow();
            for r in st.regions.iter().filter(|r| r.enabled && r.opacity > 0) {
                op = op.max(r.opacity);
                if rect.is_none() && r.pts.len() <= 4 {
                    let (mut minx, mut miny, mut maxx, mut maxy) =
                        (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
                    for (nx, ny) in &r.pts {
                        minx = minx.min(*nx);
                        miny = miny.min(*ny);
                        maxx = maxx.max(*nx);
                        maxy = maxy.max(*ny);
                    }
                    rect = Some((
                        gcx + (minx * gcw as f32) as i32,
                        gcy + (miny * gch as f32) as i32,
                        gcx + (maxx * gcw as f32) as i32,
                        gcy + (maxy * gch as f32) as i32,
                    ));
                }
            }
        });
        match rect {
            Some(rc) if op > 0 => crate::gpu_hole::update(target, rc, op),
            _ => crate::gpu_hole::hide(),
        }
    }
    if crate::gpu_hole::using() {
        let _ = ShowWindow(overlay, SW_HIDE);
        return;
    }
    let hwnd = HWND(target as *mut _);
    let Some((cx, cy, cw, ch)) = client_screen_rect(hwnd) else {
        let _ = ShowWindow(overlay, SW_HIDE);
        return;
    };
    if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() {
        let _ = ShowWindow(overlay, SW_HIDE);
        return;
    }
    // 遮挡：把目标上面那些窗口的矩形从洞区里扣掉，只显示真正露出来的部分；
    // 全被盖住才隐藏（以前按采样点判断，挡住一点就整块消失）。
    let base = CreateRectRgn(0, 0, 0, 0);
    if base.is_invalid() {
        let _ = ShowWindow(overlay, SW_HIDE);
        return;
    }
    let regions_now = VS.with(|v| v.borrow().regions.clone());
    for r in regions_now.iter().filter(|r| r.enabled && r.opacity > 0) {
        if r.pts.len() < 3 {
            continue;
        }
        let pts: Vec<POINT> = r
            .pts
            .iter()
            .map(|(nx, ny)| POINT {
                x: (nx * cw as f32).round() as i32,
                y: (ny * ch as f32).round() as i32,
            })
            .collect();
        let pr = CreatePolygonRgn(&pts, WINDING);
        if pr.is_invalid() {
            continue;
        }
        let _ = CombineRgn(Some(&base), Some(&base), Some(&pr), RGN_OR);
        let _ = DeleteObject(HGDIOBJ(pr.0));
    }
    let null = HWND(std::ptr::null_mut());
    let mut above = GetTopWindow(null).unwrap_or(null);
    while !above.0.is_null() && above.0 as isize != target {
        if !crate::is_own_hwnd(above) && !crate::is_shell_hwnd(above) && IsWindowVisible(above).as_bool() {
            let mut rr = RECT::default();
            if GetWindowRect(above, &mut rr).is_ok() {
                let clip = CreateRectRgn(rr.left - cx, rr.top - cy, rr.right - cx, rr.bottom - cy);
                if !clip.is_invalid() {
                    let _ = CombineRgn(Some(&base), Some(&base), Some(&clip), RGN_DIFF);
                    let _ = DeleteObject(HGDIOBJ(clip.0));
                }
            }
        }
        above = GetWindow(above, GW_HWNDNEXT).unwrap_or(null);
    }
    let mut bx = RECT::default();
    let _ = GetRgnBox(base, &mut bx);
    if bx.left >= bx.right || bx.top >= bx.bottom {
        let _ = DeleteObject(HGDIOBJ(base.0));
        let _ = ShowWindow(overlay, SW_HIDE);
        return;
    }
    let _ = SetWindowRgn(overlay, Some(&base), true);
    if (rect.0, rect.1, rect.2, rect.3) != (cx, cy, cw, ch) {
        // 窗动洞动：整屏快照已经在 VS 里了，直接按新位置重裁一块，不用再抓屏 → 拖动/松手都不闪。
        VS.with(|v| v.borrow_mut().rect = (cx, cy, cw, ch));
        draw_visual(overlay);
    }
    let _ = SetWindowPos(
        overlay,
        HWND_TOPMOST,
        cx,
        cy,
        cw,
        ch,
        SWP_NOACTIVATE | SWP_SHOWWINDOW,
    );
    let _ = ShowWindow(overlay, SW_SHOW);
}

/// 从 z 序最上面往下找：包含这一点的、不是我们进程的顶层窗口（忽略 shell/桌面）。
unsafe fn top_foreign_window_at(x: i32, y: i32) -> isize {
    let mut h = match GetTopWindow(HWND(std::ptr::null_mut())) {
        Ok(h) => h,
        Err(_) => return 0,
    };
    while !h.0.is_null() {
        if !crate::is_own_hwnd(h) && !crate::is_shell_hwnd(h) {
            let mut r = RECT::default();
            if IsWindowVisible(h).as_bool()
                && GetWindowRect(h, &mut r).is_ok()
                && x >= r.left
                && x < r.right
                && y >= r.top
                && y < r.bottom
            {
                return h.0 as isize;
            }
        }
        h = match GetWindow(h, GW_HWNDNEXT) {
            Ok(n) => n,
            Err(_) => return 0,
        };
    }
    0
}

/// 在洞里找一个采样点（屏幕坐标），用于判断洞有没有被别的窗口压住。
fn hole_sample(regions: &[Region], cx: i32, cy: i32, cw: i32, ch: i32) -> Option<(i32, i32)> {
    if cw <= 0 || ch <= 0 {
        return None;
    }
    // 先试每个选区包围盒中心（大多数情况一下就中）
    for r in regions {
        if !r.enabled || r.opacity == 0 {
            continue;
        }
        let (mut minx, mut miny, mut maxx, mut maxy) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for (x, y) in &r.pts {
            minx = minx.min(*x);
            miny = miny.min(*y);
            maxx = maxx.max(*x);
            maxy = maxy.max(*y);
        }
        let nx = (minx + maxx) * 0.5;
        let ny = (miny + maxy) * 0.5;
        if region_contains(r, nx, ny) {
            return Some((
                cx + (nx * cw as f32) as i32,
                cy + (ny * ch as f32) as i32,
            ));
        }
    }
    // 反向选区（洞在圈外）或细长洞：网格扫一遍
    for gy in 0..5 {
        for gx in 0..5 {
            let x = cx + cw / 10 + gx * cw / 5;
            let y = cy + ch / 10 + gy * ch / 5;
            let nx = (x - cx) as f32 / cw as f32;
            let ny = (y - cy) as f32 / ch as f32;
            if regions
                .iter()
                .any(|r| r.enabled && r.opacity > 0 && region_contains(r, nx, ny))
            {
                return Some((x, y));
            }
        }
    }
    None
}

/// 抓目标窗口客户区画面（PrintWindow，1:1）。失败返回 None，洞里退回"贴下层快照"的老画法。
/// 从屏幕截取客户区画面（窗口完好、可见时用这个，Chromium/KOOK 也能抓到）。
unsafe fn capture_screen_client(cx: i32, cy: i32, w: i32, h: i32) -> Option<Vec<u8>> {
    if w <= 0 || h <= 0 {
        return None;
    }
    let screen = GetDC(None);
    let mem = CreateCompatibleDC(screen);
    let mut bi: BITMAPINFO = std::mem::zeroed();
    bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    bi.bmiHeader.biWidth = w;
    bi.bmiHeader.biHeight = -h;
    bi.bmiHeader.biPlanes = 1;
    bi.bmiHeader.biBitCount = 32;
    bi.bmiHeader.biCompression = BI_RGB.0;
    let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
    let mut out = None;
    if let Ok(dib) = CreateDIBSection(mem, &bi, DIB_RGB_COLORS, &mut bits, None, 0) {
        let old = SelectObject(mem, HGDIOBJ(dib.0));
        if BitBlt(mem, 0, 0, w, h, screen, cx, cy, SRCCOPY).is_ok() {
            let buf = std::slice::from_raw_parts(bits as *const u8, (w * h * 4) as usize);
            let mut v = buf.to_vec();
            for px in v.chunks_exact_mut(4) {
                px[3] = 255;
            }
            out = Some(v);
        }
        SelectObject(mem, old);
        let _ = DeleteObject(HGDIOBJ(dib.0));
    }
    let _ = DeleteDC(mem);
    ReleaseDC(None, screen);
    out
}

unsafe fn capture_window_client(hwnd: HWND, w: i32, h: i32) -> Option<Vec<u8>> {
    if w <= 0 || h <= 0 {
        return None;
    }
    let screen = GetDC(None);
    let mem = CreateCompatibleDC(screen);
    let mut bi: BITMAPINFO = std::mem::zeroed();
    bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    bi.bmiHeader.biWidth = w;
    bi.bmiHeader.biHeight = -h;
    bi.bmiHeader.biPlanes = 1;
    bi.bmiHeader.biBitCount = 32;
    bi.bmiHeader.biCompression = BI_RGB.0;
    let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
    let mut out = None;
    if let Ok(dib) = CreateDIBSection(mem, &bi, DIB_RGB_COLORS, &mut bits, None, 0) {
        let old = SelectObject(mem, HGDIOBJ(dib.0));
        // PW_CLIENTONLY(1)：只画客户区，正好是客户区尺寸，1:1
        if windows::Win32::Storage::Xps::PrintWindow(
            hwnd,
            mem,
            windows::Win32::Storage::Xps::PRINT_WINDOW_FLAGS(1),
        )
        .as_bool()
        {
            let buf = std::slice::from_raw_parts(bits as *const u8, (w * h * 4) as usize);
            let mut v = buf.to_vec();
            for px in v.chunks_exact_mut(4) {
                px[3] = 255;
            }
            out = Some(v);
        }
        SelectObject(mem, old);
        let _ = DeleteObject(HGDIOBJ(dib.0));
    }
    let _ = DeleteDC(mem);
    ReleaseDC(None, screen);
    out
}

unsafe fn draw_visual(overlay: HWND) {
    let (origin, sw, sh, rect, snapshot, win_snap, win_w, win_h, regions) = VS.with(|v| {
        let st = v.borrow();
        (
            st.origin,
            st.w,
            st.h,
            st.rect,
            st.snapshot.clone(),
            st.win_snap.clone(),
            st.win_w,
            st.win_h,
            st.regions.clone(),
        )
    });
    let Some(snapshot) = snapshot else { return };
    let (rx, ry, w, h) = rect;
    if w <= 0 || h <= 0 || sw <= 0 || sh <= 0 {
        return;
    }
    let screen = GetDC(None);
    let mem = CreateCompatibleDC(screen);
    let mut bi: BITMAPINFO = std::mem::zeroed();
    bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    bi.bmiHeader.biWidth = w;
    bi.bmiHeader.biHeight = -h;
    bi.bmiHeader.biPlanes = 1;
    bi.bmiHeader.biBitCount = 32;
    bi.bmiHeader.biCompression = BI_RGB.0;
    let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
    let Ok(dib) = CreateDIBSection(mem, &bi, DIB_RGB_COLORS, &mut bits, None, 0) else {
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        return;
    };
    let old = SelectObject(mem, HGDIOBJ(dib.0));
    let buf = std::slice::from_raw_parts_mut(bits as *mut u8, (w * h * 4) as usize);

    for y in 0..h {
        let ny = (y as f32 + 0.5) / h as f32;
        let sy = ry + y - origin.1;
        let row = (y * w) as usize;
        for x in 0..w {
            let nx = (x as f32 + 0.5) / w as f32;
            let mut alpha = 0u32;
            for r in &regions {
                if r.opacity > 0 && region_contains(r, nx, ny) {
                    alpha = alpha.max(r.opacity as u32);
                }
            }
            let di = (row + x as usize) * 4;
            let sx = rx + x - origin.0;
            if alpha > 0 && sx >= 0 && sy >= 0 && sx < sw && sy < sh {
                // 下层最多压到 70%，保住至少 30% 的窗口画面：透明度拉满也是"半透明"，不会真消失
                let a = (alpha * 255 / 100) as u32;
                let si = ((sy as usize) * (sw as usize) + sx as usize) * 4;
                if let Some(win) = &win_snap {
                    // 窗口自己的画面按 (1-透明度) 与下层混合，结果完全不透明：
                    // 透明度作用在"窗口画的那块区域"上，而不是把下层图直接盖上去。
                    let (wx, wy) = if win_w == w && win_h == h {
                        (x, y)
                    } else {
                        (
                            (x as f32 * win_w as f32 / w as f32) as i32,
                            (y as f32 * win_h as f32 / h as f32) as i32,
                        )
                    };
                    let wi = ((wy.max(0) as usize) * (win_w.max(1) as usize) + wx.max(0) as usize) * 4;
                    let wb = win.get(wi).copied().unwrap_or(0) as u32;
                    let wg = win.get(wi + 1).copied().unwrap_or(0) as u32;
                    let wr = win.get(wi + 2).copied().unwrap_or(0) as u32;
                    let lb = snapshot[si] as u32;
                    let lg = snapshot[si + 1] as u32;
                    let lr = snapshot[si + 2] as u32;
                    buf[di] = ((wb * (255 - a) + lb * a) / 255) as u8;
                    buf[di + 1] = ((wg * (255 - a) + lg * a) / 255) as u8;
                    buf[di + 2] = ((wr * (255 - a) + lr * a) / 255) as u8;
                    buf[di + 3] = 255;
                } else {
                    // 抓不到窗口画面时退回老行为：把下层快照按 alpha 贴上去
                    buf[di] = ((snapshot[si] as u32 * a) / 255) as u8;
                    buf[di + 1] = ((snapshot[si + 1] as u32 * a) / 255) as u8;
                    buf[di + 2] = ((snapshot[si + 2] as u32 * a) / 255) as u8;
                    buf[di + 3] = a as u8;
                }
            } else {
                buf[di] = 0;
                buf[di + 1] = 0;
                buf[di + 2] = 0;
                buf[di + 3] = 0;
            }
        }
    }

    let pos = POINT { x: rect.0, y: rect.1 };
    let size = SIZE { cx: w, cy: h };
    let src = POINT { x: 0, y: 0 };
    let blend = BLENDFUNCTION {
        BlendOp: 0,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: 1,
    };
    let _ = UpdateLayeredWindow(
        overlay,
        screen,
        Some(&pos),
        Some(&size),
        mem,
        Some(&src),
        COLORREF(0),
        Some(&blend),
        ULW_ALPHA,
    );

    SelectObject(mem, old);
    let _ = DeleteObject(HGDIOBJ(dib.0));
    let _ = DeleteDC(mem);
    ReleaseDC(None, screen);
}

// ---------------------------------------------------------------------------
// 基础绘图
// ---------------------------------------------------------------------------

fn blend_px(buf: &mut [u8], w: i32, x: i32, y: i32, rgb: (u8, u8, u8), a: u8) {
    if x < 0 || y < 0 || x >= w {
        return;
    }
    let idx = ((y as usize) * (w as usize) + x as usize) * 4;
    if idx + 3 >= buf.len() {
        return;
    }
    let a32 = a as u32;
    buf[idx] = ((rgb.2 as u32 * a32) / 255) as u8;
    buf[idx + 1] = ((rgb.1 as u32 * a32) / 255) as u8;
    buf[idx + 2] = ((rgb.0 as u32 * a32) / 255) as u8;
    buf[idx + 3] = a;
}

fn fill_rect(
    buf: &mut [u8],
    w: i32,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    rgb: (u8, u8, u8),
    a: u8,
) {
    for y in y0..y1 {
        for x in x0..x1 {
            blend_px(buf, w, x, y, rgb, a);
        }
    }
}

fn fill_poly(buf: &mut [u8], w: i32, h: i32, pts: &[(i32, i32)], rgb: (u8, u8, u8), a: u8) {
    if pts.len() < 3 {
        return;
    }
    let min_y = pts.iter().map(|p| p.1).min().unwrap_or(0).max(0);
    let max_y = pts.iter().map(|p| p.1).max().unwrap_or(0).min(h - 1);
    let mut xs: Vec<i32> = Vec::new();
    for y in min_y..=max_y {
        xs.clear();
        let yf = y as f32 + 0.5;
        for i in 0..pts.len() {
            let (x0, y0) = pts[i];
            let (x1, y1) = pts[(i + 1) % pts.len()];
            if (y0 as f32 > yf) != (y1 as f32 > yf) {
                let t = (yf - y0 as f32) / (y1 - y0) as f32;
                xs.push((x0 as f32 + t * (x1 - x0) as f32) as i32);
            }
        }
        if xs.len() < 2 {
            continue;
        }
        xs.sort_unstable();
        for pair in xs.chunks_exact(2) {
            let (xa, xb) = (pair[0].max(0), pair[1].min(w - 1));
            for x in xa..=xb {
                blend_px(buf, w, x, y, rgb, a);
            }
        }
    }
}

fn stroke_poly(buf: &mut [u8], w: i32, h: i32, pts: &[(i32, i32)], rgb: (u8, u8, u8), t: i32) {
    if pts.len() < 2 {
        return;
    }
    for i in 0..pts.len() {
        line(buf, w, h, pts[i], pts[(i + 1) % pts.len()], rgb, t);
    }
}

fn line(buf: &mut [u8], w: i32, h: i32, a: (i32, i32), b: (i32, i32), rgb: (u8, u8, u8), t: i32) {
    let (mut x0, mut y0) = a;
    let (x1, y1) = b;
    let dx = (x1 - x0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let dy = -(y1 - y0).abs();
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    loop {
        for oy in 0..t {
            for ox in 0..t {
                let px = x0 + ox - t / 2;
                let py = y0 + oy - t / 2;
                if px >= 0 && py >= 0 && px < w && py < h {
                    blend_px(buf, w, px, py, rgb, 255);
                }
            }
        }
        if x0 == x1 && y0 == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x0 += sx;
        }
        if e2 <= dx {
            err += dx;
            y0 += sy;
        }
    }
}

unsafe fn create_font(height: i32, weight: i32) -> HFONT {
    CreateFontW(
        height,
        0,
        0,
        0,
        weight,
        0,
        0,
        0,
        1,
        0,
        0,
        // 小字号 + 抗锯齿会把白字糊成半透明绿块（GDI AA 跟背景混合后再补 alpha）；
        // 工具栏用非抗锯齿，笔画实心、白字清晰
        3,
        0,
        windows::core::w!("Microsoft YaHei"),
    )
}

unsafe fn draw_text(
    hdc: HDC,
    text: &str,
    rc: &mut RECT,
    color: (u8, u8, u8),
    font: HFONT,
    flags: DRAW_TEXT_FORMAT,
) {
    let _ = SelectObject(hdc, HGDIOBJ(font.0));
    let _ = SetBkMode(hdc, TRANSPARENT);
    let _ = SetTextColor(
        hdc,
        COLORREF((color.0 as u32) | ((color.1 as u32) << 8) | ((color.2 as u32) << 16)),
    );
    let mut w: Vec<u16> = text.encode_utf16().collect();
    let _ = DrawTextW(hdc, &mut w, rc, flags);
}
