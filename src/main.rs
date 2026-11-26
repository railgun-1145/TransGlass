#![windows_subsystem = "windows"]

use dashmap::DashMap;
use eframe::egui;
use lazy_static::lazy_static;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use self_update::backends::github::Update;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, AtomicU32, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::sync::RwLock;
use std::thread;
use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    MouseButton, TrayIcon, TrayIconBuilder, TrayIconEvent,
};
use windows::core::PCWSTR;
use windows::core::w;
use windows::Win32::Foundation::*;
use windows::Win32::System::Threading::*;

use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

mod selftest;

// --- 核心状态注册表 ---
#[derive(Clone)]
struct WindowState {
    original_ex_style: u32,
    current_alpha: u8,
    original_is_topmost: bool,
    user_pref_topmost: bool,
    mouse_passthrough: bool,
    pen_passthrough: bool,
    title: String,
}

lazy_static! {
    static ref GLOBAL_REGISTRY: DashMap<isize, WindowState> = DashMap::new();
    static ref MOUSE_BINDINGS: RwLock<MouseBindings> = RwLock::new(MouseBindings::default());
}

#[derive(Clone, Copy)]
struct PendingChange {
    hwnd_val: isize,
    alpha: Option<u8>,
    topmost: Option<bool>,
    mouse_passthrough: Option<bool>,
    pen_passthrough: Option<bool>,
}

static EGUI_CTX: OnceLock<egui::Context> = OnceLock::new();
static WINDOW_VISIBLE: AtomicBool = AtomicBool::new(true);
static EXITING: AtomicBool = AtomicBool::new(false);
static APP_EXIT_REQUESTED: AtomicBool = AtomicBool::new(false);
static ROOT_HWND: AtomicIsize = AtomicIsize::new(0);
static MOUSE_HOOK: AtomicIsize = AtomicIsize::new(0);
static HOTKEY_THREAD_ID: AtomicU32 = AtomicU32::new(0);
static INPUT_MODE: AtomicI32 = AtomicI32::new(0);
/// 最近一次识别到的输入设备：-1 未知、0 鼠标、1 笔（面板上显示，也用来避免反复改样式）
static LAST_INPUT_IS_PEN: AtomicI32 = AtomicI32::new(-1);
/// 是否有窗口处于"只开鼠标点透/只开笔点透"的单设备模式；没有就完全跳过低级钩子的重活
static DEVICE_TRACKING: AtomicBool = AtomicBool::new(false);

// ---- 可选性能计数（设 TRANSGLASS_DEBUG_PERF=1 才启用，关闭时只有一次原子读开销）----
static PERF_ON: AtomicBool = AtomicBool::new(false);
static PERF_HOOK_CALLS: AtomicU64 = AtomicU64::new(0);
static PERF_FAST_CALLS: AtomicU64 = AtomicU64::new(0);
static PERF_TRACK_CALLS: AtomicU64 = AtomicU64::new(0);
static PERF_HOOK_NANOS: AtomicU64 = AtomicU64::new(0);
static PERF_STYLE_FLIPS: AtomicU64 = AtomicU64::new(0);

/// 钩子耗时统计守卫：只在 PERF_ON 时取一次 Instant，Drop 时累加纳秒。
struct PerfGuard {
    start: Option<std::time::Instant>,
}

impl PerfGuard {
    fn new() -> Self {
        if PERF_ON.load(Ordering::Relaxed) {
            PERF_HOOK_CALLS.fetch_add(1, Ordering::Relaxed);
            Self {
                start: Some(std::time::Instant::now()),
            }
        } else {
            Self { start: None }
        }
    }
}

impl Drop for PerfGuard {
    fn drop(&mut self) {
        if let Some(start) = self.start {
            PERF_HOOK_NANOS.fetch_add(start.elapsed().as_nanos() as u64, Ordering::Relaxed);
        }
    }
}
fn perf_dump() {
    if !PERF_ON.load(Ordering::Relaxed) {
        return;
    }
    let calls = PERF_HOOK_CALLS.load(Ordering::Relaxed);
    let nanos = PERF_HOOK_NANOS.load(Ordering::Relaxed);
    let avg = if calls > 0 { nanos as f64 / calls as f64 } else { 0.0 };
    let text = format!(
        "hook_calls={}\nfast_path={}\ntracking_path={}\nstyle_flips={}\ntotal_ms={:.3}\navg_us={:.2}\n",
        calls,
        PERF_FAST_CALLS.load(Ordering::Relaxed),
        PERF_TRACK_CALLS.load(Ordering::Relaxed),
        PERF_STYLE_FLIPS.load(Ordering::Relaxed),
        nanos as f64 / 1_000_000.0,
        avg / 1000.0
    );
    let _ = fs::write(get_config_path().with_file_name("transglass_perf.txt"), text);
}
/// 命中测试缓存：(root_hwnd, left, top, right, bottom)。
/// 快速拖动/高采样率笔时，指针大量事件落在同一个窗口，没必要每条都 WindowFromPoint + GetAncestor。
static HIT_CACHE: std::sync::Mutex<Option<(isize, i32, i32, i32, i32)>> = std::sync::Mutex::new(None);

unsafe fn hit_root_at(x: i32, y: i32) -> Option<HWND> {
    if let Ok(cache) = HIT_CACHE.lock() {
        if let Some((h, l, t, r, b)) = *cache {
            if h != 0 && x >= l && x < r && y >= t && y < b {
                return Some(HWND(h as *mut _));
            }
        }
    }
    let hit = WindowFromPoint(POINT { x, y });
    if hit.0.is_null() {
        return None;
    }
    let root = GetAncestor(hit, GA_ROOT);
    if root.0.is_null() {
        return None;
    }
    let mut rect = RECT::default();
    let ok = GetWindowRect(root, &mut rect).is_ok();
    if let Ok(mut slot) = HIT_CACHE.lock() {
        *slot = if ok {
            Some((root.0 as isize, rect.left, rect.top, rect.right, rect.bottom))
        } else {
            None
        };
    }
    Some(root)
}
static LAST_DEVICE: AtomicI32 = AtomicI32::new(-1);
static LAST_DEVICE_HWND: AtomicIsize = AtomicIsize::new(0);
const TRANSG_GLASS_INJECT_EXTRA_INFO: usize = 0x5452474Cu64 as usize;
const WM_RELOAD_HOTKEYS: u32 = WM_APP + 77;

/// 热键优先模式：true = 用 WH_KEYBOARD_LL 抢在其它程序前面吞掉组合键
static PRIORITY_MODE: AtomicBool = AtomicBool::new(false);
static KEYBOARD_HOOK: AtomicIsize = AtomicIsize::new(0);
/// 优先模式下当前生效的组合键快照：(注册 id, modifiers 位, vk)
static HOTKEY_MATCH: RwLock<Vec<(i32, u32, u32)>> = RwLock::new(Vec::new());
/// 抑制长按重复触发（RegisterHotKey 不会连发，钩子会）
static LAST_FIRED_VK: AtomicU32 = AtomicU32::new(0);

const QUIT_EVENT_NAME: PCWSTR = w!("Local\\TransGlass_Quit_Event");

/// 把当前配置里的组合键做成快照，供键盘钩子在按键回调里无阻塞读取。
fn refresh_hotkey_match(cfg: &HotkeyConfig) {
    let mut list = Vec::new();
    for (id, _name, spec) in hotkey_slots(cfg) {
        let mods = unsafe { parse_modifiers(&spec.modifiers) };
        if let Some(vk) = unsafe { parse_vk(&spec.key) } {
            list.push((id, mods.0, vk));
        }
    }
    if let Ok(mut slot) = HOTKEY_MATCH.write() {
        *slot = list;
    }
}

unsafe fn current_hotkey_mods() -> u32 {
    // 与 parse_modifiers 的位定义保持一致：MOD_ALT=1, MOD_CONTROL=2, MOD_SHIFT=4, MOD_WIN=8
    let mut m = 0u32;
    if GetAsyncKeyState(VK_MENU.0 as i32) < 0 {
        m |= 1;
    }
    if GetAsyncKeyState(VK_CONTROL.0 as i32) < 0 {
        m |= 2;
    }
    if GetAsyncKeyState(VK_SHIFT.0 as i32) < 0 {
        m |= 4;
    }
    if GetAsyncKeyState(VK_LWIN.0 as i32) < 0 || GetAsyncKeyState(VK_RWIN.0 as i32) < 0 {
        m |= 8;
    }
    m
}

/// 优先模式的核心：低级键盘钩子。命中已配置的组合键就投递给热键线程并吞掉按键，
/// 这样即使别的程序先 RegisterHotKey 占用了这个键，也轮不到它收到。
unsafe extern "system" fn keyboard_priority_hook(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if code < 0 || !PRIORITY_MODE.load(Ordering::Relaxed) {
        return CallNextHookEx(None, code, wparam, lparam);
    }
    let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
    let msg = wparam.0 as u32;

    if msg == WM_KEYUP || msg == WM_SYSKEYUP {
        if kb.vkCode == LAST_FIRED_VK.load(Ordering::Relaxed) {
            LAST_FIRED_VK.store(0, Ordering::Relaxed);
        }
        return CallNextHookEx(None, code, wparam, lparam);
    }
    if msg != WM_KEYDOWN && msg != WM_SYSKEYDOWN {
        return CallNextHookEx(None, code, wparam, lparam);
    }
    // 修饰键自身不参与匹配，否则按下 Alt 就会被当成一次组合
    if matches!(
        kb.vkCode,
        0x10 | 0x11 | 0x12 | 0x5B | 0x5C | 0xA0 | 0xA1 | 0xA2 | 0xA3 | 0xA4 | 0xA5
    ) {
        return CallNextHookEx(None, code, wparam, lparam);
    }

    let mods = current_hotkey_mods();
    let hit = HOTKEY_MATCH.read().ok().and_then(|list| {
        list.iter()
            .find(|(_, m, vk)| *vk == kb.vkCode && *m == mods)
            .map(|(id, _, _)| *id)
    });

    if let Some(id) = hit {
        // 长按不连发（与 RegisterHotKey 行为一致）：记录本次已触发，抬起时清空
        if LAST_FIRED_VK.swap(kb.vkCode, Ordering::Relaxed) != kb.vkCode {
            let tid = HOTKEY_THREAD_ID.load(Ordering::Relaxed);
            if tid != 0 {
                let _ = PostThreadMessageW(tid, WM_HOTKEY, WPARAM(id as usize), LPARAM(0));
            }
        }
        return LRESULT(1);
    }
    CallNextHookEx(None, code, wparam, lparam)
}

unsafe fn install_keyboard_priority_hook() -> bool {
    if KEYBOARD_HOOK.load(Ordering::Relaxed) != 0 {
        return true;
    }
    match SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_priority_hook), None, 0) {
        Ok(hook) => {
            KEYBOARD_HOOK.store(hook.0 as isize, Ordering::SeqCst);
            true
        }
        Err(e) => {
            push_issue(format!(
                "热键优先模式：低级键盘钩子安装失败（{e}），已退回普通注册模式"
            ));
            false
        }
    }
}

unsafe fn uninstall_keyboard_priority_hook() {
    let raw = KEYBOARD_HOOK.swap(0, Ordering::SeqCst);
    if raw != 0 {
        let _ = UnhookWindowsHookEx(HHOOK(raw as *mut core::ffi::c_void));
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum InputMode {
    Auto = 0,
    ForceMouse = 1,
    ForcePen = 2,
}

fn get_input_mode() -> InputMode {
    match INPUT_MODE.load(Ordering::Relaxed) {
        1 => InputMode::ForceMouse,
        2 => InputMode::ForcePen,
        _ => InputMode::Auto,
    }
}

fn input_mode_label() -> &'static str {
    match get_input_mode() {
        InputMode::Auto => "自动",
        InputMode::ForceMouse => "强制鼠标",
        InputMode::ForcePen => "强制笔",
    }
}

fn cycle_input_mode() {
    let next = match get_input_mode() {
        InputMode::Auto => InputMode::ForceMouse,
        InputMode::ForceMouse => InputMode::ForcePen,
        InputMode::ForcePen => InputMode::Auto,
    };
    INPUT_MODE.store(next as i32, Ordering::Relaxed);
    request_ui_repaint();
}

/// 退出路径诊断（设 `TRANSGLASS_DEBUG_EXIT=1` 才写，默认关闭）。
fn debug_exit_note(kind: &str) {
    if std::env::var_os("TRANSGLASS_DEBUG_EXIT").is_none() {
        return;
    }
    let path = std::env::temp_dir().join("transglass_exit.txt");
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        use std::io::Write as _;
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let _ = writeln!(f, "{ms} {kind}");
    }
}

fn request_app_exit() {
    APP_EXIT_REQUESTED.store(true, Ordering::SeqCst);
    debug_exit_note("exit-requested");

    // 兜底：窗口隐藏时 egui 不再重绘，只靠 WM_CLOSE 可能永远走不到退出流程，
    // 进程会一直挂在后台，用户再从托盘启动时就容易看到"图标越来越多"。
    // 正常情况下 eframe 会关窗退出、main 返回时 TrayIcon 被 drop（图标随之删除）；
    // 这里只兜底进程残留。
    thread::spawn(|| {
        thread::sleep(std::time::Duration::from_millis(1500));
        debug_exit_note("fallback-exit");
        unsafe {
            restore_all_windows();
            uninstall_mouse_hook();
            uninstall_keyboard_priority_hook();
        }
        unsafe { ExitProcess(0) };
    });

    let hwnd_val = ROOT_HWND.load(Ordering::Relaxed);
    if hwnd_val != 0 {
        let hwnd = HWND(hwnd_val as *mut _);
        unsafe {
            let _ = PostMessageW(hwnd, WM_CLOSE, WPARAM(0), LPARAM(0));
        }
    }
    if let Some(ctx) = EGUI_CTX.get() {
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        ctx.request_repaint();
    }
}

