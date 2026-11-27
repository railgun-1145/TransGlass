//! 自由形状点透选区（「点透洞」）。
//!
//! 语义：窗口整体仍然可操作，只有用户画出来的多边形区域把点击/拖动放给下面的窗口/桌面。
//! 实现：不裁剪窗口（`SetWindowRgn` 会把画面也挖掉），而是在低级鼠标钩子里按坐标
//! 临时给目标窗口加 `WS_EX_TRANSPARENT`——鼠标一进洞，窗口就整块让出命中测试，
//! 点击自然落到下层；鼠标一出洞立刻恢复。区域按窗口客户区归一化坐标保存，跟随缩放。
//!
//! 编辑：全屏/客户区大小的分层窗口套索编辑器（独立线程 + GDI），
//! 拖动画多边形 → Enter 保存、Backspace 撤销、Esc 取消。

use std::cell::RefCell;
use std::sync::atomic::{AtomicIsize, Ordering};
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

/// (hwnd, 归一化多边形列表)；归一化相对客户区，0..1
static REGIONS: Mutex<Vec<(isize, Vec<Vec<(f32, f32)>>)>> = Mutex::new(Vec::new());
/// 当前因为鼠标在洞里而被临时加了 WS_EX_TRANSPARENT 的窗口
static HOLE_STYLES: Mutex<Vec<isize>> = Mutex::new(Vec::new());

static INIT: Once = Once::new();
static EDITOR_HWND: AtomicIsize = AtomicIsize::new(0);
static PENDING: Mutex<Option<isize>> = Mutex::new(None);

pub fn init() {
    INIT.call_once(|| {
        let _ = thread::Builder::new()
            .name("tg-regions".into())
            .spawn(|| unsafe { editor_thread() });
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
}

pub fn set_regions(hwnd: isize, regions: Vec<Vec<(f32, f32)>>) {
    if let Ok(mut v) = REGIONS.lock() {
        v.retain(|(h, _)| *h != hwnd);
        if !regions.is_empty() {
            v.push((hwnd, regions));
        }
    }
    clear_hole_style(hwnd);
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

/// 鼠标钩子每条鼠标消息都会调：按当前坐标和窗口/多边形，维护临时 WS_EX_TRANSPARENT。
pub fn enforce_at(x: i32, y: i32) {
    let has_any = REGIONS.lock().map(|v| !v.is_empty()).unwrap_or(false);
    if !has_any {
        return;
    }
    let Ok(map) = REGIONS.lock() else { return };
    let Ok(mut styles) = HOLE_STYLES.lock() else { return };

    for (hwnd_val, polys) in map.iter() {
        let hwnd = HWND(*hwnd_val as *mut _);
        let tracked = styles.contains(hwnd_val);

        // 点透会话正在操作的窗口 / 整窗点透的窗口：样式由别的逻辑负责，别抢
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
        let inside = inside_rect
            && {
                let nx = (x - cx) as f32 / w as f32;
                let ny = (y - cy) as f32 / h as f32;
                polys.iter().any(|p| point_in_poly(p, nx, ny))
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
// 编辑器线程 / 窗口
// ---------------------------------------------------------------------------

#[derive(Default)]
struct EditState {
    active: bool,
    hwnd: isize,
    screen: (i32, i32, i32, i32),
    regions: Vec<Vec<(f32, f32)>>,
    drawing: Vec<(f32, f32)>,
    drawing_px: Vec<(i32, i32)>,
    was_minimized: bool,
    prev_above: isize,
    topmost: bool,
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

    // 把目标提到同层级最上，免得被别的普通窗口挡住；结束再插回去
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
            drawing: Vec::new(),
            drawing_px: Vec::new(),
            was_minimized,
            prev_above,
            topmost,
        };
    });
    // 先把分层窗口的尺寸/位置定下来，再 UpdateLayeredWindow。
    // 窗口如果还是 0×0，UpdateLayeredWindow 会返回 0x8007001F（真实踩过）。
    let _ = SetWindowPos(
        overlay,
        HWND_TOPMOST,
        cx,
        cy,
        w,
        h,
        SWP_NOACTIVATE,
    );
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

// ---------------------------------------------------------------------------
// 编辑输入
// ---------------------------------------------------------------------------

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
            let poly = st.drawing.drain(..).collect::<Vec<_>>();
            st.drawing_px.clear();
            st.regions.push(poly);
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
                } else if !st.regions.is_empty() {
                    st.regions.pop();
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

// ---------------------------------------------------------------------------
// 绘制
// ---------------------------------------------------------------------------

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

    // 压暗客户区，让选区线条看得清
    for px in buf.chunks_exact_mut(4) {
        px[0] = 0;
        px[1] = 0;
        px[2] = 0;
        px[3] = 100;
    }

    let (regions, drawing, n) = ES.with(|e| {
        let st = e.borrow();
        (st.regions.clone(), st.drawing.clone(), st.regions.len())
    });

    // 已有区域：绿色半透明填充 + 2px 边框
    for poly in &regions {
        let pts: Vec<(i32, i32)> = poly
            .iter()
            .map(|(x, y)| ((x * w as f32) as i32, (y * h as f32) as i32))
            .collect();
        fill_poly(buf, w, h, &pts, (47, 194, 124), 38);
        stroke_poly(buf, w, h, &pts, (47, 194, 124), 2);
    }
    // 正在画的那一条：橙色开放折线
    if !drawing.is_empty() {
        let pts: Vec<(i32, i32)> = drawing
            .iter()
            .map(|(x, y)| ((x * w as f32) as i32, (y * h as f32) as i32))
            .collect();
        stroke_open(buf, w, h, &pts, (255, 140, 70), 2);
    }

    // 顶部提示条（不透明，保证文字 alpha 正确）
    fill_rect(buf, w, 0, 0, w, 42, (255, 255, 255), 248);
    fill_rect(buf, w, 0, 40, w, 42, (226, 230, 235), 255);

    let font = create_font(14, 700);
    let font_small = create_font(11, 400);
    let mut rc = RECT {
        left: 14,
        top: 7,
        right: w - 180,
        bottom: 34,
    };
    draw_text(
        mem,
        "拖动画出点透区域 · Enter 保存 · Backspace 撤销 · Esc 取消",
        &mut rc,
        (30, 30, 30),
        font,
        DT_LEFT | DT_SINGLELINE | DT_VCENTER,
    );
    let mut rc = RECT {
        left: w - 170,
        top: 9,
        right: w - 14,
        bottom: 34,
    };
    draw_text(
        mem,
        &format!("已画 {n} 个区域"),
        &mut rc,
        (110, 115, 120),
        font_small,
        DT_RIGHT | DT_SINGLELINE | DT_VCENTER,
    );

    // GDI 不写 alpha；提示条区域是不透明的，补成 255
    for y in 0..42.min(h) {
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

    // 先 Update 再取消 DIB 选中；取消早了 mem DC 里就不是这张 DIB，Update 会 0x8007001F
    SelectObject(mem, old);
    let _ = DeleteObject(HGDIOBJ(dib.0));
    let _ = DeleteDC(mem);
    ReleaseDC(None, screen);
}

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
