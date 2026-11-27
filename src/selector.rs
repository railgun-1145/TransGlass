//! 点透会话：Alt+Tab 式窗口选择器。
//!
//! 交互：
//! - 热键（默认 Alt+`）唤出全屏选择器；此时点透不需要动，选择器自己会接住鼠标。
//! - ←/→、Tab 切换，Enter/点击选中，Esc 取消。
//! - 选中后：点透临时关闭 → 窗口提到最上层 → 可正常拖动/操作。
//! - Esc 或再次按热键结束：恢复选中窗口的点透样式、最小化状态和原 z 序。
//!
//! 实现：独立线程 + `UpdateLayeredWindow` 全屏分层窗口（GDI 画卡片），
//! 键鼠消息都在这个线程里；主线程的鼠标钩子通过 `is_session_window` 跳过
//! 会话窗口的单设备点透翻转。

use std::cell::RefCell;
use std::sync::atomic::{AtomicI32, AtomicIsize, Ordering};
use std::sync::Once;

use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::{
    cached_app_name, is_own_hwnd, is_shell_hwnd, reapply_managed_state, reset_passthrough_tracking,
};

const WM_SELECTOR_TOGGLE: u32 = WM_APP + 0x41;
const WM_SELECTOR_END: u32 = WM_APP + 0x42;

const MODE_IDLE: i32 = 0;
const MODE_CHOOSING: i32 = 1;
const MODE_ACTIVE: i32 = 2;

static INIT: Once = Once::new();
static SELECTOR_HWND: AtomicIsize = AtomicIsize::new(0);
static MODE: AtomicI32 = AtomicI32::new(MODE_IDLE);
static SESSION_HWND: AtomicIsize = AtomicIsize::new(0);

/// 热键线程调这个；真正的 UI 都在选择器线程里。
pub fn toggle() {
    let h = SELECTOR_HWND.load(Ordering::Relaxed);
    if h != 0 {
        unsafe {
            let _ = PostMessageW(
                HWND(h as *mut _),
                WM_SELECTOR_TOGGLE,
                WPARAM(0),
                LPARAM(0),
            );
        }
    }
}

/// 主线程鼠标钩子用：这个窗口在会话期间临时不点透。
pub fn is_session_window(hwnd: isize) -> bool {
    MODE.load(Ordering::Relaxed) == MODE_ACTIVE
        && SESSION_HWND.load(Ordering::Relaxed) == hwnd
}

pub fn init() {
    INIT.call_once(|| {
        let _ = std::thread::Builder::new()
            .name("tg-selector".into())
            .spawn(|| unsafe { selector_thread() });
    });
}

struct Candidate {
    hwnd: isize,
    title: String,
    app: String,
    /// HICON 以 isize 存，避免线程状态里的 Send 问题（只在本线程用）
    icon: isize,
    /// 唤出选择器之前的 z 序：这个窗口下面紧挨着谁（还原用）
    prev_above: isize,
    minimized: bool,
    topmost: bool,
    /// 窗口缩略图（最小化/抓取失败时 None，卡片退回图标）
    preview: Option<Preview>,
}

struct Preview {
    w: i32,
    h: i32,
    bgra: Vec<u8>,
}

struct Session {
    hwnd: isize,
    prev_above: isize,
    topmost: bool,
    minimized: bool,
    was_transparent: bool,
}

struct State {
    candidates: Vec<Candidate>,
    selected: usize,
    page_start: usize,
    /// 卡片矩形（选择器窗口客户区坐标），鼠标命中用
    cards: Vec<(i32, i32, i32, i32)>,
    panel: (i32, i32, i32, i32),
    session: Option<Session>,
    esc_hook: isize,
    /// 唤出时的光标位置：鼠标没动之前不要被刚好压在卡片上改掉初始选中
    mouse_anchor: (i32, i32),
}

impl Default for State {
    fn default() -> Self {
        Self {
            candidates: Vec::new(),
            selected: 0,
            page_start: 0,
            cards: Vec::new(),
            panel: (0, 0, 0, 0),
            session: None,
            esc_hook: 0,
            mouse_anchor: (0, 0),
        }
    }
}