/// 第二个实例带 `--quit` 启动时，通知已在运行的实例优雅退出。
fn request_running_instance_quit() -> bool {
    unsafe {
        // SYNCHRONIZE | EVENT_MODIFY_STATE：少了后者 SetEvent 会 ACCESS_DENIED
        match OpenEventW(SYNCHRONIZATION_ACCESS_RIGHTS(0x0010_0002), false, QUIT_EVENT_NAME) {
            Ok(event) => {
                let _ = SetEvent(event);
                let _ = CloseHandle(event);
                true
            }
            Err(_) => false,
        }
    }
}

/// 每 300ms 重算"是否需要按设备跟踪点透"，避免在持锁路径上扫描注册表导致自锁。
fn spawn_device_tracking_ticker() {
    thread::spawn(|| loop {
        thread::sleep(std::time::Duration::from_millis(300));
        if APP_EXIT_REQUESTED.load(Ordering::Relaxed) {
            break;
        }
        refresh_device_tracking();
        perf_dump();
    });
}
fn spawn_quit_listener() {
    thread::spawn(|| unsafe {
        if let Ok(event) = CreateEventW(None, true, false, QUIT_EVENT_NAME) {
            let _ = ResetEvent(event);
            WaitForSingleObject(event, INFINITE);
            request_app_exit();
        }
    });
}

struct SingleInstanceGuard(HANDLE);

