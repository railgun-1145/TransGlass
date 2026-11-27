#![windows_subsystem = "windows"]

use dashmap::DashMap;
use eframe::egui;
use lazy_static::lazy_static;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use self_update::backends::github::Update;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, AtomicU32, AtomicU64, AtomicU8, Ordering};
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

mod regions;
mod selection;
mod selector;
mod selftest;
mod doll;

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
/// RawInput 最近一次来自"笔/数位板"还是"鼠标"（-1 未知），以及时间戳（anim_secs 的 f32 bits）
static LAST_RAW_PEN: AtomicI32 = AtomicI32::new(-1);
static LAST_RAW_AT: AtomicU32 = AtomicU32::new(0);
/// 用户在设置里选定的设备路径（None = 自动）
static DEV_PEN: std::sync::RwLock<Option<String>> = std::sync::RwLock::new(None);
static DEV_MOUSE: std::sync::RwLock<Option<String>> = std::sync::RwLock::new(None);
static DEV_TABLET: std::sync::RwLock<Option<String>> = std::sync::RwLock::new(None);
/// 是否有窗口处于"只开鼠标点透/只开笔点透"的单设备模式；没有就完全跳过低级钩子的重活
static DEVICE_TRACKING: AtomicBool = AtomicBool::new(false);

// ---- 可选性能计数（设 TRANSGLASS_DEBUG_PERF=1 才启用，关闭时只有一次原子读开销）----
/// 悬停高亮的候选窗口（0 = 无）与起始时间戳（毫秒）
static HOVER_HWND: AtomicIsize = AtomicIsize::new(0);
static HOVER_START_MS: AtomicU64 = AtomicU64::new(0);
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
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
    ball_mark_dirty();
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
        thread::sleep(std::time::Duration::from_millis(150));
        if APP_EXIT_REQUESTED.load(Ordering::Relaxed) {
            break;
        }
        refresh_device_tracking();
        unsafe {
            let mut cp = POINT::default();
            let _ = GetCursorPos(&mut cp);
            let packed = ((cp.x as u32 as u64) << 32) | (cp.y as u32 as u64);
            let prev = BALL_CURSOR.swap(packed, Ordering::Relaxed);
            if prev != packed && BALL_HWND.load(Ordering::Relaxed) != 0 {
                ball_render();
            }
        }
        unsafe {
            let fg = GetForegroundWindow();
            if !fg.0.is_null() && !is_own_hwnd(fg) && !is_shell_hwnd(fg) {
                TARGET_HWND.store(fg.0 as isize, Ordering::Relaxed);
            }
        }
        perf_dump();
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

pub(crate) unsafe fn set_hwnd_mouse_transparent(hwnd: HWND, enabled: bool) {
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

/// 这个窗口现在是不是"整窗点透"（鼠标或笔任一开）——点透选区在这种窗口上不生效。
pub(crate) fn window_passthrough_active(hwnd: isize) -> bool {
    GLOBAL_REGISTRY
        .get(&hwnd)
        .map(|s| s.mouse_passthrough || s.pen_passthrough)
        .unwrap_or(false)
}

/// 输入设备类别：0 鼠标，1 笔/数位板，2 触摸，3 其它 HID
#[derive(Clone)]
pub(crate) struct InputDev {
    pub(crate) kind: u8,
    pub(crate) path: String,
    pub(crate) label: String,
}

/// 设备列表缓存：设置页每帧都枚举一次太浪费，2 秒刷新
static DEVICE_CACHE: std::sync::RwLock<(f32, usize)> = std::sync::RwLock::new((0.0, 0));
static DEVICES: std::sync::RwLock<Vec<InputDev>> = std::sync::RwLock::new(Vec::new());

/// 带缓存的设备列表（kind：0 鼠标 / 1 笔 / 2 触摸 / 3 其它）
fn input_devices_cached() -> Vec<InputDev> {
    let now = anim_secs();
    if let (Ok(cache), Ok(list)) = (DEVICE_CACHE.read(), DEVICES.read()) {
        if now - cache.0 < 2.0 && cache.1 > 0 {
            return list.clone();
        }
    }
    let fresh = list_input_devices();
    if let Ok(mut list) = DEVICES.write() {
        *list = fresh.clone();
    }
    if let Ok(mut cache) = DEVICE_CACHE.write() {
        *cache = (now, fresh.len());
    }
    fresh
}

/// 枚举 RawInput 能看到的输入设备（鼠标 / 笔 / 触摸）
pub(crate) fn list_input_devices() -> Vec<InputDev> {
    use windows::Win32::UI::Input::{
        GetRawInputDeviceInfoW, GetRawInputDeviceList, RAWINPUTDEVICELIST, RIDI_DEVICEINFO,
        RIDI_DEVICENAME, RID_DEVICE_INFO, RIM_TYPEHID, RIM_TYPEMOUSE,
    };
    let mut out = Vec::new();
    unsafe {
        let mut count: u32 = 0;
        let cb = std::mem::size_of::<RAWINPUTDEVICELIST>() as u32;
        if GetRawInputDeviceList(None, &mut count, cb) == u32::MAX || count == 0 {
            return out;
        }
        let mut list = vec![RAWINPUTDEVICELIST::default(); count as usize];
        let got = GetRawInputDeviceList(Some(list.as_mut_ptr()), &mut count, cb);
        if got == u32::MAX {
            return out;
        }
        for d in list.iter().take(got as usize) {
            // 设备路径
            let mut len: u32 = 0;
            let _ = GetRawInputDeviceInfoW(d.hDevice, RIDI_DEVICENAME, None, &mut len);
            if len == 0 {
                continue;
            }
            let mut buf = vec![0u16; len as usize + 1];
            let rc = GetRawInputDeviceInfoW(
                d.hDevice,
                RIDI_DEVICENAME,
                Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
                &mut len,
            );
            if rc == u32::MAX {
                continue;
            }
            let path = String::from_utf16_lossy(&buf[..len as usize]);
            // 设备类型
            let mut info: RID_DEVICE_INFO = std::mem::zeroed();
            info.cbSize = std::mem::size_of::<RID_DEVICE_INFO>() as u32;
            let mut sz = info.cbSize;
            let _ = GetRawInputDeviceInfoW(
                d.hDevice,
                RIDI_DEVICEINFO,
                Some(&mut info as *mut _ as *mut core::ffi::c_void),
                &mut sz,
            );
            let kind = if d.dwType == RIM_TYPEMOUSE {
                0u8
            } else if d.dwType == RIM_TYPEHID {
                let page = info.Anonymous.hid.usUsagePage;
                let usage = info.Anonymous.hid.usUsage;
                match (page, usage) {
                    (0x0D, 0x02) => 1, // 笔
                    (0x0D, 0x04) => 2, // 触摸
                    _ => 3,
                }
            } else {
                continue; // 键盘之类不要
            };
            // 标签：从路径里抠 VID/PID，太长的路径只显示尾巴
            let mut label = String::new();
            if let Some(i) = path.find("VID_") {
                label.push_str(&path[i..(i + 8).min(path.len())]);
            }
            if let Some(i) = path.find("PID_") {
                label.push(' ');
                label.push_str(&path[i..(i + 8).min(path.len())]);
            }
            if label.trim().is_empty() {
                label = path.rsplit('#').next().unwrap_or(&path).to_string();
            }
            let kind_name = match kind {
                0 => "鼠标",
                1 => "笔",
                2 => "触摸",
                _ => "HID",
            };
            out.push(InputDev {
                kind,
                path,
                label: format!("{kind_name} {}", label.trim()),
            });
        }
    }
    out
}

/// 后台线程：注册 RawInput（INPUTSINK，不抢走其它程序），按设备标出"这一下是笔还是鼠标"
fn spawn_raw_input_tracker() {
    use windows::Win32::UI::Input::{RegisterRawInputDevices, RAWINPUTDEVICE, RIDEV_INPUTSINK};
    thread::spawn(|| unsafe {
        let class = w!("TransGlassRawInput");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(raw_input_proc),
            hInstance: HINSTANCE(windows::Win32::System::LibraryLoader::GetModuleHandleW(None).unwrap_or_default().0),
            lpszClassName: class,
            ..Default::default()
        };
        let _ = RegisterClassW(&wc);
        let hwnd = match CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!("TransGlassRawInput"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            None,
            None,
            None,
        ) {
            Ok(h) => h,
            Err(_) => return,
        };
        // 鼠标 + 笔 + 触摸，INPUTSINK 表示不抢别人的输入，只是想旁听
        let devices = [
            RAWINPUTDEVICE {
                usUsagePage: 1,
                usUsage: 2, // 鼠标
                dwFlags: RIDEV_INPUTSINK,
                hwndTarget: hwnd,
            },
            RAWINPUTDEVICE {
                usUsagePage: 0x0D,
                usUsage: 0x02, // 笔
                dwFlags: RIDEV_INPUTSINK,
                hwndTarget: hwnd,
            },
            RAWINPUTDEVICE {
                usUsagePage: 0x0D,
                usUsage: 0x04, // 触摸
                dwFlags: RIDEV_INPUTSINK,
                hwndTarget: hwnd,
            },
        ];
        if RegisterRawInputDevices(&devices, std::mem::size_of::<RAWINPUTDEVICE>() as u32).is_err() {
            return;
        }
        // 设备句柄 -> 类别 的缓存
        let (devs, hids) = build_raw_cache();
        let _ = (&devs, &hids);
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            let _ = DispatchMessageW(&msg);
        }
    });
}

/// 缓存设备句柄 → (类别, 路径)。RAWINPUT 里只给 HANDLE，要先反查。
fn build_raw_cache() -> (Vec<(isize, u8, String)>, ()) {
    use windows::Win32::UI::Input::{
        GetRawInputDeviceInfoW, GetRawInputDeviceList, RAWINPUTDEVICELIST, RIDI_DEVICEINFO,
        RIM_TYPEHID, RIM_TYPEMOUSE, RID_DEVICE_INFO,
    };
    let mut out = Vec::new();
    unsafe {
        let mut count: u32 = 0;
        let cb = std::mem::size_of::<RAWINPUTDEVICELIST>() as u32;
        if GetRawInputDeviceList(None, &mut count, cb) == u32::MAX || count == 0 {
            return (out, ());
        }
        let mut list = vec![RAWINPUTDEVICELIST::default(); count as usize];
        let got = GetRawInputDeviceList(Some(list.as_mut_ptr()), &mut count, cb);
        if got == u32::MAX {
            return (out, ());
        }
        for d in list.iter().take(got as usize) {
            let mut info: RID_DEVICE_INFO = std::mem::zeroed();
            info.cbSize = std::mem::size_of::<RID_DEVICE_INFO>() as u32;
            let mut sz = info.cbSize;
            let _ = GetRawInputDeviceInfoW(
                d.hDevice,
                RIDI_DEVICEINFO,
                Some(&mut info as *mut _ as *mut core::ffi::c_void),
                &mut sz,
            );
            let kind = if d.dwType == RIM_TYPEMOUSE {
                0u8
            } else if d.dwType == RIM_TYPEHID {
                match (info.Anonymous.hid.usUsagePage, info.Anonymous.hid.usUsage) {
                    (0x0D, 0x02) => 1,
                    (0x0D, 0x04) => 2,
                    _ => 3,
                }
            } else {
                continue;
            };
            let mut len: u32 = 0;
            let _ = GetRawInputDeviceInfoW(d.hDevice, windows::Win32::UI::Input::RIDI_DEVICENAME, None, &mut len);
            let mut buf = vec![0u16; len as usize + 1];
            let rc = GetRawInputDeviceInfoW(
                d.hDevice,
                windows::Win32::UI::Input::RIDI_DEVICENAME,
                Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
                &mut len,
            );
            let path = if rc == u32::MAX {
                String::new()
            } else {
                String::from_utf16_lossy(&buf[..len as usize])
            };
            out.push((d.hDevice.0 as isize, kind, path));
        }
    }
    (out, ())
}

unsafe extern "system" fn raw_input_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if msg == WM_INPUT {
        use windows::Win32::UI::Input::{
            GetRawInputData, HRAWINPUT, RAWINPUT, RAWINPUTHEADER, RID_INPUT,
        };
        let mut size: u32 = std::mem::size_of::<RAWINPUT>() as u32;
        let mut buf = vec![0u8; size as usize];
        let got = GetRawInputData(
            HRAWINPUT(l.0 as *mut _),
            RID_INPUT,
            Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
            &mut size,
            std::mem::size_of::<RAWINPUTHEADER>() as u32,
        );
        if got != u32::MAX {
            let raw = &*(buf.as_ptr() as *const RAWINPUT);
            let dev = raw.header.hDevice.0 as isize;
            let (cache, _) = build_raw_cache();
            let mut is_pen: i32 = -1;
            if let Some((_, kind, path)) = cache.iter().find(|(h, _, _)| *h == dev) {
                let pen_sel = DEV_PEN.read().ok().and_then(|g| g.clone());
                let tab_sel = DEV_TABLET.read().ok().and_then(|g| g.clone());
                let mouse_sel = DEV_MOUSE.read().ok().and_then(|g| g.clone());
                if path.is_empty() {
                    is_pen = if *kind >= 1 && *kind <= 2 { 1 } else { 0 };
                } else if pen_sel.as_deref() == Some(path.as_str())
                    || tab_sel.as_deref() == Some(path.as_str())
                {
                    is_pen = 1;
                } else if mouse_sel.as_deref() == Some(path.as_str()) {
                    is_pen = 0;
                } else if *kind == 0 {
                    is_pen = 0;
                } else if *kind == 1 || *kind == 2 {
                    is_pen = 1;
                }
            }
            if is_pen >= 0 {
                LAST_RAW_PEN.store(is_pen, Ordering::Relaxed);
                LAST_RAW_AT.store(anim_secs().to_bits(), Ordering::Relaxed);
            }
        }
    }
    DefWindowProcW(hwnd, msg, w, l)
}