thread_local! {
    static ST: RefCell<State> = RefCell::new(State::default());
}

// ---------------------------------------------------------------------------
// 窗口线程
// ---------------------------------------------------------------------------

unsafe fn selector_thread() {
    let hinst = match GetModuleHandleW(None) {
        Ok(h) => h,
        Err(_) => return,
    };
    let class_w: Vec<u16> = "TransGlassSelector"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let title_w: Vec<u16> = "TransGlassSelector"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let wc = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(selector_proc),
        hInstance: HINSTANCE(hinst.0),
        hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
        lpszClassName: windows::core::PCWSTR(class_w.as_ptr()),
        ..Default::default()
    };
    let _ = RegisterClassW(&wc);
    let hwnd = match CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
        windows::core::PCWSTR(class_w.as_ptr()),
        windows::core::PCWSTR(title_w.as_ptr()),
        WS_POPUP,
        0,
        0,
        0,
        0,
        None,
        None,
        HINSTANCE(hinst.0),
        None,
    ) {
        Ok(h) => h,
        Err(_) => return,
    };
    SELECTOR_HWND.store(hwnd.0 as isize, Ordering::Relaxed);

    let mut msg = MSG::default();
    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
}

unsafe extern "system" fn selector_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        WM_SELECTOR_TOGGLE => {
            on_toggle();
            LRESULT(0)
        }
        WM_SELECTOR_END => {
            end_session();
            LRESULT(0)
        }
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            on_key(w.0 as u32);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            on_mouse_move(l);
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            on_click(l);
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            let delta = ((w.0 >> 16) & 0xFFFF) as u16 as i16;
            move_selection(if delta > 0 { -1 } else { 1 });
            LRESULT(0)
        }
        WM_ACTIVATE => {
            // 注意：不要在这里"一失焦就取消"——AttachThreadInput 抢前台的过程中
            // 会收到一次瞬时的 WM_ACTIVATE(WA_INACTIVE)，会把刚显示出来的选择器
            // 立刻隐藏掉（表现为前台是这个窗口、可见状态却是 false）。
            // 失焦不取消；Esc / 再按热键 / 点空白 都能退出。
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, w, l),
    }
}