impl Drop for SingleInstanceGuard {
    fn drop(&mut self) {
        unsafe {
            if !self.0.is_invalid() {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

unsafe fn focus_existing_instance_window() -> bool {
    let title = windows::core::w!("TransGlass 控制面板");
    for _ in 0..40 {
        if let Ok(hwnd) = FindWindowW(PCWSTR::null(), title) {
            let _ = ShowWindow(hwnd, SW_RESTORE);
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = SetForegroundWindow(hwnd);
            return true;
        }
        Sleep(50);
    }
    false
}

fn request_hotkey_reload() {
    let tid = HOTKEY_THREAD_ID.load(Ordering::Relaxed);
    if tid != 0 {
        unsafe {
            let _ = PostThreadMessageW(tid, WM_RELOAD_HOTKEYS, WPARAM(0), LPARAM(0));
        }
    }
}

/// 单实例互斥体名。自检模块也引用它，避免两边各写一份字符串而漂移。
pub(crate) const SINGLE_INSTANCE_NAME: PCWSTR =
    w!("Local\\TransGlass_SingleInstance_C1E0B6B4-18A0-4D5D-9D7A-2C5DFD4D0A2F");

unsafe fn acquire_single_instance() -> Option<SingleInstanceGuard> {
    // 先复位 last error，否则线程上残留的 183 会让新建的互斥体被误判成"已存在"
    SetLastError(ERROR_SUCCESS);
    if let Ok(handle) = CreateMutexW(None, true, SINGLE_INSTANCE_NAME) {
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let _ = CloseHandle(handle);
            let _ = focus_existing_instance_window();
            ExitProcess(0);
        }
        return Some(SingleInstanceGuard(handle));
    }
    None
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MouseAction {
    None,
    Increase,
    Decrease,
    ToggleTopmost,
    ToggleClickThrough,
    TogglePenPassthrough,
    CycleInputMode,
    ResetCurrent,
    ResetAll,
    Update,
}

#[derive(Clone, Copy)]
struct MouseBindings {
    xbutton1: MouseAction,
    xbutton2: MouseAction,
}

impl Default for MouseBindings {
    fn default() -> Self {
        Self {
            xbutton1: MouseAction::Decrease,
            xbutton2: MouseAction::Increase,
        }
    }
}

fn request_ui_repaint() {
    if let Some(ctx) = EGUI_CTX.get() {
        ctx.request_repaint();
    }
}

fn parse_mouse_action(s: &str) -> MouseAction {
    match s.trim().to_lowercase().as_str() {
        "increase" | "inc" | "up" => MouseAction::Increase,
        "decrease" | "dec" | "down" => MouseAction::Decrease,
        "toggle_topmost" | "topmost" | "toggle_top" => MouseAction::ToggleTopmost,
        "toggle_click_through" | "toggle_mouse_passthrough" | "click_through" | "toggle_click" => {
            MouseAction::ToggleClickThrough
        }
        "toggle_pen_passthrough" | "pen_passthrough" => MouseAction::TogglePenPassthrough,
        "cycle_input_mode" | "toggle_input_mode" | "input_mode" => MouseAction::CycleInputMode,
        "reset_current" | "reset" => MouseAction::ResetCurrent,
        "reset_all" => MouseAction::ResetAll,
        "update" => MouseAction::Update,
        _ => MouseAction::None,
    }
}

fn set_mouse_bindings(cfg: &HotkeyConfig) {
    let mut b = MouseBindings::default();
    if let Some(spec) = cfg.mouse.as_ref() {
        if let Some(s) = spec.xbutton1.as_ref() {
            b.xbutton1 = parse_mouse_action(s);
        }
        if let Some(s) = spec.xbutton2.as_ref() {
            b.xbutton2 = parse_mouse_action(s);
        }
    }
    if let Ok(mut w) = MOUSE_BINDINGS.write() {
        *w = b;
    }
}

unsafe fn install_mouse_hook() {
    if MOUSE_HOOK.load(Ordering::SeqCst) != 0 {
        return;
    }
    let hook = SetWindowsHookExW(
        WH_MOUSE_LL,
        Some(mouse_hook_proc),
        HINSTANCE(std::ptr::null_mut()),
        0,
    );
    if let Ok(hook) = hook {
        if !hook.0.is_null() {
            MOUSE_HOOK.store(hook.0 as isize, Ordering::SeqCst);
        }
    }
}

unsafe fn uninstall_mouse_hook() {
    let hook = MOUSE_HOOK.swap(0, Ordering::SeqCst);
    if hook != 0 {
        let _ = UnhookWindowsHookEx(HHOOK(hook as *mut _));
    }
}

unsafe fn set_hwnd_mouse_transparent(hwnd: HWND, enabled: bool) {
    let current = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
    let next = if enabled {
        current | WS_EX_TRANSPARENT.0
    } else {
        current & !WS_EX_TRANSPARENT.0
    };
    if next == current {
        return;
    }
    let _ = SetWindowLongW(hwnd, GWL_EXSTYLE, next as i32);
    let _ = SetWindowPos(
        hwnd,
        HWND(0 as *mut _),
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_FRAMECHANGED | SWP_NOZORDER | SWP_NOOWNERZORDER,
    );
}

/// 笔识别：只认 Windows 的"指针提升"标记（0xFF515700 | cursorId）。
/// 钩子线程里 GetCurrentInputMessageSource 必然是 IMDT_UNAVAILABLE，调它是白费一次系统调用，
/// 高频笔（1000Hz）下这个差别会被放大，所以热路径只做位运算。
#[inline]
unsafe fn is_pen_input(info: &MSLLHOOKSTRUCT) -> bool {
    (info.dwExtraInfo & 0xFFFFFF00) == 0xFF515700
}

unsafe extern "system" fn mouse_hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let _perf = PerfGuard::new();
    if code == HC_ACTION as i32 {
        let msg = wparam.0 as u32;
        let info = *(lparam.0 as *const MSLLHOOKSTRUCT);
        if info.dwExtraInfo == TRANSG_GLASS_INJECT_EXTRA_INFO {
            return CallNextHookEx(
                HHOOK(MOUSE_HOOK.load(Ordering::SeqCst) as *mut _),
                code,
                wparam,
                lparam,
            );
        }

        if msg == WM_XBUTTONDOWN {
            let button = ((info.mouseData >> 16) & 0xffff) as u16;
            let action = if let Ok(r) = MOUSE_BINDINGS.read() {
                match button {
                    1 => r.xbutton1,
                    2 => r.xbutton2,
                    _ => MouseAction::None,
                }
            } else {
                MouseAction::None
            };
            if action != MouseAction::None {
                let pt = POINT {
                    x: info.pt.x,
                    y: info.pt.y,
                };
                let hit = WindowFromPoint(pt);
                if hit.0.is_null() {
                    return CallNextHookEx(
                        HHOOK(MOUSE_HOOK.load(Ordering::SeqCst) as *mut _),
                        code,
                        wparam,
                        lparam,
                    );
                }
                let hwnd = GetAncestor(hit, GA_ROOT);
                if hwnd.0.is_null() || is_own_hwnd(hwnd) || is_shell_hwnd(hwnd) {
                    return CallNextHookEx(
                        HHOOK(MOUSE_HOOK.load(Ordering::SeqCst) as *mut _),
                        code,
                        wparam,
                        lparam,
                    );
                }
                match action {
                    MouseAction::Increase => {
                        let _ = adjust_window_transparency(hwnd, 25);
                    }
                    MouseAction::Decrease => {
                        let _ = adjust_window_transparency(hwnd, -25);
                    }
                    MouseAction::ToggleTopmost => {
                        toggle_topmost(hwnd);
                    }
                    MouseAction::ToggleClickThrough => {
                        toggle_mouse_passthrough(hwnd);
                    }
                    MouseAction::TogglePenPassthrough => {
                        toggle_pen_passthrough(hwnd);
                    }
                    MouseAction::CycleInputMode => {
                        cycle_input_mode();
                    }
                    MouseAction::ResetCurrent => {
                        restore_window(hwnd);
                    }
                    MouseAction::ResetAll => {
                        restore_all_windows();
                    }
                    MouseAction::Update => {
                        thread::spawn(|| {
                            let _ = run_self_update();
                        });
                    }
                    MouseAction::None => {}
                }
            }
        } else if matches!(
            msg,
            WM_LBUTTONDOWN
                | WM_LBUTTONUP
                | WM_RBUTTONDOWN
                | WM_RBUTTONUP
                | WM_MBUTTONDOWN
                | WM_MBUTTONUP
                | WM_MOUSEWHEEL
                | WM_MOUSEHWHEEL
                | WM_MOUSEMOVE
        ) {
            // 快路径：面板不可见、也没有"单设备点透"窗口时，钩子里什么都不用做
            if !WINDOW_VISIBLE.load(Ordering::Relaxed) && !DEVICE_TRACKING.load(Ordering::Relaxed) {
                PERF_FAST_CALLS.fetch_add(1, Ordering::Relaxed);
                return CallNextHookEx(
                    HHOOK(MOUSE_HOOK.load(Ordering::SeqCst) as *mut _),
                    code,
                    wparam,
                    lparam,
                );
            }

            // 状态行只需要设备类型：只读 ExtraInfo，不查窗口、不调 API
            if WINDOW_VISIBLE.load(Ordering::Relaxed) {
                let quick_pen = (info.dwExtraInfo & 0xFFFFFF00) == 0xFF515700;
                let dev = if quick_pen { 1 } else { 0 };
                if LAST_INPUT_IS_PEN.swap(dev, Ordering::Relaxed) != dev {
                    request_ui_repaint();
                }
            }

            // 没有单设备点透的窗口 → 后面的 WindowFromPoint / 注册表查询全部跳过
            if !DEVICE_TRACKING.load(Ordering::Relaxed) || GLOBAL_REGISTRY.is_empty() {
                PERF_FAST_CALLS.fetch_add(1, Ordering::Relaxed);
                return CallNextHookEx(
                    HHOOK(MOUSE_HOOK.load(Ordering::SeqCst) as *mut _),
                    code,
                    wparam,
                    lparam,
                );
            }

            PERF_TRACK_CALLS.fetch_add(1, Ordering::Relaxed);
            let root = match hit_root_at(info.pt.x, info.pt.y) {
                Some(hwnd) => hwnd,
                None => {
                    return CallNextHookEx(
                        HHOOK(MOUSE_HOOK.load(Ordering::SeqCst) as *mut _),
                        code,
                        wparam,
                        lparam,
                    )
                }
            };

            let root_val = root.0 as isize;
            if let Some(state) = GLOBAL_REGISTRY.get(&root_val) {
                if state.mouse_passthrough != state.pen_passthrough {
                    let is_pen = match get_input_mode() {
                        InputMode::Auto => is_pen_input(&info),
                        InputMode::ForceMouse => false,
                        InputMode::ForcePen => true,
                    };
                    let dev = if is_pen { 1 } else { 0 };
                    let passthrough = if is_pen {
                        state.pen_passthrough
                    } else {
                        state.mouse_passthrough
                    };
                    // 只有"设备或窗口变了"才改扩展样式：以前每个鼠标移动都改一次，笔悬停时会疯狂抖动
                    let same = LAST_DEVICE.load(Ordering::Relaxed) == dev
                        && LAST_DEVICE_HWND.load(Ordering::Relaxed) == root_val;
                    if !same {
                        LAST_DEVICE.store(dev, Ordering::Relaxed);
                        LAST_DEVICE_HWND.store(root_val, Ordering::Relaxed);
                        set_hwnd_mouse_transparent(root, passthrough);
                        PERF_STYLE_FLIPS.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
    }
    CallNextHookEx(
        HHOOK(MOUSE_HOOK.load(Ordering::SeqCst) as *mut _),
        code,
        wparam,
        lparam,
    )
}

fn show_root_window() {
    WINDOW_VISIBLE.store(true, Ordering::Relaxed);
    if let Some(ctx) = EGUI_CTX.get() {
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
    }
    let hwnd_val = ROOT_HWND.load(Ordering::Relaxed);
    if hwnd_val != 0 {
        let hwnd = HWND(hwnd_val as *mut _);
        unsafe {
            let _ = ShowWindow(hwnd, SW_RESTORE);
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = SetForegroundWindow(hwnd);
        }
    }
    if let Some(ctx) = EGUI_CTX.get() {
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        ctx.request_repaint();
    }
}

fn hide_root_window() {
    WINDOW_VISIBLE.store(false, Ordering::Relaxed);
    if let Some(ctx) = EGUI_CTX.get() {
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
    }
    let hwnd_val = ROOT_HWND.load(Ordering::Relaxed);
    if hwnd_val != 0 {
        let hwnd = HWND(hwnd_val as *mut _);
        unsafe {
            let _ = ShowWindow(hwnd, SW_MINIMIZE);
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
    }
    if let Some(ctx) = EGUI_CTX.get() {
        ctx.request_repaint();
    }
}

// --- 底层核心逻辑 ---

unsafe fn get_window_title(hwnd: HWND) -> String {
    let mut text: [u16; 512] = [0; 512];
    let len = GetWindowTextW(hwnd, &mut text);
    if len > 0 {
        String::from_utf16_lossy(&text[..len as usize])
    } else {
        format!("未知窗口 ({:?})", hwnd.0)
    }
}

unsafe fn adjust_window_transparency(hwnd: HWND, delta: i32) -> Result<(), String> {
    if hwnd.0.is_null() {
        return Err("Invalid HWND".into());
    }
    if is_own_hwnd(hwnd) {
        return Ok(());
    }
    if is_shell_hwnd(hwnd) {
        return Ok(());
    }
    let hwnd_val = hwnd.0 as isize;

    let mut state = if let Some(s) = GLOBAL_REGISTRY.get_mut(&hwnd_val) {
        s
    } else {
        let ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        let is_top = (ex_style & WS_EX_TOPMOST.0) != 0;
        let title = get_window_title(hwnd);
        GLOBAL_REGISTRY.insert(
            hwnd_val,
            WindowState {
                original_ex_style: ex_style,
                current_alpha: 255,
                original_is_topmost: is_top,
                user_pref_topmost: is_top,
                mouse_passthrough: false,
                pen_passthrough: false,
                title,
            },
        );
        GLOBAL_REGISTRY.get_mut(&hwnd_val).unwrap()
    };

    let new_alpha = (state.current_alpha as i32 + delta).clamp(30, 255) as u8;
    state.current_alpha = new_alpha;

    apply_transparency_to_hwnd(
        hwnd,
        new_alpha,
        state.user_pref_topmost,
        state.mouse_passthrough,
        state.pen_passthrough,
    )?;
    request_ui_repaint();
    Ok(())
}

unsafe fn apply_transparency_to_hwnd(
    hwnd: HWND,
    alpha: u8,
    topmost: bool,
    mouse_passthrough: bool,
    pen_passthrough: bool,
) -> Result<(), String> {
    let current_style = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
    let mut next_style = current_style | WS_EX_LAYERED.0;
    if mouse_passthrough && pen_passthrough {
        next_style |= WS_EX_TRANSPARENT.0;
    } else if !mouse_passthrough && !pen_passthrough {
        next_style &= !WS_EX_TRANSPARENT.0;
    }
    if next_style != current_style {
        let _ = SetWindowLongW(hwnd, GWL_EXSTYLE, next_style as i32);
    }
    SetLayeredWindowAttributes(hwnd, COLORREF(0), alpha, LWA_ALPHA).map_err(|e| e.to_string())?;

    let pos = if topmost {
        HWND_TOPMOST
    } else {
        HWND_NOTOPMOST
    };
    let _ = SetWindowPos(
        hwnd,
        pos,
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS | SWP_FRAMECHANGED,
    );
    Ok(())
}

/// 只有存在"单设备点透"的窗口时，钩子才需要逐事件判断笔/鼠标；否则一律走快路径。
/// 只在用户操作（改透明度/开关/还原）时调用，不在每条鼠标消息里扫描。
fn refresh_device_tracking() {
    let need = GLOBAL_REGISTRY
        .iter()
        .any(|kv| kv.value().mouse_passthrough != kv.value().pen_passthrough);
    DEVICE_TRACKING.store(need, Ordering::Relaxed);
}

unsafe fn toggle_topmost(hwnd: HWND) {
    if hwnd.0.is_null() {
        return;
    }
    if is_own_hwnd(hwnd) {
        return;
    }
    if is_shell_hwnd(hwnd) {
        return;
    }
    if let Some(mut state) = GLOBAL_REGISTRY.get_mut(&(hwnd.0 as isize)) {
        state.user_pref_topmost = !state.user_pref_topmost;
        let _ = apply_transparency_to_hwnd(
            hwnd,
            state.current_alpha,
            state.user_pref_topmost,
            state.mouse_passthrough,
            state.pen_passthrough,
        );
    }
    request_ui_repaint();
}

unsafe fn toggle_mouse_passthrough(hwnd: HWND) {
    if hwnd.0.is_null() {
        return;
    }
    if is_own_hwnd(hwnd) {
        return;
    }
    if is_shell_hwnd(hwnd) {
        return;
    }
    let hwnd_val = hwnd.0 as isize;
    if GLOBAL_REGISTRY.get(&hwnd_val).is_none() {
        let ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        let is_top = (ex_style & WS_EX_TOPMOST.0) != 0;
        let title = get_window_title(hwnd);
        GLOBAL_REGISTRY.insert(
            hwnd_val,
            WindowState {
                original_ex_style: ex_style,
                current_alpha: 255,
                original_is_topmost: is_top,
                user_pref_topmost: is_top,
                mouse_passthrough: false,
                pen_passthrough: false,
                title,
            },
        );
    }
    if let Some(mut state) = GLOBAL_REGISTRY.get_mut(&hwnd_val) {
        state.mouse_passthrough = !state.mouse_passthrough;
        let _ = apply_transparency_to_hwnd(
            hwnd,
            state.current_alpha,
            state.user_pref_topmost,
            state.mouse_passthrough,
            state.pen_passthrough,
        );
    }
    request_ui_repaint();
}

unsafe fn toggle_pen_passthrough(hwnd: HWND) {
    if hwnd.0.is_null() {
        return;
    }
    if is_own_hwnd(hwnd) {
        return;
    }
    if is_shell_hwnd(hwnd) {
        return;
    }
    let hwnd_val = hwnd.0 as isize;
    if GLOBAL_REGISTRY.get(&hwnd_val).is_none() {
        let ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        let is_top = (ex_style & WS_EX_TOPMOST.0) != 0;
        let title = get_window_title(hwnd);
        GLOBAL_REGISTRY.insert(
            hwnd_val,
            WindowState {
                original_ex_style: ex_style,
                current_alpha: 255,
                original_is_topmost: is_top,
                user_pref_topmost: is_top,
                mouse_passthrough: false,
                pen_passthrough: false,
                title,
            },
        );
    }
    if let Some(mut state) = GLOBAL_REGISTRY.get_mut(&hwnd_val) {
        state.pen_passthrough = !state.pen_passthrough;
        let _ = apply_transparency_to_hwnd(
            hwnd,
            state.current_alpha,
            state.user_pref_topmost,
            state.mouse_passthrough,
            state.pen_passthrough,
        );
    }
    request_ui_repaint();
}

unsafe fn restore_window(hwnd: HWND) {
    if hwnd.0.is_null() {
        return;
    }
    if let Some((_, state)) = GLOBAL_REGISTRY.remove(&(hwnd.0 as isize)) {
        // 移除后由 refresh_device_tracking() 统一重算
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA);
        let _ = SetWindowLongW(hwnd, GWL_EXSTYLE, state.original_ex_style as i32);
        if !state.original_is_topmost {
            let _ = SetWindowPos(
                hwnd,
                HWND_NOTOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS,
            );
        }
    }
    refresh_device_tracking();
    request_ui_repaint();
}

unsafe fn restore_all_windows() {
    let hwnds: Vec<isize> = GLOBAL_REGISTRY.iter().map(|kv| *kv.key()).collect();
    for hwnd_val in hwnds {
        restore_window(HWND(hwnd_val as *mut _));
    }
    request_ui_repaint();
}

unsafe fn is_shell_hwnd(hwnd: HWND) -> bool {
    if hwnd.0.is_null() {
        return false;
    }
    let desktop = GetDesktopWindow();
    if !desktop.0.is_null() && hwnd.0 == desktop.0 {
        return true;
    }
    let shell = GetShellWindow();
    if !shell.0.is_null() && hwnd.0 == shell.0 {
        return true;
    }

    let mut class_buf: [u16; 256] = [0; 256];
    let len = GetClassNameW(hwnd, &mut class_buf);
    if len <= 0 {
        return false;
    }
    let name = String::from_utf16_lossy(&class_buf[..len as usize]);
    matches!(
        name.as_str(),
        "Progman" | "WorkerW" | "Shell_TrayWnd" | "NotifyIconOverflowWindow"
    )
}

unsafe fn is_own_hwnd(hwnd: HWND) -> bool {
    if hwnd.0.is_null() {
        return false;
    }
    let mut pid: u32 = 0;
    let _ = GetWindowThreadProcessId(hwnd, Some(&mut pid));
    pid != 0 && pid == GetCurrentProcessId()
}

// --- 事件钩子 ---
// --- GUI 应用程序 ---
#[derive(Clone, Copy, PartialEq, Eq)]
enum HotkeyCaptureTarget {
    Increase,
    Decrease,
    ToggleTop,
    ToggleMouse,
    TogglePen,
    CycleInputMode,
    ResetCurrent,
    ResetAll,
    Update,
    Reload,
}

struct TransGlassApp {
    should_exit: bool,
    hotkey_editor_open: bool,
    hotkey_draft: HotkeyConfig,
    hotkey_message: Option<String>,
    hotkey_capture: Option<HotkeyCaptureTarget>,
    hotkey_priority_ui: bool,
    ui_state: UiState,
    always_on_top: bool,
    last_geo: Option<[f32; 4]>,
    geo_changed_at: Option<std::time::Instant>,
    selftest_open: bool,
    selftest_text: String,
    wins: Vec<WinInfo>,
    wins_at: Option<std::time::Instant>,
    selected: Option<isize>,
    issues_ver: u32,
    issues_cache: Vec<String>,
    overview_ver: u32,
    overview_cache: Vec<(String, String, bool)>,
}

impl TransGlassApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        // 1. 设置中文字体 (尝试多个常用路径)
        let mut fonts = egui::FontDefinitions::default();
        let font_paths = [
            "C:\\Windows\\Fonts\\simhei.ttf", // 黑体 (TTF)
            "C:\\Windows\\Fonts\\simkai.ttf", // 楷体 (TTF)
            "C:\\Windows\\Fonts\\msyh.ttc",   // 微软雅黑 (TTC)
            "C:\\Windows\\Fonts\\msyh.ttf",
            "C:\\Windows\\Fonts\\simsun.ttc", // 宋体 (TTC)
            "C:\\Windows\\Fonts\\simsun.ttf",
        ];

        let mut font_loaded = false;
        for path in font_paths {
            if let Ok(font_data) = std::fs::read(path) {
                fonts
                    .font_data
                    .insert("my_font".to_owned(), egui::FontData::from_owned(font_data));
                fonts
                    .families
                    .get_mut(&egui::FontFamily::Proportional)
                    .unwrap()
                    .insert(0, "my_font".to_owned());
                fonts
                    .families
                    .get_mut(&egui::FontFamily::Monospace)
                    .unwrap()
                    .push("my_font".to_owned());
                font_loaded = true;
                break;
            }
        }

        let _ = font_loaded;
        cc.egui_ctx.set_fonts(fonts);

        // 2. 仿 Trae 风格的深色 UI
        // 浅色主题：色值取自 DSH 界面取样（底 #FAFAFA / 卡 #FFFFFF / 边 #E5E7EB / 主色 #4176E6）
        apply_panel_theme(&cc.egui_ctx);
        let _ = EGUI_CTX.set(cc.egui_ctx.clone());

        if let Ok(handle) = cc.window_handle() {
            if let RawWindowHandle::Win32(h) = handle.as_raw() {
                ROOT_HWND.store(h.hwnd.get(), Ordering::Relaxed);
            }
        }

        let ui_state = load_ui_state();
        let always_on_top = ui_state.always_on_top.unwrap_or(false);
        Self {
            should_exit: false,
            hotkey_editor_open: false,
            hotkey_draft: load_or_create_hotkey_config(),
            hotkey_message: None,
            hotkey_capture: None,
            hotkey_priority_ui: current_priority_flag(),
            ui_state,
            always_on_top,
            last_geo: None,
            geo_changed_at: None,
            selftest_open: false,
            selftest_text: String::new(),
            wins: Vec::new(),
            wins_at: None,
            selected: None,
            issues_ver: u32::MAX,
            issues_cache: Vec::new(),
            overview_ver: u32::MAX,
            overview_cache: Vec::new(),
        }
    }

    fn refresh_window_list(&mut self, force: bool) {
        let due = self
            .wins_at
            .map(|t| t.elapsed().as_millis() > 1500)
            .unwrap_or(true);
        if !force && !due {
            return;
        }
        self.wins_at = Some(std::time::Instant::now());
        self.wins = enumerate_windows();
        if let Some(sel) = self.selected {
            if !self.wins.iter().any(|w| w.hwnd == sel) {
                self.selected = None;
            }
        }
    }

    fn nav_windows(&mut self, ui: &mut egui::Ui) {
        let list: Vec<(isize, String, String)> = self
            .wins
            .iter()
            .map(|w| (w.hwnd, w.title.clone(), w.desktop.clone()))
            .collect();
        if list.is_empty() {
            ui.label(egui::RichText::new("没有可见窗口").small().weak());
            return;
        }
        ui.label(egui::RichText::new("单击窗口 = 控制 ｜ 再单击 = 复原 ｜ 右键看设置").small().weak());
        ui.add_space(4.0);
        let mut order: Vec<String> = Vec::new();
        for (_, _, d) in &list {
            if !order.contains(d) {
                order.push(d.clone());
            }
        }
        let cur_sel = self.selected;
        let mut select: Option<isize> = None;
        let mut control_now: Option<isize> = None;
        let mut restore_now: Option<isize> = None;
        for desk in order {
            let items: Vec<&(isize, String, String)> =
                list.iter().filter(|(_, _, d)| *d == desk).collect();
            let managed_in = items
                .iter()
                .filter(|(h, _, _)| GLOBAL_REGISTRY.contains_key(h))
                .count();
            let header = if managed_in > 0 {
                format!("{desk} · {managed_in} 个已管理")
            } else {
                desk.clone()
            };
            egui::CollapsingHeader::new(egui::RichText::new(header).small().strong())
                .default_open(true)
                .id_source(desk.clone())
                .show(ui, |ui| {
                    for (hwnd, title, _) in &items {
                        let managed = GLOBAL_REGISTRY.contains_key(hwnd);
                        let mark = if managed { "●" } else { "○" };
                        let mut text = egui::RichText::new(format!("{mark} {title}")).small();
                        if !managed {
                            text = text.weak();
                        }
                        let resp = ui
                            .selectable_label(cur_sel == Some(*hwnd), text)
                            .on_hover_text(if managed {
                                "点击复原这个窗口（再点一次重新控制）"
                            } else {
                                "点击控制这个窗口（透明度 90%）；再点一次复原"
                            });
                        if resp.clicked() {
                            select = Some(*hwnd);
                            if managed {
                                restore_now = Some(*hwnd);
                            } else {
                                control_now = Some(*hwnd);
                            }
                        }
                        resp.context_menu(|ui| {
                            ui.label(egui::RichText::new(title.as_str()).strong().small());
                            ui.separator();
                            match GLOBAL_REGISTRY.get(hwnd) {
                                Some(st) => {
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "透明度 {}%",
                                            (st.current_alpha as f32 / 255.0 * 100.0).round() as i32
                                        ))
                                        .small(),
                                    );
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "置顶：{}",
                                            if st.user_pref_topmost { "是" } else { "否" }
                                        ))
                                        .small(),
                                    );
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "鼠标点透：{}",
                                            if st.mouse_passthrough { "开" } else { "关" }
                                        ))
                                        .small(),
                                    );
                                    ui.label(
                                        egui::RichText::new(format!(
                                            "笔点透：{}",
                                            if st.pen_passthrough { "开" } else { "关" }
                                        ))
                                        .small(),
                                    );
                                    ui.separator();
                                    if ui.button("还原这个窗口").clicked() {
                                        restore_now = Some(*hwnd);
                                    }
                                }
                                None => {
                                    ui.label(egui::RichText::new("未管理").small().weak());
                                }
                            }
                        });
                    }
                });
        }
        if let Some(h) = select {
            self.selected = Some(h);
        }
        if let Some(h) = control_now {
            unsafe { control_window(HWND(h as *mut _)) };
            self.refresh_window_list(true);
        }
        if let Some(h) = restore_now {
            unsafe { restore_window(HWND(h as *mut _)) };
            self.refresh_window_list(true);
        }
    }

    fn detail_pane(&mut self, ui: &mut egui::Ui) {
        let Some(sel) = self.selected else {
            ui.vertical_centered(|ui| {
                ui.add_space(140.0);
                ui.label(egui::RichText::new("从左侧选择一个窗口").size(14.0).weak());
            });
            return;
        };
        let info = self.wins.iter().find(|w| w.hwnd == sel).cloned();
        let title = info
            .as_ref()
            .map(|w| w.title.clone())
            .unwrap_or_else(|| format!("HWND {sel:#x}"));
        let desktop = info.as_ref().map(|w| w.desktop.clone()).unwrap_or_default();
        let st = GLOBAL_REGISTRY
            .get(&sel)
            .map(|s| (s.current_alpha, s.user_pref_topmost, s.mouse_passthrough, s.pen_passthrough));
        let (alpha, top, mouse, pen) = st.unwrap_or((255, false, false, false));
        ui.add_space(4.0);
        ui.label(egui::RichText::new(&title).strong().size(17.0));
        ui.add_space(2.0);
        ui.label(
            egui::RichText::new(format!(
                "{desktop} ｜ {} ｜ HWND {sel:#x}",
                if st.is_some() { "已管理" } else { "未管理" }
            ))
            .small()
            .weak(),
        );
        ui.add_space(12.0);
        card(ui, |ui| {
            ui.label(egui::RichText::new("透明度").small().color(TEXT_SECONDARY));
            let mut a = alpha as f32;
            let resp = ui.add(egui::Slider::new(&mut a, 30.0..=255.0).show_value(false));
            ui.label(egui::RichText::new(format!("{}%", (a / 255.0 * 100.0).round() as i32)).small());
            if resp.changed() {
                unsafe {
                    ensure_managed(HWND(sel as *mut _));
                    set_window_alpha(HWND(sel as *mut _), a as u8);
                }
            }
            ui.add_space(10.0);
            ui.label(egui::RichText::new("状态").small().color(TEXT_SECONDARY));
            ui.horizontal(|ui| {
                if ui.selectable_label(top, "置顶").clicked() {
                    unsafe { toggle_topmost(HWND(sel as *mut _)) };
                }
                ui.add_space(6.0);
                if ui.selectable_label(mouse, "鼠标点透").clicked() {
                    unsafe { toggle_mouse_passthrough(HWND(sel as *mut _)) };
                }
                ui.add_space(6.0);
                if ui.selectable_label(pen, "笔点透").clicked() {
                    unsafe { toggle_pen_passthrough(HWND(sel as *mut _)) };
                }
            });
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button("♻ 还原这个窗口").clicked() {
                    unsafe { restore_window(HWND(sel as *mut _)) };
                    self.refresh_window_list(true);
                }
                if ui.button("⟳ 刷新列表").clicked() {
                    self.refresh_window_list(true);
                }
            });
        });
    }
    fn open_hotkey_editor(&mut self) {
        self.hotkey_draft = load_or_create_hotkey_config();
        self.hotkey_message = None;
        self.hotkey_capture = None;
        self.hotkey_editor_open = true;
    }

    fn validate_hotkey_spec(spec: &HotkeySpec) -> Result<(), String> {
        unsafe {
            let mods = parse_modifiers(&spec.modifiers);
            if mods.0 == 0 {
                return Err("修饰键不能为空（建议至少包含 ALT/CTRL/SHIFT/WIN 之一）".into());
            }
            let vk = parse_vk(&spec.key);
            if vk.is_none() {
                return Err(format!("无效按键：{}", spec.key));
            }
        }
        Ok(())
    }

    fn validate_config(cfg: &HotkeyConfig) -> Result<(), String> {
        let mut used: std::collections::HashMap<(String, String), String> = std::collections::HashMap::new();

        let mut check = |name: &str, spec: &HotkeySpec| -> Result<(), String> {
            Self::validate_hotkey_spec(spec)?;
            let key = (spec.modifiers.trim().to_uppercase(), spec.key.trim().to_uppercase());
            if let Some(prev) = used.insert(key.clone(), name.to_string()) {
                return Err(format!("快捷键冲突：{} 与 {} 使用了相同组合（{} + {}）", prev, name, key.0, key.1));
            }
            Ok(())
        };

        check("增加透明度", &cfg.increase)?;
        check("减少透明度", &cfg.decrease)?;
        check("切换置顶", &cfg.toggle_top)?;
        if let Some(spec) = cfg.toggle_click_through.as_ref() {
            check("切换鼠标点透", spec)?;
        }
        if let Some(spec) = cfg.toggle_pen_passthrough.as_ref() {
            check("切换笔点透", spec)?;
        }
        if let Some(spec) = cfg.input_mode_cycle.as_ref() {
            check("输入模式切换", spec)?;
        }
        check("还原当前窗口", &cfg.reset_current)?;
        check("还原所有窗口", &cfg.reset_all)?;
        check("检查更新", &cfg.update)?;
        if let Some(spec) = cfg.reload.as_ref() {
            check("重载配置", spec)?;
        }
        Ok(())
    }

    fn apply_hotkey_capture(&mut self, mods: String, key: String) {
        if let Some(target) = self.hotkey_capture.take() {
            let spec = HotkeySpec { modifiers: mods, key };
            if let Err(e) = Self::validate_hotkey_spec(&spec) {
                self.hotkey_message = Some(e);
                return;
            }
            match target {
                HotkeyCaptureTarget::Increase => self.hotkey_draft.increase = spec,
                HotkeyCaptureTarget::Decrease => self.hotkey_draft.decrease = spec,
                HotkeyCaptureTarget::ToggleTop => self.hotkey_draft.toggle_top = spec,
                HotkeyCaptureTarget::ToggleMouse => self.hotkey_draft.toggle_click_through = Some(spec),
                HotkeyCaptureTarget::TogglePen => self.hotkey_draft.toggle_pen_passthrough = Some(spec),
                HotkeyCaptureTarget::CycleInputMode => self.hotkey_draft.input_mode_cycle = Some(spec),
                HotkeyCaptureTarget::ResetCurrent => self.hotkey_draft.reset_current = spec,
                HotkeyCaptureTarget::ResetAll => self.hotkey_draft.reset_all = spec,
                HotkeyCaptureTarget::Update => self.hotkey_draft.update = spec,
                HotkeyCaptureTarget::Reload => self.hotkey_draft.reload = Some(spec),
            }
            self.hotkey_message = Some("已录入快捷键".into());
        }
    }

    fn save_and_reload_hotkeys(&mut self) {
        if let Err(e) = Self::validate_config(&self.hotkey_draft) {
            self.hotkey_message = Some(e);
            return;
        }
        let path = get_config_path();
        match serde_json::to_string_pretty(&self.hotkey_draft)
            .map_err(|e| e.to_string())
            .and_then(|s| fs::write(&path, s).map_err(|e| e.to_string()))
        {
            Ok(_) => {
                request_hotkey_reload();
                self.hotkey_message = Some("已保存并应用".into());
            }
            Err(e) => {
                self.hotkey_message = Some(format!("保存失败：{}", e));
            }
        }
    }
}