/// 判断这一次输入算不算"笔"：优先看去设置里选定的设备（RawInput 150ms 内的上报），否则退回旧启发式
unsafe fn effective_is_pen(info: &MSLLHOOKSTRUCT) -> bool {
    let at = f32::from_bits(LAST_RAW_AT.load(Ordering::Relaxed));
    if at > 0.0 {
        let now = anim_secs();
        if now - at < 0.15 {
            let p = LAST_RAW_PEN.load(Ordering::Relaxed);
            if p >= 0 {
                return p == 1;
            }
        }
    }
    is_pen_input(info)
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
        // 记录"最后点过的非自身窗口"，供悬浮球当遥控目标
        if msg == WM_LBUTTONDOWN {
            if let Some(root) = hit_root_at(info.pt.x, info.pt.y) {
                if !is_own_hwnd(root) && !is_shell_hwnd(root) {
                    TARGET_HWND.store(root.0 as isize, Ordering::Relaxed);
                    ball_mark_dirty();
                    log_hilite(&format!("target={:?}", root.0));
                }
            }
        }
        if info.dwExtraInfo == TRANSG_GLASS_INJECT_EXTRA_INFO {
            return CallNextHookEx(
                HHOOK(MOUSE_HOOK.load(Ordering::SeqCst) as *mut _),
                code,
                wparam,
                lparam,
            );
        }

        // 战争雷霆式绑定：鼠标键也能当热键（左/右/中/侧键1/2、滚轮上下）
        let mouse_vk = match msg {
            WM_LBUTTONDOWN => Some(0x01u32),
            WM_RBUTTONDOWN => Some(0x02),
            WM_MBUTTONDOWN => Some(0x04),
            WM_XBUTTONDOWN => Some(if ((info.mouseData >> 16) & 0xffff) == 1 {
                0x05
            } else {
                0x06
            }),
            WM_MOUSEWHEEL => Some(if (info.mouseData >> 16) as i16 > 0 {
                0x20A
            } else {
                0x20B
            }),
            _ => None,
        };
        if let Some(vk) = mouse_vk {
            let mods = current_hotkey_mods();
            let hit = HOTKEY_MATCH.read().ok().and_then(|list| {
                list.iter()
                    .find(|(_, m, v)| *v == vk && *m == mods)
                    .map(|(id, _, _)| *id)
            });
            if let Some(id) = hit {
                let tid = HOTKEY_THREAD_ID.load(Ordering::Relaxed);
                if tid != 0 {
                    let _ = PostThreadMessageW(tid, WM_HOTKEY, WPARAM(id as usize), LPARAM(0));
                }
                // 侧键/中键吞掉；左/右键和滚轮放行，否则正常操作会坏
                if matches!(vk, 0x04 | 0x05 | 0x06) {
                    return LRESULT(1);
                }
            }
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
            // 快路径：面板不可见、也没有"单设备点透"窗口、也没画过点透选区时，钩子里什么都不用做
            if !WINDOW_VISIBLE.load(Ordering::Relaxed)
                && !DEVICE_TRACKING.load(Ordering::Relaxed)
                && !crate::regions::has_any()
            {
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

            // 没有单设备点透的窗口、也没有选区 → 后面的 WindowFromPoint / 注册表查询全部跳过
            if (!DEVICE_TRACKING.load(Ordering::Relaxed) || GLOBAL_REGISTRY.is_empty())
                && !crate::regions::has_any()
            {
                PERF_FAST_CALLS.fetch_add(1, Ordering::Relaxed);
                return CallNextHookEx(
                    HHOOK(MOUSE_HOOK.load(Ordering::SeqCst) as *mut _),
                    code,
                    wparam,
                    lparam,
                );
            }

            // 点透选区：按当前坐标维护临时 WS_EX_TRANSPARENT，必须早于 hit_root_at，
            // 否则选区里的窗口被判定成"下层"后就走不到这里了
            crate::regions::enforce_at(info.pt.x, info.pt.y);

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
            // 点透会话期间选中的窗口临时不点透：别让单设备逻辑按鼠标/笔把
            // WS_EX_TRANSPARENT 又加回去，否则选中后还是点不动。
            if !crate::selector::is_session_window(root_val) {
                if let Some(state) = GLOBAL_REGISTRY.get(&root_val) {
                    if state.mouse_passthrough != state.pen_passthrough {
                        let is_pen = match get_input_mode() {
                            InputMode::Auto => effective_is_pen(&info),
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

pub(crate) unsafe fn get_window_title(hwnd: HWND) -> String {
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
    // GPU 合成的重型应用（PS / AE / LR / 画世界等）改完 alpha 会留旧帧，
    // 强制它重画一次并让 DWM 立刻提交，不然要等用户动一下鼠标才更新
    if let Some(app) = compat_app_name(hwnd) {
        use windows::Win32::Graphics::Dwm::DwmFlush;
        use windows::Win32::Graphics::Gdi::{
            RedrawWindow, RDW_ALLCHILDREN, RDW_FRAME, RDW_INVALIDATE, RDW_UPDATENOW,
        };
        let _ = DwmFlush();
        let _ = RedrawWindow(
            hwnd,
            None,
            None,
            RDW_INVALIDATE | RDW_FRAME | RDW_UPDATENOW | RDW_ALLCHILDREN,
        );
        log_hilite(&format!("compat redraw: {app}"));
    }

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

/// 会话结束时按管理状态重放样式：临时摘掉的 WS_EX_TRANSPARENT 要还回去。
/// 单设备点透（只鼠标或只笔）的样式由钩子下一次事件决定，这里只负责恢复 alpha/置顶。
pub(crate) unsafe fn reapply_managed_state(hwnd: HWND) -> bool {
    let v = hwnd.0 as isize;
    let values = GLOBAL_REGISTRY
        .get(&v)
        .map(|st| (st.current_alpha, st.user_pref_topmost, st.mouse_passthrough, st.pen_passthrough));
    if let Some((alpha, top, mouse, pen)) = values {
        let _ = apply_transparency_to_hwnd(hwnd, alpha, top, mouse, pen);
        true
    } else {
        false
    }
}

/// 让鼠标钩子下一次事件重新判断单设备点透样式（会话结束后必须调一次）。
pub(crate) fn reset_passthrough_tracking() {
    LAST_DEVICE_HWND.store(0, Ordering::Relaxed);
}

/// 只有存在"单设备点透"的窗口时，钩子才需要逐事件判断笔/鼠标；否则一律走快路径。
/// 只在用户操作（改透明度/开关/还原）时调用，不在每条鼠标消息里扫描。
fn refresh_device_tracking() {    let need = GLOBAL_REGISTRY
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
    // 未管理的窗口先纳入管理，否则点了没反应（旧 bug：toggle_topmost 没有自动注册）
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

pub(crate) unsafe fn is_shell_hwnd(hwnd: HWND) -> bool {
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
        "Progman"
            | "WorkerW"
            | "Shell_TrayWnd"
            | "Shell_SecondaryTrayWnd"
            | "NotifyIconOverflowWindow"
            | "XamlExplorerHostIslandWindow"
            | "TopLevelWindowForOverflowXamlIsland"
            | "Windows.UI.Composition.DesktopWindowContentBridge"
            | "Windows.Internal.Shell.TabProxyWindow"
            | "DV2ControlHost"
            | "Xaml_WindowedPopupClass"
            | "Shell_Flyout"
            | "Windows.UI.Core.CoreWindow"
    )
}

pub(crate) unsafe fn is_own_hwnd(hwnd: HWND) -> bool {
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
    SelectSession,
}

struct TransGlassApp {
    /// 标题栏图标
    app_icon: Option<egui::TextureHandle>,
    should_exit: bool,
    hotkey_draft: HotkeyConfig,
    hotkey_message: Option<String>,
    hotkey_capture: Option<HotkeyCaptureTarget>,
    /// 开始录入的时间（刚点「录入」的那一下鼠标左键不算）
    hotkey_capture_at: Option<std::time::Instant>,
    hotkey_priority_ui: bool,
    ui_state: UiState,
    always_on_top: bool,
    last_geo: Option<[f32; 4]>,
    geo_changed_at: Option<std::time::Instant>,
    selftest_open: bool,
    selftest_text: String,
    wins: Vec<WinInfo>,
    thumbs: std::collections::HashMap<isize, (egui::TextureHandle, std::time::Instant)>,
    wins_at: Option<std::time::Instant>,
    selected: Option<isize>,
    settings_open: bool,
    ps_selected: usize,
    /// 设置页当前分类
    settings_tab: usize,
    /// 设置页淡入起点（切分类/刚打开时重置）
    settings_fade: Option<std::time::Instant>,
    mini_mode: bool,
    desk_filter: Option<String>,
    preview_fps: u32,
    view_mode: u8,
    card_rects: std::collections::HashMap<isize, egui::Rect>,
    ball_open: bool,
    ball_expanded: bool,
    ball_size: egui::Vec2,
    ball_pos: egui::Pos2,
    hover: Option<(isize, std::time::Instant)>,
    hover_seen: bool,
    last_mini: bool,
    issues_ver: u32,
    issues_cache: Vec<String>,
    overview_ver: u32,
    overview_cache: Vec<(String, String, bool)>,
}

impl TransGlassApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        // 1. 字体：优先微软雅黑（现代 UI 字体），失败了才退回黑体/宋体。
        //    以前把 simhei 放第一优先，黑体在小字号下笔画又硬又挤，看着像"代码注释"。
        let mut fonts = egui::FontDefinitions::default();
        let font_paths = [
            "C:\\Windows\\Fonts\\msyh.ttc",
            "C:\\Windows\\Fonts\\msyh.ttf",
            "C:\\Windows\\Fonts\\simhei.ttf",
            "C:\\Windows\\Fonts\\simsun.ttc",
            "C:\\Windows\\Fonts\\simsun.ttf",
        ];
        for path in font_paths {
            if let Ok(font_data) = std::fs::read(path) {
                fonts
                    .font_data
                    .insert("cjk".to_owned(), egui::FontData::from_owned(font_data));
                fonts
                    .families
                    .get_mut(&egui::FontFamily::Proportional)
                    .unwrap()
                    .insert(0, "cjk".to_owned());
                fonts
                    .families
                    .get_mut(&egui::FontFamily::Monospace)
                    .unwrap()
                    .push("cjk".to_owned());
                break;
            }
        }
        // 粗体家族（雅黑 Bold）：只给标题用，正文用常规字重
        let bold_loaded = if let Ok(bold) = std::fs::read("C:\\Windows\\Fonts\\msyhbd.ttc") {
            fonts
                .font_data
                .insert("cjk_bold".to_owned(), egui::FontData::from_owned(bold));
            let fam = egui::FontFamily::Name("bold".into());
            let mut list = vec!["cjk_bold".to_owned()];
            if let Some(base) = fonts.families.get(&egui::FontFamily::Proportional) {
                list.extend(base.iter().cloned());
            }
            fonts.families.insert(fam, list);
            true
        } else {
            false
        };
        cc.egui_ctx.set_fonts(fonts);

        // 2. 仿 Trae 风格的深色 UI
        // 浅色主题：色值取自 DSH 界面取样（底 #FAFAFA / 卡 #FFFFFF / 边 #E5E7EB / 主色 #4176E6）
        apply_panel_theme(&cc.egui_ctx);
        // 标题栏用的小图标（就是那块 MC 玻璃）
        let app_icon = window_icon_data().map(|icon| {
            let img = egui::ColorImage::from_rgba_unmultiplied(
                [icon.width as usize, icon.height as usize],
                &icon.rgba,
            );
            cc.egui_ctx
                .load_texture("app_icon_small", img, egui::TextureOptions::LINEAR)
        });
        BOLD_FONT_OK.store(bold_loaded, Ordering::Relaxed);
        if bold_loaded {
            let mut style = (*cc.egui_ctx.style()).clone();
            style.text_styles.insert(
                egui::TextStyle::Heading,
                egui::FontId::new(20.0, egui::FontFamily::Name("bold".into())),
            );
            cc.egui_ctx.set_style(style);
        }
        let _ = EGUI_CTX.set(cc.egui_ctx.clone());

        if let Ok(handle) = cc.window_handle() {
            if let RawWindowHandle::Win32(h) = handle.as_raw() {
                ROOT_HWND.store(h.hwnd.get(), Ordering::Relaxed);
                // 系统深色模式会给浅色界面配一条纯黑标题栏，很难看；强制用浅色标题栏
                unsafe {
                    use windows::Win32::Graphics::Dwm::DwmSetWindowAttribute;
                    let dark: i32 = 0;
                    let _ = DwmSetWindowAttribute(
                        HWND(h.hwnd.get() as *mut _),
                        windows::Win32::Graphics::Dwm::DWMWA_USE_IMMERSIVE_DARK_MODE,
                        &dark as *const i32 as *const core::ffi::c_void,
                        std::mem::size_of::<i32>() as u32,
                    );
                }
            }
        }

        let ui_state = load_ui_state();
        let always_on_top = ui_state.always_on_top.unwrap_or(false);
        Self {
            app_icon,
            should_exit: false,
            hotkey_draft: load_or_create_hotkey_config(),
            hotkey_message: None,
            hotkey_capture: None,
            hotkey_capture_at: None,
            hotkey_priority_ui: current_priority_flag(),
            ui_state,
            always_on_top,
            last_geo: None,
            geo_changed_at: None,
            selftest_open: false,
            selftest_text: String::new(),
            wins: Vec::new(),
            thumbs: std::collections::HashMap::new(),
            wins_at: None,
            selected: None,
            settings_open: std::env::var_os("TRANSGLASS_OPEN_SETTINGS").is_some(),
            ps_selected: 0,
            settings_tab: 0,
            settings_fade: None,
            mini_mode: false,
            desk_filter: None,
            view_mode: load_ui_state().view_mode.unwrap_or(0),
            card_rects: std::collections::HashMap::new(),
            ball_open: true,
            ball_expanded: false,
            ball_size: egui::vec2(68.0, 68.0),
            ball_pos: egui::pos2(1540.0, 420.0),
            hover: None,
            hover_seen: false,
            preview_fps: { let s = load_ui_state(); s.preview_fps.unwrap_or(match s.preview_rate { Some(3) => 0, Some(2) => 2, Some(1) => 8, _ => 15 }) },
            last_mini: false,
            issues_ver: u32::MAX,
            issues_cache: Vec::new(),
            overview_ver: u32::MAX,
            overview_cache: Vec::new(),
        }
    }

    fn note_hover(&mut self, hwnd: isize) {
        log_hilite(&format!("hover hwnd={:#x}", hwnd));
        self.hover_seen = true;
        if HOVER_HWND.load(Ordering::Relaxed) != hwnd {
            HOVER_HWND.store(hwnd, Ordering::Relaxed);
            HOVER_START_MS.store(now_ms(), Ordering::Relaxed);
        }
        request_ui_repaint();
        let fresh = match self.hover {
            Some((h, _)) if h == hwnd => false,
            _ => true,
        };
        if fresh {
            self.hover = Some((hwnd, std::time::Instant::now()));
            unsafe { hide_hilite() };
        }
    }
    fn refresh_window_list(&mut self, force: bool) {
        selection_cleanup();
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

    /// 切换到指定虚拟桌面：用 Win+Ctrl+←/→ 逐级切换，并按 IsWindowOnCurrentVirtualDesktop 校验。
    /// 公开 API 不支持直接 SwitchDesktop 到某个 GUID，所以按当前桌面与目标桌面的相对位置步进。
    fn switch_desktop_to(&mut self, target: &str) {
        let Some(vdm) = virtual_desktop_manager() else {
            return;
        };
        let mut order: Vec<String> = Vec::new();
        for w in &self.wins {
            if !order.contains(&w.desktop) {
                order.push(w.desktop.clone());
            }
        }
        let mut cur: Option<String> = None;
        for w in &self.wins {
            let on_cur = unsafe {
                vdm.IsWindowOnCurrentVirtualDesktop(HWND(w.hwnd as *mut _))
                    .map(|b| b.as_bool())
                    .unwrap_or(false)
            };
            if on_cur {
                cur = Some(w.desktop.clone());
                break;
            }
        }
        let Some(cur) = cur else { return };
        let (Some(ci), Some(ti)) = (
            order.iter().position(|d| d == &cur),
            order.iter().position(|d| d == target),
        ) else {
            return;
        };
        if ci == ti {
            return;
        }
        let right = ti > ci;
        let steps = (ti as i32 - ci as i32).abs();
        unsafe {
            use windows::Win32::UI::Input::KeyboardAndMouse::keybd_event;
            const VK_LWIN: u8 = 0x5B;
            const VK_CONTROL: u8 = 0x11;
            const VK_LEFT: u8 = 0x25;
            const VK_RIGHT: u8 = 0x27;
            let arrow = if right { VK_RIGHT } else { VK_LEFT };
            for _ in 0..steps {
                keybd_event(VK_LWIN, 0, Default::default(), 0);
                keybd_event(VK_CONTROL, 0, Default::default(), 0);
                keybd_event(arrow, 0, Default::default(), 0);
                keybd_event(arrow, 0, windows::Win32::UI::Input::KeyboardAndMouse::KEYEVENTF_KEYUP, 0);
                keybd_event(VK_CONTROL, 0, windows::Win32::UI::Input::KeyboardAndMouse::KEYEVENTF_KEYUP, 0);
                keybd_event(VK_LWIN, 0, windows::Win32::UI::Input::KeyboardAndMouse::KEYEVENTF_KEYUP, 0);
                std::thread::sleep(std::time::Duration::from_millis(220));
            }
        }
        self.refresh_window_list(true);
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
        let mut sel_set: Vec<isize> = Vec::new();
        let tgt = crate::current_target();
        if tgt != 0 {
            sel_set.push(tgt);
        }
        for (h, _) in crate::regions::snapshot_all() {
            if !sel_set.contains(&h) {
                sel_set.push(h);
            }
        }
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
                        let label = format!("{mark} {title}");
                        let resp = nav_row(ui, cur_sel == Some(*hwnd), &label, !managed)
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
                            ui.menu_button("选中控制", |ui| selection_controls_ui(ui, *hwnd));
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
                        if sel_set.contains(hwnd) {
                            let (tag, color) = crate::selection::mark_for(*hwnd).unwrap_or_else(|| {
                                (title.clone(), (47u8, 194u8, 124u8))
                            });
                            let header = format!("选中控制 · {tag}");
                            egui::CollapsingHeader::new(
                                egui::RichText::new(header)
                                    .size(12.0)
                                    .color(egui::Color32::from_rgb(color.0, color.1, color.2)),
                            )
                            .id_source(("nav_sel", *hwnd))
                            .default_open(false)
                            .show(ui, |ui| {
                                let mut toggle = None;
                                self.window_card(ui, *hwnd, title, &desk, &mut toggle);
                                if let Some(th) = toggle {
                                    select = Some(th);
                                    if GLOBAL_REGISTRY.contains_key(&th) {
                                        restore_now = Some(th);
                                    } else {
                                        control_now = Some(th);
                                    }
                                }
                                ui.horizontal(|ui| {
                                    if ui.button("选中并显示").clicked() {
                                        crate::select_window_for_control(*hwnd);
                                        crate::selection::set_enabled(true);
                                    }
                                    if ui.button("编辑选区…").clicked() {
                                        crate::regions::open_editor(*hwnd);
                                    }
                                    if ui.button("清空选区").clicked() {
                                        crate::regions::clear(*hwnd);
                                    }
                                });
                            });
                        }
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
        self.refresh_window_list(false);
        self.hover_seen = false;
        // 顶部桌面选择：控制当前显示的桌面
        let (ids, current_id) = virtual_desktops();
        let mut desks: Vec<String> = Vec::new();
        for (i, id) in ids.iter().enumerate() {
            let mark = if Some(id) == current_id.as_ref() {
                "（当前）"
            } else {
                ""
            };
            desks.push(format!("桌面 {}{mark}", i + 1));
        }
        if desks.is_empty() {
            for w in &self.wins {
                if !desks.contains(&w.desktop) {
                    desks.push(w.desktop.clone());
                }
            }
        }
        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new("桌面").small().weak());
            if chip(ui, self.desk_filter.is_none(), "全部").clicked() {
                self.desk_filter = None;
            }
            for d in &desks {
                if chip(ui, self.desk_filter.as_deref() == Some(d.as_str()), d).clicked() {
                    self.desk_filter = Some(d.clone());
                }
            }
        });
        ui.add_space(6.0);
        ui.add_space(8.0);
        let wins: Vec<(isize, String, String)> = self
            .wins
            .iter()
            .filter(|w| match &self.desk_filter {
                Some(d) => &w.desktop == d,
                None => true,
            })
            .map(|w| (w.hwnd, w.title.clone(), w.desktop.clone()))
            .collect();
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("窗口总览")
                    .font(font_title(19.0))
                    .color(TEXT_PRIMARY),
            );
            ui.label(
                egui::RichText::new(format!(
                    "共 {} 个 ｜ 已管理 {}",
                    wins.len(),
                    GLOBAL_REGISTRY.len()
                ))
                .small()
                .weak(),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add(egui::Button::new(
                        egui::RichText::new("全部还原").size(13.0),
                    ))
                    .on_hover_text("把已控制的窗口全部恢复原样")
                    .clicked()
                {
                    unsafe { restore_all_windows() };
                    self.refresh_window_list(true);
                }
            });
        });
        ui.add_space(10.0);
        if wins.is_empty() {
            ui.vertical_centered(|ui| {
                ui.add_space(120.0);
                ui.label(egui::RichText::new("没有可见窗口").weak());
            });
            return;
        }
        // 预览要动起来，按帧率档位请求下一帧重绘（关闭档不再空转）
        let repaint_ms: u64 = if self.preview_fps == 0 {
            0
        } else {
            (1000 / self.preview_fps.max(1) as u64).max(16)
        };
        if repaint_ms > 0 {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(repaint_ms));
        }

        // 紧凑列表只用于小窗；大界面保持缩略图网格不变
        if self.mini_mode && self.view_mode == 0 {
            self.list_view(ui, &wins);
            return;
        }
        let mut toggle: Option<isize> = None;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    for (hwnd, title, desktop) in &wins {
                        self.window_card(ui, *hwnd, title, desktop, &mut toggle);
                    }
                });
            });
        if let Some(h) = toggle {
            let managed = GLOBAL_REGISTRY.get(&h).is_some();
            unsafe {
                if managed {
                    restore_window(HWND(h as *mut _));
                } else {
                    control_window(HWND(h as *mut _));
                }
            }
            self.refresh_window_list(true);
        }
        // 悬停 1.5 秒后在桌面上高亮该窗口
        match self.hover {
            Some((h, t0)) if self.hover_seen => {
                if t0.elapsed().as_millis() >= 1500 {
                    unsafe { show_hilite(HWND(h as *mut _)) };
                } else {
                    ui.ctx().request_repaint_after(std::time::Duration::from_millis(150));
                }
            }
            _ => {
                self.hover = None;
                unsafe { hide_hilite() };
            }
        }
    }

    /// 紧凑列表视图：一行一个窗口，空间利用率高
    fn list_view(&mut self, ui: &mut egui::Ui, wins: &[(isize, String, String)]) {
        let mut toggle: Option<isize> = None;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for (hwnd, title, desktop) in wins {
                    let minimized = unsafe { IsIconic(HWND(*hwnd as *mut _)).as_bool() };
                    let (alpha, top) = GLOBAL_REGISTRY
                        .get(hwnd)
                        .map(|s| (s.current_alpha, s.user_pref_topmost))
                        .unwrap_or((255, false));
                    egui::Frame::none()
                        .fill(SURFACE)
                        .stroke(egui::Stroke::new(1.0, BORDER))
                        .rounding(egui::Rounding::same(8.0))
                        .inner_margin(egui::Margin::symmetric(8.0, 6.0))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                if let Some(tex) = self.thumbnail(ui.ctx(), *hwnd) {
                                    let ts = tex.size();
                                    let aspect = ts[1] as f32 / ts[0].max(1) as f32;
                                    let w = 58.0;
                                    let h = (w * aspect).clamp(26.0, 36.0);
                                    let resp = ui.add(
                                        egui::Image::new(egui::load::SizedTexture::new(
                                            tex.id(),
                                            egui::vec2(w, h),
                                        ))
                                        .rounding(4.0)
                                        .sense(egui::Sense::click()),
                                    );
                                    if resp.clicked() {
                                        toggle = Some(*hwnd);
                                    }
                                    resp.context_menu(|ui| window_menu(ui, *hwnd, title));
                                } else {
                                    let (rect, resp) = ui
                                        .allocate_exact_size(egui::vec2(58.0, 32.0), egui::Sense::click());
                                    ui.painter().rect_filled(
                                        rect,
                                        4.0,
                                        egui::Color32::from_rgb(0xF1, 0xF3, 0xF6),
                                    );
                                    if resp.clicked() {
                                        toggle = Some(*hwnd);
                                    }
                                }
                                ui.vertical(|ui| {
                                    ui.add(
                                        egui::Label::new(
                                            egui::RichText::new(title).small().strong(),
                                        )
                                        .truncate(),
                                    );
                                    ui.label(
                                        egui::RichText::new({
                                            let mut meta = format!(
                                                "{}% ｜ {desktop}",
                                                (alpha as f32 / 255.0 * 100.0).round() as i32
                                            );
                                            if minimized {
                                                meta.push_str(" · 已最小化");
                                            }
                                            meta
                                        })
                                        .small()
                                        .weak(),
                                    );
                                });
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui
                                            .selectable_label(
                                                top,
                                                egui::RichText::new("置顶").small(),
                                            )
                                            .clicked()
                                        {
                                            unsafe { toggle_topmost(HWND(*hwnd as *mut _)) };
                                        }
                                    },
                                );
                            });
                        });
                    ui.add_space(4.0);
                }
            });
        let repaint_ms = if self.preview_fps == 0 {
            0
        } else {
            (1000 / self.preview_fps.max(1) as u64).max(16)
        };
        if repaint_ms > 0 {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(repaint_ms));
        }
        if let Some(h) = toggle {
            let managed = GLOBAL_REGISTRY.get(&h).is_some();
            unsafe {
                if managed {
                    restore_window(HWND(h as *mut _));
                } else {
                    control_window(HWND(h as *mut _));
                }
            }
            self.refresh_window_list(true);
        }
        // 悬停 1.5 秒后在桌面上高亮该窗口
        match self.hover {
            Some((h, t0)) if self.hover_seen => {
                if t0.elapsed().as_millis() >= 1500 {
                    unsafe { show_hilite(HWND(h as *mut _)) };
                } else {
                    ui.ctx().request_repaint_after(std::time::Duration::from_millis(150));
                }
            }
            _ => {
                self.hover = None;
                unsafe { hide_hilite() };
            }
        }
    }
    fn window_card(
        &mut self,
        ui: &mut egui::Ui,
        hwnd: isize,
        title: &str,
        desktop: &str,
        toggle: &mut Option<isize>,
    ) {
        let minimized = unsafe { IsIconic(HWND(hwnd as *mut _)).as_bool() };
        let managed = GLOBAL_REGISTRY
            .get(&hwnd)
            .map(|s| (s.current_alpha, s.user_pref_topmost, s.mouse_passthrough, s.pen_passthrough));
        let (alpha, top, _mouse, _pen) = managed.unwrap_or((255, false, false, false));
        // 悬停缩放动画（用上一帧的矩形判断，1 帧延迟无感）
        let hovered = self
            .card_rects
            .get(&hwnd)
            .and_then(|r| ui.ctx().pointer_hover_pos().map(|p| r.contains(p)))
            .unwrap_or(false);
        let anim = ui
            .ctx()
            .animate_bool_with_time(egui::Id::new(("card_hover", hwnd)), hovered, 0.12);
        let zoom = 1.0 + 0.035 * anim;
        let card_w = if self.mini_mode { 172.0 } else { 232.0 };
        let card_resp = ui.allocate_ui_with_layout(
            egui::vec2(card_w, if self.mini_mode { 190.0 } else { 224.0 }),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                egui::Frame::none()
                    .fill(SURFACE)
                    .stroke(egui::Stroke::new(
                        1.0,
                        lerp_color(BORDER, egui::Color32::from_rgb(0xA8, 0xE6, 0xC8), anim),
                    ))
                    .rounding(egui::Rounding::same(12.0))
                    .shadow(egui::epaint::Shadow {
                        offset: egui::vec2(0.0, 1.0 + anim),
                        blur: 6.0 + 8.0 * anim,
                        spread: 0.0,
                        color: egui::Color32::from_black_alpha((10.0 + 16.0 * anim) as u8),
                    })
                    .inner_margin(egui::Margin::same(8.0))
                    .show(ui, |ui| {
                        let box_w = card_w - 18.0;
                        let box_h = if self.mini_mode { 96.0 } else { 118.0 };
                        let img_size = egui::vec2(box_w * zoom, box_h * zoom);
                        match self.thumbnail(ui.ctx(), hwnd) {
                            Some(tex) => {
                                let resp = ui.add(
                                    {
                                        let ts = tex.size();
                                        let aspect = ts[1] as f32 / ts[0].max(1) as f32;
                                        let (dw, dh) = if box_w * aspect <= box_h {
                                            (box_w, box_w * aspect)
                                        } else {
                                            (box_h / aspect, box_h)
                                        };
                                        egui::Image::new(egui::load::SizedTexture::new(
                                            tex.id(),
                                            egui::vec2(dw, dh),
                                        ))
                                    }
                                        .rounding(8.0)
                                        .sense(egui::Sense::click()),
                                );
                                if resp.clicked() {
                                    *toggle = Some(hwnd);
                                }
                                resp.context_menu(|ui| window_menu(ui, hwnd, title));
                            }
                            None => {
                                let (rect, resp) = ui.allocate_exact_size(img_size, egui::Sense::click());
                                ui.painter().rect_filled(
                                    rect,
                                    8.0,
                                    egui::Color32::from_rgb(0xF1, 0xF3, 0xF6),
                                );
                                ui.painter().text(
                                    rect.center(),
                                    egui::Align2::CENTER_CENTER,
                                    if self.preview_fps == 0 {
                                        "预览已关闭"
                                    } else if minimized {
                                        "已最小化"
                                    } else {
                                        "无预览"
                                    },
                                    egui::FontId::proportional(12.0),
                                    TEXT_SECONDARY,
                                );
                                if resp.clicked() {
                                    *toggle = Some(hwnd);
                                }
                                resp.context_menu(|ui| window_menu(ui, hwnd, title));
                            }
                        }
                        ui.add_space(6.0);
                        ui.add(
                            egui::Label::new({
                                let app = unsafe { cached_app_name(HWND(hwnd as *mut _)) };
                                let text = if app.is_empty() {
                                    title.to_string()
                                } else {
                                    format!("【{app}】 {title}")
                                };
                                egui::RichText::new(text)
                                    .font(font_title(14.0))
                                    .color(TEXT_PRIMARY)
                            })
                            .truncate(),
                        );
                        let mut badges = String::new();
                        match managed {
                            Some((a, top, m, p)) => {
                                badges.push_str(&format!(
                                    "{}%",
                                    (a as f32 / 255.0 * 100.0).round() as i32
                                ));
                                if top {
                                    badges.push_str(" · 置顶");
                                }
                                if m {
                                    badges.push_str(" · 鼠标透");
                                }
                                if p {
                                    badges.push_str(" · 笔透");
                                }
                            }
                            None => badges.push_str("未管理"),
                        }
                        let mut meta = format!("{badges} ｜ {desktop}");
                        if minimized {
                            meta.push_str(" · 已最小化");
                        }
                        ui.label(egui::RichText::new(meta).small().weak());
                        ui.add_space(4.0);
                        let mut a = alpha as f32;
                        ui.horizontal(|ui| {
                            let slider_w = if self.mini_mode { 84.0 } else { 128.0 };
                            ui.label(egui::RichText::new("透明").small().weak());
                            ui.spacing_mut().slider_width = slider_w;
                            // 滑轨在白色卡片上默认几乎看不见，给个浅灰底
                            ui.visuals_mut().widgets.inactive.bg_fill =
                                egui::Color32::from_rgb(0xE1, 0xE4, 0xE9);
                            ui.visuals_mut().widgets.hovered.bg_fill =
                                egui::Color32::from_rgb(0xCF, 0xD4, 0xDB);
                            let resp = ui.add(egui::Slider::new(&mut a, 30.0..=255.0).show_value(false));
                            if resp.changed() {
                                unsafe {
                                    ensure_managed(HWND(hwnd as *mut _));
                                    set_window_alpha(HWND(hwnd as *mut _), a as u8);
                                }
                            }
                            if ui
                                .selectable_label(top, egui::RichText::new("置顶").small())
                                .clicked()
                            {
                                unsafe { toggle_topmost(HWND(hwnd as *mut _)) };
                            }
                        });
                    });
            },
        );
        self.card_rects.insert(hwnd, card_resp.response.rect);
        ui.add_space(8.0);
    }

    fn thumbnail(&mut self, ctx: &egui::Context, hwnd: isize) -> Option<egui::TextureHandle> {
        let now = std::time::Instant::now();
        if self.preview_fps == 0 {
            return None;
        }
        let win = HWND(hwnd as *mut _);
        let minimized = unsafe { IsIconic(win).as_bool() };
        let is_fg = unsafe { GetForegroundWindow() == win };
        let fg_ms = (1000 / self.preview_fps.max(1) as u64).max(16);
        let interval_ms = if is_fg { fg_ms } else { fg_ms * 4 };
        if let Some((tex, at)) = self.thumbs.get(&hwnd) {
            // 最小化时不要再抓：PrintWindow 对最小化窗口只会给黑帧，
            // 保留上一帧比变回「无预览」有用得多。
            if minimized || (now.duration_since(*at).as_millis() as u64) < interval_ms {
                return Some(tex.clone());
            }
        }
        if minimized {
            return None;
        }
        unsafe {
            if let Some(img) = capture_window(win) {
                let tex = ctx.load_texture(
                    format!("thumb_{hwnd}"),
                    img,
                    egui::TextureOptions::LINEAR,
                );
                self.thumbs.insert(hwnd, (tex.clone(), now));
                return Some(tex);
            }
            // 抓取失败（GPU 黑帧 / 暂时不可见）时别把旧帧扔掉：
            // 继续显示上一帧，等下个 interval 再重试。
            if let Some((old, _)) = self.thumbs.get(&hwnd) {
                let old = old.clone();
                self.thumbs.insert(hwnd, (old.clone(), now));
                return Some(old);
            }
        }
        None
    }
    fn validate_hotkey_spec(spec: &HotkeySpec) -> Result<(), String> {
        unsafe {
            // 空键 = 未绑定，允许保存
            if spec.key.trim().is_empty() {
                return Ok(());
            }
            let mods = parse_modifiers(&spec.modifiers);
            let vk = parse_vk(&spec.key);
            if vk.is_none() {
                return Err(format!("无效按键：{}", spec.key));
            }
            let mouse = matches!(vk, Some(v) if is_mouse_vk(v));
            if mods.0 == 0 {
                // 鼠标键可以单按；但左右键必须带修饰键，否则会抢掉正常点击
                if !mouse {
                    return Err("修饰键不能为空（建议至少包含 ALT/CTRL/SHIFT/WIN 之一）".into());
                }
                if matches!(vk, Some(0x01) | Some(0x02)) {
                    return Err("鼠标左键/右键必须带修饰键（例如 Alt+鼠标左键）".into());
                }
            }
        }
        Ok(())
    }

    fn validate_config(cfg: &HotkeyConfig) -> Result<(), String> {
        let mut used: std::collections::HashMap<(String, String), String> = std::collections::HashMap::new();

        let mut check = |name: &str, spec: &HotkeySpec| -> Result<(), String> {
            Self::validate_hotkey_spec(spec)?;
            if spec.key.trim().is_empty() {
                return Ok(());
            }
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
        check("选择/控制窗口", &cfg.select_session)?;
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
                HotkeyCaptureTarget::SelectSession => self.hotkey_draft.select_session = spec,
            }
            self.hotkey_message = Some("已录入快捷键".into());
        }
    }

    fn save_and_reload_hotkeys(&mut self) {
        if let Err(e) = Self::validate_config(&self.hotkey_draft) {
            self.hotkey_message = Some(e);
            return;
        }
        // 槽位里已经用到鼠标侧键时，把旧版「鼠标侧键」那套清空，不然一次按下会触发两次
        {
            let used_x1 = HotkeySlot::ALL.iter().any(|s| {
                let k = s.get(&self.hotkey_draft).key.trim().to_uppercase();
                k == "鼠标侧键1" || k == "XBUTTON1" || k == "MOUSE4"
            });
            let used_x2 = HotkeySlot::ALL.iter().any(|s| {
                let k = s.get(&self.hotkey_draft).key.trim().to_uppercase();
                k == "鼠标侧键2" || k == "XBUTTON2" || k == "MOUSE5"
            });
            if let Some(m) = self.hotkey_draft.mouse.as_mut() {
                if used_x1 {
                    m.xbutton1 = None;
                }
                if used_x2 {
                    m.xbutton2 = None;
                }
            }
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

/// 开机自启动：写 HKCU\...\Run，值名 TransGlass，内容是带引号的 exe 路径
fn autostart_enabled() -> bool {
    use windows::Win32::System::Registry::{RegOpenKeyExW, RegQueryValueExW, HKEY, KEY_READ};
    unsafe {
        let sub = wide_reg("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
        let mut hkey = HKEY::default();
        if RegOpenKeyExW(
            windows::Win32::System::Registry::HKEY_CURRENT_USER,
            windows::core::PCWSTR(sub.as_ptr()),
            0,
            KEY_READ,
            &mut hkey,
        )
        .is_err()
        {
            return false;
        }
        let name = wide_reg("TransGlass");
        let mut size: u32 = 0;
        let ok = RegQueryValueExW(
            hkey,
            windows::core::PCWSTR(name.as_ptr()),
            None,
            None,
            None,
            Some(&mut size),
        )
        .is_ok();
        let _ = windows::Win32::System::Registry::RegCloseKey(hkey);
        ok && size > 0
    }
}

/// 写/删开机自启动项
fn set_autostart(on: bool) -> Result<(), String> {
    use windows::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegSetValueExW, HKEY, KEY_SET_VALUE,
        REG_OPTION_NON_VOLATILE, REG_SZ,
    };
    unsafe {
        let sub = wide_reg("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
        let mut hkey = HKEY::default();
        let rc = RegCreateKeyExW(
            windows::Win32::System::Registry::HKEY_CURRENT_USER,
            windows::core::PCWSTR(sub.as_ptr()),
            0,
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut hkey,
            None,
        );
        if rc.is_err() {
            return Err(format!("打开 Run 键失败（Win32 {}）", rc.0));
        }
        let name = wide_reg("TransGlass");
        let res = if on {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let val = format!("\"{}\"", exe.display());
            let data: Vec<u16> = val.encode_utf16().chain(std::iter::once(0)).collect();
            let bytes = std::slice::from_raw_parts(data.as_ptr() as *const u8, data.len() * 2);
            let rc = RegSetValueExW(
                hkey,
                windows::core::PCWSTR(name.as_ptr()),
                0,
                REG_SZ,
                Some(bytes),
            );
            if rc.is_err() {
                Err(format!("写入失败（Win32 {}）", rc.0))
            } else {
                Ok(())
            }
        } else {
            let rc = RegDeleteValueW(hkey, windows::core::PCWSTR(name.as_ptr()));
            if rc.is_err() {
                Err(format!("删除失败（Win32 {}）", rc.0))
            } else {
                Ok(())
            }
        };
        let _ = RegCloseKey(hkey);
        res
    }
}

fn wide_reg(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 窗口所属进程的 exe 名（小写，不含路径）
unsafe fn process_exe_name(hwnd: HWND) -> Option<String> {
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid == 0 {
        return None;
    }
    let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
    let mut buf = vec![0u16; 1024];
    let mut len = buf.len() as u32;
    let ok = QueryFullProcessImageNameW(
        h,
        PROCESS_NAME_WIN32,
        windows::core::PWSTR(buf.as_mut_ptr()),
        &mut len,
    )
    .is_ok();
    let _ = windows::Win32::Foundation::CloseHandle(h);
    if !ok || len == 0 {
        return None;
    }
    let path = String::from_utf16_lossy(&buf[..len as usize]);
    Some(path.rsplit('\\').next().unwrap_or("").to_lowercase())
}

/// 软件名缓存：(hwnd, 名字, 时间)。列表每帧刷新，不缓存会每帧 OpenProcess 好几次
static APP_CACHE: std::sync::RwLock<Vec<(isize, String, f32)>> = std::sync::RwLock::new(Vec::new());

/// 带缓存的软件短名（3 秒过期，窗口换了也能跟上）
pub(crate) unsafe fn cached_app_name(hwnd: HWND) -> String {
    let key = hwnd.0 as isize;
    let now = anim_secs();
    if let Ok(c) = APP_CACHE.read() {
        if let Some((_, name, at)) = c.iter().find(|(k, _, _)| *k == key) {
            if now - at < 3.0 {
                return name.clone();
            }
        }
    }
    let name = app_short_name(hwnd);
    if let Ok(mut c) = APP_CACHE.write() {
        if let Some(e) = c.iter_mut().find(|(k, _, _)| *k == key) {
            e.1 = name.clone();
            e.2 = now;
        } else {
            c.push((key, name.clone(), now));
            if c.len() > 96 {
                c.remove(0);
            }
        }
    }
    name
}

/// 给界面用的软件短名（Photoshop / Lightroom / AE …）
pub(crate) unsafe fn app_short_name(hwnd: HWND) -> String {
    let Some(exe) = process_exe_name(hwnd) else {
        return String::new();
    };
    let stem = exe.trim_end_matches(".exe");
    let name = if stem.contains("photoshop") {
        "Photoshop"
    } else if stem.contains("lightroom") {
        "Lightroom"
    } else if stem.contains("aftereffects") {
        "AE"
    } else if stem.contains("illustrator") {
        "AI"
    } else if stem.contains("premiere") {
        "PR"
    } else if stem.contains("indesign") {
        "ID"
    } else if stem.contains("huashijie") || stem.contains("artworld") || stem.contains("画世界") {
        "画世界"
    } else if stem.contains("chrome") {
        "Chrome"
    } else if stem.contains("msedge") {
        "Edge"
    } else if stem.contains("firefox") {
        "Firefox"
    } else if stem.contains("code") {
        "VS Code"
    } else if stem.contains("explorer") {
        "资源管理器"
    } else if stem.contains("transglass") {
        "TransGlass"
    } else if stem.contains("qqmusic") || stem.contains("cloudmusic") {
        "QQ音乐"
    } else if stem.contains("sakuracat") {
        "SakuraCat"
    } else if stem.contains("wechat") {
        "微信"
    } else if stem.contains("notepad") {
        "记事本"
    } else {
        // 未知的：按空格/连字符切词，每段首字母大写，读起来像软件名
        let mut out = String::new();
        let mut upper = true;
        for ch in stem.chars() {
            if ch == ' ' || ch == '-' || ch == '_' {
                out.push(' ');
                upper = true;
            } else if upper {
                out.extend(ch.to_uppercase());
                upper = false;
            } else {
                out.push(ch);
            }
        }
        return out;
    };
    name.to_string()
}

/// 需要兼容处理的重型应用（PS 家族、画世界等）
unsafe fn compat_app_name(hwnd: HWND) -> Option<String> {
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid == 0 {
        return None;
    }
    let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
    let mut buf = vec![0u16; 1024];
    let mut len = buf.len() as u32;
    let ok = QueryFullProcessImageNameW(
        h,
        PROCESS_NAME_WIN32,
        windows::core::PWSTR(buf.as_mut_ptr()),
        &mut len,
    )
    .is_ok();
    let _ = windows::Win32::Foundation::CloseHandle(h);
    if !ok || len == 0 {
        return None;
    }
    let path = String::from_utf16_lossy(&buf[..len as usize]);
    let exe = path.rsplit('\\').next().unwrap_or("").to_lowercase();
    const LIST: [&str; 17] = [
        "photoshop",
        "illustrator",
        "aftereffects",
        "lightroom",
        "indesign",
        "premiere",
        "huashijie",
        "artworld",
        "画世界",
        "painter",
        "obs64",
        "obs32",
        "vlc",
        "potplayer",
        "winword",
        "excel",
        "powerpnt",
    ];
    if LIST.iter().any(|n| exe.contains(n)) {
        Some(exe)
    } else {
        None
    }
}

/// 直接改主窗口大小：无边框窗口尺寸就是客户区尺寸，比 ViewportCommand::InnerSize 稳
fn resize_root_window(w: i32, h: i32) {
    let raw = ROOT_HWND.load(Ordering::Relaxed);
    if raw == 0 {
        return;
    }
    unsafe {
        let _ = SetWindowPos(
            HWND(raw as *mut _),
            None,
            0,
            0,
            w,
            h,
            SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
        );
    }
}

/// 用默认浏览器打开使用教程
fn open_help_page() {
    let url = "https://github.com/railgun-1145/TransGlass/blob/master/docs/%E4%BD%BF%E7%94%A8%E6%95%99%E7%A8%8B.md";
    let _ = std::process::Command::new("cmd")
        .args(["/c", "start", "", url])
        .spawn();
}

/// 无边框窗口的边缘缩放：指针进到 6px 里就设置光标，按下就发 BeginResize
fn handle_window_resize(ctx: &egui::Context) {
    let rect = ctx.screen_rect();
    let Some(p) = ctx.pointer_hover_pos() else {
        return;
    };
    const M: f32 = 6.0;
    let left = p.x < rect.left() + M;
    let right = p.x > rect.right() - M;
    let top = p.y < rect.top() + M;
    let bottom = p.y > rect.bottom() - M;
    if !(left || right || top || bottom) {
        return;
    }
    let dir = if left && top {
        egui::ResizeDirection::NorthWest
    } else if right && top {
        egui::ResizeDirection::NorthEast
    } else if left && bottom {
        egui::ResizeDirection::SouthWest
    } else if right && bottom {
        egui::ResizeDirection::SouthEast
    } else if left {
        egui::ResizeDirection::West
    } else if right {
        egui::ResizeDirection::East
    } else if top {
        egui::ResizeDirection::North
    } else {
        egui::ResizeDirection::South
    };
    let cursor = match dir {
        egui::ResizeDirection::North | egui::ResizeDirection::South => {
            egui::CursorIcon::ResizeVertical
        }
        egui::ResizeDirection::East | egui::ResizeDirection::West => {
            egui::CursorIcon::ResizeHorizontal
        }
        egui::ResizeDirection::NorthWest | egui::ResizeDirection::SouthEast => {
            egui::CursorIcon::ResizeNwSe
        }
        _ => egui::CursorIcon::ResizeNeSw,
    };
    ctx.set_cursor_icon(cursor);
    if ctx.input(|i| i.pointer.primary_down()) {
        ctx.send_viewport_cmd(egui::ViewportCommand::BeginResize(dir));
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TitleBtn {
    Min,
    Max,
    Restore,
    Close,
}

/// 标题栏按钮：自绘小方块，悬停才出底色（关闭键悬停变红）
fn titlebar_button(ui: &mut egui::Ui, kind: TitleBtn) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(34.0, 28.0), egui::Sense::click());
    let hover = ui
        .ctx()
        .animate_bool_with_time(resp.id.with("h"), resp.hovered(), 0.1);
    let is_close = kind == TitleBtn::Close;
    if hover > 0.01 {
        let bg = if is_close {
            egui::Color32::from_rgb(0xE8, 0x11, 0x23)
        } else {
            egui::Color32::from_rgb(0xF0, 0xF0, 0xF0)
        };
        let bg = egui::Color32::from_rgba_unmultiplied(
            bg.r(),
            bg.g(),
            bg.b(),
            (255.0 * hover) as u8,
        );
        ui.painter()
            .rect_filled(rect.shrink(3.0), egui::Rounding::same(4.0), bg);
    }
    let fg = if is_close && hover > 0.5 {
        egui::Color32::WHITE
    } else {
        TEXT_PRIMARY
    };
    let c = rect.center();
    let stroke = egui::Stroke::new(1.2, fg);
    match kind {
        TitleBtn::Min => {
            ui.painter().hline(c.x - 4.5..=c.x + 4.5, c.y + 0.5, stroke);
        }
        TitleBtn::Max => {
            ui.painter().rect_stroke(
                egui::Rect::from_center_size(c, egui::vec2(9.0, 9.0)),
                egui::Rounding::same(1.0),
                stroke,
            );
        }
        TitleBtn::Restore => {
            ui.painter().rect_stroke(
                egui::Rect::from_center_size(c + egui::vec2(1.0, 1.0), egui::vec2(8.0, 8.0)),
                egui::Rounding::same(1.0),
                stroke,
            );
            ui.painter().hline(
                (c.x - 5.0)..=(c.x + 3.0),
                c.y - 4.5,
                egui::Stroke::new(1.2, fg),
            );
        }
        TitleBtn::Close => {
            let d = 4.5;
            ui.painter().line_segment(
                [egui::pos2(c.x - d, c.y - d), egui::pos2(c.x + d, c.y + d)],
                stroke,
            );
            ui.painter().line_segment(
                [egui::pos2(c.x + d, c.y - d), egui::pos2(c.x - d, c.y + d)],
                stroke,
            );
        }
    }
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp
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
            let mut cancel = false;
            // 刚点绑定位的那一下鼠标左键不能算，否则会立刻绑成"鼠标左键"
            let armed = self
                .hotkey_capture_at
                .map(|t| t.elapsed().as_secs_f32() > 0.18)
                .unwrap_or(true);
            ctx.input(|i| {
                for e in &i.events {
                    if let egui::Event::Key {
                        key,
                        pressed: true,
                        modifiers,
                        ..
                    } = e
                    {
                        if *key == egui::Key::Escape {
                            cancel = true;
                            break;
                        }
                        if let Some(k) = egui_key_to_vk_name(*key) {
                            captured = Some((egui_modifiers_to_string(*modifiers), k));
                            break;
                        }
                    }
                    if let egui::Event::PointerButton {
                        button,
                        pressed: true,
                        modifiers,
                        ..
                    } = e
                    {
                        if !armed && matches!(button, egui::PointerButton::Primary) {
                            continue;
                        }
                        let name = match button {
                            egui::PointerButton::Primary => "鼠标左键",
                            egui::PointerButton::Secondary => "鼠标右键",
                            egui::PointerButton::Middle => "鼠标中键",
                            egui::PointerButton::Extra1 => "鼠标侧键1",
                            egui::PointerButton::Extra2 => "鼠标侧键2",
                        };
                        captured = Some((egui_modifiers_to_string(*modifiers), name.to_string()));
                        break;
                    }
                    if let egui::Event::MouseWheel { delta, modifiers, .. } = e {
                        let name = if delta.y > 0.0 { "滚轮上" } else { "滚轮下" };
                        captured = Some((egui_modifiers_to_string(*modifiers), name.to_string()));
                        break;
                    }
                }
            });
            if cancel {
                self.hotkey_capture = None;
                self.hotkey_message = Some("已取消".into());
            } else if let Some((mods, key)) = captured {
                self.apply_hotkey_capture(mods, key);
            }
        }

        // 3. UI 绘制 (更简洁现代的布局)
        // 3. 大界面：左侧导航（桌面分组 + 窗口），右侧详情
        self.refresh_window_list(false);

        // 无边框窗口：边缘 6px 内可以拖动缩放（原生边框没了，得自己处理）
        handle_window_resize(ctx);
        self.draw_titlebar(ctx);

        // 小窗模式：切换窗口尺寸与置顶
        if self.mini_mode != self.last_mini {
            self.last_mini = self.mini_mode;
            if self.mini_mode {
                resize_root_window(400, 560);
                ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(
                    egui::WindowLevel::AlwaysOnTop,
                ));
            } else {
                resize_root_window(1000, 700);
                ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(if self.always_on_top {
                    egui::WindowLevel::AlwaysOnTop
                } else {
                    egui::WindowLevel::Normal
                }));
            }
        }


        if self.settings_open {
            self.settings_page_ui(ctx);
        } else if !self.mini_mode {
        egui::TopBottomPanel::bottom("actions_bar").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("设置").clicked() {
                    self.settings_open = true;
                    self.settings_fade = Some(std::time::Instant::now());
                }
                // 小窗模式放右下角，字号比左边的小一档
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new(if self.mini_mode {
                                    "返回大界面"
                                } else {
                                    "小窗模式"
                                })
                                .size(12.0)
                                .color(TEXT_SECONDARY),
                            )
                            .frame(false),
                        )
                        .clicked()
                    {
                        self.mini_mode = !self.mini_mode;
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
                    ui.label(
                        egui::RichText::new("TransGlass")
                            .color(ACCENT)
                            .font(font_title(20.0)),
                    );
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

        }

        // 设置页占满主区域，这里不再画窗口列表
        if !self.settings_open {
        egui::SidePanel::right("ps_panel")
            .resizable(false)
            .default_width(256.0)
            .show(ctx, |ui| {
                ps_selection_panel(ui, &mut self.ps_selected);
            });
        egui::CentralPanel::default().show(ctx, |ui| {
            if self.mini_mode {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("TransGlass").color(ACCENT).strong().size(16.0));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("还原").clicked() {
                            self.mini_mode = false;
                        }
                        if ui.button("设置").clicked() {
                            // 小窗里点设置：先恢复成大界面，再进设置页
                            self.mini_mode = false;
                            self.settings_open = true;
                            self.settings_fade = Some(std::time::Instant::now());
                        }
                    });
                });
                ui.separator();
            }
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
                            egui::RichText::new(format!("{} 条配置/热键问题", issues.len()))
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
        }

        // 悬停计时由后台线程驱动（不依赖 UI 帧），这里只负责离开时清除
        if !self.hover_seen {
            self.hover = None;
            HOVER_HWND.store(0, Ordering::Relaxed);
            unsafe { hide_hilite() };
        }

        // 无边框窗口：自己描一圈细边，不然跟桌面糊在一起分不清边界
        {
            let painter = ctx.layer_painter(egui::LayerId::new(
                egui::Order::Foreground,
                egui::Id::new("win_border"),
            ));
            // 四条 1px 的实心线，比 rect_stroke 稳（半像素会被抗锯齿糊掉）
            let r = ctx.screen_rect();
            let c = egui::Color32::from_rgb(0xB0, 0xB0, 0xB0);
            let one = 1.0;
            painter.rect_filled(
                egui::Rect::from_min_max(r.min, egui::pos2(r.max.x, r.min.y + one)),
                0.0,
                c,
            );
            painter.rect_filled(
                egui::Rect::from_min_max(egui::pos2(r.min.x, r.max.y - one), r.max),
                0.0,
                c,
            );
            painter.rect_filled(
                egui::Rect::from_min_max(r.min, egui::pos2(r.min.x + one, r.max.y)),
                0.0,
                c,
            );
            painter.rect_filled(
                egui::Rect::from_min_max(egui::pos2(r.max.x - one, r.min.y), r.max),
                0.0,
                c,
            );
        }

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

    }
}

