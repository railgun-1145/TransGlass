//! GPU 实时光洞 v2：每个矩形洞一个独立的置顶小窗口 + DWM 缩略图。
//!
//! 关键教训（v1 的坑）：DWM 缩略图**不认** `SetWindowRgn` 的窗口区域（Vista 起的老问题），
//! 给整窗贴缩略图会把整个窗口铺满。所以这里反过来：**小窗口 = 洞本身**，缩略图天然被窗口
//! 边界裁住，不会再"整窗透明"。
//!
//! 只支持矩形洞（编辑器画的矩形/椭圆的包围盒）；自由形状/椭圆继续走 CPU 快照回退。
//! 失败（缩略图注册失败、下层是桌面、找不到下层窗口）时 `using()` 为 false，调用方回退 CPU。

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::Mutex;
use windows::core::w;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

const WM_GPU_REQ: u32 = WM_APP + 0x700;
const WM_GPU_HIDE: u32 = WM_APP + 0x701;

static GPU_HWND: AtomicIsize = AtomicIsize::new(0);
static GPU_USING: AtomicBool = AtomicBool::new(false);

#[derive(Clone)]
struct Req {
    target: isize,
    /// 洞的屏幕矩形 (left, top, right, bottom)
    rect: (i32, i32, i32, i32),
    opacity: u8,
}

static REQ: Mutex<Option<Req>> = Mutex::new(None);

pub fn using() -> bool {
    GPU_USING.load(Ordering::Relaxed)
}

pub fn update(target: isize, rect: (i32, i32, i32, i32), opacity: u8) {
    if let Ok(mut r) = REQ.lock() {
        *r = Some(Req {
            target,
            rect,
            opacity,
        });
    }
    let h = GPU_HWND.load(Ordering::Relaxed);
    if h != 0 {
        unsafe {
            let _ = PostMessageW(HWND(h as *mut _), WM_GPU_REQ, WPARAM(0), LPARAM(0));
        }
    }
}

pub fn hide() {
    if let Ok(mut r) = REQ.lock() {
        *r = None;
    }
    GPU_USING.store(false, Ordering::Relaxed);
    let h = GPU_HWND.load(Ordering::Relaxed);
    if h != 0 {
        unsafe {
            let _ = PostMessageW(HWND(h as *mut _), WM_GPU_HIDE, WPARAM(0), LPARAM(0));
        }
    }
}

struct State {
    thumb: isize,
    source: isize,
    shown_opacity: u8,
}

static STATE: Mutex<State> = Mutex::new(State {
    thumb: 0,
    source: 0,
    shown_opacity: 0,
});

pub fn init() {
    std::thread::Builder::new()
        .name("tg-gpu-hole".into())
        .spawn(|| unsafe { thread_main() })
        .ok();
}

unsafe fn thread_main() {
    let hinst = match GetModuleHandleW(None) {
        Ok(h) => h,
        Err(_) => return,
    };
    let cls = w!("TransGlassGpuHole2");
    let wc = WNDCLASSW {
        style: WNDCLASS_STYLES(0),
        lpfnWndProc: Some(wndproc),
        hInstance: HINSTANCE(hinst.0),
        lpszClassName: cls,
        ..Default::default()
    };
    let _ = RegisterClassW(&wc);
    let hwnd = match CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT,
        cls,
        w!(""),
        WS_POPUP,
        0,
        0,
        0,
        0,
        HWND(std::ptr::null_mut()),
        HMENU(std::ptr::null_mut()),
        HINSTANCE(hinst.0),
        None,
    ) {
        Ok(h) => h,
        Err(_) => return,
    };
    GPU_HWND.store(hwnd.0 as isize, Ordering::Relaxed);
    let _ = SetTimer(hwnd, 1, 16, None);
    let mut msg = MSG::default();
    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        WM_ERASEBKGND => LRESULT(1),
        WM_PAINT => {
            let _ = BeginPaint(hwnd, &mut PAINTSTRUCT::default());
            let _ = EndPaint(hwnd, &PAINTSTRUCT::default());
            LRESULT(0)
        }
        WM_TIMER => {
            apply(hwnd);
            LRESULT(0)
        }
        WM_GPU_REQ => {
            apply(hwnd);
            LRESULT(0)
        }
        WM_GPU_HIDE => {
            hide_now(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, w, l),
    }
}