fn egui_key_to_vk_name(key: egui::Key) -> Option<String> {
    use egui::Key::*;
    let s = match key {
        A => "A",
        B => "B",
        C => "C",
        D => "D",
        E => "E",
        F => "F",
        G => "G",
        H => "H",
        I => "I",
        J => "J",
        K => "K",
        L => "L",
        M => "M",
        N => "N",
        O => "O",
        P => "P",
        Q => "Q",
        R => "R",
        S => "S",
        T => "T",
        U => "U",
        V => "V",
        W => "W",
        X => "X",
        Y => "Y",
        Z => "Z",
        Num0 => "0",
        Num1 => "1",
        Num2 => "2",
        Num3 => "3",
        Num4 => "4",
        Num5 => "5",
        Num6 => "6",
        Num7 => "7",
        Num8 => "8",
        Num9 => "9",
        F1 => "F1",
        F2 => "F2",
        F3 => "F3",
        F4 => "F4",
        F5 => "F5",
        F6 => "F6",
        F7 => "F7",
        F8 => "F8",
        F9 => "F9",
        F10 => "F10",
        F11 => "F11",
        F12 => "F12",
        _ => return None,
    };
    Some(s.to_string())
}

fn egui_modifiers_to_string(m: egui::Modifiers) -> String {
    let mut parts: Vec<&'static str> = Vec::new();
    if m.ctrl {
        parts.push("CTRL");
    }
    if m.alt {
        parts.push("ALT");
    }
    if m.shift {
        parts.push("SHIFT");
    }
    parts.join("+")
}