fn create_tray_icon() -> TrayIcon {
    let tray_menu = Menu::new();
    let show_item = MenuItem::with_id("show", "打开 TransGlass", true, None);
    let ball_item = MenuItem::with_id("ball", "唤出悬浮球", true, None);
    let reset_all_item = MenuItem::with_id("reset_all", "全部窗口还原", true, None);
    let exit_item = MenuItem::with_id("exit", "退出程序", true, None);

    let _ = tray_menu.append_items(&[
        &show_item,
        &ball_item,
        &PredefinedMenuItem::separator(),
        &reset_all_item,
        &PredefinedMenuItem::separator(),
        &exit_item,
    ]);

    // 加载自定义图标
    let icon = load_custom_icon().unwrap_or_else(|| {
        // 默认图标：我的世界玻璃方块（浅青玻璃 + 白边框 + 斜向高光）
        let mut rgba = Vec::with_capacity(32 * 32 * 4);
        for y in 0..32i32 {
            for x in 0..32i32 {
                let inside = (5..=26).contains(&x) && (5..=26).contains(&y);
                if !inside {
                    rgba.extend_from_slice(&[0, 0, 0, 0]);
                    continue;
                }
                let edge = x == 5 || x == 26 || y == 5 || y == 26;
                let inner_edge = x == 6 || x == 25 || y == 6 || y == 25;
                if edge {
                    rgba.extend_from_slice(&[236, 246, 250, 245]);
                } else if inner_edge {
                    rgba.extend_from_slice(&[210, 232, 240, 220]);
                } else {
                    // 斜向玻璃高光
                    let d1 = (x - y).abs();
                    let d2 = ((x - 5) + (y - 5) - 20).abs();
                    let a = if d1 <= 1 || d2 <= 1 { 205 } else { 170 };
                    let shade = ((x + y) as f32 / 52.0 * 18.0) as u8;
                    rgba.extend_from_slice(&[
                        205u8.saturating_sub(shade),
                        228u8.saturating_sub(shade / 2),
                        238u8.saturating_sub(shade / 3),
                        a,
                    ]);
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
    load_ball_tex();
    let boot_state = load_ui_state();
    load_ball_skin(boot_state.skin.as_deref(), ui_doll_model(&boot_state));
    if let Ok(mut g) = DEV_MOUSE.write() {
        *g = boot_state.input_mouse.clone();
    }
    if let Ok(mut g) = DEV_PEN.write() {
        *g = boot_state.input_pen.clone();
    }
    if let Ok(mut g) = DEV_TABLET.write() {
        *g = boot_state.input_tablet.clone();
    }
    spawn_raw_input_tracker();
    BALL_FLOAT.store(boot_state.idle_float.unwrap_or(true), Ordering::Relaxed);
    BALL_SWAY.store(boot_state.idle_sway.unwrap_or(true), Ordering::Relaxed);
    if let Some(sc) = boot_state.ball_scale {
        BALL_SCALE.store(sc.to_bits(), Ordering::Relaxed);
    }
    unsafe { ball_spawn() };
    DOLL_DBLCLICK.store(boot_state.doll_dblclick.unwrap_or(true), Ordering::Relaxed);
    if boot_state.ball_visible == Some(false) {
        unsafe { set_ball_visible(false) };
    }
    unsafe { purge_shell_managed() };
    spawn_device_tracking_ticker();
    spawn_doll_ticker();

    thread::spawn(|| {
        while let Ok(event) = MenuEvent::receiver().recv() {
            match event.id.0.as_str() {
                "show" => {
                    show_root_window();
                }
                "ball" => unsafe { set_ball_visible(true) },
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
            match event {
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    ..
                }
                | TrayIconEvent::DoubleClick {
                    button: MouseButton::Left,
                    ..
                } => show_root_window(),
                // 托盘图标右键：唤出悬浮球（没显示的时候才需要）
                TrayIconEvent::Click {
                    button: MouseButton::Right,
                    ..
                } => unsafe {
                    if !BALL_SHOWN.load(Ordering::Relaxed) {
                        set_ball_visible(true);
                    }
                },
                _ => {}
            }
        }
    });

    let tray_icon = create_tray_icon();

    // 点透会话选择器：独立线程 + 分层窗口，热键只往它投消息
    selector::init();
    regions::init();
    selection::init();

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
                    11 => {
                        crate::selector::toggle();
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
            ui_state.h.filter(|h| *h >= 560.0).unwrap_or(720.0),
        ])
        .with_title("TransGlass 控制面板")
        // 无系统边框：标题栏自己画，跟界面风格统一
        .with_decorations(false)
        .with_visible(true);
    // 任务栏/标题栏图标：就是那块 MC 玻璃（程序目录的 icon.png）
    if let Some(icon) = window_icon_data() {
        viewport = viewport.with_icon(icon);
    }
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
    /// 点透会话：Alt+Tab 式选窗口，临时关点透，选中后可拖动/控制，Esc 恢复
    #[serde(default = "default_select_session_spec")]
    select_session: HotkeySpec,
    mouse: Option<MouseBindingsSpec>,
    /// true = 用低级键盘钩子强占组合键（即使被其它程序占用也能生效）
    #[serde(default)]
    hotkey_priority: Option<bool>,
}

fn default_select_session_spec() -> HotkeySpec {
    HotkeySlot::SelectSession.default_spec()
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
        update: HotkeySpec { modifiers: "ALT".to_string(), key: "U".to_string() },
        reload: Some(HotkeySlot::Reload.default_spec()),
        select_session: HotkeySlot::SelectSession.default_spec(),
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
    /// 预览帧率（0 = 关闭）
    preview_fps: Option<u32>,
    ball_scale: Option<f32>,
    skin: Option<String>,
    /// 纸娃娃模型：None = 按皮肤自动判定，Some("classic"/"slim") = 强制
    doll_model: Option<String>,
    /// 待机上下浮动（默认开）
    idle_float: Option<bool>,
    /// 待机手臂轻摆（默认开）
    idle_sway: Option<bool>,
    /// 设置里选定的输入设备路径（None = 自动）
    input_mouse: Option<String>,
    /// 是否显示悬浮球（默认显示）
    ball_visible: Option<bool>,
    /// 双击纸娃娃打开主界面（默认开）
    doll_dblclick: Option<bool>,
    input_pen: Option<String>,
    input_tablet: Option<String>,
    view_mode: Option<u8>,
    /// 兼容旧配置
    preview_rate: Option<u8>,
}

fn ui_doll_model(st: &UiState) -> Option<doll::ModelKind> {
    st.doll_model.as_deref().and_then(doll::ModelKind::parse)
}

/// 任务栏 / 标题栏图标：读程序目录的 icon.png（那块 MC 玻璃），失败就没图标。
fn window_icon_data() -> Option<egui::IconData> {
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    for name in ["icon.png", "icon2.png", "tray_icon.png"] {
        if let Ok(img) = image::open(dir.join(name)) {
            let rgba = img.to_rgba8();
            let (w, h) = (rgba.width(), rgba.height());
            if w == 0 || h == 0 {
                continue;
            }
            return Some(egui::IconData {
                rgba: rgba.into_raw(),
                width: w,
                height: h,
            });
        }
    }
    None
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
pub(crate) const HOTKEY_IDS: [i32; 11] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11];

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
        // 鼠标键（虚拟键码）+ 滚轮（自定义码，只有鼠标钩子认）
        "鼠标左键" | "MOUSE1" => 0x01,
        "鼠标右键" | "MOUSE2" => 0x02,
        "鼠标中键" | "MOUSE3" => 0x04,
        "鼠标侧键1" | "XBUTTON1" | "MOUSE4" => 0x05,
        "鼠标侧键2" | "XBUTTON2" | "MOUSE5" => 0x06,
        "滚轮上" | "WHEELUP" => 0x20A,
        "滚轮下" | "WHEELDOWN" => 0x20B,
        _ => return None,
    };
    Some(vk)
}

/// 是不是"鼠标键"：这些键 RegisterHotKey 注册不了，由鼠标钩子接管
pub(crate) fn is_mouse_vk(vk: u32) -> bool {
    matches!(vk, 0x01 | 0x02 | 0x04 | 0x05 | 0x06 | 0x20A | 0x20B)
}

/// Windows 没有"查询这个全局热键被谁注册了"的接口（RegisterHotKey 只返回
/// 1409），所以冲突时只能按"进程在跑 + 它默认占这个键"给用户一个提示。
/// 表里只放默认键位稳定、且真的会抢 Alt+R 这类组合的常见软件；
/// 文案里写明是"很可能被谁占用"，而不是甩一个 Win32 错误码。
struct KnownHotkeyOwner {
    /// 小写 exe 名
    exe: &'static str,
    /// 给用户看的软件名
    app: &'static str,
    /// 对方默认用这个键做什么
    feature: &'static str,
    /// MOD_* 位
    mods: u32,
    /// 虚拟键码
    vk: u32,
}

const KNOWN_HOTKEY_OWNERS: &[KnownHotkeyOwner] = &[
    // NVIDIA App / GeForce 叠加层（ALT+R 性能统计、ALT+SHIFT+R 轮换指标）
    KnownHotkeyOwner { exe: "nvidia overlay.exe", app: "NVIDIA App 的游戏内叠加层", feature: "打开/关闭性能统计", mods: 0x0001, vk: 0x52 },
    KnownHotkeyOwner { exe: "nvidia overlay.exe", app: "NVIDIA App 的游戏内叠加层", feature: "轮换性能统计指标", mods: 0x0005, vk: 0x52 },
    KnownHotkeyOwner { exe: "nvidia overlay.exe", app: "NVIDIA App 的游戏内叠加层", feature: "呼出叠加层", mods: 0x0001, vk: 0x5A },
    KnownHotkeyOwner { exe: "nvidia share.exe", app: "NVIDIA GeForce 叠加层", feature: "打开/关闭性能统计", mods: 0x0001, vk: 0x52 },
    KnownHotkeyOwner { exe: "nvidia share.exe", app: "NVIDIA GeForce 叠加层", feature: "轮换性能统计指标", mods: 0x0005, vk: 0x52 },
    KnownHotkeyOwner { exe: "nvidia share.exe", app: "NVIDIA GeForce 叠加层", feature: "呼出叠加层", mods: 0x0001, vk: 0x5A },
    // AMD Software 的覆盖层
    KnownHotkeyOwner { exe: "radeonsoftware.exe", app: "AMD Software 的覆盖层", feature: "打开/关闭覆盖层", mods: 0x0001, vk: 0x52 },
    // 微信 / QQ 的截图
    KnownHotkeyOwner { exe: "weixin.exe", app: "微信", feature: "截图", mods: 0x0001, vk: 0x41 },
    KnownHotkeyOwner { exe: "wechat.exe", app: "微信", feature: "截图", mods: 0x0001, vk: 0x41 },
    KnownHotkeyOwner { exe: "qq.exe", app: "QQ", feature: "截图", mods: 0x0003, vk: 0x41 },
    // Snipaste
    KnownHotkeyOwner { exe: "snipaste.exe", app: "Snipaste", feature: "截图", mods: 0x0000, vk: 0x70 },
    KnownHotkeyOwner { exe: "snipaste.exe", app: "Snipaste", feature: "贴图", mods: 0x0000, vk: 0x73 },
];

/// 当前进程的 exe 名（小写）。快照失败就返回空表，只影响提示文案。
fn running_exe_names() -> Vec<String> {
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    let mut out = Vec::new();
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return out;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                let end = entry
                    .szExeFile
                    .iter()
                    .position(|c| *c == 0)
                    .unwrap_or(entry.szExeFile.len());
                out.push(String::from_utf16_lossy(&entry.szExeFile[..end]).to_ascii_lowercase());
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
    }
    out
}

/// 猜测占用者：进程在跑、且这个组合键正好是它的默认键位。
/// 返回 (软件名, 对方用这个键做什么)。Windows 不提供真实占用者，所以只作提示。
pub(crate) fn hotkey_owner_hint(mods: u32, vk: u32) -> Option<(&'static str, &'static str)> {
    let exes = running_exe_names();
    if exes.is_empty() {
        return None;
    }
    KNOWN_HOTKEY_OWNERS
        .iter()
        .find(|k| k.mods == mods && k.vk == vk && exes.iter().any(|e| e == k.exe))
        .map(|k| (k.app, k.feature))
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
    // 鼠标键没有 RegisterHotKey，交给低级鼠标钩子（见 mouse_hook_proc）
    if is_mouse_vk(vk) {
        return true;
    }
    match RegisterHotKey(None, id, mods, vk) {
        Ok(()) => true,
        Err(e) => {
            let win32 = GetLastError().0;
            let combo = format!(
                "{}+{}",
                spec.modifiers.trim().to_uppercase(),
                spec.key.trim().to_uppercase()
            );
            if win32 == 1409 || e.code().is_err() {
                let why = match hotkey_owner_hint(mods.0, vk) {
                    Some((app, feature)) => format!("很可能被 {app}占用（它默认用这个键{feature}）"),
                    None => "被另一个程序占用（Windows 不会告诉我们是谁）".to_string(),
                };
                push_issue(format!(
                    "「{name}」{combo} {why}。可开启「设置 → 快捷键 → 快捷键优先」强制生效，或换绑其它组合键。"
                ));
            } else {
                push_issue(format!(
                    "「{name}」{combo} 注册失败（系统错误 {win32}），该功能的快捷键暂时不可用。"
                ));
            }
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
        .filter(|slot| {
            // 键位空着 = 未绑定：不注册、也不报"无法识别"
            slot.enabled(cfg) && !slot.get(cfg).key.trim().is_empty()
        })
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
    Reload,
    SelectSession,
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
        HotkeySlot::Reload,
        HotkeySlot::SelectSession,
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
            HotkeySlot::Reload => "重载热键配置",
            HotkeySlot::SelectSession => "选择/控制窗口",
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
            HotkeySlot::Reload => "reload",
            HotkeySlot::SelectSession => "select_session",
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
            HotkeySlot::Reload => ("ALT+SHIFT", "C"),
            HotkeySlot::SelectSession => ("ALT", "`"),
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
            HotkeySlot::Reload => 9,
            HotkeySlot::CycleInputMode => 10,
            HotkeySlot::SelectSession => 11,
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
            HotkeySlot::Reload => cfg.reload.clone().unwrap_or_else(|| self.default_spec()),
            HotkeySlot::SelectSession => cfg.select_session.clone(),
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
            HotkeySlot::Reload => cfg.reload = Some(spec),
            HotkeySlot::SelectSession => cfg.select_session = spec,
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
            HotkeySlot::Reload => HotkeyCaptureTarget::Reload,
            HotkeySlot::SelectSession => HotkeyCaptureTarget::SelectSession,
        }
    }
}

/// 编辑器下拉可选的按键（`parse_vk` 全部支持这些名字）
/// 找出重复的绑定组合，返回 "Alt + P（切换鼠标点透、切换笔点透）" 这种说明
pub(crate) fn displays_conflict_text(cfg: &HotkeyConfig) -> String {
    let mut map: std::collections::HashMap<String, Vec<&'static str>> =
        std::collections::HashMap::new();
    for slot in HotkeySlot::ALL.iter() {
        let spec = slot.get(cfg);
        if spec.key.trim().is_empty() {
            continue;
        }
        map.entry(hotkey_display(&spec)).or_default().push(slot.label());
    }
    let mut keys: Vec<&String> = map.keys().collect();
    keys.sort();
    let mut parts: Vec<String> = Vec::new();
    for k in keys {
        if let Some(list) = map.get(k) {
            if list.len() > 1 {
                parts.push(format!("{}（{}）", k, list.join("、")));
            }
        }
    }
    parts.join("；")
}

/// 键位显示：`ALT+SHIFT` + `P` → `Alt + Shift + P`
pub(crate) fn hotkey_display(spec: &HotkeySpec) -> String {
    if spec.key.trim().is_empty() {
        return String::new();
    }
    let mut parts: Vec<String> = Vec::new();
    for m in spec.modifiers.split('+') {
        let m = m.trim();
        if m.is_empty() {
            continue;
        }
        let upper = m.to_uppercase();
        let nice = match upper.as_str() {
            "ALT" => "Alt",
            "CTRL" | "CONTROL" => "Ctrl",
            "SHIFT" => "Shift",
            "WIN" => "Win",
            other => other,
        };
        parts.push(nice.to_string());
    }
    parts.push(spec.key.trim().to_string());
    parts.join(" + ")
}

/// 战争雷霆式的绑定位：一格按钮，点一下进入录入；录入中整格变强调色。
/// 返回 (点击, 右键点击)
fn hotkey_cell(
    ui: &mut egui::Ui,
    text: &str,
    capturing: bool,
    conflict: bool,
    dim: bool,
) -> (bool, bool) {
    let font = egui::FontId::proportional(13.0);
    let pad = egui::vec2(10.0, 5.0);
    let min_w = 150.0;
    let galley = ui.painter().layout_no_wrap(
        text.to_string(),
        font.clone(),
        egui::Color32::WHITE,
    );
    let size = egui::vec2(
        (galley.size().x + pad.x * 2.0).max(min_w),
        galley.size().y + pad.y * 2.0,
    );
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    let hover = ui
        .ctx()
        .animate_bool_with_time(resp.id.with("hover"), resp.hovered(), 0.12);
    let (bg, border, fg) = if capturing {
        (ACCENT_SOFT, ACCENT, ACCENT)
    } else if conflict {
        (egui::Color32::from_rgb(0xFD, 0xEC, 0xEC), ERROR, ERROR)
    } else if dim {
        (egui::Color32::from_rgb(0xFA, 0xFA, 0xFA), BORDER, TEXT_SECONDARY)
    } else if hover > 0.01 {
        (
            lerp_color(SURFACE, HOVER_FILL, hover),
            lerp_color(BORDER, egui::Color32::from_rgb(0xC8, 0xC8, 0xC8), hover),
            TEXT_PRIMARY,
        )
    } else {
        (SURFACE, BORDER, TEXT_PRIMARY)
    };
    ui.painter()
        .rect(rect, egui::Rounding::same(6.0), bg, egui::Stroke::new(1.0, border));
    ui.painter().text(
        egui::pos2(rect.left() + pad.x, rect.center().y),
        egui::Align2::LEFT_CENTER,
        text,
        font,
        fg,
    );
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    (resp.clicked(), resp.secondary_clicked())
}

/// 统一卡片外观：圆角 10、细描边、内边距，和 DSH 面板的卡片风格保持一致。/// 统一卡片外观：圆角 10、细描边、内边距，和 DSH 面板的卡片风格保持一致。
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

/// 两个颜色按 t 插值（含 alpha），做悬停/选中的过渡
/// 系统里有没有雅黑 Bold（没有就用常规字重兜底，避免 egui 找不到字体家族）
static BOLD_FONT_OK: AtomicBool = AtomicBool::new(false);

/// 标题字体：有粗体家族就用，没有退回常规
pub(crate) fn font_title(size: f32) -> egui::FontId {
    if BOLD_FONT_OK.load(Ordering::Relaxed) {
        egui::FontId::new(size, egui::FontFamily::Name("bold".into()))
    } else {
        egui::FontId::proportional(size)
    }
}

fn lerp_color(a: egui::Color32, b: egui::Color32, t: f32) -> egui::Color32 {
    let t = t.clamp(0.0, 1.0);
    let f = |x: u8, y: u8| -> u8 { (x as f32 + (y as f32 - x as f32) * t).round() as u8 };
    egui::Color32::from_rgba_unmultiplied(
        f(a.r(), b.r()),
        f(a.g(), b.g()),
        f(a.b(), b.b()),
        f(a.a(), b.a()),
    )
}

/// 导航行（驱动 App 那种）：圆角悬停底 + 选中时左侧强调条，悬停/选中都带过渡
fn nav_row(ui: &mut egui::Ui, selected: bool, label: &str, weak: bool) -> egui::Response {
    let height = 24.0;
    let width = ui.available_width().max(40.0);
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::click());
    let hover = ui
        .ctx()
        .animate_bool_with_time(resp.id.with("hover"), resp.hovered(), 0.12);
    let sel = ui
        .ctx()
        .animate_bool_with_time(resp.id.with("sel"), selected, 0.16);
    if sel > 0.01 {
        ui.painter()
            .rect_filled(rect, egui::Rounding::same(7.0), lerp_color(HOVER_FILL, ACCENT_SOFT, sel));
    } else if hover > 0.01 {
        ui.painter().rect_filled(
            rect,
            egui::Rounding::same(7.0),
            lerp_color(egui::Color32::TRANSPARENT, HOVER_FILL, hover),
        );
    }
    if sel > 0.01 {
        let bar = egui::Rect::from_min_size(
            egui::pos2(rect.left() + 3.0, rect.top() + 6.0),
            egui::vec2(3.0, height - 12.0),
        );
        ui.painter().rect_filled(
            bar,
            egui::Rounding::same(2.0),
            egui::Color32::from_rgba_unmultiplied(ACCENT.r(), ACCENT.g(), ACCENT.b(), (255.0 * sel) as u8),
        );
    }
    let base = if weak { TEXT_SECONDARY } else { TEXT_PRIMARY };
    let color = lerp_color(base, ACCENT, sel * 0.85);
    ui.painter().text(
        egui::pos2(rect.left() + 13.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        egui::FontId::proportional(12.5),
        color,
    );
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp
}

/// 圆角胶囊（桌面筛选那种），选中 = 强调色浅底 + 强调色字
fn chip(ui: &mut egui::Ui, selected: bool, label: &str) -> egui::Response {
    let font = egui::FontId::proportional(12.0);
    let galley = ui
        .painter()
        .layout_no_wrap(label.to_string(), font.clone(), egui::Color32::WHITE);
    let pad = egui::vec2(10.0, 4.0);
    let size = egui::vec2(galley.size().x + pad.x * 2.0, galley.size().y + pad.y * 2.0);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    let hover = ui
        .ctx()
        .animate_bool_with_time(resp.id.with("hover"), resp.hovered(), 0.12);
    let sel = ui
        .ctx()
        .animate_bool_with_time(resp.id.with("sel"), selected, 0.16);
    let bg = if sel > 0.01 {
        lerp_color(HOVER_FILL, ACCENT_SOFT, sel)
    } else if hover > 0.01 {
        lerp_color(egui::Color32::TRANSPARENT, HOVER_FILL, hover)
    } else {
        egui::Color32::TRANSPARENT
    };
    if bg != egui::Color32::TRANSPARENT {
        ui.painter().rect_filled(rect, egui::Rounding::same(999.0), bg);
    }
    let fg = lerp_color(TEXT_SECONDARY, ACCENT, sel);
    ui.painter()
        .text(rect.center(), egui::Align2::CENTER_CENTER, label, font, fg);
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp
}

// ---- 面板配色（参考 QQ 音乐：白底 + 浅灰分隔 + 一个绿色主色）----
pub(crate) const ACCENT: egui::Color32 = egui::Color32::from_rgb(0x2F, 0xC2, 0x7C);
/// 强调色的淡底（导航选中、胶囊选中）
pub(crate) const ACCENT_SOFT: egui::Color32 = egui::Color32::from_rgb(0xE9, 0xF9, 0xF1);
/// 悬停底色
pub(crate) const HOVER_FILL: egui::Color32 = egui::Color32::from_rgb(0xF5, 0xF5, 0xF5);
pub(crate) const SURFACE: egui::Color32 = egui::Color32::WHITE;
pub(crate) const BG_BASE: egui::Color32 = egui::Color32::WHITE;
pub(crate) const BG_SIDEBAR: egui::Color32 = egui::Color32::from_rgb(0xF7, 0xF7, 0xF7);
pub(crate) const BORDER: egui::Color32 = egui::Color32::from_rgb(0xEE, 0xEE, 0xEE);
pub(crate) const TEXT_PRIMARY: egui::Color32 = egui::Color32::from_rgb(0x1A, 0x1A, 0x1A);
pub(crate) const TEXT_SECONDARY: egui::Color32 = egui::Color32::from_rgb(0x8A, 0x8A, 0x8A);
pub(crate) const ERROR: egui::Color32 = egui::Color32::from_rgb(0xE6, 0x4B, 0x4B);

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
    // 弹窗/菜单的柔和投影（驱动 App 那种轻微悬浮感）
    v.popup_shadow = egui::epaint::Shadow {
        offset: egui::vec2(0.0, 4.0),
        blur: 16.0,
        spread: 0.0,
        color: egui::Color32::from_black_alpha(28),
    };
    style.visuals = v;
    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    style.spacing.button_padding = egui::vec2(11.0, 6.0);
    style.spacing.window_margin = egui::Margin::same(12.0);
    style.spacing.menu_margin = egui::Margin::same(8.0);
    // 字号对齐 QQ 音乐那种排版：标题 20、正文 14、辅助 12
    style.text_styles.insert(egui::TextStyle::Heading, egui::FontId::proportional(20.0));
    style.text_styles.insert(egui::TextStyle::Body, egui::FontId::proportional(14.0));
    style.text_styles.insert(egui::TextStyle::Button, egui::FontId::proportional(14.0));
    style.text_styles.insert(egui::TextStyle::Small, egui::FontId::proportional(12.0));
    style.text_styles.insert(egui::TextStyle::Monospace, egui::FontId::monospace(11.5));
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

/// 从注册表读取全部虚拟桌面 GUID（顺序即桌面顺序），解决"没有窗口的桌面看不到"的问题。
fn virtual_desktops() -> (Vec<String>, Option<String>) {
    use windows::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ,
    };
    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }
    let mut out = Vec::new();
    let mut current: Option<String> = None;
    unsafe {
        let sub = wide("Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\VirtualDesktops");
        let mut hkey = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, windows::core::PCWSTR(sub.as_ptr()), 0, KEY_READ, &mut hkey).is_err() {
            return (out, current);
        }
        for name in ["VirtualDesktopIDs", "CurrentVirtualDesktop"] {
            let name_w = wide(name);
            let mut ty = windows::Win32::System::Registry::REG_VALUE_TYPE::default();
            let mut size: u32 = 0;
            if RegQueryValueExW(
                hkey,
                windows::core::PCWSTR(name_w.as_ptr()),
                None,
                Some(&mut ty),
                None,
                Some(&mut size),
            )
            .is_ok()
                && size >= 16
            {
                let mut buf = vec![0u8; size as usize];
                if RegQueryValueExW(
                    hkey,
                    windows::core::PCWSTR(name_w.as_ptr()),
                    None,
                    Some(&mut ty),
                    Some(buf.as_mut_ptr()),
                    Some(&mut size),
                )
                .is_ok()
                {
                    for chunk in buf.chunks_exact(16) {
                        let d1 = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                        let id = format!("{d1:08x}");
                        if name == "CurrentVirtualDesktop" {
                            current = Some(id);
                        } else {
                            out.push(id);
                        }
                    }
                }
            }
        }
        let _ = RegCloseKey(hkey);
    }
    (out, current)
}
fn window_desktop(hwnd: HWND) -> String {
    let Some(manager) = virtual_desktop_manager() else {
        return "未分组".to_string();
    };
    unsafe {
        if let Ok(guid) = manager.GetWindowDesktopId(hwnd) {
            let empty = guid.data1 == 0
                && guid.data2 == 0
                && guid.data3 == 0
                && guid.data4.iter().all(|b| *b == 0);
            if !empty {
                let id = format!("{:08x}", guid.data1);
                if let Some(idx) = virtual_desktops().0.iter().position(|x| *x == id) {
                    return format!("桌面 {}", idx + 1);
                }
                return format!("桌面 {id}");
            }
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
            // 注意：其它虚拟桌面上的窗口会被 DWM 标记为 cloaked，
            // 以前把它们过滤掉了，导致「全部」只能看到当前桌面。这里不再过滤。
            {
                let desktop = window_desktop(hwnd);
                // 判断不出桌面的窗口直接不显示（否则筛选无效）
                if desktop != "未分组" {
                    out.push(WinInfo {
                        hwnd: hwnd.0 as isize,
                        title: String::from_utf16_lossy(&buf[..len as usize]),
                        desktop,
                    });
                }
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

fn ps_selection_panel(ui: &mut egui::Ui, selected: &mut usize) {
    let target = crate::current_target();
    let all = crate::regions::snapshot_all();
    let regions = all
        .iter()
        .find(|(h, _)| *h == target)
        .map(|(_, r)| r.clone())
        .unwrap_or_default();
    ui.label(
        egui::RichText::new("选区 / 点透")
            .font(font_title(16.0))
            .color(TEXT_PRIMARY),
    );
    ui.add_space(4.0);
    if target == 0 && regions.is_empty() {
        ui.label(
            egui::RichText::new("右键窗口卡片/左侧窗口，或右键悬浮球选中窗口后，这里显示选区和蒙版图层")
                .small()
                .weak(),
        );
        return;
    }
    let hwnd = HWND(target as *mut _);
    let app = unsafe { cached_app_name(hwnd) };
    let win_title = unsafe { get_window_title(hwnd) };
    ui.label(egui::RichText::new(format!("{app}  ·  {win_title}")).size(12.0).weak());
    ui.add_space(4.0);

    let (tag, color) = crate::selection::mark_for(target).unwrap_or_else(|| {
        (if app.trim().is_empty() { "窗口".to_string() } else { app.clone() }, (47u8, 194u8, 124u8))
    });
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("标记").small().weak());
        let mut edit = tag.clone();
        if ui
            .add(egui::TextEdit::singleline(&mut edit).desired_width(110.0))
            .changed()
        {
            crate::selection::set_mark(target, edit, color);
        }
        for c in [
            (47u8, 194u8, 124u8),
            (70u8, 130u8, 220u8),
            (240u8, 150u8, 60u8),
            (180u8, 80u8, 200u8),
        ] {
            let btn = egui::Button::new(
                egui::RichText::new("●").color(egui::Color32::from_rgb(c.0, c.1, c.2)),
            )
            .frame(false);
            if ui.add(btn).clicked() {
                crate::selection::set_mark(target, tag.clone(), c);
            }
        }
    });
    ui.separator();
    ui.label(egui::RichText::new("选区图层").small().strong());
    if regions.is_empty() {
        ui.label(egui::RichText::new("还没有点透选区").small().weak());
    } else {
        if *selected >= regions.len() {
            *selected = regions.len() - 1;
        }
        for (i, r) in regions.iter().enumerate() {
            let is_sel = i == *selected;
            let bg = if is_sel {
                egui::Color32::from_rgb(0xE8, 0xF7, 0xEE)
            } else {
                egui::Color32::TRANSPARENT
            };
            let f = egui::Frame::none()
                .fill(bg)
                .rounding(6.0)
                .inner_margin(egui::Margin::symmetric(4.0, 3.0));
            f.show(ui, |ui| {
                ui.horizontal(|ui| {
                    let mut en = r.enabled;
                    if ui
                        .checkbox(&mut en, "")
                        .on_hover_text("眼睛：启用/停用这个圈")
                        .changed()
                    {
                        crate::regions::update_region(target, i, None, None, Some(en));
                    }
                    let name = format!("圈 {}  {}", i + 1, if r.invert { "反向" } else { "洞" });
                    if ui.selectable_label(is_sel, egui::RichText::new(name).size(12.0)).clicked() {
                        *selected = i;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("×").clicked() {
                            crate::regions::remove_region(target, i);
                        }
                        ui.label(egui::RichText::new(format!("{}%", r.opacity)).small().weak());
                    });
                });
            });
        }
    }
    ui.separator();
    if let Some(r) = regions.get(*selected) {
        ui.label(egui::RichText::new("属性").small().strong());
        let mut inv = r.invert;
        if ui.checkbox(&mut inv, "反向（圈内保留、圈外穿透）").changed() {
            crate::regions::update_region(target, *selected, Some(inv), None, None);
        }
        let mut op = r.opacity as i32;
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("透明度").small().weak());
            if ui
                .add(egui::Slider::new(&mut op, 0..=100).suffix("%"))
                .changed()
            {
                crate::regions::update_region(target, *selected, None, Some(op as u8), None);
            }
        });
    }
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        if ui.button("编辑选区…").clicked() {
            crate::regions::open_editor_at(target, Some(*selected));
        }
        if ui.button("新建").clicked() {
            crate::regions::open_editor_at(target, None);
        }
    });
    ui.horizontal(|ui| {
        if ui.button("清空选区").clicked() {
            crate::regions::clear(target);
        }
        if ui.button("刷新透明洞").clicked() {
            crate::regions::refresh_visual(target);
        }
    });
    let mut on = crate::selection::enabled();
    if ui.checkbox(&mut on, "显示选中框").changed() {
        crate::selection::set_enabled(on);
    }
    ui.separator();
    if let Some(st) = GLOBAL_REGISTRY.get(&target) {
        let pct = (st.current_alpha as f32 / 255.0 * 100.0).round();
        ui.label(
            egui::RichText::new(format!(
                "窗口：透明度 {}% · 置顶 {} · 鼠标点透 {} · 笔点透 {}",
                pct,
                if st.user_pref_topmost { "开" } else { "关" },
                if st.mouse_passthrough { "开" } else { "关" },
                if st.pen_passthrough { "开" } else { "关" }
            ))
            .small()
            .weak(),
        );
    } else {
        ui.label(egui::RichText::new("窗口未管理（右键卡片可控制）").small().weak());
    }
}

