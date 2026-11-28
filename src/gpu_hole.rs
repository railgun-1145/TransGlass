//! GPU（DWM）实时光洞：用 DWM 缩略图把「目标窗口下面那扇窗口」实时贴到洞区。
//! 合成完全由 DWM 在 GPU 上完成，不走 CPU 逐像素，所以洞里的下层画面是实时的、跟手不拖影。
//!
//! 用法：regions 的视觉线程每帧把目标/洞/透明度丢进来；本模块自己一个线程 + 16ms 定时器。
//! 缩略图注册失败（比如下层是桌面、或系统不支持）时 `using()` 为 false，调用方回退到 CPU 快照方案。

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
/// GPU 覆盖层当前是否成功在显示（调用方据此决定要不要回退 CPU 覆盖层）
static GPU_USING: AtomicBool = AtomicBool::new(false);

#[derive(Clone)]
struct Req {
    target: isize,
    /// 归一化客户区多边形
    polys: Vec<Vec<(f32, f32)>>,
    opacity: u8,
}

static REQ: Mutex<Option<Req>> = Mutex::new(None);

pub fn using() -> bool {
    GPU_USING.load(Ordering::Relaxed)
}

/// 由 regions 视觉线程调用：更新目标/洞/透明度（非阻塞）。
pub fn update(target: isize, polys: Vec<Vec<(f32, f32)>>, opacity: u8) {
    if let Ok(mut r) = REQ.lock() {
        *r = Some(Req {
            target,
            polys,
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

struct GpuState {
    thumb: isize,
    source: isize,
}

static STATE: Mutex<GpuState> = Mutex::new(GpuState {
    thumb: 0,
    source: 0,
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
    let cls = w!("TransGlassGpuHole");
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
    let _ = SetWindowRgn(hwnd, None::<&HRGN>, true);
}

unsafe fn apply(hwnd: HWND) {
    let Some(req) = REQ.lock().ok().and_then(|mut r| r.take()) else {
        // 没有新请求：保持现状即可
        return;
    };
    if req.target == 0 || req.polys.is_empty() || req.opacity == 0 {
        hide_now(hwnd);
        return;
    }
    let target = HWND(req.target as *mut _);
    if !IsWindow(target).as_bool() || !IsWindowVisible(target).as_bool() || IsIconic(target).as_bool() {
        hide_now(hwnd);
        return;
    }
    // 目标客户区
    let mut rc = RECT::default();
    if GetClientRect(target, &mut rc).is_err() {
        hide_now(hwnd);
        return;
    }
    let cw = rc.right - rc.left;
    let ch = rc.bottom - rc.top;
    if cw <= 0 || ch <= 0 {
        hide_now(hwnd);
        return;
    }
    let mut origin = POINT { x: 0, y: 0 };
    if ClientToScreen(target, &mut origin).as_bool() == false {
        hide_now(hwnd);
        return;
    }
    let (cx, cy) = (origin.x, origin.y);

    // 取下层窗口：从 z 序最上面往下找"包含洞里采样点、不是目标/不是我们进程"的窗口
    let sample = sample_point(&req.polys, cx, cy, cw, ch);
    let (source, below) = match sample {
        Some((sx, sy)) => (lower_window_at(sx, sy, req.target), top_foreign_at(sx, sy)),
        None => (0, 0),
    };
    // 层数遮盖：洞里那一点最上面的外部窗口必须就是目标本身，
    // 否则说明目标被别的页面盖住了，覆盖层不能画上去
    if below != req.target || source == 0 {
        hide_now(hwnd);
        return;
    }

    // 区域：窗口坐标（覆盖层是无边框 popup，客户区 = 窗口区）
    let base = CreateRectRgn(0, 0, cw, ch);
    if base.is_invalid() {
        hide_now(hwnd);
        return;
    }
    for poly in &req.polys {
        if poly.len() < 3 {
            continue;
        }
        let pts: Vec<POINT> = poly
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
    let _ = SetWindowRgn(hwnd, Some(&base), true);

    // 缩略图：源变了就重新注册
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
            Ok(t) => {
                st.thumb = t;
                st.source = source;
            }
            Err(_) => {
                GPU_USING.store(false, Ordering::Relaxed);
                let _ = ShowWindow(hwnd, SW_HIDE);
                return;
            }
        }
    }
    // 源窗口的屏幕矩形 -> 覆盖层客户区坐标
    let mut sr = RECT::default();
    if GetWindowRect(HWND(source as *mut _), &mut sr).is_err() {
        hide_now(hwnd);
        return;
    }
    let props = DWM_THUMBNAIL_PROPERTIES {
        dwFlags: DWM_TNP_RECTDESTINATION | DWM_TNP_OPACITY | DWM_TNP_VISIBLE | DWM_TNP_SOURCECLIENTAREAONLY,
        rcDestination: RECT {
            left: sr.left - cx,
            top: sr.top - cy,
            right: sr.right - cx,
            bottom: sr.bottom - cy,
        },
        rcSource: RECT::default(),
        opacity: req.opacity,
        fVisible: TRUE,
        fSourceClientAreaOnly: FALSE,
    };
    let _ = DwmUpdateThumbnailProperties(st.thumb, &props);

    let _ = SetWindowPos(
        hwnd,
        HWND_TOPMOST,
        cx,
        cy,
        cw,
        ch,
        SWP_NOACTIVATE | SWP_SHOWWINDOW,
    );
    let _ = ShowWindow(hwnd, SW_SHOW);
    GPU_USING.store(true, Ordering::Relaxed);
}

fn sample_point(polys: &[Vec<(f32, f32)>], cx: i32, cy: i32, cw: i32, ch: i32) -> Option<(i32, i32)> {
    for poly in polys {
        if poly.len() < 3 {
            continue;
        }
        let (mut minx, mut miny, mut maxx, mut maxy) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for (x, y) in poly {
            minx = minx.min(*x);
            miny = miny.min(*y);
            maxx = maxx.max(*x);
            maxy = maxy.max(*y);
        }
        let nx = (minx + maxx) * 0.5;
        let ny = (miny + maxy) * 0.5;
        // 简化的"点在多边形内"：这里只用于取采样点，用 bbox 中心就够（矩形洞最常见）
        return Some((
            cx + (nx * cw as f32) as i32,
            cy + (ny * ch as f32) as i32,
        ));
    }
    None
}

unsafe fn lower_window_at(x: i32, y: i32, target: isize) -> isize {
    let mut h = match GetTopWindow(HWND(std::ptr::null_mut())) {
        Ok(h) => h,
        Err(_) => return 0,
    };
    let mut passed_target = false;
    while !h.0.is_null() {
        let hwnd_val = h.0 as isize;
        if hwnd_val == target {
            passed_target = true;
        } else if passed_target && !crate::is_own_hwnd(h) && !crate::is_shell_hwnd(h) {
            let mut r = RECT::default();
            if IsWindowVisible(h).as_bool()
                && GetWindowRect(h, &mut r).is_ok()
                && x >= r.left
                && x < r.right
                && y >= r.top
                && y < r.bottom
            {
                return hwnd_val;
            }
        }
        h = match GetWindow(h, GW_HWNDNEXT) {
            Ok(n) => n,
            Err(_) => return 0,
        };
    }
    0
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