impl eframe::App for TransGlassApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 1. 处理托盘和菜单事件 (逻辑保持不变)

        if APP_EXIT_REQUESTED.load(Ordering::Relaxed) && !self.should_exit {
            self.should_exit = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        // 2. 拦截关闭
        if ctx.input(|i| i.viewport().close_requested()) && !self.should_exit {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            hide_root_window();
        }

        if ctx.input(|i| i.viewport().minimized.unwrap_or(false)) {
            WINDOW_VISIBLE.store(false, Ordering::Relaxed);
        }

        // 记住面板位置/大小：拖动或缩放后停顿约 0.8 秒再落盘。
        // 位置用 outer_rect（窗口外框），尺寸用 inner_rect（客户区），两者配对才不会每次启动漂移。
        {
            // ViewportInfo 的生命周期绑在 input 闭包里，所以在闭包内就把需要的数值取出来
            let (pos, size) = ctx.input(|i| {
                let vp = i.viewport();
                (
                    vp.outer_rect.map(|r| [r.min.x, r.min.y]),
                    vp.inner_rect.map(|r| [r.width(), r.height()]),
                )
            });
            if let (Some(pos), Some(size)) = (pos, size) {
                let geo = [pos[0], pos[1], size[0], size[1]];
                let changed = match self.last_geo {
                    Some(prev) => (prev[0] - geo[0]).abs() > 1.0
                        || (prev[1] - geo[1]).abs() > 1.0
                        || (prev[2] - geo[2]).abs() > 1.0
                        || (prev[3] - geo[3]).abs() > 1.0,
                    None => false,
                };
                if self.last_geo.is_none() {
                    self.last_geo = Some(geo);
                } else if changed {
                    self.last_geo = Some(geo);
                    self.geo_changed_at = Some(std::time::Instant::now());
                    // 关键：拖完窗口后 egui 不再重绘，必须主动排一帧来做去抖落盘
                    ctx.request_repaint_after(std::time::Duration::from_millis(900));
                }
                if let Some(since) = self.geo_changed_at {
                    if since.elapsed().as_millis() > 800 {
                        self.geo_changed_at = None;
                        self.ui_state.x = Some(geo[0]);
                        self.ui_state.y = Some(geo[1]);
                        self.ui_state.w = Some(geo[2]);
                        self.ui_state.h = Some(geo[3]);
                        self.ui_state.always_on_top = Some(self.always_on_top);
                        save_ui_state(&self.ui_state);
                    }
                }
            }
        }

        if self.hotkey_capture.is_some() {
            let mut captured: Option<(String, String)> = None;
            ctx.input(|i| {
                for e in &i.events {
                    if let egui::Event::Key {
                        key,
                        pressed: true,
                        modifiers,
                        ..
                    } = e
                    {
                        if let Some(k) = egui_key_to_vk_name(*key) {
                            captured = Some((egui_modifiers_to_string(*modifiers), k));
                            break;
                        }
                    }
                }
            });
            if let Some((mods, key)) = captured {
                self.apply_hotkey_capture(mods, key);
            }
        }

        // 3. UI 绘制 (更简洁现代的布局)
        // 3. 大界面：左侧导航（桌面分组 + 窗口），右侧详情
        self.refresh_window_list(false);

        egui::TopBottomPanel::bottom("actions_bar").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("♻ 全部还原").clicked() {
                    unsafe { restore_all_windows() };
                    self.refresh_window_list(true);
                }
                if ui.button("🛠 快捷键").clicked() {
                    self.open_hotkey_editor();
                }
                if ui.button("🧪 自检").clicked() {
                    let _ = selftest::run_hosted();
                    self.selftest_text = selftest::last_report();
                    self.selftest_open = true;
                }
                let busy = UPDATE_RUNNING.load(Ordering::Relaxed);
                let label = if busy { "检查中…" } else { "🚀 检查更新" };
                if ui.add_enabled(!busy, egui::Button::new(label)).clicked() {
                    thread::spawn(|| {
                        let _ = run_self_update();
                    });
                }
                if busy {
                    ctx.request_repaint_after(std::time::Duration::from_millis(400));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let mut priority = self.hotkey_priority_ui;
                    if ui.checkbox(&mut priority, "快捷键优先").changed() {
                        self.hotkey_priority_ui = priority;
                        if let Err(e) = save_priority_flag(priority) {
                            push_issue(format!("保存快捷键优先设置失败: {e}"));
                        }
                    }
                });
            });
            if let Ok(msg) = UPDATE_MSG.read() {
                if let Some(msg) = msg.as_ref() {
                    ui.label(egui::RichText::new(msg).small().weak());
                }
            }
            ui.add_space(4.0);
        });

        egui::SidePanel::left("nav_panel")
            .resizable(true)
            .default_width(300.0)
            .width_range(230.0..=460.0)
            .show(ctx, |ui| {
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("TransGlass").color(ACCENT).strong().size(18.0));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let pin = ui
                            .add(egui::Button::new(if self.always_on_top { "📌" } else { "📍" }).frame(false))
                            .on_hover_text(if self.always_on_top { "面板已置顶，点击取消" } else { "点击让面板置顶" });
                        if pin.clicked() {
                            self.always_on_top = !self.always_on_top;
                            self.ui_state.always_on_top = Some(self.always_on_top);
                            save_ui_state(&self.ui_state);
                            ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(
                                if self.always_on_top {
                                    egui::WindowLevel::AlwaysOnTop
                                } else {
                                    egui::WindowLevel::Normal
                                },
                            ));
                        }
                        if ui.button("🗙").on_hover_text("隐藏到托盘").clicked() {
                            hide_root_window();
                        }
                    });
                });
                ui.add_space(6.0);
                if ui
                    .add(egui::Button::new(
                        egui::RichText::new(format!("输入模式：{}", input_mode_label())).small(),
                    ))
                    .on_hover_text("点击切换（Alt+Shift+M）")
                    .clicked()
                {
                    cycle_input_mode();
                }
                ui.add_space(4.0);
                let dev = match LAST_INPUT_IS_PEN.load(Ordering::Relaxed) {
                    1 => "笔",
                    0 => "鼠标",
                    _ => "—",
                };
                ui.label(
                    egui::RichText::new(format!(
                        "最近输入：{dev} ｜ 已管理 {} 个",
                        GLOBAL_REGISTRY.len()
                    ))
                    .small()
                    .weak(),
                );
                ui.add_space(6.0);
                ui.separator();
                ui.add_space(6.0);
                self.nav_windows(ui);
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(10.0);
            let issues = snapshot_issues();
            if !issues.is_empty() {
                egui::Frame::none()
                    .fill(egui::Color32::from_rgb(255, 247, 237))
                    .stroke(egui::Stroke::new(1.0, egui::Color32::from_rgb(0xFD, 0xBA, 0x74)))
                    .rounding(egui::Rounding::same(10.0))
                    .inner_margin(egui::Margin::symmetric(10.0, 6.0))
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new(format!("⚠ {} 条配置/热键问题", issues.len()))
                                .color(egui::Color32::from_rgb(180, 83, 9))
                                .small()
                                .strong(),
                        );
                        for msg in issues.iter().take(4) {
                            ui.label(
                                egui::RichText::new(format!("• {msg}"))
                                    .small()
                                    .color(egui::Color32::from_rgb(146, 64, 14)),
                            );
                        }
                    });
                ui.add_space(10.0);
            }
            self.detail_pane(ui);
        });

        if self.selftest_open {
            let mut open = self.selftest_open;
            egui::Window::new("自检报告")
                .open(&mut open)
                .collapsible(false)
                .default_width(520.0)
                .show(ctx, |ui| {
                    egui::ScrollArea::vertical()
                        .max_height(380.0)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(&self.selftest_text)
                                        .monospace()
                                        .size(11.0),
                                )
                                .wrap(),
                            );
                        });
                    ui.separator();
                    ui.horizontal(|ui| {
                        if ui.button("复制报告").clicked() {
                            ctx.copy_text(self.selftest_text.clone());
                        }
                        if ui.button("重新运行").clicked() {
                            let _ = selftest::run_hosted();
                            self.selftest_text = selftest::last_report();
                        }
                    });
                });
            self.selftest_open = open;
        }

        if self.hotkey_editor_open {
            let defaults = default_config();
            let mut open = self.hotkey_editor_open;
            egui::Window::new("快捷键设置")
                .open(&mut open)
                .collapsible(false)
                .resizable(true)
                .default_width(380.0)
                .show(ctx, |ui| {
                    let path = get_config_path();
                    ui.label(format!("配置文件：{}", path.display()));
                    ui.add_space(8.0);

                    // 统一槽位表驱动：新增热键 = HotkeySlot 加一个变体 + ALL 加一项
                    egui::ScrollArea::vertical()
                        .max_height(430.0)
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            for slot in HotkeySlot::ALL {
                                let optional = slot.optional();
                                let mut enabled = !optional || slot.enabled(&self.hotkey_draft);
                                let mut spec = slot.get(&self.hotkey_draft);
                                ui.group(|ui| {
                                    ui.horizontal(|ui| {
                                        if optional {
                                            if ui.checkbox(&mut enabled, "").changed() {
                                                slot.set_enabled(&mut self.hotkey_draft, enabled);
                                            }
                                        } else {
                                            ui.add_space(18.0);
                                        }
                                        ui.add(egui::Label::new(
                                            egui::RichText::new(slot.label()).small(),
                                        ));
                                        ui.with_layout(
                                            egui::Layout::right_to_left(egui::Align::Center),
                                            |ui| {
                                                if ui
                                                    .add_enabled(
                                                        enabled,
                                                        egui::Button::new("录入").small(),
                                                    )
                                                    .clicked()
                                                {
                                                    self.hotkey_capture =
                                                        Some(slot.capture_target());
                                                    self.hotkey_message =
                                                        Some("请按下组合键".into());
                                                }
                                            },
                                        );
                                    });
                                    ui.horizontal(|ui| {
                                        ui.add_space(18.0);
                                        ui.add_enabled_ui(enabled, |ui| {
                                            hotkey_mods_ui(ui, &mut spec.modifiers);
                                            hotkey_key_ui(ui, &mut spec.key, slot.id());
                                        });
                                    });
                                });
                                if enabled {
                                    slot.set(&mut self.hotkey_draft, spec);
                                }
                            }
                        });

                    ui.add_space(6.0);
                    match Self::validate_config(&self.hotkey_draft) {
                        Ok(()) => {
                            ui.label(egui::RichText::new("组合键均合法且无冲突").small().weak());
                        }
                        Err(err) => {
                            ui.label(
                                egui::RichText::new(format!("配置有问题：{err}"))
                                    .small()
                                    .color(ERROR),
                            );
                        }
                    }

                    ui.add_space(10.0);
                    if self.hotkey_draft.mouse.is_none() {
                        self.hotkey_draft.mouse = defaults.mouse.clone();
                    }
                    if let Some(spec) = self.hotkey_draft.mouse.as_mut() {
                        ui.label("鼠标侧键绑定");

                        let mut x1 = spec.xbutton1.clone().unwrap_or_else(|| "none".into());
                        let mut x2 = spec.xbutton2.clone().unwrap_or_else(|| "none".into());

                        ui.horizontal(|ui| {
                            ui.label("XButton1");
                            egui::ComboBox::from_id_source("xbutton1_combo")
                                .selected_text(x1.as_str())
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(&mut x1, "none".into(), "none");
                                    ui.selectable_value(&mut x1, "decrease".into(), "decrease");
                                    ui.selectable_value(&mut x1, "increase".into(), "increase");
                                    ui.selectable_value(&mut x1, "toggle_topmost".into(), "toggle_topmost");
                                    ui.selectable_value(&mut x1, "toggle_click_through".into(), "toggle_click_through");
                                    ui.selectable_value(&mut x1, "toggle_pen_passthrough".into(), "toggle_pen_passthrough");
                                    ui.selectable_value(&mut x1, "cycle_input_mode".into(), "cycle_input_mode");
                                    ui.selectable_value(&mut x1, "reset_current".into(), "reset_current");
                                    ui.selectable_value(&mut x1, "reset_all".into(), "reset_all");
                                    ui.selectable_value(&mut x1, "update".into(), "update");
                                });
                        });

                        ui.horizontal(|ui| {
                            ui.label("XButton2");
                            egui::ComboBox::from_id_source("xbutton2_combo")
                                .selected_text(x2.as_str())
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(&mut x2, "none".into(), "none");
                                    ui.selectable_value(&mut x2, "decrease".into(), "decrease");
                                    ui.selectable_value(&mut x2, "increase".into(), "increase");
                                    ui.selectable_value(&mut x2, "toggle_topmost".into(), "toggle_topmost");
                                    ui.selectable_value(&mut x2, "toggle_click_through".into(), "toggle_click_through");
                                    ui.selectable_value(&mut x2, "toggle_pen_passthrough".into(), "toggle_pen_passthrough");
                                    ui.selectable_value(&mut x2, "cycle_input_mode".into(), "cycle_input_mode");
                                    ui.selectable_value(&mut x2, "reset_current".into(), "reset_current");
                                    ui.selectable_value(&mut x2, "reset_all".into(), "reset_all");
                                    ui.selectable_value(&mut x2, "update".into(), "update");
                                });
                        });

                        spec.xbutton1 = if x1 == "none" { None } else { Some(x1) };
                        spec.xbutton2 = if x2 == "none" { None } else { Some(x2) };
                    }

                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        if ui.button("保存并应用").clicked() {
                            self.save_and_reload_hotkeys();
                        }
                        if ui.button("恢复默认").clicked() {
                            self.hotkey_draft = default_config();
                            self.hotkey_message = Some("已恢复默认（尚未保存）".into());
                        }
                        if ui.button("关闭").clicked() {
                            self.hotkey_editor_open = false;
                        }
                    });
                    if let Some(msg) = self.hotkey_message.as_ref() {
                        ui.add_space(6.0);
                        ui.label(msg);
                    }
                });
            self.hotkey_editor_open = open;
        }
    }
}

fn create_tray_icon() -> TrayIcon {
    let tray_menu = Menu::new();
    let show_item = MenuItem::with_id("show", "打开 TransGlass", true, None);
    let reset_all_item = MenuItem::with_id("reset_all", "全部窗口还原", true, None);
    let exit_item = MenuItem::with_id("exit", "退出程序", true, None);

    let _ = tray_menu.append_items(&[
        &show_item,
        &PredefinedMenuItem::separator(),
        &reset_all_item,
        &PredefinedMenuItem::separator(),
        &exit_item,
    ]);

    // 加载自定义图标
    let icon = load_custom_icon().unwrap_or_else(|| {
        let mut rgba = Vec::with_capacity(32 * 32 * 4);
        for y in 0..32 {
            for x in 0..32 {
                let dx = x as f32 - 15.5;
                let dy = y as f32 - 15.5;
                let dist = (dx * dx + dy * dy).sqrt();
                if dist < 14.0 {
                    if dist > 11.0 {
                        // 外圈 (更深的蓝色)
                        rgba.extend_from_slice(&[0, 120, 215, 255]);
                    } else {
                        // 内圈 (半透明蓝色，模仿玻璃)
                        rgba.extend_from_slice(&[0, 150, 255, 128]);
                    }
                } else {
                    rgba.extend_from_slice(&[0, 0, 0, 0]);
                }
            }
        }
        tray_icon::Icon::from_rgba(rgba, 32, 32).unwrap()
    });

    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(tray_menu))
        .with_tooltip("TransGlass - 运行中")
        .with_icon(icon)
        .build()
        .unwrap();

    tray.set_show_menu_on_left_click(false);
    tray
}