fn selection_controls_ui(ui: &mut egui::Ui, hwnd: isize) {
    let hwnd_h = HWND(hwnd as *mut _);
    let app = unsafe { cached_app_name(hwnd_h) };
    let (tag, color) = crate::selection::mark_for(hwnd).unwrap_or_else(|| {
        (
            if app.trim().is_empty() {
                "窗口".to_string()
            } else {
                app.clone()
            },
            (47u8, 194u8, 124u8),
        )
    });
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("标记").small().weak());
        let mut edit = tag.clone();
        if ui
            .add(egui::TextEdit::singleline(&mut edit).desired_width(120.0))
            .changed()
        {
            crate::selection::set_mark(hwnd, edit, color);
        }
        for c in [
            (47u8, 194u8, 124u8),
            (70u8, 130u8, 220u8),
            (240u8, 150u8, 60u8),
            (180u8, 80u8, 200u8),
        ] {
            let btn = egui::Button::new(
                egui::RichText::new("●").color(egui::Color32::from_rgb(c.0, c.1, c.2)),
            )
            .frame(false);
            if ui.add(btn).clicked() {
                crate::selection::set_mark(hwnd, tag.clone(), c);
            }
        }
    });
    let mut on = crate::selection::enabled();
    if ui.checkbox(&mut on, "显示选中框").changed() {
        crate::selection::set_enabled(on);
    }
    if ui.button("选中并显示").clicked() {
        crate::select_window_for_control(hwnd);
        crate::selection::set_enabled(true);
        ui.close_menu();
    }
    let all = crate::regions::snapshot_all();
    let regions = all
        .iter()
        .find(|(h, _)| *h == hwnd)
        .map(|(_, r)| r.clone())
        .unwrap_or_default();
    ui.separator();
    if regions.is_empty() {
        ui.label(egui::RichText::new("还没有点透选区").small().weak());
    } else {
        for (i, r) in regions.iter().enumerate() {
            ui.horizontal(|ui| {
                let mut en = r.enabled;
                if ui
                    .checkbox(&mut en, "")
                    .on_hover_text("眼睛：启用/停用这个圈")
                    .changed()
                {
                    crate::regions::update_region(hwnd, i, None, None, Some(en));
                }
                ui.label(format!("圈 {}", i + 1));
                let mut inv = r.invert;
                if ui.checkbox(&mut inv, "反向").changed() {
                    crate::regions::update_region(hwnd, i, Some(inv), None, None);
                }
                let mut op = r.opacity as i32;
                if ui
                    .add(egui::Slider::new(&mut op, 0..=100).suffix("%"))
                    .changed()
                {
                    crate::regions::update_region(hwnd, i, None, Some(op as u8), None);
                }
                if ui.button("删").clicked() {
                    crate::regions::remove_region(hwnd, i);
                }
            });
        }
    }
    ui.horizontal(|ui| {
        if ui.button("编辑选区…").clicked() {
            crate::regions::open_editor(hwnd);
            ui.close_menu();
        }
        if ui.button("清空选区").clicked() {
            crate::regions::clear(hwnd);
            ui.close_menu();
        }
        if ui.button("刷新透明洞").clicked() {
            crate::regions::refresh_visual(hwnd);
            ui.close_menu();
        }
    });
}

