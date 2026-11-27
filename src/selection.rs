//! 「选择后的可视化」：给当前控制目标画一圈选择框 + 软件名标签。
//!
//! 全屏置顶、点击穿透、不抢焦点的分层窗口；每 150ms 跟随 TARGET_HWND。
//! 目标最小化/隐藏/关闭时自动隐藏。选择框只是视觉提示，不吃输入。

use std::cell::RefCell;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Once;
use std::thread;

use windows::core::w;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

static INIT: Once = Once::new();
static SEL_HWND: AtomicIsize = AtomicIsize::new(0);

thread_local! {
    static LAST: RefCell<Option<(isize, (i32, i32, i32, i32), String)>> = RefCell::new(None);
}

pub fn init() {
    INIT.call_once(|| {
        let _ = thread::Builder::new()
            .name("tg-selection".into())
            .spawn(|| unsafe { selection_thread() });
    });
}

unsafe fn selection_thread() {
    let hinst = match GetModuleHandleW(None) {
        Ok(h) => h,
        Err(_) => return,
    };
    let class_w: Vec<u16> = "TransGlassSelectionFrame"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let sw = GetSystemMetrics(SM_CXSCREEN);
    let sh = GetSystemMetrics(SM_CYSCREEN);
    if sw <= 0 || sh <= 0 {
        return;
    }
    let wc = WNDCLASSW {
        lpfnWndProc: Some(selection_proc),
        hInstance: HINSTANCE(hinst.0),
        lpszClassName: windows::core::PCWSTR(class_w.as_ptr()),
        ..Default::default()
    };
    let _ = RegisterClassW(&wc);
    let hwnd = match CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
        windows::core::PCWSTR(class_w.as_ptr()),
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
    SEL_HWND.store(hwnd.0 as isize, Ordering::Relaxed);
    SetTimer(hwnd, 1, 150, None);

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
    let app = crate::cached_app_name(th);
    let key = (target, (rc.left, rc.top, rc.right, rc.bottom), app.clone());
    let need = LAST.with(|l| l.borrow().as_ref() != Some(&key))
        || !IsWindowVisible(overlay).as_bool();
    if !need {
        return;
    }
    if !draw(overlay, rc, &app) {
        hide(overlay);
        return;
    }
    LAST.with(|l| *l.borrow_mut() = Some(key));
}

unsafe fn hide(overlay: HWND) {
    if IsWindowVisible(overlay).as_bool() {
        let _ = ShowWindow(overlay, SW_HIDE);
    }
    LAST.with(|l| *l.borrow_mut() = None);
}

unsafe fn draw(overlay: HWND, target: RECT, app: &str) -> bool {
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

    // 2px 绿色外框
    let accent = (47u8, 194u8, 124u8);
    let t = 2i32;
    fill_rect(buf, sw, x0, y0, x1, y0 + t, accent, 255);
    fill_rect(buf, sw, x0, y1 - t, x1, y1, accent, 255);
    fill_rect(buf, sw, x0, y0, x0 + t, y1, accent, 255);
    fill_rect(buf, sw, x1 - t, y0, x1, y1, accent, 255);

    // 左上角标签：已选中 · 软件名
    let label = if app.trim().is_empty() {
        "已选中窗口".to_string()
    } else {
        let short: String = app.chars().take(16).collect();
        format!("已选中 · {short}")
    };
    let lw = (label.chars().count() as i32 * 13 + 22).min(300);
    let lh = 24i32;
    let lx = x0.max(0);
    let ly = if y0 >= lh + 8 { y0 - lh - 6 } else { y0 + 6 };
    let lx = lx.min(sw - lw);
    if lx >= 0 && ly >= 0 && ly + lh <= sh {
        fill_rect(buf, sw, lx, ly, lx + lw, ly + lh, (22, 28, 24), 245);
        fill_rect(buf, sw, lx, ly, lx + lw, ly + 2, accent, 255);
        let font = create_font(12, 700);
        let mut rc = RECT {
            left: lx + 8,
            top: ly + 2,
            right: lx + lw - 6,
            bottom: ly + lh,
        };
        draw_text(mem, &label, &mut rc, (255, 255, 255), font, DT_LEFT | DT_SINGLELINE | DT_VCENTER);
        let _ = SelectObject(mem, GetStockObject(DEFAULT_GUI_FONT));
        let _ = DeleteObject(HGDIOBJ(font.0));
    }

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
    true
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