unsafe extern "system" fn esc_proc(code: i32, w: WPARAM, l: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && MODE.load(Ordering::Relaxed) == MODE_ACTIVE {
        let kb = &*(l.0 as *const KBDLLHOOKSTRUCT);
        let is_down = matches!(w.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
        if is_down && kb.vkCode == VK_ESCAPE.0 as u32 {
            let h = SELECTOR_HWND.load(Ordering::Relaxed);
            if h != 0 {
                let _ = PostMessageW(HWND(h as *mut _), WM_SELECTOR_END, WPARAM(0), LPARAM(0));
            }
            return LRESULT(1); // 吞掉 Esc，避免同时关掉别的菜单
        }
    }
    CallNextHookEx(None, code, w, l)
}

// ---------------------------------------------------------------------------
// 会话状态机
// ---------------------------------------------------------------------------

unsafe fn on_toggle() {
    match MODE.load(Ordering::Relaxed) {
        MODE_IDLE => open_chooser(),
        MODE_CHOOSING => move_selection(1),
        _ => end_session(),
    }
}

unsafe fn open_chooser() {
    let fg = GetForegroundWindow();
    let fg_val = fg.0 as isize;
    let mut candidates: Vec<Candidate> = Vec::new();
    let _ = EnumWindows(Some(enum_candidate), LPARAM(&mut candidates as *mut _ as isize));
    if candidates.is_empty() {
        return;
    }
    // 给非最小化窗口抓一张缩略图；最小化/GPU 黑帧保持 None，卡片退回图标
    for c in candidates.iter_mut() {
        if c.minimized {
            continue;
        }
        if let Some(img) = crate::capture_window(HWND(c.hwnd as *mut _)) {
            let (pw, ph) = (img.size[0] as i32, img.size[1] as i32);
            if pw > 0 && ph > 0 {
                let mut bgra = Vec::with_capacity((pw * ph * 4) as usize);
                for px in &img.pixels {
                    bgra.extend_from_slice(&[px.b(), px.g(), px.r(), 255]);
                }
                c.preview = Some(Preview { w: pw, h: ph, bgra });
            }
        }
    }
    let selected = candidates
        .iter()
        .position(|c| c.hwnd == fg_val)
        .unwrap_or(0);

    let mut p = POINT::default();
    let _ = GetCursorPos(&mut p);
    ST.with(|cell| {
        let mut st = cell.borrow_mut();
        st.candidates = candidates;
        st.selected = selected;
        st.page_start = 0;
        st.cards.clear();
        st.session = None;
        st.mouse_anchor = (p.x, p.y);
    });
    MODE.store(MODE_CHOOSING, Ordering::Relaxed);
    draw_and_show();
}

unsafe fn cancel_chooser() {
    let h = SELECTOR_HWND.load(Ordering::Relaxed);
    if h != 0 {
        let _ = ShowWindow(HWND(h as *mut _), SW_HIDE);
    }
    MODE.store(MODE_IDLE, Ordering::Relaxed);
}

unsafe fn choose(index: usize) {
    let cand = ST.with(|cell| {
        let st = cell.borrow();
        st.candidates
            .get(index)
            .map(|c| (c.hwnd, c.prev_above, c.topmost, c.minimized))
    });
    let Some((cand_hwnd, cand_prev_above, cand_topmost, cand_minimized)) = cand else {
        return;
    };
    let win = HWND(cand_hwnd as *mut _);
    if !IsWindow(win).as_bool() {
        MODE.store(MODE_IDLE, Ordering::Relaxed);
        return;
    }

    // 先把会话立起来：后面的 SW_RESTORE / 激活 / 隐藏选择器会触发
    // WM_ACTIVATE(失焦)，这时已经不是 CHOOSING，不会被当成取消。
    let ex = GetWindowLongW(win, GWL_EXSTYLE) as u32;
    let was_transparent = (ex & WS_EX_TRANSPARENT.0) != 0;
    ST.with(|cell| {
        let mut st = cell.borrow_mut();
        st.session = Some(Session {
            hwnd: cand_hwnd,
            prev_above: cand_prev_above,
            topmost: cand_topmost,
            minimized: cand_minimized,
            was_transparent,
        });
        st.esc_hook = 0;
    });
    SESSION_HWND.store(cand_hwnd, Ordering::Relaxed);
    MODE.store(MODE_ACTIVE, Ordering::Relaxed);
    crate::set_target_window(cand_hwnd);
    crate::ball_mark_dirty();

    let h = SELECTOR_HWND.load(Ordering::Relaxed);
    if h != 0 {
        let _ = ShowWindow(HWND(h as *mut _), SW_HIDE);
    }

    if cand_minimized {
        let _ = ShowWindow(win, SW_RESTORE);
    }
    if was_transparent {
        let ex = GetWindowLongW(win, GWL_EXSTYLE) as u32;
        let _ = SetWindowLongW(win, GWL_EXSTYLE, (ex & !WS_EX_TRANSPARENT.0) as i32);
    }
    // 提到最上层但保持原来的 topmost 属性；位置/大小不动
    let _ = SetWindowPos(
        win,
        HWND_TOP,
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
    );
    let _ = SetForegroundWindow(win);
    install_esc_hook();
}

unsafe fn end_session() {
    let (session, hook) = ST.with(|cell| {
        let mut st = cell.borrow_mut();
        (st.session.take(), std::mem::replace(&mut st.esc_hook, 0))
    });
    MODE.store(MODE_IDLE, Ordering::Relaxed);
    SESSION_HWND.store(0, Ordering::Relaxed);
    crate::ball_mark_dirty();

    if hook != 0 {
        let _ = UnhookWindowsHookEx(HHOOK(hook as *mut _));
    }

    let Some(session) = session else { return };
    let win = HWND(session.hwnd as *mut _);
    if !IsWindow(win).as_bool() {
        return;
    }

    // 点透样式：托管窗口按注册表重放（alpha/置顶/点透）；未托管窗口只把摘掉的位还回去
    if !reapply_managed_state(win) {
        if session.was_transparent {
            let ex = GetWindowLongW(win, GWL_EXSTYLE) as u32;
            let _ = SetWindowLongW(win, GWL_EXSTYLE, (ex | WS_EX_TRANSPARENT.0) as i32);
        }
    }
    // 单设备点透的样式由鼠标钩子下一次事件重新判断
    reset_passthrough_tracking();

    // 恢复 z 序：插回原来上方的那个窗口下面；原来是顶就回到顶
    let target = if session.prev_above != 0
        && IsWindow(HWND(session.prev_above as *mut _)).as_bool()
    {
        HWND(session.prev_above as *mut _)
    } else if session.topmost {
        HWND_TOPMOST
    } else {
        HWND_TOP
    };
    let _ = SetWindowPos(
        win,
        target,
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
    );

    // 选中前是最小化的，退出时还原成最小化（"恢复窗口排布"）
    if session.minimized {
        let _ = ShowWindow(win, SW_MINIMIZE);
    }
}

unsafe fn install_esc_hook() {
    if let Ok(hook) = SetWindowsHookExW(WH_KEYBOARD_LL, Some(esc_proc), None, 0) {
        ST.with(|cell| cell.borrow_mut().esc_hook = hook.0 as isize);
    }
}

// ---------------------------------------------------------------------------
// 输入
// ---------------------------------------------------------------------------

unsafe fn on_key(vk: u32) {
    if MODE.load(Ordering::Relaxed) != MODE_CHOOSING {
        return;
    }
    let shift = (GetKeyState(VK_SHIFT.0 as i32) as u16 & 0x8000) != 0;
    match vk {
        v if v == VK_LEFT.0 as u32 || v == VK_UP.0 as u32 => move_selection(-1),
        v if v == VK_RIGHT.0 as u32 || v == VK_DOWN.0 as u32 => move_selection(1),
        v if v == VK_TAB.0 as u32 => move_selection(if shift { -1 } else { 1 }),
        v if v == VK_RETURN.0 as u32 => confirm_selected(),
        v if v == VK_ESCAPE.0 as u32 => cancel_chooser(),
        _ => {}
    }
}

unsafe fn on_mouse_move(l: LPARAM) {
    if MODE.load(Ordering::Relaxed) != MODE_CHOOSING {
        return;
    }
    let (x, y) = mouse_xy(l);
    let anchor = ST.with(|cell| cell.borrow().mouse_anchor);
    if (x - anchor.0).abs() + (y - anchor.1).abs() < 6 {
        return;
    }
    let hit = ST.with(|cell| {
        let st = cell.borrow();
        st.cards
            .iter()
            .position(|(x0, y0, x1, y1)| x >= *x0 && x < *x1 && y >= *y0 && y < *y1)
            .map(|i| st.page_start + i)
    });
    if let Some(idx) = hit {
        let changed = ST.with(|cell| {
            let mut st = cell.borrow_mut();
            if st.selected != idx {
                st.selected = idx;
                true
            } else {
                false
            }
        });
        if changed {
            draw_and_show();
        }
    }
}

unsafe fn on_click(l: LPARAM) {
    if MODE.load(Ordering::Relaxed) != MODE_CHOOSING {
        return;
    }
    let (x, y) = mouse_xy(l);
    let (hit, inside_panel) = ST.with(|cell| {
        let st = cell.borrow();
        let hit = st
            .cards
            .iter()
            .position(|(x0, y0, x1, y1)| x >= *x0 && x < *x1 && y >= *y0 && y < *y1)
            .map(|i| st.page_start + i);
        let (px0, py0, px1, py1) = st.panel;
        (hit, x >= px0 && x < px1 && y >= py0 && y < py1)
    });
    if let Some(idx) = hit {
        choose(idx);
    } else if !inside_panel {
        cancel_chooser();
    }
}

fn move_selection(delta: i32) {
    unsafe {
        if MODE.load(Ordering::Relaxed) != MODE_CHOOSING {
            return;
        }
        let n = ST.with(|cell| cell.borrow().candidates.len());
        if n == 0 {
            return;
        }
        let changed = ST.with(|cell| {
            let mut st = cell.borrow_mut();
            let n = st.candidates.len();
            let next = (st.selected as i32 + delta).rem_euclid(n as i32) as usize;
            if next != st.selected {
                st.selected = next;
                true
            } else {
                false
            }
        });
        if changed {
            draw_and_show();
        }
    }
}

unsafe fn confirm_selected() {
    let idx = ST.with(|cell| cell.borrow().selected);
    choose(idx);
}

fn mouse_xy(l: LPARAM) -> (i32, i32) {
    let x = (l.0 & 0xFFFF) as u16 as i16 as i32;
    let y = ((l.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
    (x, y)
}

// ---------------------------------------------------------------------------
// 枚举候选窗口
// ---------------------------------------------------------------------------

unsafe extern "system" fn enum_candidate(hwnd: HWND, lp: LPARAM) -> BOOL {
    let out = &mut *(lp.0 as *mut Vec<Candidate>);
    if !IsWindowVisible(hwnd).as_bool() || is_own_hwnd(hwnd) || is_shell_hwnd(hwnd) {
        return BOOL(1);
    }
    let mut buf: [u16; 256] = [0; 256];
    let len = GetWindowTextW(hwnd, &mut buf);
    if len <= 0 {
        return BOOL(1);
    }
    let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
    // 工具窗/不激活窗不参与（输入法皮肤、覆盖层之类）
    if (ex & WS_EX_TOOLWINDOW.0) != 0 || (ex & WS_EX_NOACTIVATE.0) != 0 {
        return BOOL(1);
    }
    // 其它虚拟桌面 / 挂起的 UWP 会被 DWM 标成 cloaked，跳过
    let mut cloaked: u32 = 0;
    let _ = DwmGetWindowAttribute(
        hwnd,
        DWMWA_CLOAKED,
        &mut cloaked as *mut _ as *mut _,
        std::mem::size_of::<u32>() as u32,
    );
    if cloaked != 0 {
        return BOOL(1);
    }
    let mut rc = RECT::default();
    let _ = GetWindowRect(hwnd, &mut rc);
    if rc.right - rc.left < 40 || rc.bottom - rc.top < 24 {
        return BOOL(1);
    }

    let icon = {
        let big = GetClassLongPtrW(hwnd, GCLP_HICON);
        if big != 0 {
            big
        } else {
            GetClassLongPtrW(hwnd, GCLP_HICONSM)
        }
    };
    let topmost = (ex & WS_EX_TOPMOST.0) != 0;
    let prev_above = {
        // 跳过隐藏窗口和另一个 topmost 分组里的窗口：
        // 非 topmost 窗口如果以悬浮球（topmost）为还原锚点，SetWindowPos 会把它也顶成 topmost。
        let mut above = GetWindow(hwnd, GW_HWNDPREV).ok();
        while let Some(h) = above {
            if IsWindowVisible(h).as_bool() {
                let hex = GetWindowLongW(h, GWL_EXSTYLE) as u32;
                let same_band = ((hex & WS_EX_TOPMOST.0) != 0) == topmost;
                if same_band {
                    break;
                }
            }
            above = GetWindow(h, GW_HWNDPREV).ok();
        }
        above.map(|h| h.0 as isize).unwrap_or(0)
    };
    out.push(Candidate {
        hwnd: hwnd.0 as isize,
        title: String::from_utf16_lossy(&buf[..len as usize]),
        app: cached_app_name(hwnd),
        icon: icon as isize,
        prev_above,
        minimized: IsIconic(hwnd).as_bool(),
        topmost,
        preview: None,
    });
    BOOL(1)
}

// ---------------------------------------------------------------------------
// 画选择器（分层窗口，GDI 文字/图标 + 手写像素背景）
// ---------------------------------------------------------------------------

const CARD_W: i32 = 148;
const CARD_H: i32 = 196;
const CARD_GAP: i32 = 14;
const MAX_CARDS: usize = 5;

unsafe fn draw_and_show() {
    let hwnd_val = SELECTOR_HWND.load(Ordering::Relaxed);
    if hwnd_val == 0 {
        return;
    }
    let hwnd = HWND(hwnd_val as *mut _);
    let sw = GetSystemMetrics(SM_CXSCREEN).max(640);
    let sh = GetSystemMetrics(SM_CYSCREEN).max(480);
    let bw = sw;
    let bh = sh;
    let screen = GetDC(None);
    let mem = CreateCompatibleDC(screen);
    let mut bi: BITMAPINFO = std::mem::zeroed();
    bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    bi.bmiHeader.biWidth = bw;
    bi.bmiHeader.biHeight = -bh;
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
    let buf = std::slice::from_raw_parts_mut(bits as *mut u8, (bw * bh * 4) as usize);

    // 全屏压暗（预乘黑，alpha 92）；选择器窗口本身会吃掉鼠标
    for px in buf.chunks_exact_mut(4) {
        px[0] = 0;
        px[1] = 0;
        px[2] = 0;
        px[3] = 92;
    }

    let n = ST.with(|cell| cell.borrow().candidates.len());
    if n == 0 {
        SelectObject(mem, old);
        let _ = DeleteObject(HGDIOBJ(dib.0));
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        return;
    }

    let pw = 880.min(sw - 80).max(640);
    let ph = 340;
    let px0 = (sw - pw) / 2;
    let py0 = (sh - ph) / 2;
    fill_rect(buf, bw, px0, py0, px0 + pw, py0 + ph, (255, 255, 255), 255);
    frame_rect(buf, bw, px0, py0, px0 + pw, py0 + ph, (222, 226, 232), 1);
    // 顶部一条很淡的强调色，跟主界面的绿色呼应
    fill_rect(buf, bw, px0, py0, px0 + pw, py0 + 3, (47, 194, 124), 255);

    let font_title = create_font(22, 700);
    let font_hint = create_font(12, 400);
    let font_app = create_font(14, 700);
    let font_sub = create_font(11, 400);
    let font_mini = create_font(10, 400);
    let font_icon = create_font(22, 700);

    let mut rc = RECT {
        left: px0 + 28,
        top: py0 + 16,
        right: px0 + pw - 28,
        bottom: py0 + 48,
    };
    draw_text(mem, "选择要控制的窗口", &mut rc, (26, 26, 26), font_title, DT_LEFT);
    let mut rc = RECT {
        left: px0 + 28,
        top: py0 + 18,
        right: px0 + pw - 28,
        bottom: py0 + 44,
    };
    draw_text(
        mem,
        "← → / 再按热键 切换 · Enter 控制 · Esc 取消",
        &mut rc,
        (138, 138, 138),
        font_hint,
        DT_RIGHT,
    );
    // 标题下的细分隔线
    fill_rect(
        buf,
        bw,
        px0 + 24,
        py0 + 54,
        px0 + pw - 24,
        py0 + 55,
        (236, 239, 243),
        255,
    );

    let visible = MAX_CARDS.min(n);
    let start = if n <= MAX_CARDS {
        0
    } else {
        ST.with(|cell| {
            let st = cell.borrow();
            st.selected
                .saturating_sub(MAX_CARDS / 2)
                .min(n.saturating_sub(MAX_CARDS))
        })
    };
    let total_w = visible as i32 * CARD_W + (visible as i32 - 1).max(0) * CARD_GAP;
    let card_x0 = px0 + (pw - total_w) / 2;
    let card_y0 = py0 + 68;
    let selected = ST.with(|cell| cell.borrow().selected);

    let mut card_rects = Vec::with_capacity(visible);
    for i in 0..visible {
        let idx = start + i;
        let (app, title, icon, minimized, has_preview) = ST.with(|cell| {
            let st = cell.borrow();
            let c = &st.candidates[idx];
            let app = if c.app.trim().is_empty() {
                c.title.chars().take(2).collect::<String>()
            } else {
                c.app.clone()
            };
            (app, c.title.clone(), c.icon, c.minimized, c.preview.is_some())
        });
        let x0 = card_x0 + i as i32 * (CARD_W + CARD_GAP);
        let y0 = card_y0;
        let x1 = x0 + CARD_W;
        let y1 = y0 + CARD_H;
        card_rects.push((x0, y0, x1, y1));
        let is_sel = idx == selected;
        if is_sel {
            fill_rect(buf, bw, x0, y0, x1, y1, (233, 249, 241), 255);
            frame_rect(buf, bw, x0, y0, x1, y1, (47, 194, 124), 2);
        } else {
            fill_rect(buf, bw, x0, y0, x1, y1, (247, 248, 250), 255);
            frame_rect(buf, bw, x0, y0, x1, y1, (232, 235, 239), 1);
        }

        if has_preview {
            // 预览图区域：128×80，等比缩放 + 细边框（Alt+Tab 感）
            ST.with(|cell| {
                let st = cell.borrow();
                if let Some(p) = st.candidates[idx].preview.as_ref() {
                    frame_rect(buf, bw, x0 + 9, y0 + 11, x0 + 139, y0 + 93, (205, 210, 216), 1);
                    draw_preview(buf, bw, x0 + 10, y0 + 12, 128, 80, p);
                }
            });
        } else {
            // 没有缩略图：大图标 / 首字母块
            let iw = 44;
            let ih = 44;
            let ix = x0 + (CARD_W - iw) / 2;
            let iy = y0 + 18;
            if icon != 0 {
                let _ = DrawIconEx(
                    mem,
                    ix,
                    iy,
                    HICON(icon as *mut _),
                    iw,
                    ih,
                    0,
                    None,
                    DI_NORMAL,
                );
            } else {
                fill_rect(buf, bw, ix, iy, ix + iw, iy + ih, (226, 231, 236), 255);
                let mut rc = RECT {
                    left: ix,
                    top: iy + 8,
                    right: ix + iw,
                    bottom: iy + ih,
                };
                let initial: String = app.chars().next().map(|c| c.to_string()).unwrap_or_default();
                draw_text(
                    mem,
                    &initial,
                    &mut rc,
                    (70, 90, 100),
                    font_icon,
                    DT_CENTER | DT_TOP,
                );
            }
        }

        let (app_top, title_top) = if has_preview { (100, 126) } else { (76, 102) };
        let mut rc = RECT {
            left: x0 + 10,
            top: y0 + app_top,
            right: x1 - 10,
            bottom: y0 + app_top + 24,
        };
        draw_text(
            mem,
            &app,
            &mut rc,
            (26, 26, 26),
            font_app,
            DT_CENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
        );
        let mut rc = RECT {
            left: x0 + 10,
            top: y0 + title_top,
            right: x1 - 10,
            bottom: y0 + title_top + 26,
        };
        draw_text(
            mem,
            &title,
            &mut rc,
            (120, 124, 130),
            font_sub,
            DT_CENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
        );
        if minimized {
            let mut rc = RECT {
                left: x0 + 10,
                top: y0 + 150,
                right: x1 - 10,
                bottom: y0 + 172,
            };
            draw_text(
                mem,
                "已最小化",
                &mut rc,
                (170, 150, 90),
                font_mini,
                DT_CENTER | DT_SINGLELINE,
            );
        }
    }

    // 底部提示
    let mut rc = RECT {
        left: px0 + 28,
        top: py0 + ph - 34,
        right: px0 + pw - 28,
        bottom: py0 + ph - 12,
    };
    let hint = format!(
        "共 {} 个窗口 · ←/→ 或再按热键切换 · 选中后临时关闭点透，Esc 取消/结束",
        n
    );
    draw_text(mem, &hint, &mut rc, (138, 138, 138), font_hint, DT_LEFT);

    // GDI 不写 alpha；面板区域本身是不透明的，统一补成 255（预乘 = 原色）
    for y in py0.max(0)..(py0 + ph).min(bh) {
        let row = (y * bw) as usize;
        for x in px0.max(0)..(px0 + pw).min(bw) {
            buf[(row + x as usize) * 4 + 3] = 255;
        }
    }

    ST.with(|cell| {
        let mut st = cell.borrow_mut();
        st.cards = card_rects;
        st.page_start = start;
        st.panel = (px0, py0, px0 + pw, py0 + ph);
    });

    let pos = POINT { x: 0, y: 0 };
    let size = SIZE { cx: bw, cy: bh };
    let src = POINT { x: 0, y: 0 };
    let blend = BLENDFUNCTION {
        BlendOp: 0,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: 1, // AC_SRC_ALPHA
    };
    let _ = UpdateLayeredWindow(
        hwnd,
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
    let _ = DeleteObject(HGDIOBJ(font_title.0));
    let _ = DeleteObject(HGDIOBJ(font_hint.0));
    let _ = DeleteObject(HGDIOBJ(font_app.0));
    let _ = DeleteObject(HGDIOBJ(font_sub.0));
    let _ = DeleteObject(HGDIOBJ(font_mini.0));
    let _ = DeleteObject(HGDIOBJ(font_icon.0));
    let _ = DeleteObject(HGDIOBJ(dib.0));
    let _ = DeleteDC(mem);
    ReleaseDC(None, screen);

    // 先显示再抢前台；有些系统上 SetForegroundWindow 会被前台锁拒掉，用 AttachThreadInput 兜底
    let _ = SetWindowPos(
        hwnd,
        HWND_TOPMOST,
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
    );
    let _ = ShowWindow(hwnd, SW_SHOW);
    focus_overlay(hwnd);
}

unsafe fn focus_overlay(hwnd: HWND) {
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

// ---------------------------------------------------------------------------
// GDI 小工具
// ---------------------------------------------------------------------------

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
        4, // ANTIALIASED_QUALITY（避免 ClearType 在透明边缘出彩边）
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
    let _ = SetTextColor(hdc, COLORREF((color.0 as u32) | ((color.1 as u32) << 8) | ((color.2 as u32) << 16)));
    let mut w: Vec<u16> = text.encode_utf16().collect();
    let _ = DrawTextW(hdc, &mut w, rc, flags);
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

fn frame_rect(
    buf: &mut [u8],
    w: i32,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    rgb: (u8, u8, u8),
    t: i32,
) {
    fill_rect(buf, w, x0, y0, x1, y0 + t, rgb, 255);
    fill_rect(buf, w, x0, y1 - t, x1, y1, rgb, 255);
    fill_rect(buf, w, x0, y0, x0 + t, y1, rgb, 255);
    fill_rect(buf, w, x1 - t, y0, x1, y1, rgb, 255);
}

/// 把窗口缩略图等比缩放进卡片预览区（最近邻；窗口图本身已经够小，不做插值免得糊）。
fn draw_preview(
    buf: &mut [u8],
    bw: i32,
    dx: i32,
    dy: i32,
    dw: i32,
    dh: i32,
    p: &Preview,
) {
    if p.w <= 0 || p.h <= 0 || p.bgra.len() < 4 {
        return;
    }
    let scale = (dw as f32 / p.w as f32).min(dh as f32 / p.h as f32);
    let tw = ((p.w as f32 * scale).round() as i32).clamp(1, dw);
    let th = ((p.h as f32 * scale).round() as i32).clamp(1, dh);
    let ox = dx + (dw - tw) / 2;
    let oy = dy + (dh - th) / 2;
    for yy in 0..th {
        let sy = ((yy as i64 * p.h as i64) / th as i64) as i32;
        for xx in 0..tw {
            let sx = ((xx as i64 * p.w as i64) / tw as i64) as i32;
            let si = ((sy * p.w + sx) * 4) as usize;
            if si + 3 >= p.bgra.len() {
                continue;
            }
            let px = ox + xx;
            let py = oy + yy;
            if px < 0 || py < 0 || px >= bw {
                continue;
            }
            let di = ((py * bw + px) * 4) as usize;
            if di + 3 >= buf.len() {
                continue;
            }
            buf[di] = p.bgra[si];
            buf[di + 1] = p.bgra[si + 1];
            buf[di + 2] = p.bgra[si + 2];
            buf[di + 3] = 255;
        }
    }
}