fn window_menu(ui: &mut egui::Ui, hwnd: isize, title: &str) {
    ui.spacing_mut().item_spacing = egui::vec2(10.0, 8.0);
    ui.spacing_mut().button_padding = egui::vec2(12.0, 6.0);
    ui.style_mut()
        .text_styles
        .insert(egui::TextStyle::Button, egui::FontId::proportional(14.0));
    ui.label(egui::RichText::new(title).strong().size(14.0));
    if ui.button("选中并显示控制").clicked() {
        crate::select_window_for_control(hwnd);
        ui.close_menu();
    }
    ui.separator();
    let st = GLOBAL_REGISTRY
        .get(&hwnd)
        .map(|s| (s.current_alpha, s.user_pref_topmost, s.mouse_passthrough, s.pen_passthrough));
    let (alpha, top, mouse, pen) = st.unwrap_or((255, false, false, false));
    // 全部功能并列：置顶 / 鼠标点透 / 笔点透 / 透明度 / 控制或复原
    let mut top_flag = top;
    if ui.checkbox(&mut top_flag, "置顶").changed() {
        unsafe { toggle_topmost(HWND(hwnd as *mut _)) };
    }
    let mut mouse_flag = mouse;
    if ui.checkbox(&mut mouse_flag, "鼠标点透").changed() {
        unsafe { toggle_mouse_passthrough(HWND(hwnd as *mut _)) };
    }
    let mut pen_flag = pen;
    if ui.checkbox(&mut pen_flag, "笔点透").changed() {
        unsafe { toggle_pen_passthrough(HWND(hwnd as *mut _)) };
    }
    let mut pct = (alpha as f32 / 255.0 * 100.0).round();
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new("透明度").small().weak());
        ui.spacing_mut().slider_width = 128.0;
        ui.visuals_mut().widgets.inactive.bg_fill = egui::Color32::from_rgb(0xC3, 0xC9, 0xD3);
        ui.visuals_mut().widgets.hovered.bg_fill = egui::Color32::from_rgb(0xA8, 0xB0, 0xBC);
        let resp = ui.add(
            egui::Slider::new(&mut pct, 30.0..=100.0)
                .suffix("%")
                .show_value(true),
        );
        if resp.changed() {
            unsafe {
                ensure_managed(HWND(hwnd as *mut _));
                set_window_alpha(HWND(hwnd as *mut _), (pct * 2.55).round() as u8);
            }
        }
    });
    if st.is_some() {
        if ui.button("复原这个窗口").clicked() {
            unsafe { restore_window(HWND(hwnd as *mut _)) };
        }
    } else if ui.button("控制（90% 透明）").clicked() {
        unsafe { control_window(HWND(hwnd as *mut _)) };
    }
    ui.separator();
    ui.menu_button("选中控制", |ui| selection_controls_ui(ui, hwnd));
}

/// 用 PrintWindow 抓一帧窗口缩略图（失败返回 None，卡片会显示"无预览"）
pub(crate) unsafe fn capture_window(hwnd: HWND) -> Option<egui::ColorImage> {
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits,
        ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HGDIOBJ,
    };
    let mut rect = RECT::default();
    if GetWindowRect(hwnd, &mut rect).is_err() {
        return None;
    }
    let w = rect.right - rect.left;
    let h = rect.bottom - rect.top;
    if w <= 0 || h <= 0 || w > 4096 || h > 4096 {
        return None;
    }
    let screen = GetDC(None);
    let mem = CreateCompatibleDC(screen);
    let bmp = CreateCompatibleBitmap(screen, w, h);
    let old = SelectObject(mem, HGDIOBJ(bmp.0));
    let mut img = None;
    if windows::Win32::Storage::Xps::PrintWindow(hwnd, mem, windows::Win32::Storage::Xps::PRINT_WINDOW_FLAGS(2)).as_bool() {
        let mut bi: BITMAPINFO = std::mem::zeroed();
        bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bi.bmiHeader.biWidth = w;
        bi.bmiHeader.biHeight = -h;
        bi.bmiHeader.biPlanes = 1;
        bi.bmiHeader.biBitCount = 32;
        bi.bmiHeader.biCompression = BI_RGB.0;
        let mut buf = vec![0u8; (w * h * 4) as usize];
        let lines = GetDIBits(
            mem,
            bmp,
            0,
            h as u32,
            Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
            &mut bi,
            DIB_RGB_COLORS,
        );
        if lines > 0 {
            for px in buf.chunks_exact_mut(4) {
                px.swap(0, 2);
                px[3] = 255;
            }
            // 花屏检测：GPU/DirectComposition 窗口常抓到扫描线噪声，
            // 用相邻像素亮度差均值判断，超过阈值就当作抓取失败（显示"无预览"而不是花屏）
            let stride = (w * 4) as usize;
            let mut diff_sum = 0u64;
            let mut diff_n = 0u64;
            let mut points = 0i32;
            let mut y = 0i32;
            while y < h {
                let mut x = 0i32;
                while x + 1 < w {
                    let i = (y as usize) * stride + (x as usize) * 4;
                    let a = (buf[i] as i32 + buf[i + 1] as i32 + buf[i + 2] as i32) / 3;
                    let b = (buf[i + 4] as i32 + buf[i + 5] as i32 + buf[i + 6] as i32) / 3;
                    diff_sum += (a - b).unsigned_abs() as u64;
                    diff_n += 1;
                    x += 7;
                }
                y += 5;
                points += 1;
            }
            // 纯黑帧（GPU 窗口抓不到内容）也算失败
            let mut luma = 0u64;
            let mut luma_n = 0u64;
            for px in buf.chunks_exact(4) {
                luma += (px[0] as u64 + px[1] as u64 + px[2] as u64) / 3;
                luma_n += 1;
            }
            let black = luma_n > 0 && (luma / luma_n) < 10;
            let noisy = diff_n > 0 && (diff_sum / diff_n) > 42;
            if !noisy && !black {
                img = Some(egui::ColorImage::from_rgba_unmultiplied(
                    [w as usize, h as usize],
                    &buf,
                ));
            }
        }
    }
    let _ = SelectObject(mem, old);
    let _ = DeleteObject(HGDIOBJ(bmp.0));
    let _ = DeleteDC(mem);
    ReleaseDC(None, screen);
    img
}

// ---------------------------------------------------------------------------
// 桌面高亮预览：悬停 1.5 秒后在目标窗口四周画一圈强调色边框
// ---------------------------------------------------------------------------

/// 当前被高亮的窗口（0 = 无）
static HILITE_TARGET: AtomicIsize = AtomicIsize::new(0);

fn log_hilite(msg: &str) {
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(get_config_path().with_file_name("hilite.log"))
        .and_then(|mut f| {
            use std::io::Write;
            writeln!(f, "{msg}")
        });
}

/// 用 DWM 给目标窗口换边框颜色（Win11 生效；Win10 会静默失败）
unsafe fn dwm_border_color(hwnd: HWND, color: u32) {
    use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWINDOWATTRIBUTE};
    // DWMWA_BORDER_COLOR = 34；0xFFFFFFFF 表示恢复默认
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWINDOWATTRIBUTE(34),
        &color as *const u32 as *const core::ffi::c_void,
        std::mem::size_of::<u32>() as u32,
    );
}

/// 高亮目标窗口：DWM 边框 + 提到最前（不抢焦点）
unsafe fn show_hilite(target: HWND) {
    let prev = HILITE_TARGET.load(Ordering::Relaxed);
    if prev == target.0 as isize {
        return;
    }
    if prev != 0 {
        dwm_border_color(HWND(prev as *mut _), 0xFFFF_FFFF);
    }
    HILITE_TARGET.store(target.0 as isize, Ordering::Relaxed);
    dwm_border_color(target, 0x00E6_7641);
    let _ = SetWindowPos(
        target,
        HWND_TOP,
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
    );
    log_hilite(&format!("hilite {:?}", target.0));
}

unsafe fn hide_hilite() {
    let prev = HILITE_TARGET.load(Ordering::Relaxed);
    if prev != 0 {
        dwm_border_color(HWND(prev as *mut _), 0xFFFF_FFFF);
        HILITE_TARGET.store(0, Ordering::Relaxed);
    }
}


/// 悬浮球点击：显示/隐藏主窗口
unsafe fn toggle_main_window() {
    let h = ROOT_HWND.load(Ordering::Relaxed);
    if h == 0 {
        return;
    }
    let hwnd = HWND(h as *mut _);
    if WINDOW_VISIBLE.load(Ordering::Relaxed) {
        hide_root_window();
        WINDOW_VISIBLE.store(false, Ordering::Relaxed);
    } else {
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        WINDOW_VISIBLE.store(true, Ordering::Relaxed);
    }
}

// ---- 悬浮球试验窗口：Win32 分层窗口 + UpdateLayeredWindow（逐像素透明） ----
// ---- 悬浮球：Win32 分层窗口（ARGB 逐像素透明）+ 自带点击处理 ----
static BALL_HWND: AtomicIsize = AtomicIsize::new(0);
/// 最后一次点击命中的（非自身的）窗口，悬浮球遥控的目标
static TARGET_HWND: AtomicIsize = AtomicIsize::new(0);
static BALL_EXPANDED: AtomicBool = AtomicBool::new(false);
static BALL_ANIM: AtomicU32 = AtomicU32::new(0);
static BALL_CURSOR: AtomicU64 = AtomicU64::new(0);
static BALL_SCALE: AtomicU32 = AtomicU32::new(0); // f32 bits，0..1 弹出动画
/// 待机动画开关：关掉后静置完全不重绘（省 CPU）
static BALL_FLOAT: AtomicBool = AtomicBool::new(true);
static BALL_SWAY: AtomicBool = AtomicBool::new(true);
/// 纸娃娃：平滑后的身体朝向 / 头部额外偏转 / 头部俯仰（f32 bits）
static BALL_YAW: AtomicU32 = AtomicU32::new(0);
static BALL_HEAD_YAW: AtomicU32 = AtomicU32::new(0);
static BALL_HEAD_PITCH: AtomicU32 = AtomicU32::new(0);
/// 上一次"光标在动"的时间（anim_secs 的 f32 bits）
static BALL_LAST_ACTIVE: AtomicU32 = AtomicU32::new(0);
/// 纸娃娃当前占用的窗口内矩形（用于点击命中，0 = 无）
static BALL_DOLL_RECT: AtomicU64 = AtomicU64::new(0);
static BALL_REQ_PANEL: AtomicBool = AtomicBool::new(false);
const WM_BALL_POPUP: u32 = WM_APP + 0x5A;
static BALL_REQ_MINI: AtomicBool = AtomicBool::new(false);
static BALL_REQ_RESTORE: AtomicBool = AtomicBool::new(false);
/// 面板上的状态变了（输入模式 / Alt 会话状态 / 目标窗口）时，叫 ticker 重画一次，
/// 即使待机动画全关也不会一直显示旧状态。
static BALL_DIRTY: AtomicBool = AtomicBool::new(false);

pub(crate) fn ball_mark_dirty() {
    BALL_DIRTY.store(true, Ordering::Relaxed);
}

/// 点透会话选中窗口后，把遥控目标也切过去，面板才能显示「操作中」。
pub(crate) fn set_target_window(hwnd: isize) {
    if hwnd != 0 {
        TARGET_HWND.store(hwnd, Ordering::Relaxed);
        ball_mark_dirty();
    }
}

pub(crate) fn main_window_handle() -> isize {
    ROOT_HWND.load(Ordering::Relaxed)
}

pub(crate) fn current_target() -> isize {
    TARGET_HWND.load(Ordering::Relaxed)
}

/// 目标窗口关掉后清掉目标/选区/标记，别让选中界面一直挂着。
pub(crate) fn selection_cleanup() {
    let t = TARGET_HWND.load(Ordering::Relaxed);
    if t != 0 && !unsafe { IsWindow(HWND(t as *mut _)).as_bool() } {
        TARGET_HWND.store(0, Ordering::Relaxed);
        ball_mark_dirty();
    }
    crate::regions::prune();
    crate::selection::prune();
}

/// 右键卡片/Alt 会话选中后：把遥感目标切过去并主动弹出悬浮球控制面板。
pub(crate) fn select_window_for_control(hwnd: isize) {
    if hwnd == 0 {
        return;
    }
    set_target_window(hwnd);
    ball_popup_panel();
}

/// 主动弹出悬浮球面板（必要时先显示悬浮球）。
pub(crate) fn ball_popup_panel() {
    let mut h = BALL_HWND.load(Ordering::Relaxed);
    if h == 0 || !unsafe { IsWindowVisible(HWND(h as *mut _)).as_bool() } {
        unsafe { set_ball_visible(true) };
        h = BALL_HWND.load(Ordering::Relaxed);
    }
    if h != 0 {
        unsafe {
            let _ = PostMessageW(HWND(h as *mut _), WM_BALL_POPUP, WPARAM(0), LPARAM(0));
        }
    }
}

const BALL_W: i32 = 250;
const BALL_H: i32 = 250;
static BALL_POS_X: AtomicI32 = AtomicI32::new(1500);
static BALL_POS_Y: AtomicI32 = AtomicI32::new(380);
static BALL_DRAGGING: AtomicBool = AtomicBool::new(false);
static BALL_DOWN_POS: AtomicU64 = AtomicU64::new(0);
static BALL_MOVED: AtomicBool = AtomicBool::new(false);
/// 悬停的面板元素：0 无，1..4 按钮，5 透明度滑条
static BALL_HOVER: AtomicU8 = AtomicU8::new(0);
/// 正在拖透明度滑条
static BALL_SLIDER_DRAG: AtomicBool = AtomicBool::new(false);
/// 悬浮球当前是否显示（设置里可开关）
static BALL_SHOWN: AtomicBool = AtomicBool::new(true);
/// 双击纸娃娃打开主界面（设置里可关）
static DOLL_DBLCLICK: AtomicBool = AtomicBool::new(true);
/// 上次点纸娃娃的时间/位置（判断双击）
static DOLL_LAST_CLICK: AtomicU32 = AtomicU32::new(0);
static DOLL_LAST_POS: AtomicU32 = AtomicU32::new(0);

fn ball_expand_target() -> bool {
    BALL_EXPANDED.load(Ordering::Relaxed)
}

unsafe fn ball_center() -> (i32, i32) {
    (BALL_W - 42, BALL_H / 2)
}

// ---- 展开面板：MC 背包 / 设置界面风格（GUI scale ≈2） ----
// 取色参考 MC Java 的 GUI 贴图：背景 #C6C6C6、黑框 + 上左白下右深灰的斜面、
// 按钮 #8B8B8B，滑条用手动把里的进度条样式（凹槽 + 按钮式滑块）。
const MC_BG: (u8, u8, u8) = (198, 198, 198);
const MC_BLACK: (u8, u8, u8) = (0, 0, 0);
const MC_WHITE: (u8, u8, u8) = (255, 255, 255);
const MC_SHADOW: (u8, u8, u8) = (85, 85, 85);
const MC_BTN: (u8, u8, u8) = (139, 139, 139);
const MC_BTN_HL: (u8, u8, u8) = (200, 200, 200);
const MC_BTN_SH: (u8, u8, u8) = (65, 65, 65);
const MC_GROOVE: (u8, u8, u8) = (55, 55, 55);