fn load_custom_icon() -> Option<tray_icon::Icon> {
    // 优先级：icon2.png (用户指定的第二张图片) -> icon.png -> tray_icon.png
    let paths = [
        "icon2.png",
        "icon.png",
        "tray_icon.png",
        "TransGlass_Distribution/icon2.png",
        "TransGlass_Distribution/icon.png",
    ];

    let mut bases: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            bases.push(dir.to_path_buf());
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        bases.push(cwd);
    }
    bases.push(PathBuf::from("."));

    for base in bases {
        for rel in paths {
            let candidate = base.join(rel);
            if let Ok(img) = image::open(&candidate) {
                let rgba = img.to_rgba8();
                let (width, height) = rgba.dimensions();
                if let Ok(icon) = tray_icon::Icon::from_rgba(rgba.into_raw(), width, height) {
                    return Some(icon);
                }
            }
        }
    }
    None
}

fn main() -> Result<(), eframe::Error> {
    // 自检模式：不进 GUI、不建托盘，跑完直接退出
    //   0 = 全部通过，1 = 有失败项，2 = 严格模式下有警告项
    let args: Vec<String> = std::env::args().skip(1).collect();
    // --quit：让已在运行的实例优雅退出（不会新开一个进程，也不会多留一个托盘图标）
    if args.iter().any(|a| a == "--quit") {
        request_running_instance_quit();
        return Ok(());
    }
    if args.iter().any(|a| a == "--self-test") {
        let strict = args.iter().any(|a| a == "--strict");
        std::process::exit(selftest::run(strict));
    }

    unsafe {
        let _ = windows::Win32::System::Console::FreeConsole();
    }

    let _single_instance = unsafe { acquire_single_instance() };
    PERF_ON.store(std::env::var_os("TRANSGLASS_DEBUG_PERF").is_some(), Ordering::Relaxed);
    spawn_quit_listener();
    spawn_device_tracking_ticker();

    thread::spawn(|| {
        while let Ok(event) = MenuEvent::receiver().recv() {
            match event.id.0.as_str() {
                "show" => {
                    show_root_window();
                }
                "reset_all" => unsafe { restore_all_windows() },
                "exit" => {
                    if EXITING.swap(true, Ordering::SeqCst) {
                        continue;
                    }
                    unsafe { restore_all_windows() };
                    unsafe { uninstall_mouse_hook() };
                    request_app_exit();
                }
                _ => {}
            }
        }
    });

    thread::spawn(|| {
        while let Ok(event) = TrayIconEvent::receiver().recv() {
            if matches!(
                event,
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    ..
                } | TrayIconEvent::DoubleClick {
                    button: MouseButton::Left,
                    ..
                }
            ) {
                show_root_window();
            }
        }
    });

    let tray_icon = create_tray_icon();

    thread::spawn(|| unsafe {
        HOTKEY_THREAD_ID.store(GetCurrentThreadId(), Ordering::Relaxed);
        let mut init = MSG::default();
        let _ = PeekMessageW(&mut init, None, 0, 0, PM_NOREMOVE);

        reload_hotkeys_and_mouse();
        install_mouse_hook();

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if msg.message == WM_HOTKEY {
                let hwnd = GetForegroundWindow();
                match msg.wParam.0 as i32 {
                    1 => {
                        let _ = adjust_window_transparency(hwnd, -25);
                    }
                    2 => {
                        let _ = adjust_window_transparency(hwnd, 25);
                    }
                    3 => {
                        toggle_topmost(hwnd);
                    }
                    4 => {
                        toggle_mouse_passthrough(hwnd);
                    }
                    5 => {
                        toggle_pen_passthrough(hwnd);
                    }
                    6 => {
                        restore_window(hwnd);
                    }
                    7 => {
                        restore_all_windows();
                    }
                    8 => {
                        thread::spawn(|| {
                            let _ = run_self_update();
                        });
                    }
                    9 => {
                        reload_hotkeys_and_mouse();
                    }
                    10 => {
                        cycle_input_mode();
                    }
                    _ => {}
                }
            } else if msg.message == WM_RELOAD_HOTKEYS {
                reload_hotkeys_and_mouse();
            }
            let _ = TranslateMessage(&msg);
            let _ = DispatchMessageW(&msg);
        }
        uninstall_mouse_hook();
        restore_all_windows();
    });

    // 复用上次的面板位置/大小/置顶状态；没有记录时给一个放得下新布局的默认尺寸
    let ui_state = load_ui_state();
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([
            ui_state.w.filter(|w| *w >= 860.0).unwrap_or(1000.0),
            ui_state.h.filter(|h| *h >= 560.0).unwrap_or(700.0),
        ])
        .with_title("TransGlass 控制面板")
        .with_visible(true);
    if let (Some(x), Some(y)) = (ui_state.x, ui_state.y) {
        viewport = viewport.with_position([x, y]);
    }
    if ui_state.always_on_top.unwrap_or(false) {
        viewport = viewport.with_window_level(egui::WindowLevel::AlwaysOnTop);
    }
    let options = eframe::NativeOptions {
        viewport,
        // 必须是 true：false 时 eframe 会在关闭后直接 std::process::exit，
        // main 里的局部变量（尤其是 TrayIcon）不会 drop，托盘图标就会变成 ghost
        run_and_return: true,
        ..Default::default()
    };

    debug_exit_note("run-native-enter");
    let result = eframe::run_native(
        "TransGlass 控制面板",
        options,
        Box::new(|cc| Ok(Box::new(TransGlassApp::new(cc)))),
    );
    debug_exit_note("run-native-return");
    // 显式删除托盘图标：drop 会触发 Shell_NotifyIconW(NIM_DELETE)
    drop(tray_icon);
    result
}

static UPDATE_RUNNING: AtomicBool = AtomicBool::new(false);
static UPDATE_MSG: RwLock<Option<String>> = RwLock::new(None);

fn run_self_update() -> Result<(), Box<dyn std::error::Error>> {
    UPDATE_RUNNING.store(true, Ordering::SeqCst);
    if let Ok(mut m) = UPDATE_MSG.write() {
        *m = None;
    }
    let outcome = (|| -> Result<String, Box<dyn std::error::Error>> {
        let current = env!("CARGO_PKG_VERSION");
        let status = Update::configure()
            .repo_owner("railgun-1145")
            .repo_name("TransGlass")
            .bin_name("transglass")
            .show_download_progress(true)
            .current_version(current)
            .build()?
            .update()?;
        Ok(if status.updated() {
            format!("已更新到 v{}，重启程序后生效", status.version())
        } else {
            format!("已是最新版本 v{current}")
        })
    })();
    let text = match &outcome {
        Ok(s) => s.clone(),
        Err(e) => format!("检查更新失败：{e}"),
    };
    if let Ok(mut m) = UPDATE_MSG.write() {
        *m = Some(text);
    }
    UPDATE_RUNNING.store(false, Ordering::SeqCst);
    outcome.map(|_| ())
}

#[derive(Deserialize, Serialize, Clone)]
struct HotkeySpec {
    modifiers: String,
    key: String,
}

#[derive(Deserialize, Serialize, Clone)]
struct MouseBindingsSpec {
    xbutton1: Option<String>,
    xbutton2: Option<String>,
}

#[derive(Deserialize, Serialize, Clone)]
struct HotkeyConfig {
    increase: HotkeySpec,
    decrease: HotkeySpec,
    toggle_top: HotkeySpec,
    toggle_click_through: Option<HotkeySpec>,
    toggle_pen_passthrough: Option<HotkeySpec>,
    input_mode_cycle: Option<HotkeySpec>,
    reset_current: HotkeySpec,
    reset_all: HotkeySpec,
    update: HotkeySpec,
    reload: Option<HotkeySpec>,
    mouse: Option<MouseBindingsSpec>,
    /// true = 用低级键盘钩子强占组合键（即使被其它程序占用也能生效）
    #[serde(default)]
    hotkey_priority: Option<bool>,
}

fn default_config() -> HotkeyConfig {
    HotkeyConfig {
        increase: HotkeySlot::Increase.default_spec(),
        decrease: HotkeySlot::Decrease.default_spec(),
        toggle_top: HotkeySlot::ToggleTop.default_spec(),
        toggle_click_through: Some(HotkeySlot::ToggleClick.default_spec()),
        toggle_pen_passthrough: Some(HotkeySlot::TogglePen.default_spec()),
        input_mode_cycle: Some(HotkeySlot::CycleInputMode.default_spec()),
        reset_current: HotkeySlot::ResetCurrent.default_spec(),
        reset_all: HotkeySlot::ResetAll.default_spec(),
        update: HotkeySlot::Update.default_spec(),
        reload: Some(HotkeySlot::Reload.default_spec()),
        mouse: Some(MouseBindingsSpec {
            xbutton1: Some("decrease".into()),
            xbutton2: Some("increase".into()),
        }),
        hotkey_priority: Some(false),
    }
}

fn get_config_path() -> PathBuf {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("."));
    exe.parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .join("transglass_hotkeys.json")
}

/// 解析失败的配置文件不直接覆盖：先改名备份，再写一份默认配置，用户还能找回原文件。
fn backup_broken_config(path: &Path) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("transglass_hotkeys.json");
    let backup = path.with_file_name(format!("{name}.broken-{stamp}.bak"));
    let _ = fs::rename(path, &backup);
    backup
}