unsafe fn hide_now(hwnd: HWND) {
    GPU_USING.store(false, Ordering::Relaxed);
    if let Ok(mut st) = STATE.lock() {
        if st.thumb != 0 {
            let _ = DwmUnregisterThumbnail(st.thumb);
        }
        st.thumb = 0;
        st.source = 0;
    }
    let _ = ShowWindow(hwnd, SW_HIDE);
}

unsafe fn apply(hwnd: HWND) {
    let Some(req) = REQ.lock().ok().and_then(|mut r| r.take()) else {
        return;
    };
    let (l, t, r, b) = req.rect;
    if req.target == 0 || r <= l || b <= t || req.opacity == 0 {
        hide_now(hwnd);
        return;
    }
    let target = HWND(req.target as *mut _);
    if !IsWindow(target).as_bool() || !IsWindowVisible(target).as_bool() || IsIconic(target).as_bool() {
        hide_now(hwnd);
        return;
    }
    // 目标在这一层必须露着（中心点）；被盖住就藏
    let cx = (l + r) / 2;
    let cy = (t + b) / 2;
    if top_foreign_at(cx, cy) != req.target {
        hide_now(hwnd);
        return;
    }
    let source = lower_window_at(cx, cy, req.target);
    if source == 0 {
        hide_now(hwnd);
        return;
    }
    let mut st = match STATE.lock() {
        Ok(s) => s,
        Err(_) => return,
    };
    if st.thumb != 0 && st.source != source {
        let _ = DwmUnregisterThumbnail(st.thumb);
        st.thumb = 0;
    }
    if st.thumb == 0 {
        match DwmRegisterThumbnail(hwnd, HWND(source as *mut _)) {
            Ok(th) => {
                st.thumb = th;
                st.source = source;
                st.shown_opacity = 0;
            }
            Err(_) => {
                GPU_USING.store(false, Ordering::Relaxed);
                let _ = ShowWindow(hwnd, SW_HIDE);
                return;
            }
        }
    }
    // 源窗口屏幕矩形 -> 小窗口客户区坐标（洞的左上角为原点）
    let mut sr = RECT::default();
    if GetWindowRect(HWND(source as *mut _), &mut sr).is_err() {
        hide_now(hwnd);
        return;
    }
    let props = DWM_THUMBNAIL_PROPERTIES {
        dwFlags: DWM_TNP_RECTDESTINATION
            | DWM_TNP_OPACITY
            | DWM_TNP_VISIBLE
            | DWM_TNP_SOURCECLIENTAREAONLY,
        rcDestination: RECT {
            left: sr.left - l,
            top: sr.top - t,
            right: sr.right - l,
            bottom: sr.bottom - t,
        },
        rcSource: RECT::default(),
        opacity: req.opacity,
        fVisible: TRUE,
        fSourceClientAreaOnly: FALSE,
    };
    let _ = DwmUpdateThumbnailProperties(st.thumb, &props);
    st.shown_opacity = req.opacity;

    if IsWindowVisible(hwnd).as_bool() {
        let _ = SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            l,
            t,
            r - l,
            b - t,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
    } else {
        let _ = SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            l,
            t,
            r - l,
            b - t,
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        );
        let _ = ShowWindow(hwnd, SW_SHOW);
    }
    GPU_USING.store(true, Ordering::Relaxed);
}

unsafe fn top_foreign_at(x: i32, y: i32) -> isize {
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

unsafe fn lower_window_at(x: i32, y: i32, target: isize) -> isize {
    let mut h = match GetTopWindow(HWND(std::ptr::null_mut())) {
        Ok(h) => h,
        Err(_) => return 0,
    };
    let mut passed = false;
    while !h.0.is_null() {
        let v = h.0 as isize;
        if v == target {
            passed = true;
        } else if passed && !crate::is_own_hwnd(h) && !crate::is_shell_hwnd(h) {
            let mut r = RECT::default();
            if IsWindowVisible(h).as_bool()
                && GetWindowRect(h, &mut r).is_ok()
                && x >= r.left
                && x < r.right
                && y >= r.top
                && y < r.bottom
            {
                return v;
            }
        }
        h = match GetWindow(h, GW_HWNDNEXT) {
            Ok(n) => n,
            Err(_) => return 0,
        };
    }
    0
}
