//! 「选择后的可视化」：当前控制目标外圈绿框 + 可点击关闭的「已选中 · 标记」标签。
//!
//! 全屏置顶、点击穿透的分层窗口画 2px 选择框；标签是一个独立的小置顶窗口
//! （可点击，点一下就取消选择框）。目标关闭/最小化/隐藏时自动隐藏。
//! 每个窗口可以设置一个标记（文字 + 颜色），标签和选择框使用这个颜色。

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Mutex, Once};
use std::thread;

use windows::core::w;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

static INIT: Once = Once::new();
static SEL_HWND: AtomicIsize = AtomicIsize::new(0);
static LABEL_HWND: AtomicIsize = AtomicIsize::new(0);
static ENABLED: AtomicBool = AtomicBool::new(true);

/// (hwnd, 标记文字, 颜色)
static MARKS: Mutex<Vec<(isize, String, (u8, u8, u8))>> = Mutex::new(Vec::new());
const DEFAULT_COLOR: (u8, u8, u8) = (47, 194, 124);

thread_local! {
    static LAST: RefCell<Option<(isize, (i32, i32, i32, i32), String, (u8, u8, u8))>> = RefCell::new(None);
    static LABEL_STATE: RefCell<(String, (u8, u8, u8))> = RefCell::new((String::new(), DEFAULT_COLOR));
    static SHOW_UNTIL: RefCell<Option<std::time::Instant>> = RefCell::new(None);
}

pub fn init() {
    INIT.call_once(|| {
        let _ = thread::Builder::new()
            .name("tg-selection".into())
            .spawn(|| unsafe { selection_thread() });
    });
}

pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

pub fn mark_for(hwnd: isize) -> Option<(String, (u8, u8, u8))> {
    MARKS
        .lock()
        .ok()
        .and_then(|v| v.iter().find(|(h, _, _)| *h == hwnd).map(|(_, t, c)| (t.clone(), *c)))
}

pub fn set_mark(hwnd: isize, tag: String, color: (u8, u8, u8)) {
    if let Ok(mut v) = MARKS.lock() {
        if let Some(e) = v.iter_mut().find(|(h, _, _)| *h == hwnd) {
            e.1 = tag;
            e.2 = color;
        } else {
            v.push((hwnd, tag, color));
        }
    }
    LAST.with(|l| *l.borrow_mut() = None);
}

unsafe fn selection_thread() {
    let hinst = match GetModuleHandleW(None) {
        Ok(h) => h,
        Err(_) => return,
    };
    let frame_class: Vec<u16> = "TransGlassSelectionFrame"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let wc = WNDCLASSW {
        lpfnWndProc: Some(selection_proc),
        hInstance: HINSTANCE(hinst.0),
        lpszClassName: windows::core::PCWSTR(frame_class.as_ptr()),
        ..Default::default()
    };
    let _ = RegisterClassW(&wc);
    let sw = GetSystemMetrics(SM_CXSCREEN);
    let sh = GetSystemMetrics(SM_CYSCREEN);
    if sw <= 0 || sh <= 0 {
        return;
    }
    let frame = match CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
        windows::core::PCWSTR(frame_class.as_ptr()),
        w!("TransGlassSelectionFrame"),
        WS_POPUP,
        0,
        0,
        sw,
        sh,
        None,
        None,
        HINSTANCE(hinst.0),
        None,
    ) {
        Ok(h) => h,
        Err(_) => return,
    };
    SEL_HWND.store(frame.0 as isize, Ordering::Relaxed);

    let label_class: Vec<u16> = "TransGlassSelectionLabel"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let wc2 = WNDCLASSW {
        lpfnWndProc: Some(label_proc),
        hInstance: HINSTANCE(hinst.0),
        lpszClassName: windows::core::PCWSTR(label_class.as_ptr()),
        ..Default::default()
    };
    let _ = RegisterClassW(&wc2);
    if let Ok(label) = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
        windows::core::PCWSTR(label_class.as_ptr()),
        w!("TransGlassSelectionLabel"),
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
        LABEL_HWND.store(label.0 as isize, Ordering::Relaxed);
    }

    SetTimer(frame, 1, 150, None);
    let mut msg = MSG::default();
    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
}