struct BallPanel {
    panel: (i32, i32, i32, i32),
    /// 标题栏（MC 设置界面顶部那种标题）
    title: (i32, i32, i32, i32),
    /// 标题栏下面的分隔线：(x0, x1, y)
    divider: (i32, i32, i32),
    /// 纸娃娃站立的深色底（MC 背包里放角色/物品的那块凹槽）
    stage: (i32, i32, i32, i32),
    buttons: [(i32, i32, i32, i32); 4],
    slider: (i32, i32, i32, i32),
    /// 标题栏右上角的 MC 风格关闭键
    close_btn: (i32, i32, i32, i32),
    /// 左下方显示「当前目标窗口」的一行
    target_row: (i32, i32, i32, i32),
    /// 面板最底下显示「最近输入」的一行
    input_row: (i32, i32, i32, i32),
}

/// 展开布局：MC 背包/设置界面风格——标题栏 + 左侧 4 个按钮 + 右侧深色槽底站纸娃娃 + 底部透明度条。
/// t = 0..1 弹出动画，面板从右侧滑进来并淡入。
fn ball_panel(t: f32) -> BallPanel {
    let off = ((1.0 - t.clamp(0.0, 1.0)) * 22.0).round() as i32;
    let px0 = 24 + off;
    let panel = (px0, 22, px0 + 218, 246);
    let title = (px0 + 8, 22, px0 + 210, 46);
    let divider = (px0 + 8, px0 + 210, 46);
    let bx = px0 + 10;
    let bw = 120;
    let mut buttons = [(0, 0, 0, 0); 4];
    for (i, y) in [54i32, 84, 114, 144].iter().enumerate() {
        buttons[i] = (bx, *y, bx + bw, *y + 26);
    }
    let stage = (px0 + 136, 52, px0 + 212, 192);
    let slider = (px0 + 10, 198, px0 + 208, 224);
    let close_btn = (px0 + 188, 26, px0 + 210, 42);
    let target_row = (px0 + 10, 176, px0 + 130, 194);
    let input_row = (px0 + 10, 226, px0 + 130, 242);
    BallPanel {
        panel,
        title,
        divider,
        stage,
        buttons,
        slider,
        close_btn,
        target_row,
        input_row,
    }
}

/// 直通 alpha 合成一个像素（BGRA 目标），用于面板淡入
#[inline]
fn mc_blend_px(buf: &mut [u8], o: usize, c: (u8, u8, u8), a: f32) {
    if a <= 0.004 {
        return;
    }
    let da = buf[o + 3] as f32 / 255.0;
    let oa = a + da * (1.0 - a);
    if oa <= 0.004 {
        return;
    }
    let k = 1.0 / oa;
    let mix = |s: u8, d: u8| -> u8 {
        (((s as f32 * a + d as f32 * da * (1.0 - a)) * k).clamp(0.0, 255.0)) as u8
    };
    buf[o] = mix(c.2, buf[o]);
    buf[o + 1] = mix(c.1, buf[o + 1]);
    buf[o + 2] = mix(c.0, buf[o + 2]);
    buf[o + 3] = (oa * 255.0) as u8;
}

fn mc_fill(
    buf: &mut [u8],
    w: i32,
    h: i32,
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    c: (u8, u8, u8),
    a: f32,
) {
    if a <= 0.004 {
        return;
    }
    for y in y0.max(0)..y1.min(h) {
        for x in x0.max(0)..x1.min(w) {
            let o = ((y * w + x) * 4) as usize;
            mc_blend_px(buf, o, c, a);
        }
    }
}

fn mc_frame(
    buf: &mut [u8],
    w: i32,
    h: i32,
    r: (i32, i32, i32, i32),
    c: (u8, u8, u8),
    t: i32,
    a: f32,
) {
    let (x0, y0, x1, y1) = r;
    mc_fill(buf, w, h, x0, y0, x1, y0 + t, c, a);
    mc_fill(buf, w, h, x0, y1 - t, x1, y1, c, a);
    mc_fill(buf, w, h, x0, y0, x0 + t, y1, c, a);
    mc_fill(buf, w, h, x1 - t, y0, x1, y1, c, a);
}

fn draw_mc_panel(buf: &mut [u8], w: i32, h: i32, r: (i32, i32, i32, i32), a: f32) {
    let (x0, y0, x1, y1) = r;
    mc_fill(buf, w, h, x0, y0, x1, y1, MC_BG, a);
    mc_frame(buf, w, h, r, MC_BLACK, 2, a);
    // 内侧斜面：上/左白，下/右深灰
    mc_fill(buf, w, h, x0 + 2, y0 + 2, x1 - 2, y0 + 3, MC_WHITE, a);
    mc_fill(buf, w, h, x0 + 2, y0 + 2, x0 + 3, y1 - 2, MC_WHITE, a);
    mc_fill(buf, w, h, x0 + 2, y1 - 3, x1 - 2, y1 - 2, MC_SHADOW, a);
    mc_fill(buf, w, h, x1 - 3, y0 + 2, x1 - 2, y1 - 2, MC_SHADOW, a);
}

/// 标题栏和内容之间那道分隔线（MC 设置界面里标题下面那一横）
fn draw_mc_divider(buf: &mut [u8], w: i32, h: i32, d: (i32, i32, i32), a: f32) {
    let (x0, x1, y) = d;
    mc_fill(buf, w, h, x0, y, x1, y + 1, MC_SHADOW, a);
    mc_fill(buf, w, h, x0, y + 1, x1, y + 2, MC_WHITE, a);
}

/// 纸娃娃的深色底：MC 背包里那种凹进去的槽底，外面套一圈黑框
fn draw_mc_stage(buf: &mut [u8], w: i32, h: i32, r: (i32, i32, i32, i32), a: f32) {
    let (x0, y0, x1, y1) = r;
    mc_fill(buf, w, h, x0, y0, x1, y1, (58, 58, 58), a);
    mc_frame(buf, w, h, r, MC_BLACK, 2, a);
    mc_fill(buf, w, h, x0 + 2, y0 + 2, x1 - 2, y0 + 4, (34, 34, 34), a);
    mc_fill(buf, w, h, x0 + 2, y0 + 2, x0 + 4, y1 - 2, (34, 34, 34), a);
    mc_fill(buf, w, h, x0 + 2, y1 - 4, x1 - 2, y1 - 2, (96, 96, 96), a);
    mc_fill(buf, w, h, x1 - 4, y0 + 2, x1 - 2, y1 - 2, (96, 96, 96), a);
}

/// MC 风格的 ×：两根 2px 斜线，带深色投影
fn draw_mc_cross(buf: &mut [u8], w: i32, h: i32, r: (i32, i32, i32, i32), c: (u8, u8, u8), a: f32) {
    let (x0, y0, x1, y1) = r;
    let n = ((x1 - x0).min(y1 - y0) - 8).max(3);
    let cx = (x0 + x1) / 2;
    let cy = (y0 + y1) / 2;
    let half = n / 2;
    for i in -half..=half {
        // 投影（右下偏移 1px）
        for (ax, ay) in [(1, 1), (-1, 1)] {
            let bx0 = if ax > 0 { cx + i } else { cx - i - 2 };
            let bx1 = if ax > 0 { cx + i + 2 } else { cx - i };
            mc_fill(buf, w, h, bx0, cy + ay + i.min(0), bx1, cy + ay + i + 2, (60, 60, 60), a);
        }
        // 主笔画：一条 \ 一条 /
        mc_fill(buf, w, h, cx + i, cy + i, cx + i + 2, cy + i + 2, c, a);
        mc_fill(buf, w, h, cx - i - 2, cy + i, cx - i, cy + i + 2, c, a);
    }
}

fn draw_mc_button(
    buf: &mut [u8],
    w: i32,
    h: i32,
    r: (i32, i32, i32, i32),
    hover: bool,
    pressed: bool,
    a: f32,
) {
    let (x0, y0, x1, y1) = r;
    let body = if pressed {
        (110, 110, 110)
    } else if hover {
        (160, 160, 160)
    } else {
        MC_BTN
    };
    mc_fill(buf, w, h, x0, y0, x1, y1, body, a);
    mc_frame(buf, w, h, r, MC_BLACK, 2, a);
    let hl = if pressed { (128, 128, 128) } else { MC_BTN_HL };
    let sh = if pressed { (150, 150, 150) } else { MC_BTN_SH };
    mc_fill(buf, w, h, x0 + 2, y0 + 2, x1 - 2, y0 + 4, hl, a);
    mc_fill(buf, w, h, x0 + 2, y0 + 2, x0 + 4, y1 - 2, hl, a);
    mc_fill(buf, w, h, x0 + 2, y1 - 4, x1 - 2, y1 - 2, sh, a);
    mc_fill(buf, w, h, x1 - 4, y0 + 2, x1 - 2, y1 - 2, sh, a);
}

/// 滑条可走行程（滑块宽 20，两端各留 6）
fn slider_travel(r: (i32, i32, i32, i32)) -> f32 {
    ((r.2 - r.0) - 32).max(1) as f32
}

/// t=0 → 最不透明（滑块在最左），t=1 → 最透明（滑块在最右）
/// 「当前目标窗口」那一行的底：MC 那种凹进去的小槽
fn draw_mc_target_row(buf: &mut [u8], w: i32, h: i32, r: (i32, i32, i32, i32), a: f32) {
    let (x0, y0, x1, y1) = r;
    mc_fill(buf, w, h, x0, y0, x1, y1, (58, 58, 58), a);
    mc_frame(buf, w, h, r, MC_BLACK, 2, a);
    mc_fill(buf, w, h, x0 + 2, y0 + 2, x1 - 2, y0 + 3, (34, 34, 34), a);
    mc_fill(buf, w, h, x0 + 2, y1 - 3, x1 - 2, y1 - 2, (96, 96, 96), a);
}

fn draw_mc_slider(buf: &mut [u8], w: i32, h: i32, r: (i32, i32, i32, i32), t: f32, a: f32) {
    let (x0, y0, x1, y1) = r;
    mc_fill(buf, w, h, x0, y0, x1, y1, MC_GROOVE, a);
    mc_frame(buf, w, h, r, MC_BLACK, 2, a);
    // 凹槽的内斜面：上/左更暗，下/右稍亮
    mc_fill(buf, w, h, x0 + 2, y0 + 2, x1 - 2, y0 + 4, (28, 28, 28), a);
    mc_fill(buf, w, h, x0 + 2, y0 + 2, x0 + 4, y1 - 2, (28, 28, 28), a);
    mc_fill(buf, w, h, x0 + 2, y1 - 4, x1 - 2, y1 - 2, (110, 110, 110), a);
    mc_fill(buf, w, h, x1 - 4, y0 + 2, x1 - 2, y1 - 2, (110, 110, 110), a);
    let hx = x0 + 6 + (slider_travel(r) * t.clamp(0.0, 1.0)) as i32;
    draw_mc_button(buf, w, h, (hx, y0 - 2, hx + 20, y1 + 2), false, false, a);
}

unsafe fn ball_target() -> HWND {
    let stored = TARGET_HWND.load(Ordering::Relaxed);
    if stored != 0 {
        let h = HWND(stored as *mut _);
        if !is_own_hwnd(h) && !is_shell_hwnd(h) {
            return h;
        }
    }
    let fg = GetForegroundWindow();
    if !fg.0.is_null() && !is_own_hwnd(fg) && !is_shell_hwnd(fg) {
        return fg;
    }
    HWND::default()
}

/// 当前遥控目标的透明度（没有可用目标就是 255 = 不透明）
fn target_alpha_now() -> u8 {
    unsafe {
        let h = ball_target();
        if !h.0.is_null() {
            if let Some(st) = GLOBAL_REGISTRY.get(&(h.0 as isize)) {
                return st.current_alpha;
            }
        }
    }
    255
}

/// MC 风文字：1px 深灰阴影 + 正文，居中画在矩形里。
/// 阴影用 #3F3F3F 而不是纯黑：后面"补 alpha"那一步只认 rgb 非零的像素。
unsafe fn draw_mc_label(
    mem: windows::Win32::Graphics::Gdi::HDC,
    text: &str,
    r: (i32, i32, i32, i32),
    color: (u8, u8, u8),
) {
    use windows::Win32::Graphics::Gdi::{
        DrawTextW, SetTextColor, DT_CENTER, DT_SINGLELINE, DT_VCENTER,
    };
    let flags = DT_CENTER | DT_VCENTER | DT_SINGLELINE;
    let mut v: Vec<u16> = text.encode_utf16().collect();
    let _ = SetTextColor(mem, COLORREF(0x003F_3F3F));
    let mut sr = RECT {
        left: r.0 + 1,
        top: r.1 + 1,
        right: r.2 + 1,
        bottom: r.3 + 1,
    };
    let _ = DrawTextW(mem, &mut v, &mut sr, flags);
    let c = (color.0 as u32) | ((color.1 as u32) << 8) | ((color.2 as u32) << 16);
    let _ = SetTextColor(mem, COLORREF(c));
    let mut tr = RECT {
        left: r.0,
        top: r.1,
        right: r.2,
        bottom: r.3,
    };
    let _ = DrawTextW(mem, &mut v, &mut tr, flags);
}

/// 单调动画时钟（秒）。不能用 SystemTime 毫秒直接转 f32：数值太大精度只剩分钟级。
fn anim_secs() -> f32 {
    static T0: OnceLock<std::time::Instant> = OnceLock::new();
    T0.get_or_init(std::time::Instant::now).elapsed().as_secs_f32()
}

#[inline]
fn pack_rect(x0: i32, y0: i32, x1: i32, y1: i32) -> u64 {
    let c = |v: i32| (v.clamp(0, 4095) as u64) & 0xFFF;
    c(x0) | (c(y0) << 12) | (c(x1) << 24) | (c(y1) << 36) | (1u64 << 63)
}

fn unpack_rect(v: u64) -> Option<(i32, i32, i32, i32)> {
    if v & (1u64 << 63) == 0 {
        return None;
    }
    Some((
        (v & 0xFFF) as i32,
        ((v >> 12) & 0xFFF) as i32,
        ((v >> 24) & 0xFFF) as i32,
        ((v >> 36) & 0xFFF) as i32,
    ))
}

/// 按光标方向推进纸娃娃朝向（指数平滑）。返回 (身体剩余差, 头剩余差)，
/// 都接近 0 说明姿态已经收敛，可以降低重绘帧率。
fn update_doll_pose() -> (f32, f32) {
    const DT: f32 = 0.033;
    let (bx, by) = unsafe { ball_center() };
    let cx = BALL_POS_X.load(Ordering::Relaxed) as f32 + bx as f32;
    let cy = BALL_POS_Y.load(Ordering::Relaxed) as f32 + by as f32;
    let mut cp = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut cp);
    }
    let dx = cp.x as f32 - cx;
    let dy = cp.y as f32 - cy;

    let t = anim_secs();
    // 基岩版纸娃娃基本看不到后脑勺：身体朝向按"光标方向的水平分量"给，
    // 正上方 / 正下方都是正面（0），左右最多偏 MAX_YAW，永远不会转到背面。
    // 用 dx / r 而不是 atan2，是因为它在中垂线上连续，不会左右来回弹。
    const MAX_YAW: f32 = 62.0f32.to_radians();
    let r = (dx * dx + dy * dy).sqrt().max(1.0);
    let sway = (t * 0.63).sin() * 0.035;
    let look = MAX_YAW * (dx / r) + sway;
    // 身体只吃 78%，剩下的给头去"追"，看起来是头先看过去、身子跟上
    let target = look * 0.78;

    let body = f32::from_bits(BALL_YAW.load(Ordering::Relaxed));
    let head = f32::from_bits(BALL_HEAD_YAW.load(Ordering::Relaxed));
    let pitch = f32::from_bits(BALL_HEAD_PITCH.load(Ordering::Relaxed));

    let body_k = 1.0 - (-12.0f32 * DT).exp();
    let head_k = 1.0 - (-18.0f32 * DT).exp();
    let nb = body + (target - body) * body_k;
    // 头相对身体再转一点（最多 ±0.3 rad），总朝向不会超过 MAX_YAW 太多
    let head_target = (look - nb).clamp(-0.3, 0.3) + (t * 0.9).sin() * 0.03;
    let nh = head + (head_target - head) * head_k;
    // 光标越低越低头（幅度收着点，8x8 的头转太多会显得扭曲）
    let pitch_target = (dy / 300.0).clamp(-0.22, 0.28);
    let np = pitch + (pitch_target - pitch) * head_k;

    BALL_YAW.store(nb.to_bits(), Ordering::Relaxed);
    BALL_HEAD_YAW.store(nh.to_bits(), Ordering::Relaxed);
    BALL_HEAD_PITCH.store(np.to_bits(), Ordering::Relaxed);
    ((target - nb).abs(), (head_target - nh).abs() + (pitch_target - np).abs())
}

/// 纸娃娃动画循环：光标在动或姿态未收敛时 ~30fps，静置时降到 ~7fps 保住 CPU。
fn spawn_doll_ticker() {
    thread::spawn(|| {
        let mut last_render = f32::NEG_INFINITY;
        let mut last_hover = 0u8;
        loop {
            thread::sleep(std::time::Duration::from_millis(33));
            if APP_EXIT_REQUESTED.load(Ordering::Relaxed) {
                break;
            }
            let hwnd = BALL_HWND.load(Ordering::Relaxed);
            if hwnd == 0 {
                continue;
            }
            unsafe {
                if !IsWindowVisible(HWND(hwnd as *mut _)).as_bool() {
                    continue;
                }
            }
            if BALL_SKIN.read().map(|g| g.is_none()).unwrap_or(true) {
                continue;
            }
            let mut cp = POINT::default();
            unsafe {
                let _ = GetCursorPos(&mut cp);
            }
            let packed = ((cp.x as u32 as u64) << 32) | (cp.y as u32 as u64);
            let prev = BALL_CURSOR.swap(packed, Ordering::Relaxed);
            if prev != packed {
                BALL_LAST_ACTIVE.store(anim_secs().to_bits(), Ordering::Relaxed);
            }
            let (rest_body, rest_head) = update_doll_pose();
            // 悬停高亮：直接用光标位置算，不依赖 WM_MOUSEMOVE（分层窗口下不一定收得到）
            unsafe {
                let (wx, wy) = (
                    cp.x - BALL_POS_X.load(Ordering::Relaxed),
                    cp.y - BALL_POS_Y.load(Ordering::Relaxed),
                );
                let h = ball_hit(wx, wy);
                BALL_HOVER.store(if (1..=5).contains(&h) { h } else { 0 }, Ordering::Relaxed);
            }
            let t = anim_secs();
            let last_active = f32::from_bits(BALL_LAST_ACTIVE.load(Ordering::Relaxed));
            let moving = rest_body > 0.004 || rest_head > 0.004;
            let active = last_active > 0.0 && (t - last_active) < 0.5;
            let hover = BALL_HOVER.load(Ordering::Relaxed);
            let hover_changed = hover != last_hover;
            if hover_changed {
                last_hover = hover;
            }
            // 待机动画关掉后，静置就完全不重绘
            let idle_anim = BALL_FLOAT.load(Ordering::Relaxed) || BALL_SWAY.load(Ordering::Relaxed);
            let due = idle_anim && (t - last_render) >= 0.12;
            let dirty = BALL_DIRTY.swap(false, Ordering::Relaxed);
            if moving || active || hover_changed || due || dirty {
                last_render = t;
                unsafe { ball_render() };
            }
        }
    });
}

