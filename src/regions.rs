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
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
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
}

static REGIONS: Mutex<Vec<(isize, Vec<Region>)>> = Mutex::new(Vec::new());
static HOLE_STYLES: Mutex<Vec<isize>> = Mutex::new(Vec::new());

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

pub fn open_editor(hwnd: isize) {
    if hwnd == 0 {
        return;
    }
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
    VISUAL_HIDE_REQ.store(true, Ordering::Relaxed);
    let h = VISUAL_HWND.load(Ordering::Relaxed);
    if h != 0 {
        unsafe {
            let _ = PostMessageW(HWND(h as *mut _), WM_VISUAL_HIDE, WPARAM(0), LPARAM(0));
        }
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

pub fn set_regions(hwnd: isize, regions: Vec<Region>) {
    let any_visual = regions.iter().any(|r| r.opacity > 0);
    let empty = regions.is_empty();
    if let Ok(mut v) = REGIONS.lock() {
        v.retain(|(h, _)| *h != hwnd);
        if !regions.is_empty() {
            v.push((hwnd, regions));
        }
    }
    clear_hole_style(hwnd);
    if empty || !any_visual {
        hide_visual();
    } else {
        refresh_visual(hwnd);
    }
}

pub fn snapshot_all() -> Vec<(isize, Vec<Region>)> {
    REGIONS.lock().map(|v| v.clone()).unwrap_or_default()
}

pub fn update_region(hwnd: isize, idx: usize, invert: Option<bool>, opacity: Option<u8>) {
    let mut any_visual = false;
    if let Ok(mut v) = REGIONS.lock() {
        if let Some((_, regions)) = v.iter_mut().find(|(h, _)| *h == hwnd) {
            if let Some(r) = regions.get_mut(idx) {
                if let Some(i) = invert {
                    r.invert = i;
                }
                if let Some(o) = opacity {
                    r.opacity = o.min(100);
                }
            }
            any_visual = regions.iter().any(|r| r.opacity > 0);
        }
    }
    clear_hole_style(hwnd);
    if any_visual {
        refresh_visual_mask();
    } else {
        hide_visual();
    }
}

pub fn remove_region(hwnd: isize, idx: usize) {
    let mut empty = false;
    let mut any_visual = false;
    if let Ok(mut v) = REGIONS.lock() {
        if let Some((_, regions)) = v.iter_mut().find(|(h, _)| *h == hwnd) {
            if idx < regions.len() {
                regions.remove(idx);
            }
            empty = regions.is_empty();
            any_visual = regions.iter().any(|r| r.opacity > 0);
        }
        if empty {
            v.retain(|(h, _)| *h != hwnd);
        }
    }
    clear_hole_style(hwnd);
    if empty || !any_visual {
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
    if hit && !crate::window_passthrough_active(hwnd) {
        unsafe {
            crate::set_hwnd_mouse_transparent(HWND(hwnd as *mut _), false);
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

    for (hwnd_val, regions) in map.iter() {
        let hwnd = HWND(*hwnd_val as *mut _);
        let tracked = styles.contains(hwnd_val);

        if crate::selector::is_session_window(*hwnd_val)
            || crate::window_passthrough_active(*hwnd_val)
        {
            if tracked {
                styles.retain(|h| h != hwnd_val);
            }
            continue;
        }

        let valid = unsafe {
            IsWindow(hwnd).as_bool() && IsWindowVisible(hwnd).as_bool() && !IsIconic(hwnd).as_bool()
        };
        if !valid {
            if tracked {
                styles.retain(|h| h != hwnd_val);
                unsafe {
                    crate::set_hwnd_mouse_transparent(hwnd, false);
                }
            }
            continue;
        }

        let Some((cx, cy, w, h)) = (unsafe { client_screen_rect(hwnd) }) else {
            if tracked {
                styles.retain(|h| h != hwnd_val);
                unsafe {
                    crate::set_hwnd_mouse_transparent(hwnd, false);
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
                unsafe {
                    crate::set_hwnd_mouse_transparent(hwnd, true);
                }
                styles.push(*hwnd_val);
            }
        } else if tracked {
            styles.retain(|h| h != hwnd_val);
            unsafe {
                crate::set_hwnd_mouse_transparent(hwnd, false);
            }
        }
    }
}

fn region_contains(r: &Region, x: f32, y: f32) -> bool {
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

struct EditState {
    active: bool,
    hwnd: isize,
    screen: (i32, i32, i32, i32),
    regions: Vec<Region>,
    selected: Option<usize>,
    drawing: Vec<(f32, f32)>,
    drawing_px: Vec<(i32, i32)>,
    was_minimized: bool,
    prev_above: isize,
    topmost: bool,
}

impl Default for EditState {
    fn default() -> Self {
        Self {
            active: false,
            hwnd: 0,
            screen: (0, 0, 0, 0),
            regions: Vec::new(),
            selected: None,
            drawing: Vec::new(),
            drawing_px: Vec::new(),
            was_minimized: false,
            prev_above: 0,
            topmost: false,
        }
    }
}

thread_local! {
    static ES: RefCell<EditState> = RefCell::new(EditState::default());
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
    let selected = if regions.is_empty() {
        None
    } else {
        Some(regions.len() - 1)
    };

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
            selected,
            drawing: Vec::new(),
            drawing_px: Vec::new(),
            was_minimized,
            prev_above,
            topmost,
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
        set_regions(state.hwnd, state.regions.clone());
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
    let _ = SetCapture(HWND(EDITOR_HWND.load(Ordering::Relaxed) as *mut _));
    ES.with(|e| {
        let mut st = e.borrow_mut();
        st.drawing.clear();
        st.drawing_px.clear();
        st.drawing.push((x as f32 / w as f32, y as f32 / h as f32));
        st.drawing_px.push((x, y));
    });
    redraw();
}

unsafe fn on_move(l: LPARAM) {
    let (x, y) = mouse_xy(l);
    let added = ES.with(|e| {
        let mut st = e.borrow_mut();
        if st.drawing_px.is_empty() {
            return false;
        }
        if let Some((lx, ly)) = st.drawing_px.last() {
            if (x - lx).abs() + (y - ly).abs() < 3 {
                return false;
            }
        }
        let (w, h) = (st.screen.2, st.screen.3);
        if w <= 0 || h <= 0 {
            return false;
        }
        st.drawing.push((x as f32 / w as f32, y as f32 / h as f32));
        st.drawing_px.push((x, y));
        true
    });
    if added {
        redraw();
    }
}

unsafe fn on_up() {
    let added = ES.with(|e| {
        let mut st = e.borrow_mut();
        if st.drawing_px.len() < 3 {
            st.drawing.clear();
            st.drawing_px.clear();
            false
        } else {
            let pts = st.drawing.drain(..).collect::<Vec<_>>();
            st.drawing_px.clear();
            st.regions.push(Region {
                pts,
                invert: false,
                opacity: 100,
            });
            st.selected = Some(st.regions.len() - 1);
            true
        }
    });
    let _ = ReleaseCapture();
    if added {
        redraw();
    }
}

unsafe fn on_key(vk: u32) {
    let overlay = HWND(EDITOR_HWND.load(Ordering::Relaxed) as *mut _);
    match vk {
        v if v == VK_RETURN.0 as u32 => {
            if ES.with(|e| e.borrow().drawing_px.len() >= 3) {
                on_up();
            }
            end_edit(overlay, true);
        }
        v if v == VK_ESCAPE.0 as u32 => {
            end_edit(overlay, false);
        }
        v if v == VK_BACK.0 as u32 => {
            let changed = ES.with(|e| {
                let mut st = e.borrow_mut();
                if !st.drawing_px.is_empty() {
                    st.drawing.clear();
                    st.drawing_px.clear();
                    true
                } else {
                    remove_selected(&mut st)
                }
            });
            if changed {
                redraw();
            }
        }
        v if v == VK_DELETE.0 as u32 => {
            let changed = ES.with(|e| remove_selected(&mut e.borrow_mut()));
            if changed {
                redraw();
            }
        }
        v if v == VK_TAB.0 as u32 => {
            let changed = ES.with(|e| {
                let mut st = e.borrow_mut();
                if st.regions.is_empty() {
                    return false;
                }
                let next = match st.selected {
                    Some(i) => (i + 1) % st.regions.len(),
                    None => 0,
                };
                st.selected = Some(next);
                true
            });
            if changed {
                redraw();
            }
        }
        v if v == VK_I.0 as u32 => {
            let changed = ES.with(|e| {
                let mut st = e.borrow_mut();
                let Some(i) = st.selected else { return false };
                if let Some(r) = st.regions.get_mut(i) {
                    r.invert = !r.invert;
                    true
                } else {
                    false
                }
            });
            if changed {
                redraw();
            }
        }
        v if v == VK_OEM_4.0 as u32 || v == VK_OEM_6.0 as u32 => {
            let delta: i32 = if v == VK_OEM_4.0 as u32 { -10 } else { 10 };
            let changed = ES.with(|e| {
                let mut st = e.borrow_mut();
                let Some(i) = st.selected else { return false };
                if let Some(r) = st.regions.get_mut(i) {
                    let next = (r.opacity as i32 + delta).clamp(0, 100);
                    r.opacity = next as u8;
                    true
                } else {
                    false
                }
            });
            if changed {
                redraw();
            }
        }
        _ => {}
    }
}

fn remove_selected(st: &mut EditState) -> bool {
    let Some(i) = st.selected else { return false };
    if i >= st.regions.len() {
        return false;
    }
    st.regions.remove(i);
    st.selected = if st.regions.is_empty() {
        None
    } else {
        Some(i.min(st.regions.len() - 1))
    };
    true
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

    for px in buf.chunks_exact_mut(4) {
        px[0] = 0;
        px[1] = 0;
        px[2] = 0;
        px[3] = 100;
    }

    let (regions, drawing, selected) = ES.with(|e| {
        let st = e.borrow();
        (st.regions.clone(), st.drawing.clone(), st.selected)
    });

    for (i, r) in regions.iter().enumerate() {
        let pts: Vec<(i32, i32)> = r
            .pts
            .iter()
            .map(|(x, y)| ((x * w as f32) as i32, (y * h as f32) as i32))
            .collect();
        let (rgb, fill) = if r.invert {
            ((76, 126, 220), 34)
        } else {
            ((47, 194, 124), 42)
        };
        let fa = ((fill as u32 * (10 + r.opacity as u32)) / 110) as u8;
        fill_poly(buf, w, h, &pts, rgb, fa.max(8));
        stroke_poly(buf, w, h, &pts, rgb, 2);
        if selected == Some(i) {
            stroke_poly(buf, w, h, &pts, (235, 180, 60), 3);
        }
    }
    if !drawing.is_empty() {
        let pts: Vec<(i32, i32)> = drawing
            .iter()
            .map(|(x, y)| ((x * w as f32) as i32, (y * h as f32) as i32))
            .collect();
        stroke_open(buf, w, h, &pts, (255, 140, 70), 2);
    }

    // 顶部提示条（两行）
    fill_rect(buf, w, 0, 0, w, 56, (255, 255, 255), 248);
    fill_rect(buf, w, 0, 54, w, 56, (226, 230, 235), 255);

    let font = create_font(13, 700);
    let font_small = create_font(11, 400);
    let mut rc = RECT {
        left: 14,
        top: 4,
        right: w - 14,
        bottom: 28,
    };
    draw_text(
        mem,
        "拖动画选区 · Tab 选择 · I 反向 · [ ] 透明度 · Enter 保存 · Backspace/Delete 删 · Esc 取消",
        &mut rc,
        (30, 30, 30),
        font,
        DT_LEFT | DT_SINGLELINE | DT_VCENTER,
    );
    let info = if let Some(i) = selected {
        let r = &regions[i];
        format!(
            "第 {}/{} 个 · {} · 透明度 {}%",
            i + 1,
            regions.len(),
            if r.invert { "反向（圈内保留、圈外穿透）" } else { "洞（圈内穿透）" },
            r.opacity
        )
    } else if regions.is_empty() {
        "还没有选区，拖动画一圈".to_string()
    } else {
        format!("共 {} 个选区 · Tab 选择一个", regions.len())
    };
    let mut rc = RECT {
        left: 14,
        top: 28,
        right: w - 14,
        bottom: 52,
    };
    draw_text(
        mem,
        &info,
        &mut rc,
        (105, 110, 116),
        font_small,
        DT_LEFT | DT_SINGLELINE | DT_VCENTER,
    );

    for y in 0..56.min(h) {
        let row = (y * w) as usize;
        for x in 0..w {
            buf[(row + x as usize) * 4 + 3] = 255;
        }
    }

    let _ = SelectObject(mem, GetStockObject(DEFAULT_GUI_FONT));
    let _ = DeleteObject(HGDIOBJ(font.0));
    let _ = DeleteObject(HGDIOBJ(font_small.0));

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
    snapshot: Option<Vec<u8>>,
    w: i32,
    h: i32,
    rect: (i32, i32, i32, i32),
    regions: Vec<Region>,
}

impl Default for VisualState {
    fn default() -> Self {
        Self {
            target: 0,
            snapshot: None,
            w: 0,
            h: 0,
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
    SetTimer(hwnd, 1, 200, None);

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
    if regions.iter().all(|r| r.opacity == 0) || regions.is_empty() {
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

    let _ = ShowWindow(overlay, SW_HIDE);
    let _ = ShowWindow(hwnd, SW_HIDE);
    thread::sleep(std::time::Duration::from_millis(80));

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
    let dib = CreateDIBSection(mem, &bi, DIB_RGB_COLORS, &mut bits, None, 0);

    let mut snapshot = None;
    if let Ok(dib) = dib {
        let old = SelectObject(mem, HGDIOBJ(dib.0));
        if BitBlt(mem, 0, 0, w, h, screen, cx, cy, SRCCOPY).is_ok() {
            let buf = std::slice::from_raw_parts(bits as *const u8, (w * h * 4) as usize);
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

    let Some(snapshot) = snapshot else {
        let _ = ShowWindow(overlay, SW_HIDE);
        return;
    };
    VS.with(|v| {
        *v.borrow_mut() = VisualState {
            target,
            snapshot: Some(snapshot),
            w,
            h,
            rect: (cx, cy, w, h),
            regions,
        };
    });
    draw_visual(overlay);
    follow_visual(overlay);
}

unsafe fn follow_visual(overlay: HWND) {
    let (target, rect, w, h, has_snapshot) = VS.with(|v| {
        let st = v.borrow();
        (st.target, st.rect, st.w, st.h, st.snapshot.is_some())
    });
    if target == 0 || !has_snapshot {
        let _ = ShowWindow(overlay, SW_HIDE);
        return;
    }
    let hwnd = HWND(target as *mut _);
    if !IsWindow(hwnd).as_bool()
        || !IsWindowVisible(hwnd).as_bool()
        || IsIconic(hwnd).as_bool()
        || GetForegroundWindow() != hwnd
    {
        let _ = ShowWindow(overlay, SW_HIDE);
        return;
    }
    let Some((cx, cy, cw, ch)) = client_screen_rect(hwnd) else {
        let _ = ShowWindow(overlay, SW_HIDE);
        return;
    };
    if (rect.0, rect.1, rect.2, rect.3) != (cx, cy, cw, ch) || w != cw || h != ch {
        // 窗口动了/缩放了，快照位置已不对；等用户刷新
        let _ = ShowWindow(overlay, SW_HIDE);
        return;
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

unsafe fn draw_visual(overlay: HWND) {
    let (w, h, rect, snapshot, regions) = VS.with(|v| {
        let st = v.borrow();
        (st.w, st.h, st.rect, st.snapshot.clone(), st.regions.clone())
    });
    let Some(snapshot) = snapshot else { return };
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

    for y in 0..h {
        let ny = (y as f32 + 0.5) / h as f32;
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
            if alpha > 0 {
                let a = (alpha * 255 / 100) as u32;
                let si = (row + x as usize) * 4;
                buf[di] = ((snapshot[si] as u32 * a) / 255) as u8;
                buf[di + 1] = ((snapshot[si + 1] as u32 * a) / 255) as u8;
                buf[di + 2] = ((snapshot[si + 2] as u32 * a) / 255) as u8;
                buf[di + 3] = a as u8;
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

fn stroke_open(buf: &mut [u8], w: i32, h: i32, pts: &[(i32, i32)], rgb: (u8, u8, u8), t: i32) {
    for pair in pts.windows(2) {
        line(buf, w, h, pair[0], pair[1], rgb, t);
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
        4,
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