fn load_or_create_hotkey_config() -> HotkeyConfig {
    let path = get_config_path();
    match fs::read_to_string(&path) {
        Ok(data) => {
            // 记事本/某些编辑器存 UTF-8 会带 BOM，serde_json 不认，先剥掉
            let text = data.trim_start_matches('\u{feff}');
            match serde_json::from_str::<HotkeyConfig>(text) {
                Ok(cfg) => return cfg,
                Err(e) => {
                    let backup = backup_broken_config(&path);
                    push_issue(format!(
                        "配置文件解析失败（{e}），原文件已备份为 {}，已改用默认热键",
                        backup.display()
                    ));
                    return write_default_config(&path);
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => write_default_config(&path),
        Err(e) => {
            push_issue(format!(
                "读不到配置文件（{e}），本次使用默认热键，改动不会保存"
            ));
            default_config()
        }
    }
}

fn write_default_config(path: &Path) -> HotkeyConfig {
    let cfg = default_config();
    if let Err(e) = fs::write(path, serde_json::to_string_pretty(&cfg).unwrap_or_default()) {
        push_issue(format!("无法写入默认配置文件（{e}）"));
    }
    cfg
}

unsafe fn parse_modifiers(s: &str) -> HOT_KEY_MODIFIERS {
    let mut m = HOT_KEY_MODIFIERS(0);
    for part in s.split('+') {
        match part.trim().to_uppercase().as_str() {
            "ALT" => m |= MOD_ALT,
            "CTRL" | "CONTROL" => m |= MOD_CONTROL,
            "SHIFT" => m |= MOD_SHIFT,
            "WIN" | "WINDOWS" => m |= MOD_WIN,
            _ => {}
        }
    }
    m
}

/// 把用户可读的配置/热键问题攒起来，供 UI 显示。
///
/// 修 bug 前：`try_register_hotkey` 直接吞掉 `RegisterHotKey` 的结果、
/// `load_or_create_hotkey_config` 解析失败时静默重建默认配置并覆盖原文件，
/// 用户既不知道热键没绑上，也不知道自己的配置被换了。
static RUNTIME_ISSUES: RwLock<Vec<String>> = RwLock::new(Vec::new());
/// 只在内容变化时才让 UI 重新克隆快照，避免每帧加锁+分配
static ISSUES_VERSION: AtomicU32 = AtomicU32::new(0);
static OVERVIEW_VERSION: AtomicU32 = AtomicU32::new(0);

pub(crate) fn push_issue(msg: impl Into<String>) {
    let msg = msg.into();
    if let Ok(mut issues) = RUNTIME_ISSUES.write() {
        if !issues.iter().any(|m| m == &msg) {
            issues.push(msg);
            ISSUES_VERSION.fetch_add(1, Ordering::Relaxed);
        }
    }
}

pub(crate) fn snapshot_issues() -> Vec<String> {
    RUNTIME_ISSUES
        .read()
        .map(|issues| issues.clone())
        .unwrap_or_default()
}

pub(crate) fn clear_issues() {
    if let Ok(mut issues) = RUNTIME_ISSUES.write() {
        if !issues.is_empty() {
            issues.clear();
            ISSUES_VERSION.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// 最近一次热键绑定的结果：(功能名, 组合键文本, 是否注册成功)。
/// 面板底部用它显示"我现在能用哪些键"，不用再去编辑器里翻。
static HOTKEY_OVERVIEW: RwLock<Vec<(String, String, bool)>> = RwLock::new(Vec::new());

pub(crate) fn set_hotkey_overview(rows: Vec<(String, String, bool)>) {
    if let Ok(mut slot) = HOTKEY_OVERVIEW.write() {
        *slot = rows;
        OVERVIEW_VERSION.fetch_add(1, Ordering::Relaxed);
    }
}

pub(crate) fn snapshot_hotkey_overview() -> Vec<(String, String, bool)> {
    HOTKEY_OVERVIEW
        .read()
        .map(|rows| rows.clone())
        .unwrap_or_default()
}

/// 面板自身的界面状态（位置/大小/是否置顶），单独存一个文件，不动热键配置的结构。
#[derive(Serialize, Deserialize, Default, Clone)]
struct UiState {
    x: Option<f32>,
    y: Option<f32>,
    w: Option<f32>,
    h: Option<f32>,
    always_on_top: Option<bool>,
}

fn ui_state_path() -> PathBuf {
    get_config_path().with_file_name("transglass_ui.json")
}

fn load_ui_state() -> UiState {
    match fs::read_to_string(ui_state_path()) {
        Ok(text) => serde_json::from_str(text.trim_start_matches('\u{feff}')).unwrap_or_default(),
        Err(_) => UiState::default(),
    }
}

fn save_ui_state(state: &UiState) {
    if let Ok(text) = serde_json::to_string_pretty(state) {
        let _ = fs::write(ui_state_path(), text);
    }
}

/// 所有热键槽位的 id 分配。新增功能必须在这里登记，否则重载时会漏注销。
pub(crate) const HOTKEY_IDS: [i32; 10] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10];

unsafe fn parse_vk(s: &str) -> Option<u32> {
    let up = s.trim().to_uppercase();
    if up.chars().count() == 1 {
        let ch = up.chars().next().unwrap();
        if ch.is_ascii_alphabetic() || ch.is_ascii_digit() {
            return Some(ch as u32);
        }
    }
    // 常用命名键。以前只认字母数字和 F1-F12，其余静默变 0 → 注册被跳过，
    // 用户以为配好了实际没绑上。
    let vk = match up.as_str() {
        "F1" => 0x70, "F2" => 0x71, "F3" => 0x72, "F4" => 0x73,
        "F5" => 0x74, "F6" => 0x75, "F7" => 0x76, "F8" => 0x77,
        "F9" => 0x78, "F10" => 0x79, "F11" => 0x7A, "F12" => 0x7B,
        "F13" => 0x7C, "F14" => 0x7D, "F15" => 0x7E, "F16" => 0x7F,
        "F17" => 0x80, "F18" => 0x81, "F19" => 0x82, "F20" => 0x83,
        "F21" => 0x84, "F22" => 0x85, "F23" => 0x86, "F24" => 0x87,
        "SPACE" => 0x20, "TAB" => 0x09, "ENTER" | "RETURN" => 0x0D,
        "ESC" | "ESCAPE" => 0x1B, "BACKSPACE" | "BACK" => 0x08,
        "DELETE" | "DEL" => 0x2E, "INSERT" | "INS" => 0x2D,
        "HOME" => 0x24, "END" => 0x23,
        "PAGEUP" | "PGUP" => 0x21, "PAGEDOWN" | "PGDN" => 0x22,
        "LEFT" => 0x25, "UP" => 0x26, "RIGHT" => 0x27, "DOWN" => 0x28,
        "`" | "OEM_3" => 0xC0, "-" | "MINUS" => 0xBD, "=" | "PLUS" | "OEM_PLUS" => 0xBB,
        "[" | "OEM_4" => 0xDB, "]" | "OEM_6" => 0xDD, "\\" | "OEM_5" => 0xDC,
        ";" | "OEM_1" => 0xBA, "'" | "OEM_7" => 0xDE,
        "," | "OEM_COMMA" => 0xBC, "." | "OEM_PERIOD" => 0xBE, "/" | "OEM_2" => 0xBF,
        _ => return None,
    };
    Some(vk)
}

/// 注册单个热键；失败时把原因记进 `RUNTIME_ISSUES`，不再静默吞掉。
unsafe fn try_register_hotkey(id: i32, spec: &HotkeySpec, name: &str) -> bool {
    let mods = parse_modifiers(&spec.modifiers);
    let Some(vk) = parse_vk(&spec.key) else {
        push_issue(format!(
            "热键「{name}」的键名 `{}` 无法识别，已跳过（支持 A-Z、0-9、F1-F24、方向键、SPACE 等）",
            spec.key
        ));
        return false;
    };
    match RegisterHotKey(None, id, mods, vk) {
        Ok(()) => true,
        Err(e) => {
            let win32 = GetLastError().0;
            let hint = if win32 == 1409 || e.code().is_err() {
                "，可能已被其它程序占用"
            } else {
                ""
            };
            push_issue(format!(
                "热键「{name}」({}+{}) 注册失败{}(Win32 {win32})，该功能快捷键不可用",
                spec.modifiers, spec.key, hint
            ));
            false
        }
    }
}

/// 热键槽位表：(注册 id, 名称, 配置项)。
///
/// 主程序的 `bind_hotkeys()` 与自检模块共用这一份，避免两处各写一遍默认值而漂移。
pub(crate) fn hotkey_slots(cfg: &HotkeyConfig) -> Vec<(i32, &'static str, HotkeySpec)> {
    HotkeySlot::ALL
        .iter()
        .filter(|slot| slot.enabled(cfg))
        .map(|slot| (slot.bind_id(), slot.label(), slot.get(cfg)))
        .collect()
}

unsafe fn bind_hotkeys(cfg: &HotkeyConfig) {
    let mut rows = Vec::new();
    for (id, name, spec) in hotkey_slots(cfg) {
        let ok = try_register_hotkey(id, &spec, name);
        rows.push((
            name.to_string(),
            format!(
                "{}+{}",
                spec.modifiers.trim().to_uppercase(),
                spec.key.trim().to_uppercase()
            ),
            ok,
        ));
    }
    set_hotkey_overview(rows);
}

/// 优先模式下热键由键盘钩子接管，这里只把键位列表填出来（全部视为可用）。
fn refresh_priority_overview(cfg: &HotkeyConfig) {
    let rows = hotkey_slots(cfg)
        .into_iter()
        .map(|(_, name, spec)| {
            (
                name.to_string(),
                format!(
                    "{}+{}",
                    spec.modifiers.trim().to_uppercase(),
                    spec.key.trim().to_uppercase()
                ),
                true,
            )
        })
        .collect();
    set_hotkey_overview(rows);
}

unsafe fn unregister_all_hotkeys() {
    // 用统一的 id 表，避免硬编码范围：以后加槽位忘了改这里，重载后旧绑定会残留
    for id in HOTKEY_IDS {
        let _ = UnregisterHotKey(None, id);
    }
}

unsafe fn reload_hotkeys_and_mouse() {
    clear_issues();
    let cfg = load_or_create_hotkey_config();
    unregister_all_hotkeys();
    set_mouse_bindings(&cfg);

    // 热键优先模式：用低级键盘钩子强占组合键；否则走普通的系统热键注册
    let priority = cfg.hotkey_priority.unwrap_or(false);
    PRIORITY_MODE.store(priority, Ordering::SeqCst);
    refresh_hotkey_match(&cfg);

    if priority {
        // 优先模式下不能再 RegisterHotKey：按键已经被自己吞掉，WM_HOTKEY 根本不会来，
        // 而且注册会白占着键位。
        uninstall_keyboard_priority_hook();
        if !install_keyboard_priority_hook() {
            PRIORITY_MODE.store(false, Ordering::SeqCst);
            bind_hotkeys(&cfg);
        } else {
            refresh_priority_overview(&cfg);
        }
    } else {
        uninstall_keyboard_priority_hook();
        bind_hotkeys(&cfg);
    }
}

/// 保存「热键优先」开关并立即生效。
fn save_priority_flag(enabled: bool) -> Result<(), String> {
    let path = get_config_path();
    let mut cfg = load_or_create_hotkey_config();
    cfg.hotkey_priority = Some(enabled);
    let text = serde_json::to_string_pretty(&cfg).map_err(|e| e.to_string())?;
    fs::write(&path, text).map_err(|e| e.to_string())?;
    request_hotkey_reload();
    Ok(())
}

fn current_priority_flag() -> bool {
    fs::read_to_string(get_config_path())
        .ok()
        .and_then(|raw| {
            serde_json::from_str::<HotkeyConfig>(raw.trim_start_matches('\u{feff}')).ok()
        })
        .and_then(|cfg| cfg.hotkey_priority)
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// 热键槽位表：编辑器、绑定、面板总览共用同一份描述。
// 新增一个热键功能时只需三步：加变体 -> 在 ALL 登记 -> 补 label/id/默认值。
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum HotkeySlot {
    Increase,
    Decrease,
    ToggleTop,
    ToggleClick,
    TogglePen,
    CycleInputMode,
    ResetCurrent,
    ResetAll,
    Update,
    Reload,
}

impl HotkeySlot {
    pub(crate) const ALL: [HotkeySlot; 10] = [
        HotkeySlot::Increase,
        HotkeySlot::Decrease,
        HotkeySlot::ToggleTop,
        HotkeySlot::ToggleClick,
        HotkeySlot::TogglePen,
        HotkeySlot::CycleInputMode,
        HotkeySlot::ResetCurrent,
        HotkeySlot::ResetAll,
        HotkeySlot::Update,
        HotkeySlot::Reload,
    ];

    pub(crate) fn label(self) -> &'static str {
        match self {
            HotkeySlot::Increase => "增加透明度",
            HotkeySlot::Decrease => "减少透明度",
            HotkeySlot::ToggleTop => "切换置顶",
            HotkeySlot::ToggleClick => "切换鼠标点透",
            HotkeySlot::TogglePen => "切换笔点透",
            HotkeySlot::CycleInputMode => "切换输入模式",
            HotkeySlot::ResetCurrent => "还原当前窗口",
            HotkeySlot::ResetAll => "还原全部窗口",
            HotkeySlot::Update => "检查更新",
            HotkeySlot::Reload => "重载热键配置",
        }
    }

    pub(crate) fn id(self) -> &'static str {
        match self {
            HotkeySlot::Increase => "increase",
            HotkeySlot::Decrease => "decrease",
            HotkeySlot::ToggleTop => "toggle_top",
            HotkeySlot::ToggleClick => "toggle_click",
            HotkeySlot::TogglePen => "toggle_pen",
            HotkeySlot::CycleInputMode => "input_mode",
            HotkeySlot::ResetCurrent => "reset_current",
            HotkeySlot::ResetAll => "reset_all",
            HotkeySlot::Update => "update",
            HotkeySlot::Reload => "reload",
        }
    }

    pub(crate) fn optional(self) -> bool {
        matches!(
            self,
            HotkeySlot::ToggleClick
                | HotkeySlot::TogglePen
                | HotkeySlot::CycleInputMode
                | HotkeySlot::Reload
        )
    }

    /// 默认键位：唯一来源，default_config 与编辑器共用，避免两处漂移。
    pub(crate) fn default_spec(self) -> HotkeySpec {
        let (mods, key) = match self {
            HotkeySlot::Increase => ("ALT", "Z"),
            HotkeySlot::Decrease => ("ALT", "X"),
            HotkeySlot::ToggleTop => ("ALT", "T"),
            HotkeySlot::ToggleClick => ("ALT", "P"),
            HotkeySlot::TogglePen => ("ALT+SHIFT", "P"),
            HotkeySlot::CycleInputMode => ("ALT+SHIFT", "M"),
            HotkeySlot::ResetCurrent => ("ALT", "R"),
            HotkeySlot::ResetAll => ("ALT+SHIFT", "R"),
            HotkeySlot::Update => ("ALT", "U"),
            HotkeySlot::Reload => ("ALT+SHIFT", "C"),
        };
        HotkeySpec {
            modifiers: mods.into(),
            key: key.into(),
        }
    }

    pub(crate) fn bind_id(self) -> i32 {
        match self {
            HotkeySlot::Increase => 1,
            HotkeySlot::Decrease => 2,
            HotkeySlot::ToggleTop => 3,
            HotkeySlot::ToggleClick => 4,
            HotkeySlot::TogglePen => 5,
            HotkeySlot::ResetCurrent => 6,
            HotkeySlot::ResetAll => 7,
            HotkeySlot::Update => 8,
            HotkeySlot::Reload => 9,
            HotkeySlot::CycleInputMode => 10,
        }
    }

    pub(crate) fn enabled(self, cfg: &HotkeyConfig) -> bool {
        match self {
            HotkeySlot::ToggleClick => cfg.toggle_click_through.is_some(),
            HotkeySlot::TogglePen => cfg.toggle_pen_passthrough.is_some(),
            HotkeySlot::CycleInputMode => cfg.input_mode_cycle.is_some(),
            HotkeySlot::Reload => cfg.reload.is_some(),
            _ => true,
        }
    }

    pub(crate) fn set_enabled(self, cfg: &mut HotkeyConfig, on: bool) {
        match self {
            HotkeySlot::ToggleClick => {
                cfg.toggle_click_through = if on { Some(self.default_spec()) } else { None }
            }
            HotkeySlot::TogglePen => {
                cfg.toggle_pen_passthrough = if on { Some(self.default_spec()) } else { None }
            }
            HotkeySlot::CycleInputMode => {
                cfg.input_mode_cycle = if on { Some(self.default_spec()) } else { None }
            }
            HotkeySlot::Reload => {
                cfg.reload = if on { Some(self.default_spec()) } else { None }
            }
            _ => {}
        }
    }

    pub(crate) fn get(self, cfg: &HotkeyConfig) -> HotkeySpec {
        match self {
            HotkeySlot::Increase => cfg.increase.clone(),
            HotkeySlot::Decrease => cfg.decrease.clone(),
            HotkeySlot::ToggleTop => cfg.toggle_top.clone(),
            HotkeySlot::ToggleClick => cfg
                .toggle_click_through
                .clone()
                .unwrap_or_else(|| self.default_spec()),
            HotkeySlot::TogglePen => cfg
                .toggle_pen_passthrough
                .clone()
                .unwrap_or_else(|| self.default_spec()),
            HotkeySlot::CycleInputMode => cfg
                .input_mode_cycle
                .clone()
                .unwrap_or_else(|| self.default_spec()),
            HotkeySlot::ResetCurrent => cfg.reset_current.clone(),
            HotkeySlot::ResetAll => cfg.reset_all.clone(),
            HotkeySlot::Update => cfg.update.clone(),
            HotkeySlot::Reload => cfg.reload.clone().unwrap_or_else(|| self.default_spec()),
        }
    }

    pub(crate) fn set(self, cfg: &mut HotkeyConfig, spec: HotkeySpec) {
        match self {
            HotkeySlot::Increase => cfg.increase = spec,
            HotkeySlot::Decrease => cfg.decrease = spec,
            HotkeySlot::ToggleTop => cfg.toggle_top = spec,
            HotkeySlot::ToggleClick => cfg.toggle_click_through = Some(spec),
            HotkeySlot::TogglePen => cfg.toggle_pen_passthrough = Some(spec),
            HotkeySlot::CycleInputMode => cfg.input_mode_cycle = Some(spec),
            HotkeySlot::ResetCurrent => cfg.reset_current = spec,
            HotkeySlot::ResetAll => cfg.reset_all = spec,
            HotkeySlot::Update => cfg.update = spec,
            HotkeySlot::Reload => cfg.reload = Some(spec),
        }
    }

    pub(crate) fn capture_target(self) -> HotkeyCaptureTarget {
        match self {
            HotkeySlot::Increase => HotkeyCaptureTarget::Increase,
            HotkeySlot::Decrease => HotkeyCaptureTarget::Decrease,
            HotkeySlot::ToggleTop => HotkeyCaptureTarget::ToggleTop,
            HotkeySlot::ToggleClick => HotkeyCaptureTarget::ToggleMouse,
            HotkeySlot::TogglePen => HotkeyCaptureTarget::TogglePen,
            HotkeySlot::CycleInputMode => HotkeyCaptureTarget::CycleInputMode,
            HotkeySlot::ResetCurrent => HotkeyCaptureTarget::ResetCurrent,
            HotkeySlot::ResetAll => HotkeyCaptureTarget::ResetAll,
            HotkeySlot::Update => HotkeyCaptureTarget::Update,
            HotkeySlot::Reload => HotkeyCaptureTarget::Reload,
        }
    }
}

/// 编辑器下拉可选的按键（`parse_vk` 全部支持这些名字）
pub(crate) const KEY_CHOICES: &[&str] = &[
    "A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M", "N", "O", "P", "Q", "R",
    "S", "T", "U", "V", "W", "X", "Y", "Z", "0", "1", "2", "3", "4", "5", "6", "7", "8", "9",
    "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12", "F13", "F14",
    "F15", "F16", "F17", "F18", "F19", "F20", "F21", "F22", "F23", "F24", "SPACE", "TAB",
    "ENTER", "ESC", "BACKSPACE", "DELETE", "INSERT", "HOME", "END", "PAGEUP", "PAGEDOWN",
    "LEFT", "UP", "RIGHT", "DOWN", "`", "-", "=", "[", "]", "\\", ";", "'", ",", ".", "/",
];

/// 四个修饰键的复选框；改动后重写 "ALT+SHIFT" 形式的字符串。
fn hotkey_mods_ui(ui: &mut egui::Ui, modifiers: &mut String) -> bool {
    let up = modifiers.to_uppercase();
    let mut alt = up.contains("ALT");
    let mut ctrl = up.contains("CTRL") || up.contains("CONTROL");
    let mut shift = up.contains("SHIFT");
    let mut win = up.contains("WIN");
    let mut changed = false;
    changed |= ui.checkbox(&mut alt, "Alt").changed();
    changed |= ui.checkbox(&mut ctrl, "Ctrl").changed();
    changed |= ui.checkbox(&mut shift, "Shift").changed();
    changed |= ui.checkbox(&mut win, "Win").changed();
    if changed {
        let mut parts: Vec<&str> = Vec::new();
        if alt {
            parts.push("ALT");
        }
        if ctrl {
            parts.push("CTRL");
        }
        if shift {
            parts.push("SHIFT");
        }
        if win {
            parts.push("WIN");
        }
        *modifiers = parts.join("+");
    }
    changed
}

/// 按键下拉；返回是否改动。
fn hotkey_key_ui(ui: &mut egui::Ui, key: &mut String, salt: &str) -> bool {
    let mut changed = false;
    let current = key.trim().to_uppercase();
    egui::ComboBox::from_id_source(format!("{salt}_key"))
        .selected_text(if current.is_empty() {
            "选择按键".to_string()
        } else {
            current.clone()
        })
        .width(96.0)
        .show_ui(ui, |ui| {
            for choice in KEY_CHOICES {
                if ui.selectable_label(current == *choice, *choice).clicked() {
                    *key = (*choice).to_string();
                    changed = true;
                }
            }
        });
    changed
}

/// 统一卡片外观：圆角 10、细描边、内边距，和 DSH 面板的卡片风格保持一致。
fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::none()
        .fill(SURFACE)
        .stroke(egui::Stroke::new(1.0, BORDER))
        .rounding(egui::Rounding::same(12.0))
        .shadow(egui::epaint::Shadow { offset: egui::vec2(0.0, 1.0), blur: 6.0, spread: 0.0, color: egui::Color32::from_black_alpha(10) })
        .inner_margin(egui::Margin::symmetric(10.0, 8.0))
        .show(ui, |ui| add(ui))
        .inner
}

// ---- 面板配色（取值来自 DSH 界面取样）----
pub(crate) const ACCENT: egui::Color32 = egui::Color32::from_rgb(0x41, 0x76, 0xE6);
pub(crate) const SURFACE: egui::Color32 = egui::Color32::WHITE;
pub(crate) const BG_BASE: egui::Color32 = egui::Color32::from_rgb(0xFA, 0xFA, 0xFA);
pub(crate) const BG_SIDEBAR: egui::Color32 = egui::Color32::from_rgb(0xF9, 0xFA, 0xFB);
pub(crate) const BORDER: egui::Color32 = egui::Color32::from_rgb(0xE5, 0xE7, 0xEB);
pub(crate) const TEXT_PRIMARY: egui::Color32 = egui::Color32::from_rgb(0x1F, 0x23, 0x28);
pub(crate) const TEXT_SECONDARY: egui::Color32 = egui::Color32::from_rgb(0x6B, 0x72, 0x80);
pub(crate) const ERROR: egui::Color32 = egui::Color32::from_rgb(0xDC, 0x26, 0x26);

fn apply_panel_theme(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    let mut v = egui::Visuals::light();
    v.override_text_color = Some(TEXT_PRIMARY);
    v.panel_fill = BG_BASE;
    v.window_fill = SURFACE;
    v.extreme_bg_color = SURFACE;
    v.faint_bg_color = BG_SIDEBAR;
    v.window_stroke = egui::Stroke::new(1.0, BORDER);
    v.window_rounding = egui::Rounding::same(12.0);
    v.menu_rounding = egui::Rounding::same(10.0);
    v.selection.bg_fill = ACCENT;
    v.selection.stroke = egui::Stroke::new(1.0, egui::Color32::WHITE);
    v.hyperlink_color = ACCENT;
    v.widgets.noninteractive.bg_fill = SURFACE;
    v.widgets.noninteractive.bg_stroke = egui::Stroke::new(1.0, BORDER);
    v.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, TEXT_PRIMARY);
    v.widgets.inactive.bg_fill = SURFACE;
    v.widgets.inactive.weak_bg_fill = SURFACE;
    v.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, BORDER);
    v.widgets.inactive.fg_stroke = egui::Stroke::new(1.0, TEXT_PRIMARY);
    v.widgets.hovered.bg_fill = egui::Color32::from_rgb(0xF3, 0xF4, 0xF6);
    v.widgets.hovered.weak_bg_fill = egui::Color32::from_rgb(0xF3, 0xF4, 0xF6);
    v.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, egui::Color32::from_rgb(0xD1, 0xD5, 0xDB));
    v.widgets.active.bg_fill = egui::Color32::from_rgb(0xEC, 0xEE, 0xF0);
    v.widgets.active.weak_bg_fill = egui::Color32::from_rgb(0xEC, 0xEE, 0xF0);
    v.widgets.active.bg_stroke = egui::Stroke::new(1.0, egui::Color32::from_rgb(0xD1, 0xD5, 0xDB));
    for w in [&mut v.widgets.noninteractive, &mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] {
        w.rounding = egui::Rounding::same(8.0);
    }
    style.visuals = v;
    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    style.spacing.button_padding = egui::vec2(10.0, 5.0);
    style.spacing.window_margin = egui::Margin::same(12.0);
    style.spacing.menu_margin = egui::Margin::same(8.0);
    style.text_styles.insert(egui::TextStyle::Heading, egui::FontId::proportional(19.0));
    style.text_styles.insert(egui::TextStyle::Body, egui::FontId::proportional(13.0));
    style.text_styles.insert(egui::TextStyle::Button, egui::FontId::proportional(13.0));
    style.text_styles.insert(egui::TextStyle::Small, egui::FontId::proportional(11.5));
    style.text_styles.insert(egui::TextStyle::Monospace, egui::FontId::monospace(11.0));
    ctx.set_style(style);
}