unsafe fn ball_render() {
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject, DrawTextW, GetDC,
        ReleaseDC, SelectObject, SetBkMode, SetTextColor, BITMAPINFO, BITMAPINFOHEADER,
        BLENDFUNCTION, BI_RGB, DIB_RGB_COLORS, HGDIOBJ, TRANSPARENT,
    };
    let hwnd = BALL_HWND.load(Ordering::Relaxed);
    if hwnd == 0 {
        return;
    }
    let w = BALL_W;
    let h = BALL_H;
    let screen = GetDC(HWND::default());
    let mem = CreateCompatibleDC(screen);
    let mut bi: BITMAPINFO = std::mem::zeroed();
    bi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    bi.bmiHeader.biWidth = w;
    bi.bmiHeader.biHeight = -h;
    bi.bmiHeader.biPlanes = 1;
    bi.bmiHeader.biBitCount = 32;
    bi.bmiHeader.biCompression = BI_RGB.0;
    let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
    let dib = match CreateDIBSection(mem, &bi, DIB_RGB_COLORS, &mut bits, None, 0) {
        Ok(d) => d,
        Err(_) => {
            let _ = DeleteDC(mem);
            ReleaseDC(HWND::default(), screen);
            return;
        }
    };
    let old = SelectObject(mem, HGDIOBJ(dib.0));
    let buf = std::slice::from_raw_parts_mut(bits as *mut u8, (w * h * 4) as usize);
    buf.fill(0);

    let t = f32::from_bits(BALL_ANIM.load(Ordering::Relaxed));
    let (bx, by) = ball_center();

    // 展开面板：MC 背包风格（标题栏 + 深色槽底 + 4 个按钮 + 底部透明度条）
    // t 同时当透明度用：弹出时整块面板淡入，不至于"啪"一下糊在桌面上
    let panel_alpha = t.clamp(0.0, 1.0);
    if panel_alpha > 0.02 {
        let p = ball_panel(t);
        let hover = BALL_HOVER.load(Ordering::Relaxed);
        draw_mc_panel(buf, w, h, p.panel, panel_alpha);
        draw_mc_divider(buf, w, h, p.divider, panel_alpha);
        draw_mc_stage(buf, w, h, p.stage, panel_alpha);
        for (i, r) in p.buttons.iter().enumerate() {
            draw_mc_button(buf, w, h, *r, hover == (i as u8 + 1), false, panel_alpha);
        }
        let cur = target_alpha_now();
        let sv = ((255.0 - cur as f32) / 215.0).clamp(0.0, 1.0);
        draw_mc_slider(buf, w, h, p.slider, sv, panel_alpha);
        // 标题栏右上角的关闭键（MC 风格小按钮 + ×）
        draw_mc_button(buf, w, h, p.close_btn, hover == 6, false, panel_alpha);
        draw_mc_cross(buf, w, h, p.close_btn, (255, 255, 255), panel_alpha);
        // 目标窗口那一行：MC 凹槽底 + 窗口标题
        draw_mc_target_row(buf, w, h, p.target_row, panel_alpha);
        draw_mc_target_row(buf, w, h, p.input_row, panel_alpha);
    }

    // 中间：MC 3D 纸娃娃（基岩版左上角纸娃娃观感，随光标连续转向）
    let skin_guard = BALL_SKIN.read().ok();
    if let Some(skin) = skin_guard.as_ref().and_then(|g| g.as_ref()) {
        let stored = f32::from_bits(BALL_SCALE.load(Ordering::Relaxed));
        let unit = if stored > 0.1 { stored } else { 2.1 };
        let t = anim_secs();
        // 待机上下浮动（设置里可关）
        let bob = if BALL_FLOAT.load(Ordering::Relaxed) {
            ((t / 2.6) * std::f32::consts::TAU).sin() * 1.4
        } else {
            0.0
        };
        let pose = doll::Pose {
            yaw: f32::from_bits(BALL_YAW.load(Ordering::Relaxed)),
            cam_pitch: 18.0f32.to_radians(),
            head_yaw: f32::from_bits(BALL_HEAD_YAW.load(Ordering::Relaxed)),
            head_pitch: f32::from_bits(BALL_HEAD_PITCH.load(Ordering::Relaxed)),
            // 待机手臂轻摆（左右反向，设置里可关）
            arm_swing: if BALL_SWAY.load(Ordering::Relaxed) {
                ((t / 5.0) * std::f32::consts::TAU).sin() * 2.2f32.to_radians()
            } else {
                0.0
            },
            leg_swing: 0.0,
            scale: unit,
            dist: 95.0,
            ss: 3,
        };
        // 先画到一张透明图层，再按包围盒把人物摆到舞台正中
        let mut layer = vec![0u8; (w * h * 4) as usize];
        let rect = doll::render(
            &mut layer,
            w as usize,
            h as usize,
            bx as f32,
            by as f32 + 3.0 + bob,
            skin,
            &pose,
        );
        let mut doll_rect: u64 = 0;
        if let Some((x0, y0, x1, y1)) = rect {
            if x1 > x0 && y1 > y0 {
                // 折叠时留在球的位置；展开时按进度插值到舞台中心
                let (mut cx, mut cy) = (bx, by);
                if panel_alpha > 0.02 {
                    let p = ball_panel(t);
                    let scx = (p.stage.0 + p.stage.2) / 2;
                    let scy = (p.stage.1 + p.stage.3) / 2;
                    cx = (bx as f32 + (scx - bx) as f32 * panel_alpha).round() as i32;
                    cy = (by as f32 + (scy - by) as f32 * panel_alpha).round() as i32;
                }
                let bw = x1 - x0;
                let bh = y1 - y0;
                // 水平：只按"舞台中心 - 球心"平移，不跟 bbox 走。
                // 转视角时轮廓宽度一直在变，用 bbox 居中会让整个人左右抖一下（原地抽畜）。
                let dx = cx - bx;
                // 垂直：和水平一样只按「舞台中心 - 球心」平移。
                // 人物本身在 render 里就是以 (bx,by) 为模型中心画的，再拿 bbox 去二次居中
                // 只会把包围盒每帧 ±1~2px 的抖动放大成"原地抽畜"，而且会把待机浮动抵消掉。
                let dy = cy - by;
                for yy in 0..bh {
                    for xx in 0..bw {
                        let sx = x0 + xx;
                        let sy = y0 + yy;
                        let nx = sx + dx;
                        let ny = sy + dy;
                        if sx < 0
                            || sy < 0
                            || sx >= w
                            || sy >= h
                            || nx < 0
                            || ny < 0
                            || nx >= w
                            || ny >= h
                        {
                            continue;
                        }
                        let so = ((sy * w + sx) * 4) as usize;
                        let a = layer[so + 3] as f32 / 255.0;
                        if a <= 0.004 {
                            continue;
                        }
                        let dobj = ((ny * w + nx) * 4) as usize;
                        mc_blend_px(
                            buf,
                            dobj,
                            (layer[so + 2], layer[so + 1], layer[so]),
                            a,
                        );
                    }
                }
                doll_rect = pack_rect(x0 + dx, y0 + dy, x1 + dx, y1 + dy);
            }
        }
        BALL_DOLL_RECT.store(doll_rect, Ordering::Relaxed);
    }

    // 纸娃娃的 1px 描边现在由 `doll::render` 在超采样域里画（和模型边缘一起
    // 降采样，边缘干净、粗细稳定）。这里不要再套一圈：两层描边叠起来又粗又毛。
    // 面板本身有 2px 黑框（draw_mc_panel / draw_mc_stage），也不需要这层。

    // 文字（GDI 只写 RGB，最后统一补 alpha）
    let _ = SetBkMode(mem, TRANSPARENT);
    let _ = SetTextColor(mem, COLORREF(0x00FF_FFFF));
    let font_ball = CreateFontW(
        24, 0, 0, 0, 700, 0, 0, 0, 1, 0, 0, 0, 0,
        windows::core::w!("Microsoft YaHei"),
    );
    let old_font = SelectObject(mem, HGDIOBJ(font_ball.0));
    let mut label: Vec<u16> = if t > 0.5 { "×" } else { "T" }.encode_utf16().collect();
    let mut lr = RECT { left: bx - 12, top: by - 12, right: bx + 16, bottom: by + 14 };
    if BALL_SKIN.read().map(|g| g.is_none()).unwrap_or(true) {
        let _ = DrawTextW(mem, &mut label, &mut lr, windows::Win32::Graphics::Gdi::DT_LEFT);
    }

    if t > 0.02 {
        let p = ball_panel(t);
        let font = CreateFontW(
            15, 0, 0, 0, 700, 0, 0, 0, 1, 0, 0, 0, 3,
            windows::core::w!("Microsoft YaHei"),
        );
        let old_sat = SelectObject(mem, HGDIOBJ(font.0));
        // 文字不能真的半透明（GDI 不写 alpha），用"往底色靠"的方式跟着面板淡入
        let k = ((t - 0.45) / 0.55).clamp(0.0, 1.0);
        let fade = |c: (u8, u8, u8)| -> (u8, u8, u8) {
            (
                (198.0 + (c.0 as f32 - 198.0) * k) as u8,
                (198.0 + (c.1 as f32 - 198.0) * k) as u8,
                (198.0 + (c.2 as f32 - 198.0) * k) as u8,
            )
        };
        draw_mc_label(mem, "TransGlass", p.title, fade((80, 80, 80)));
        // 目标窗口那一行：没目标就显示「未选中窗口」
        let target_hwnd = ball_target();
        let target_text = {
            let h = target_hwnd;
            if h.0.is_null() {
                "未选中窗口".to_string()
            } else {
                // 只显示"是什么软件"，用【】框起来
                let app = cached_app_name(h);
                if app.is_empty() {
                    "未选中窗口".to_string()
                } else {
                    format!("【{app}】")
                }
            }
        };
        draw_mc_label(mem, &target_text, p.target_row, fade((255, 255, 255)));
        // 最近一次输入：鼠标 / 笔（面板最底下一行）
        let last_input = match LAST_INPUT_IS_PEN.load(Ordering::Relaxed) {
            1 => "最近输入：笔",
            0 => "最近输入：鼠标",
            _ => "最近输入：—",
        };
        draw_mc_label(mem, last_input, p.input_row, fade((255, 255, 255)));
        // 右下：当前目标是不是 Alt 会话里正在操作的窗口（选中 = 红字）
        let operating = !target_hwnd.0.is_null()
            && crate::selector::is_session_window(target_hwnd.0 as isize);
        let status_color = if operating {
            (220, 70, 70)
        } else {
            (255, 255, 255)
        };
        let status_rect = (
            p.input_row.2 + 4,
            p.input_row.1,
            p.panel.2 - 10,
            p.input_row.3,
        );
        draw_mc_label(
            mem,
            if operating { "操作中" } else { "未操作" },
            status_rect,
            fade(status_color),
        );
        // 模式按钮直接把当前输入模式写在按钮上（自动 / 鼠标 / 笔）
        let mode_short = match get_input_mode() {
            InputMode::Auto => "自动",
            InputMode::ForceMouse => "鼠标",
            InputMode::ForcePen => "笔",
        };
        let mode_label = format!("模式：{mode_short}");
        let labels = ["置顶", "穿透", mode_label.as_str(), "还原"];
        for (i, r) in p.buttons.iter().enumerate() {
            draw_mc_label(mem, labels[i], *r, fade((255, 255, 255)));
        }
        let cur = target_alpha_now();
        let sv = ((255.0 - cur as f32) / 215.0).clamp(0.0, 1.0);
        draw_mc_label(
            mem,
            &format!("透明度 {}%", (sv * 100.0).round() as i32),
            p.slider,
            fade((255, 255, 255)),
        );
        SelectObject(mem, old_sat);
        let _ = DeleteObject(HGDIOBJ(font.0));
    }

    // 文字像素 alpha 补齐（GDI 不写 alpha）
    for px in buf.chunks_exact_mut(4) {
        if px[3] == 0 && (px[0] != 0 || px[1] != 0 || px[2] != 0) {
            px[3] = 255;
        }
    }

    // UpdateLayeredWindow 的 AC_SRC_ALPHA 要求预乘 alpha：
    // 不预乘的话抗锯齿边缘会往亮处偏，深色桌面上能看到一圈白边。
    for px in buf.chunks_exact_mut(4) {
        match px[3] {
            255 => {}
            0 => {
                px[0] = 0;
                px[1] = 0;
                px[2] = 0;
            }
            a => {
                let a = a as u32;
                px[0] = (px[0] as u32 * a / 255) as u8;
                px[1] = (px[1] as u32 * a / 255) as u8;
                px[2] = (px[2] as u32 * a / 255) as u8;
            }
        }
    }

    let pos = POINT {
        x: BALL_POS_X.load(Ordering::Relaxed),
        y: BALL_POS_Y.load(Ordering::Relaxed),
    };
    let size = SIZE { cx: w, cy: h };
    let src = POINT { x: 0, y: 0 };
    let blend = BLENDFUNCTION {
        BlendOp: 0,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: 1,
    };
    let _ = windows::Win32::UI::WindowsAndMessaging::UpdateLayeredWindow(
        HWND(hwnd as *mut _),
        screen,
        Some(&pos),
        Some(&size),
        mem,
        Some(&src),
        COLORREF(0),
        Some(&blend),
        windows::Win32::UI::WindowsAndMessaging::ULW_ALPHA,
    );

    SelectObject(mem, old_font);
    SelectObject(mem, old);
    let _ = DeleteObject(HGDIOBJ(font_ball.0));
    let _ = DeleteObject(HGDIOBJ(dib.0));
    let _ = DeleteDC(mem);
    ReleaseDC(HWND::default(), screen);
}
unsafe fn ball_hit(x: i32, y: i32) -> u8 {
    // 展开时先判面板上的控件（关闭键在最上层）
    {
        let t = f32::from_bits(BALL_ANIM.load(Ordering::Relaxed));
        if t > 0.5 {
            let p = ball_panel(t);
            let inside = |r: (i32, i32, i32, i32)| x >= r.0 && x < r.2 && y >= r.1 && y < r.3;
            if inside(p.close_btn) {
                return 6;
            }
        }
    }
    let t = f32::from_bits(BALL_ANIM.load(Ordering::Relaxed));
    let (bx, by) = ball_center();
    if ((x - bx).pow(2) + (y - by).pow(2)) <= 34 * 34 {
        return 9; // 纸娃娃/球本身
    }
    if let Some((rx0, ry0, rx1, ry1)) = unpack_rect(BALL_DOLL_RECT.load(Ordering::Relaxed)) {
        if x >= rx0 && y >= ry0 && x < rx1 && y < ry1 {
            return 9;
        }
    }
    if t > 0.5 {
        let p = ball_panel(t);
        for (i, r) in p.buttons.iter().enumerate() {
            if x >= r.0 && y >= r.1 && x < r.2 && y < r.3 {
                return (i + 1) as u8;
            }
        }
        let s = p.slider;
        if x >= s.0 && y >= s.1 && x < s.2 && y < s.3 {
            return 5; // 透明度滑条
        }
    }
    0
}

/// 根据滑条上的 x 设置遥控窗口的透明度（拖到底 = 最透明）
unsafe fn slider_apply(x: i32) {
    let t = f32::from_bits(BALL_ANIM.load(Ordering::Relaxed));
    let p = ball_panel(t);
    let (x0, _, _, _) = p.slider;
    let travel = slider_travel(p.slider);
    let v = ((x - x0 - 16) as f32 / travel).clamp(0.0, 1.0);
    let alpha = (255.0 - v * 215.0).round().clamp(40.0, 255.0) as u8;
    let target = ball_target();
    if target.0.is_null() {
        return;
    }
    ball_apply(target, BallAction::SetAlpha(alpha));
}

unsafe extern "system" fn ball_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match msg {
        WM_LBUTTONDOWN => {
            let x = (l.0 & 0xFFFF) as i32;
            let y = ((l.0 >> 16) & 0xFFFF) as i32;
            let hit = ball_hit(x, y);
            if hit == 9 {
                BALL_DRAGGING.store(true, Ordering::Relaxed);
                BALL_MOVED.store(false, Ordering::Relaxed);
                let mut p = POINT::default();
                let _ = GetCursorPos(&mut p);
                BALL_DOWN_POS.store(
                    ((p.x as u32 as u64) << 32) | (p.y as u32 as u64),
                    Ordering::Relaxed,
                );
                let _ = SetCapture(hwnd);
                return LRESULT(0);
            }
            if hit == 5 {
                // 透明度滑条：按下即生效，拖动持续更新
                BALL_SLIDER_DRAG.store(true, Ordering::Relaxed);
                let _ = SetCapture(hwnd);
                slider_apply(x);
                ball_render();
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, msg, w, l)
        }
        WM_MOUSEMOVE => {
            if BALL_SLIDER_DRAG.load(Ordering::Relaxed) {
                let x = (l.0 & 0xFFFF) as i32;
                slider_apply(x);
                ball_render();
                return LRESULT(0);
            }
            if BALL_DRAGGING.load(Ordering::Relaxed) {
                let mut p = POINT::default();
                let _ = GetCursorPos(&mut p);
                let packed = BALL_DOWN_POS.load(Ordering::Relaxed);
                let ox = (packed >> 32) as i32;
                let oy = (packed & 0xFFFF_FFFF) as i32;
                if (p.x - ox).abs() + (p.y - oy).abs() > 4 {
                    BALL_MOVED.store(true, Ordering::Relaxed);
                }
                if BALL_MOVED.load(Ordering::Relaxed) {
                    let nx = BALL_POS_X.load(Ordering::Relaxed) + (p.x - ox);
                    let ny = BALL_POS_Y.load(Ordering::Relaxed) + (p.y - oy);
                    BALL_POS_X.store(nx, Ordering::Relaxed);
                    BALL_POS_Y.store(ny, Ordering::Relaxed);
                    let _ = SetWindowPos(
                        hwnd,
                        HWND_TOPMOST,
                        nx,
                        ny,
                        0,
                        0,
                        SWP_NOSIZE | SWP_NOACTIVATE,
                    );
                    BALL_DOWN_POS.store(
                        ((p.x as u32 as u64) << 32) | (p.y as u32 as u64),
                        Ordering::Relaxed,
                    );
                }
                return LRESULT(0);
            }
            // 悬停高亮由 ticker 统一算，这里不再处理
            DefWindowProcW(hwnd, msg, w, l)
        }
        WM_LBUTTONUP => {
            if BALL_SLIDER_DRAG.swap(false, Ordering::Relaxed) {
                let _ = ReleaseCapture();
                ball_render();
                return LRESULT(0);
            }
            let dragged = BALL_DRAGGING.swap(false, Ordering::Relaxed)
                && BALL_MOVED.swap(false, Ordering::Relaxed);
            let _ = ReleaseCapture();
            if dragged {
                return LRESULT(0);
            }
            let x = (l.0 & 0xFFFF) as i32;
            let y = ((l.0 >> 16) & 0xFFFF) as i32;
            match ball_hit(x, y) {
                6 => {
                    // 面板上的 × ：关掉悬浮球
                    BALL_EXPANDED.store(false, Ordering::Relaxed);
                    BALL_ANIM.store(0.0f32.to_bits(), Ordering::Relaxed);
                    BALL_SHOWN.store(false, Ordering::Relaxed);
                    let _ = ShowWindow(hwnd, SW_HIDE);
                    request_ui_repaint();
                }
                9 => {
                    // 双击纸娃娃 → 打开主界面（设置里可关）
                    if DOLL_DBLCLICK.load(Ordering::Relaxed) {
                        let now = anim_secs();
                        let last = f32::from_bits(DOLL_LAST_CLICK.load(Ordering::Relaxed));
                        let pos = DOLL_LAST_POS.load(Ordering::Relaxed);
                        let this_pos = ((x as u32) << 16) | (y as u32 & 0xFFFF);
                        let ddx = ((pos >> 16) as i32 - x).abs();
                        let ddy = ((pos & 0xFFFF) as i32 - y).abs();
                        DOLL_LAST_POS.store(this_pos, Ordering::Relaxed);
                        DOLL_LAST_CLICK.store(now.to_bits(), Ordering::Relaxed);
                        if last > 0.0 && now - last < 0.35 && ddx <= 14 && ddy <= 40 {
                            DOLL_LAST_CLICK.store(0.0f32.to_bits(), Ordering::Relaxed);
                            show_root_window();
                            return LRESULT(0);
                        }
                    }
                    ball_set_expanded(!ball_expand_target());
                }
                1..=4 => {
                    let target = ball_target();
                    if target.0.is_null() {
                        return LRESULT(0);
                    }
                    match ball_hit(x, y) {
                        1 => ball_apply(target, BallAction::Topmost),
                        2 => ball_apply(target, BallAction::Passthrough),
                        3 => ball_apply(target, BallAction::CycleMode),
                        4 => ball_apply(target, BallAction::Restore),
                        _ => {}
                    }
                    ball_render();
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_BALL_POPUP => {
            ball_set_expanded(true);
            LRESULT(0)
        }
        // 右键悬浮球：选中球下面那个窗口，显示选择框并主动弹出控制面板
        WM_RBUTTONUP => {
            let pt = POINT {
                x: BALL_POS_X.load(Ordering::Relaxed) + BALL_W - 42,
                y: BALL_POS_Y.load(Ordering::Relaxed) + BALL_H / 2,
            };
            let mut hit = WindowFromPoint(pt);
            let mut n = 0;
            let mut picked = false;
            while !hit.0.is_null() && n < 64 {
                if !is_own_hwnd(hit) && !is_shell_hwnd(hit) && IsWindowVisible(hit).as_bool() {
                    let mut rc = RECT::default();
                    if GetWindowRect(hit, &mut rc).is_ok()
                        && pt.x >= rc.left
                        && pt.x < rc.right
                        && pt.y >= rc.top
                        && pt.y < rc.bottom
                    {
                        let root = GetAncestor(hit, GA_ROOT);
                        let root = if root.0.is_null() { hit } else { root };
                        if !is_own_hwnd(root) && !is_shell_hwnd(root) {
                            crate::select_window_for_control(root.0 as isize);
                            picked = true;
                            break;
                        }
                    }
                }
                let Some(next) = GetWindow(hit, GW_HWNDNEXT).ok() else {
                    break;
                };
                hit = next;
                n += 1;
            }
            if !picked {
                ball_set_expanded(true);
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, w, l),
    }
}

unsafe fn ball_set_expanded(target: bool) {
    if ball_expand_target() == target {
        return;
    }
    BALL_EXPANDED.store(target, Ordering::Relaxed);
    // 12 步 × 16ms ≈ 200ms，三次方 ease-out：起手快、收尾慢
    let steps = 12;
    for i in 0..=steps {
        let v = i as f32 / steps as f32;
        let e = 1.0 - (1.0 - v).powi(3);
        let t = if target { e } else { 1.0 - e };
        BALL_ANIM.store(t.to_bits(), Ordering::Relaxed);
        ball_render();
        std::thread::sleep(std::time::Duration::from_millis(16));
    }
}

unsafe fn set_ball_visible(on: bool) {
    BALL_SHOWN.store(on, Ordering::Relaxed);
    let raw = BALL_HWND.load(Ordering::Relaxed);
    if raw == 0 {
        if on {
            ball_spawn();
        }
        return;
    }
    let h = HWND(raw as *mut _);
    if on {
        ball_render();
        let _ = ShowWindow(h, SW_SHOWNA);
    } else {
        BALL_EXPANDED.store(false, Ordering::Relaxed);
        BALL_ANIM.store(0.0f32.to_bits(), Ordering::Relaxed);
        let _ = ShowWindow(h, SW_HIDE);
    }
}

unsafe fn ball_spawn() {
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    if BALL_HWND.load(Ordering::Relaxed) != 0 {
        return;
    }
    let hinst = match GetModuleHandleW(None) {
        Ok(h) => h,
        Err(_) => return,
    };
    let class_w: Vec<u16> = "TransGlassBall2".encode_utf16().chain(std::iter::once(0)).collect();
    let name_w: Vec<u16> = "TransGlass".encode_utf16().chain(std::iter::once(0)).collect();
    let wc = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(ball_proc),
        hInstance: windows::Win32::Foundation::HINSTANCE(hinst.0),
        lpszClassName: windows::core::PCWSTR(class_w.as_ptr()),
        ..Default::default()
    };
    let _ = RegisterClassW(&wc);
    let hwnd = match CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
        windows::core::PCWSTR(class_w.as_ptr()),
        windows::core::PCWSTR(name_w.as_ptr()),
        WS_POPUP,
        BALL_POS_X.load(Ordering::Relaxed),
        BALL_POS_Y.load(Ordering::Relaxed),
        BALL_W,
        BALL_H,
        None,
        None,
        windows::Win32::Foundation::HINSTANCE(hinst.0),
        None,
    ) {
        Ok(h) => h,
        Err(_) => return,
    };
    let _ = ShowWindow(hwnd, SW_SHOWNA);
    BALL_HWND.store(hwnd.0 as isize, Ordering::Relaxed);
    ball_render();
}

unsafe fn purge_shell_managed() {
    let keys: Vec<isize> = GLOBAL_REGISTRY.iter().map(|kv| *kv.key()).collect();
    for k in keys {
        if is_shell_hwnd(HWND(k as *mut _)) {
            restore_window(HWND(k as *mut _));
        }
    }
}

#[derive(Clone, Copy)]
enum BallAction {
    AlphaDown,
    AlphaUp,
    /// 直接设成指定透明度（滑条用）
    SetAlpha(u8),
    Topmost,
    Passthrough,
    CycleMode,
    Restore,
}

unsafe fn ball_apply(target: HWND, action: BallAction) {
    log_hilite(&format!("apply target={:?}", target.0));
    if target.0.is_null() || is_own_hwnd(target) || is_shell_hwnd(target) {
        return;
    }
    match action {
        BallAction::Restore => restore_window(target),
        BallAction::CycleMode => cycle_input_mode(),
        BallAction::Topmost => toggle_topmost(target),
        BallAction::Passthrough => {
            ensure_managed(target);
            let v = target.0 as isize;
            let (alpha, top, mouse, pen) = {
                let mut out = (255u8, false, false, false);
                if let Some(st) = GLOBAL_REGISTRY.get(&v) {
                    out = (
                        st.current_alpha,
                        st.user_pref_topmost,
                        st.mouse_passthrough,
                        st.pen_passthrough,
                    );
                }
                out
            };
            let on = !(mouse && pen);
            if let Some(mut st) = GLOBAL_REGISTRY.get_mut(&v) {
                st.mouse_passthrough = on;
                st.pen_passthrough = on;
            }
            let _ = apply_transparency_to_hwnd(target, alpha, top, on, on);
        }
        BallAction::SetAlpha(want) => {
            ensure_managed(target);
            let v = target.0 as isize;
            let (top, mouse, pen) = {
                let mut out = (false, false, false);
                if let Some(st) = GLOBAL_REGISTRY.get(&v) {
                    out = (st.user_pref_topmost, st.mouse_passthrough, st.pen_passthrough);
                }
                out
            };
            let next = want.clamp(40, 255);
            if let Some(mut st) = GLOBAL_REGISTRY.get_mut(&v) {
                st.current_alpha = next;
            }
            let _ = apply_transparency_to_hwnd(target, next, top, mouse, pen);
        }
        BallAction::AlphaDown | BallAction::AlphaUp => {
            ensure_managed(target);
            let v = target.0 as isize;
            let mut alpha = 255u8;
            let mut top = false;
            let mut mouse = false;
            let mut pen = false;
            if let Some(st) = GLOBAL_REGISTRY.get(&v) {
                alpha = st.current_alpha;
                top = st.user_pref_topmost;
                mouse = st.mouse_passthrough;
                pen = st.pen_passthrough;
            }
            let step = 26u8;
            let next = match action {
                BallAction::AlphaUp => alpha.saturating_sub(step),
                _ => alpha.saturating_add(step),
            }
            .clamp(40, 255);
            if let Some(mut st) = GLOBAL_REGISTRY.get_mut(&v) {
                st.current_alpha = next;
            }
            let _ = apply_transparency_to_hwnd(target, next, top, mouse, pen);
        }
    }
    request_ui_repaint();
}

static BALL_TEX: std::sync::OnceLock<image::RgbaImage> = std::sync::OnceLock::new();

fn load_ball_tex() {
    let p = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join("ball_glass.png")));
    if let Some(p) = p {
        if let Ok(img) = image::open(&p) {
            let _ = BALL_TEX.set(img.to_rgba8());
        }
    }
}