unsafe extern "system" fn selection_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        WM_TIMER => {
            update(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, w, l),
    }
}

unsafe fn update(overlay: HWND) {
    if !ENABLED.load(Ordering::Relaxed) {
        hide(overlay);
        return;
    }
    let target = crate::current_target();
    if target == 0 {
        hide(overlay);
        return;
    }
    let th = HWND(target as *mut _);
    if !IsWindow(th).as_bool() || !IsWindowVisible(th).as_bool() || IsIconic(th).as_bool() {
        hide(overlay);
        return;
    }
    let mut rc = RECT::default();
    if GetWindowRect(th, &mut rc).is_err() || rc.right <= rc.left || rc.bottom <= rc.top {
        hide(overlay);
        return;
    }
    let (tag, color) = match mark_for(target) {
        Some((t, c)) => (t, c),
        None => {
            let app = crate::cached_app_name(th);
            if app.trim().is_empty() {
                ("窗口".to_string(), DEFAULT_COLOR)
            } else {
                (app, DEFAULT_COLOR)
            }
        }
    };
    let key = (target, (rc.left, rc.top, rc.right, rc.bottom), tag.clone(), color);
    let changed = LAST.with(|l| l.borrow().as_ref() != Some(&key));
    if changed {
        if !draw(overlay, rc, &tag, color) {
            hide(overlay);
            return;
        }
        LAST.with(|l| *l.borrow_mut() = Some(key));
        SHOW_UNTIL.with(|s| {
            *s.borrow_mut() = Some(std::time::Instant::now() + std::time::Duration::from_secs(4))
        });
    } else {
        // 选择框是一次提示，4 秒后自动收起；目标/标记变化时再出现
        let expired = SHOW_UNTIL.with(|s| {
            s.borrow()
                .map(|u| std::time::Instant::now() > u)
                .unwrap_or(false)
        });
        if expired && IsWindowVisible(overlay).as_bool() {
            hide_only(overlay);
        }
    }
}

unsafe fn hide(overlay: HWND) {
    hide_only(overlay);
    LAST.with(|l| *l.borrow_mut() = None);
    SHOW_UNTIL.with(|s| *s.borrow_mut() = None);
}

unsafe fn hide_only(overlay: HWND) {
    if IsWindowVisible(overlay).as_bool() {
        let _ = ShowWindow(overlay, SW_HIDE);
    }
    let lh = LABEL_HWND.load(Ordering::Relaxed);
    if lh != 0 {
        let _ = ShowWindow(HWND(lh as *mut _), SW_HIDE);
    }
}