// ---------------------------------------------------------------------------
// 窗口枚举 / 虚拟桌面分组（任务视图式大界面用）
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct WinInfo {
    hwnd: isize,
    title: String,
    desktop: String,
}

struct VdmCache(Option<windows::Win32::UI::Shell::IVirtualDesktopManager>);
unsafe impl Send for VdmCache {}
unsafe impl Sync for VdmCache {}

static VDM: OnceLock<VdmCache> = OnceLock::new();

fn virtual_desktop_manager() -> Option<&'static windows::Win32::UI::Shell::IVirtualDesktopManager> {
    VDM.get_or_init(|| unsafe {
        use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_APARTMENTTHREADED};
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        // CLSID_VirtualDesktopManager = {AA509086-5CA9-4C25-8F95-589D3C07B48A}
        let clsid = windows::core::GUID::from_u128(0xAA509086_5CA9_4C25_8F95_589D_3C07_B48A);
        VdmCache(CoCreateInstance(&clsid, None, CLSCTX_ALL).ok())
    })
    .0
    .as_ref()
}

fn window_desktop(hwnd: HWND) -> String {
    let Some(manager) = virtual_desktop_manager() else {
        return "未分组".to_string();
    };
    unsafe {
        if let Ok(guid) = manager.GetWindowDesktopId(hwnd) {
            let on_current = manager.IsWindowOnCurrentVirtualDesktop(hwnd).map(|b| b.as_bool()).unwrap_or(false);
            let short = format!("{:08x}", guid.data1);
            return if on_current {
                format!("桌面 {short}（当前）")
            } else {
                format!("桌面 {short}")
            };
        }
    }
    "未分组".to_string()
}

unsafe extern "system" fn enum_win_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let out = &mut *(lparam.0 as *mut Vec<WinInfo>);
    if IsWindowVisible(hwnd).as_bool() {
        let mut buf: [u16; 256] = [0; 256];
        let len = GetWindowTextW(hwnd, &mut buf);
        if len > 0 && !is_shell_hwnd(hwnd) && !is_own_hwnd(hwnd) {
            // 过滤工具窗/不可激活窗/过小窗（输入法皮肤、NVIDIA Overlay 之类）
            let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
            if (ex & WS_EX_TOOLWINDOW.0) != 0 || (ex & WS_EX_NOACTIVATE.0) != 0 {
                return BOOL(1);
            }
            let mut rc = RECT::default();
            if GetWindowRect(hwnd, &mut rc).is_ok()
                && ((rc.right - rc.left) < 40 || (rc.bottom - rc.top) < 24)
            {
                return BOOL(1);
            }
            // DWM 隐身窗口（UWP 幽灵窗口）不算
            let mut cloaked: u32 = 0;
            let cloaked_h = windows::Win32::Graphics::Dwm::DwmGetWindowAttribute(
                hwnd,
                windows::Win32::Graphics::Dwm::DWMWA_CLOAKED,
                &mut cloaked as *mut _ as *mut core::ffi::c_void,
                std::mem::size_of::<u32>() as u32,
            )
            .is_ok()
                && cloaked != 0;
            if !cloaked_h {
                out.push(WinInfo {
                    hwnd: hwnd.0 as isize,
                    title: String::from_utf16_lossy(&buf[..len as usize]),
                    desktop: window_desktop(hwnd),
                });
            }
        }
    }
    BOOL(1)
}

fn enumerate_windows() -> Vec<WinInfo> {
    let mut out: Vec<WinInfo> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(enum_win_proc), LPARAM(&mut out as *mut _ as isize));
    }
    out.sort_by(|a, b| a.desktop.cmp(&b.desktop).then_with(|| a.title.cmp(&b.title)));
    out
}

/// 把一个窗口纳入管理（写入注册表并记录原始样式）
unsafe fn ensure_managed(hwnd: HWND) {
    let v = hwnd.0 as isize;
    if GLOBAL_REGISTRY.get(&v).is_none() {
        let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        let is_top = (ex & WS_EX_TOPMOST.0) != 0;
        let title = get_window_title(hwnd);
        GLOBAL_REGISTRY.insert(
            v,
            WindowState {
                original_ex_style: ex,
                current_alpha: 255,
                original_is_topmost: is_top,
                user_pref_topmost: is_top,
                mouse_passthrough: false,
                pen_passthrough: false,
                title,
            },
        );
    }
}

unsafe fn set_window_alpha(hwnd: HWND, alpha: u8) {
    let v = hwnd.0 as isize;
    let values = {
        let mut out = None;
        if let Some(mut st) = GLOBAL_REGISTRY.get_mut(&v) {
            st.current_alpha = alpha;
            out = Some((st.user_pref_topmost, st.mouse_passthrough, st.pen_passthrough));
        }
        out
    };
    if let Some((top, mouse, pen)) = values {
        let _ = apply_transparency_to_hwnd(hwnd, alpha, top, mouse, pen);
    }
    request_ui_repaint();
}

/// 一键控制：纳入管理并套用默认控制参数（透明度 90%），用于列表单击
unsafe fn control_window(hwnd: HWND) {
    ensure_managed(hwnd);
    set_window_alpha(hwnd, 230);
}