/// 在方块内按纹理坐标采样玻璃贴图
fn ball_tex_sample(dx: i32, dy: i32, r: i32) -> (u8, u8, u8, u8) {
    let Some(tex) = BALL_TEX.get() else {
        return (26, 36, 44, 200);
    };
    let (tw, th) = (tex.width() as f32, tex.height() as f32);
    let u = ((dx + r) as f32 / (2 * r) as f32).clamp(0.0, 1.0);
    let v = ((dy + r) as f32 / (2 * r) as f32).clamp(0.0, 1.0);
    let tx = (u * (tw - 1.0)).round() as u32;
    let ty = (v * (th - 1.0)).round() as u32;
    let p = tex.get_pixel(tx, ty);
    // 贴图 alpha 往往偏低，提一点让方块可见
    let a = (p[3] as u32 * 5 / 4).min(255) as u8;
    (p[0], p[1], p[2], a)
}

/// 圆角矩形覆盖率（0..1），用于抗锯齿
fn sq_coverage(dx: i32, dy: i32, half: f32, rad: f32) -> (f32, f32) {
    let qx = (dx.abs() as f32) - (half - rad);
    let qy = (dy.abs() as f32) - (half - rad);
    let ox = qx.max(0.0);
    let oy = qy.max(0.0);
    let d = (ox * ox + oy * oy).sqrt() + qx.max(qy).min(0.0) - rad;
    let cov = (0.5 - d).clamp(0.0, 1.0);
    (cov, d)
}


static BALL_SKIN: std::sync::RwLock<Option<doll::Skin>> = std::sync::RwLock::new(None);

fn skin_dir() -> Option<std::path::PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join("skins")))
}

fn list_skins() -> Vec<String> {
    let mut out = Vec::new();
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.to_path_buf()));
    if let Some(dir) = exe_dir {
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name.to_lowercase().ends_with(".png") {
                    out.push(name);
                }
            }
        }
        if let Ok(rd) = std::fs::read_dir(dir.join("skins")) {
            for e in rd.flatten() {
                let name = format!("skins/{}", e.file_name().to_string_lossy());
                if name.to_lowercase().ends_with(".png") {
                    out.push(name);
                }
            }
        }
    }
    out.sort();
    out
}

fn load_ball_skin(name: Option<&str>, model: Option<doll::ModelKind>) {
    let exe_dir = match std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.to_path_buf()))
    {
        Some(d) => d,
        None => return,
    };
    let rel = name.unwrap_or("skin.png");
    let path = exe_dir.join(rel);
    if let Ok(img) = image::open(&path) {
        let rgba = img.to_rgba8();
        let skin = doll::Skin::from_rgba(&rgba, model);
        if let Ok(mut g) = BALL_SKIN.write() {
            *g = Some(skin);
        }
    }
}

/// 设置的分类（左边导航）
pub(crate) const SETTINGS_TABS: [&str; 5] = [
    "快捷键",
    "输入",
    "窗口与预览",
    "悬浮球与纸娃娃",
    "维护",
];

impl TransGlassApp {
    fn settings_page_hotkeys(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("快捷键").size(18.0).color(TEXT_PRIMARY));
        ui.add_space(2.0);
        let mut priority = self.hotkey_priority_ui;
        if ui
            .checkbox(&mut priority, "快捷键优先（被别的程序占用时也强制生效）")
            .changed()
        {
            self.hotkey_priority_ui = priority;
            if let Err(e) = save_priority_flag(priority) {
                push_issue(format!("保存快捷键优先设置失败: {e}"));
            }
        }
        ui.label(
            egui::RichText::new("开启后改用低级键盘钩子强制抢占组合键，可能被反作弊/输入法干扰。")
                .small()
                .weak(),
        );
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new(
                "点按位格进入录入，再按任意键或鼠标键即可绑定（支持鼠标侧键1/2、中键、滚轮上下）；\
                 鼠标左键/右键必须配修饰键；Esc 取消，右键点按位格直接清除。",
            )
            .small()
            .weak(),
        );
        ui.add_space(10.0);
        self.hotkey_editor_inline(ui);
    }

    /// 设置页：直接作为主界面内容（不再弹二级窗口）
    /// 自绘标题栏：图标 + 标题 + 最小化/最大化/关闭；空白处可拖动、双击最大化
    fn draw_titlebar(&mut self, ctx: &egui::Context) {
        let maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
        egui::TopBottomPanel::top("app_titlebar")
            .exact_height(36.0)
            .frame(
                egui::Frame::none()
                    .fill(SURFACE)
                    .inner_margin(egui::Margin::symmetric(10.0, 0.0)),
            )
            .show(ctx, |ui| {
                let rect = ui.max_rect();
                ui.horizontal_centered(|ui| {
                    if let Some(tex) = self.app_icon.as_ref() {
                        ui.add(
                            egui::Image::new(egui::load::SizedTexture::new(
                                tex.id(),
                                egui::vec2(16.0, 16.0),
                            ))
                            .fit_to_exact_size(egui::vec2(16.0, 16.0)),
                        );
                        ui.add_space(6.0);
                    }
                    ui.label(
                        egui::RichText::new("TransGlass 控制面板")
                            .size(12.5)
                            .color(TEXT_PRIMARY),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if titlebar_button(ui, TitleBtn::Close).clicked() {
                            hide_root_window();
                        }
                        let mx = if maximized {
                            titlebar_button(ui, TitleBtn::Restore)
                        } else {
                            titlebar_button(ui, TitleBtn::Max)
                        };
                        if mx.clicked() {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
                        }
                        if titlebar_button(ui, TitleBtn::Min).clicked() {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                        }
                        if ui
                            .add(
                                egui::Button::new(
                                    egui::RichText::new("帮助")
                                        .size(12.0)
                                        .color(TEXT_SECONDARY),
                                )
                                .frame(false)
                                .min_size(egui::vec2(0.0, 22.0)),
                            )
                            .on_hover_text("用浏览器打开使用教程")
                            .clicked()
                        {
                            open_help_page();
                        }
                    });
                });
                // 拖动区：左侧 62%（避开右边的按钮）
                let drag_rect = egui::Rect::from_min_max(
                    egui::pos2(rect.left(), rect.top()),
                    egui::pos2(rect.left() + rect.width() * 0.62, rect.bottom()),
                );
                let resp = ui.interact(
                    drag_rect,
                    ui.id().with("titlebar_drag"),
                    egui::Sense::click_and_drag(),
                );
                if resp.double_clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
                } else if resp.dragged() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                }
                // 底部一条分隔线
                ui.painter().hline(
                    rect.x_range(),
                    rect.bottom() - 0.5,
                    egui::Stroke::new(1.0, BORDER),
                );
            });
    }

    fn settings_page_ui(&mut self, ctx: &egui::Context) {
        let fade = self
            .settings_fade
            .map(|t| (t.elapsed().as_secs_f32() / 0.12).clamp(0.0, 1.0))
            .unwrap_or(1.0);
        egui::TopBottomPanel::bottom("settings_bar").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui.button("← 返回").clicked() {
                    self.settings_open = false;
                }
                ui.label(
                    egui::RichText::new("设置")
                        .size(15.0)
                        .color(TEXT_PRIMARY),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(format!("v{}", env!("CARGO_PKG_VERSION")))
                            .small()
                            .weak(),
                    );
                    // 快捷键页才需要保存/恢复默认：放在右下角，永远看得见
                    if self.settings_tab == 0 {
                        if ui.button("保存并应用").clicked() {
                            self.save_and_reload_hotkeys();
                        }
                        if ui.button("恢复默认").clicked() {
                            self.hotkey_draft = default_config();
                            self.hotkey_message = Some("已恢复默认（尚未保存）".into());
                        }
                    }
                });
            });
            ui.add_space(4.0);
        });
        egui::SidePanel::left("settings_nav")
            .resizable(false)
            .exact_width(160.0)
            .show(ctx, |ui| {
                ui.add_space(12.0);
                ui.set_opacity(fade);
                for (i, name) in SETTINGS_TABS.iter().enumerate() {
                    if nav_row(ui, self.settings_tab == i, name, false).clicked()
                        && self.settings_tab != i
                    {
                        self.settings_tab = i;
                        self.settings_fade = Some(std::time::Instant::now());
                    }
                }
            });
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.set_opacity(fade);
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.add_space(8.0);
                    match self.settings_tab {
                        0 => self.settings_page_hotkeys(ui),
                        1 => self.settings_page_input(ui),
                        2 => self.settings_page_window(ui),
                        3 => self.settings_page_ball(ui),
                        _ => self.settings_page_maint(ui),
                    }
                });
        });
    }

    /// 快捷键编辑（内嵌，不再单开二级窗口）。每行一个功能：勾选 / 名称 / 修饰键 / 按键 / 录入。
    fn hotkey_editor_inline(&mut self, ui: &mut egui::Ui) {
        // 冲突检测：同一个组合出现在多个槽位 → 标红
        let displays: Vec<String> = HotkeySlot::ALL
            .iter()
            .map(|slot| hotkey_display(&slot.get(&self.hotkey_draft)))
            .collect();
        let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        for d in &displays {
            if !d.is_empty() {
                *counts.entry(d.as_str()).or_insert(0) += 1;
            }
        }
        let conflicted: Vec<bool> = displays
            .iter()
            .map(|d| !d.is_empty() && counts.get(d.as_str()).copied().unwrap_or(0) > 1)
            .collect();

        // 表头
        ui.horizontal(|ui| {
            ui.add_space(20.0);
            ui.add_sized(
                [200.0, 18.0],
                egui::Label::new(egui::RichText::new("操作").small().weak()),
            );
            ui.label(egui::RichText::new("按键").small().weak());
        });
        ui.add_space(2.0);
        ui.separator();

        for (i, slot) in HotkeySlot::ALL.iter().enumerate() {
            let enabled = slot.enabled(&self.hotkey_draft);
            let display = displays[i].clone();
            let capturing = self.hotkey_capture == Some(slot.capture_target());
            let dim = display.is_empty();
            let conflict = conflicted[i];
            let mut clear_now = false;
            let mut start_capture = false;
            let mut cancel_capture = false;
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
                if slot.optional() {
                    let mut on = enabled;
                    if ui.checkbox(&mut on, "").changed() {
                        slot.set_enabled(&mut self.hotkey_draft, on);
                    }
                } else {
                    ui.add_space(18.0);
                }
                ui.add_sized(
                    [200.0, 24.0],
                    egui::Label::new(egui::RichText::new(slot.label()).size(13.0)),
                );
                let cell_text = if capturing {
                    "按下按键…"
                } else if dim {
                    "未绑定"
                } else {
                    display.as_str()
                };
                let (clicked, right_clicked) =
                    hotkey_cell(ui, cell_text, capturing, conflict, dim && !capturing);
                if clicked {
                    if capturing {
                        cancel_capture = true;
                    } else {
                        start_capture = true;
                    }
                }
                if right_clicked && !dim {
                    clear_now = true;
                }
                if !dim {
                    if ui
                        .add(egui::Button::new(egui::RichText::new("清除").size(12.0)).frame(false))
                        .on_hover_text("右键点按格也能清除")
                        .clicked()
                    {
                        clear_now = true;
                    }
                }
            });
            if start_capture {
                self.hotkey_capture = Some(slot.capture_target());
                self.hotkey_capture_at = Some(std::time::Instant::now());
                self.hotkey_message = Some("按下按键或鼠标键，Esc 取消".into());
            }
            if cancel_capture {
                self.hotkey_capture = None;
                self.hotkey_message = Some("已取消".into());
            }
            if clear_now {
                if slot.optional() {
                    slot.set_enabled(&mut self.hotkey_draft, false);
                } else {
                    let mut s = slot.get(&self.hotkey_draft);
                    s.key.clear();
                    s.modifiers.clear();
                    slot.set(&mut self.hotkey_draft, s);
                }
                self.hotkey_message = Some(format!("已清除「{}」的按键", slot.label()));
            }
            ui.add_space(2.0);
        }

        // 只在有冲突时提示（不显示"一切正常"这种废话）
        let conflicts = displays_conflict_text(&self.hotkey_draft);
        if !conflicts.is_empty() {
            ui.add_space(6.0);
            ui.label(
                egui::RichText::new(format!("快捷键冲突：{conflicts}"))
                    .small()
                    .color(ERROR),
            );
        }
        if let Some(msg) = self.hotkey_message.as_ref() {
            ui.add_space(4.0);
            ui.label(egui::RichText::new(msg).small().weak());
        }
    }

    fn settings_page_input(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("输入").size(18.0).color(TEXT_PRIMARY));
        ui.add_space(6.0);
        if ui
            .button(format!("输入模式：{}（点击切换）", input_mode_label()))
            .clicked()
        {
            cycle_input_mode();
        }
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new("WinTab 驱动（Wacom/Huion 默认模式）的笔建议用「强制笔」。")
                .small()
                .weak(),
        );
        ui.add_space(14.0);
        ui.separator();
        ui.add_space(6.0);
        ui.label(egui::RichText::new("输入设备").size(15.0).color(TEXT_PRIMARY));
        ui.label(
            egui::RichText::new(
                "选好设备后，自动模式会按设备判断这一次是笔还是鼠标，\n适合驱动把数位板笔上报成普通鼠标的情况。",
            )
            .small()
            .weak(),
        );
        ui.add_space(8.0);

        let devs = input_devices_cached();
        let mice: Vec<String> = devs
            .iter()
            .filter(|d| d.kind == 0)
            .map(|d| d.path.clone())
            .collect();
        let pens: Vec<String> = devs
            .iter()
            .filter(|d| d.kind == 1)
            .map(|d| d.path.clone())
            .collect();
        let tablets: Vec<String> = devs
            .iter()
            .filter(|d| d.kind == 1 || d.kind == 2)
            .map(|d| d.path.clone())
            .collect();
        let label_of = |list: &[String]| -> Vec<(String, String)> {
            list.iter()
                .map(|p| {
                    let l = devs
                        .iter()
                        .find(|d| &d.path == p)
                        .map(|d| d.label.clone())
                        .unwrap_or_else(|| p.rsplit('#').next().unwrap_or(p).to_string());
                    (p.clone(), l)
                })
                .collect()
        };
        // 三个下拉：鼠标 / 笔 / 数位板
        for (title, items, cur_static, key) in [
            ("鼠标设备", label_of(&mice), 0u8, "mouse"),
            ("笔设备", label_of(&pens), 1u8, "pen"),
            ("数位板", label_of(&tablets), 2u8, "tablet"),
        ] {
            let _ = cur_static;
            let cur = match key {
                "mouse" => self.ui_state.input_mouse.clone(),
                "pen" => self.ui_state.input_pen.clone(),
                _ => self.ui_state.input_tablet.clone(),
            };
            let cur_label = cur
                .as_ref()
                .and_then(|c| items.iter().find(|(p, _)| p == c).map(|(_, l)| l.clone()))
                .unwrap_or_else(|| "自动（全部设备）".to_string());
            ui.horizontal(|ui| {
                ui.add_sized(
                    [96.0, 22.0],
                    egui::Label::new(egui::RichText::new(title).size(13.0)),
                );
                let mut picked: Option<Option<String>> = None;
                egui::ComboBox::from_id_source(format!("dev_{key}"))
                    .width(320.0)
                    .selected_text(cur_label)
                    .show_ui(ui, |ui| {
                        if ui
                            .selectable_label(cur.is_none(), "自动（全部设备）")
                            .clicked()
                        {
                            picked = Some(None);
                        }
                        for (path, label) in &items {
                            if ui
                                .selectable_label(cur.as_deref() == Some(path.as_str()), label)
                                .on_hover_text(path)
                                .clicked()
                            {
                                picked = Some(Some(path.clone()));
                            }
                        }
                        if items.is_empty() {
                            ui.label(egui::RichText::new("（没检测到这类设备）").small().weak());
                        }
                    });
                if let Some(p) = picked {
                    match key {
                        "mouse" => {
                            self.ui_state.input_mouse = p.clone();
                            if let Ok(mut g) = DEV_MOUSE.write() {
                                *g = p;
                            }
                        }
                        "pen" => {
                            self.ui_state.input_pen = p.clone();
                            if let Ok(mut g) = DEV_PEN.write() {
                                *g = p;
                            }
                        }
                        _ => {
                            self.ui_state.input_tablet = p.clone();
                            if let Ok(mut g) = DEV_TABLET.write() {
                                *g = p;
                            }
                        }
                    }
                    save_ui_state(&self.ui_state);
                }
            });
            ui.add_space(4.0);
        }
        ui.add_space(6.0);
        let dev = match LAST_INPUT_IS_PEN.load(Ordering::Relaxed) {
            1 => "笔",
            0 => "鼠标",
            _ => "—",
        };
        ui.label(
            egui::RichText::new(format!("最近一次输入：{dev}　·　共检测到 {} 个设备", devs.len()))
                .small()
                .weak(),
        );
    }

    fn settings_page_window(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("窗口与预览").size(18.0).color(TEXT_PRIMARY));
        ui.add_space(6.0);
        ui.label(egui::RichText::new("预览帧率").strong().color(TEXT_SECONDARY));
        let mut fps = self.preview_fps as f32;
        let resp = ui.add(
            egui::Slider::new(&mut fps, 0.0..=60.0)
                .step_by(1.0)
                .suffix(" fps")
                .show_value(true),
        );
        if resp.changed() {
            self.preview_fps = fps as u32;
            self.ui_state.preview_fps = Some(self.preview_fps);
            save_ui_state(&self.ui_state);
        }
        ui.label(
            egui::RichText::new(
                "0 = 关闭预览（只显示占位）；前台窗口按此帧率，其它窗口为 1/4。CPU 占用的大头就是这里。",
            )
            .small()
            .weak(),
        );
        ui.add_space(12.0);
        ui.label(egui::RichText::new("小窗视图").strong().color(TEXT_SECONDARY));
        let mut view = self.view_mode;
        ui.horizontal(|ui| {
            if ui.radio_value(&mut view, 0u8, "紧凑列表").changed() {
                self.view_mode = view;
                self.ui_state.view_mode = Some(view);
                save_ui_state(&self.ui_state);
            }
            if ui.radio_value(&mut view, 1u8, "缩略图网格").changed() {
                self.view_mode = view;
                self.ui_state.view_mode = Some(view);
                save_ui_state(&self.ui_state);
            }
        });
        ui.add_space(8.0);
        let mut mini = self.mini_mode;
        if ui
            .checkbox(&mut mini, "实时小窗（只显示缩略图网格，可随时切回大界面）")
            .changed()
        {
            self.mini_mode = mini;
        }
    }

    fn settings_page_ball(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("悬浮球与纸娃娃").size(18.0).color(TEXT_PRIMARY));
        ui.add_space(6.0);
        let mut choose_skin = self.ui_state.skin.clone();
        let skins = list_skins();
        egui::ComboBox::from_label("皮肤")
            .selected_text(choose_skin.clone().unwrap_or_else(|| "skin.png".into()))
            .show_ui(ui, |ui| {
                for name in &skins {
                    let selected = choose_skin.as_deref() == Some(name.as_str());
                    if ui.selectable_label(selected, name).clicked() {
                        choose_skin = Some(name.clone());
                    }
                }
            });
        let forced = ui_doll_model(&self.ui_state);
        let mut pick: Option<doll::ModelKind> = forced;
        let auto_label = match BALL_SKIN
            .read()
            .ok()
            .and_then(|g| g.as_ref().map(|s| s.model))
        {
            Some(m) => format!("自动（{}）", m.label()),
            None => "自动（按皮肤）".to_string(),
        };
        egui::ComboBox::from_label("纸娃娃模型")
            .selected_text(match pick {
                Some(m) => m.label(),
                None => auto_label.as_str(),
            })
            .show_ui(ui, |ui| {
                if ui
                    .selectable_label(pick.is_none(), "自动（按皮肤判定）")
                    .clicked()
                {
                    pick = None;
                }
                for m in [doll::ModelKind::Classic, doll::ModelKind::Slim] {
                    if ui.selectable_label(pick == Some(m), m.label()).clicked() {
                        pick = Some(m);
                    }
                }
            });
        let model_changed = pick != forced;
        if model_changed {
            self.ui_state.doll_model = pick.map(|m| m.as_str().to_string());
        }
        if choose_skin != self.ui_state.skin || model_changed {
            self.ui_state.skin = choose_skin.clone();
            save_ui_state(&self.ui_state);
            load_ball_skin(choose_skin.as_deref(), ui_doll_model(&self.ui_state));
            unsafe { ball_render() };
        }
        ui.add_space(8.0);
        let mut bsc = f32::from_bits(BALL_SCALE.load(Ordering::Relaxed));
        if bsc <= 0.1 {
            bsc = 2.1;
        }
        if ui
            .add(egui::Slider::new(&mut bsc, 1.0..=4.0).text("悬浮球大小"))
            .changed()
        {
            BALL_SCALE.store(bsc.to_bits(), Ordering::Relaxed);
            self.ui_state.ball_scale = Some(bsc);
            save_ui_state(&self.ui_state);
            unsafe { ball_render() };
        }
        ui.add_space(12.0);
        ui.label(egui::RichText::new("待机动画").strong().color(TEXT_SECONDARY));
        let mut fl = BALL_FLOAT.load(Ordering::Relaxed);
        if ui.checkbox(&mut fl, "上下浮动").changed() {
            BALL_FLOAT.store(fl, Ordering::Relaxed);
            self.ui_state.idle_float = Some(fl);
            save_ui_state(&self.ui_state);
            unsafe { ball_render() };
        }
        let mut sw = BALL_SWAY.load(Ordering::Relaxed);
        if ui.checkbox(&mut sw, "手臂轻摆").changed() {
            BALL_SWAY.store(sw, Ordering::Relaxed);
            self.ui_state.idle_sway = Some(sw);
            save_ui_state(&self.ui_state);
            unsafe { ball_render() };
        }
        ui.label(
            egui::RichText::new("两个都关掉后，悬浮球静置时完全不重绘（几乎不占 CPU），只在鼠标动的时候转。")
                .small()
                .weak(),
        );
        ui.add_space(12.0);
        ui.label(egui::RichText::new("显示与交互").size(15.0).color(TEXT_PRIMARY));
        let mut shown = BALL_SHOWN.load(Ordering::Relaxed);
        if ui
            .checkbox(&mut shown, "显示悬浮球")
            .on_hover_text("关掉后托盘图标右键可以重新唤出")
            .changed()
        {
            self.ui_state.ball_visible = Some(shown);
            save_ui_state(&self.ui_state);
            unsafe { set_ball_visible(shown) };
        }
        let mut dbl = DOLL_DBLCLICK.load(Ordering::Relaxed);
        if ui
            .checkbox(&mut dbl, "双击纸娃娃打开主界面")
            .changed()
        {
            DOLL_DBLCLICK.store(dbl, Ordering::Relaxed);
            self.ui_state.doll_dblclick = Some(dbl);
            save_ui_state(&self.ui_state);
        }
    }

    fn settings_page_maint(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("维护").size(18.0).color(TEXT_PRIMARY));
        ui.add_space(6.0);
        if ui.button("运行自检").clicked() {
            let _ = selftest::run_hosted();
            self.selftest_text = selftest::last_report();
            self.selftest_open = true;
        }
        ui.add_space(8.0);
        let busy = UPDATE_RUNNING.load(Ordering::Relaxed);
        if ui
            .add_enabled(
                !busy,
                egui::Button::new(if busy { "检查中…" } else { "检查更新" }),
            )
            .clicked()
        {
            thread::spawn(|| {
                let _ = run_self_update();
            });
        }
        ui.add_space(8.0);
        let mut auto = autostart_enabled();
        if ui
            .checkbox(&mut auto, "开机自启动（登录 Windows 后自动启动 TransGlass）")
            .changed()
        {
            if let Err(e) = set_autostart(auto) {
                push_issue(format!("设置开机自启动失败：{e}"));
            }
        }
        ui.add_space(8.0);
        if ui.button("打开配置目录").clicked() {
            if let Some(dir) = get_config_path().parent() {
                let _ = std::process::Command::new("explorer")
                    .arg(dir.as_os_str())
                    .spawn();
            }
        }
        ui.add_space(12.0);
        ui.separator();
        ui.label(
            egui::RichText::new(format!(
                "TransGlass v{}　·　配置文件与皮肤/物品贴图都在程序目录",
                env!("CARGO_PKG_VERSION")
            ))
            .small()
            .weak(),
        );
    }
}