unsafe fn draw(overlay: HWND, target: RECT, tag: &str, color: (u8, u8, u8)) -> bool {
    let sw = GetSystemMetrics(SM_CXSCREEN);
    let sh = GetSystemMetrics(SM_CYSCREEN);
    if sw <= 0 || sh <= 0 {
        return false;
    }
    let x0 = target.left.clamp(0, sw - 1);
    let y0 = target.top.clamp(0, sh - 1);
    let x1 = target.right.clamp(0, sw);
    let y1 = target.bottom.clamp(0, sh);
    if x1 <= x0 || y1 <= y0 {
        return false;
    }

    let screen = GetDC(None);
    let mem = CreateCompatibleDC(screen);
    let mut bi: BITMAPINFO = std::mem::zeroed();
    bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    bi.bmiHeader.biWidth = sw;
    bi.bmiHeader.biHeight = -sh;
    bi.bmiHeader.biPlanes = 1;
    bi.bmiHeader.biBitCount = 32;
    bi.bmiHeader.biCompression = BI_RGB.0;
    let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
    let Ok(dib) = CreateDIBSection(mem, &bi, DIB_RGB_COLORS, &mut bits, None, 0) else {
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        return false;
    };
    let old = SelectObject(mem, HGDIOBJ(dib.0));
    let buf = std::slice::from_raw_parts_mut(bits as *mut u8, (sw * sh * 4) as usize);
    buf.fill(0);

    let t = 2i32;
    fill_rect(buf, sw, x0, y0, x1, y0 + t, color, 255);
    fill_rect(buf, sw, x0, y1 - t, x1, y1, color, 255);
    fill_rect(buf, sw, x0, y0, x0 + t, y1, color, 255);
    fill_rect(buf, sw, x1 - t, y0, x1, y1, color, 255);

    let pos = POINT { x: 0, y: 0 };
    let size = SIZE { cx: sw, cy: sh };
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

    let _ = ShowWindow(overlay, SW_SHOWNA);

    // 标签窗口：可点击关闭
    let full = format!("已选中 · {tag}");
    let short: String = full.chars().take(28).collect();
    let lw = (short.chars().count() as i32 * 13 + 40).clamp(120, 360);
    let lh = 26i32;
    let lx = x0.max(0).min(sw - lw);
    let ly = if y0 >= lh + 8 { y0 - lh - 6 } else { y0 + 6 };
    LABEL_STATE.with(|s| *s.borrow_mut() = (short, color));
    let lhwnd = LABEL_HWND.load(Ordering::Relaxed);
    if lhwnd != 0 {
        let label = HWND(lhwnd as *mut _);
        let _ = SetWindowPos(
            label,
            HWND_TOPMOST,
            lx,
            ly,
            lw,
            lh,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
        let _ = InvalidateRect(label, None, false);
        let _ = ShowWindow(label, SW_SHOWNA);
    }
    true
}

unsafe extern "system" fn label_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            let (text, color) = LABEL_STATE.with(|s| s.borrow().clone());
            let bg = CreateSolidBrush(COLORREF(0x00221E1C));
            FillRect(hdc, &rc, bg);
            let _ = DeleteObject(HGDIOBJ(bg.0));
            let bar = CreateSolidBrush(COLORREF(
                (color.0 as u32) | ((color.1 as u32) << 8) | ((color.2 as u32) << 16),
            ));
            let left = RECT { left: 0, top: 0, right: 4, bottom: rc.bottom };
            FillRect(hdc, &left, bar);
            let _ = DeleteObject(HGDIOBJ(bar.0));

            let font = CreateFontW(
                12,
                0,
                0,
                0,
                700,
                0,
                0,
                0,
                1,
                0,
                0,
                4,
                0,
                windows::core::w!("Microsoft YaHei"),
            );
            SelectObject(hdc, HGDIOBJ(font.0));
            SetBkMode(hdc, TRANSPARENT);
            SetTextColor(hdc, COLORREF(0x00FFFFFF));
            let mut t: Vec<u16> = text.encode_utf16().collect();
            let mut trc = RECT { left: 10, top: 0, right: rc.right - 22, bottom: rc.bottom };
            DrawTextW(hdc, &mut t, &mut trc, DT_LEFT | DT_SINGLELINE | DT_VCENTER);
            let mut x: Vec<u16> = "×".encode_utf16().collect();
            let mut xrc = RECT { left: rc.right - 22, top: 0, right: rc.right - 4, bottom: rc.bottom };
            DrawTextW(hdc, &mut x, &mut xrc, DT_CENTER | DT_SINGLELINE | DT_VCENTER);
            let _ = DeleteObject(HGDIOBJ(font.0));
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_LBUTTONUP => {
            ENABLED.store(false, Ordering::Relaxed);
            let _ = ShowWindow(hwnd, SW_HIDE);
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, w, l),
    }
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
    let a32 = a as u32;
    for y in y0.max(0)..y1.max(0) {
        for x in x0.max(0)..x1.max(0) {
            let i = ((y as usize) * (w as usize) + x as usize) * 4;
            if i + 3 >= buf.len() {
                continue;
            }
            buf[i] = ((rgb.2 as u32 * a32) / 255) as u8;
            buf[i + 1] = ((rgb.1 as u32 * a32) / 255) as u8;
            buf[i + 2] = ((rgb.0 as u32 * a32) / 255) as u8;
            buf[i + 3] = a;
        }
    }
}
