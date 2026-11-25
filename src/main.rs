#![windows_subsystem = "windows"]

use dashmap::DashMap;
use eframe::egui;
use lazy_static::lazy_static;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use self_update::backends::github::Update;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicIsize, AtomicU32, Ordering};
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
use windows::Win32::UI::Input::{GetCurrentInputMessageSource, INPUT_MESSAGE_SOURCE, IMDT_PEN};
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

fn request_app_exit() {
    APP_EXIT_REQUESTED.store(true, Ordering::SeqCst);

    // 兜底：窗口隐藏时 egui 不再重绘，只靠 WM_CLOSE 可能永远走不到退出流程，
    // 进程会一直挂在后台，用户再从托盘启动时就容易看到"图标越来越多"。
    // 正常情况下 eframe 会关窗退出、main 返回时 TrayIcon 被 drop（图标随之删除）；
    // 这里只兜底进程残留。
    thread::spawn(|| {
        thread::sleep(std::time::Duration::from_millis(1500));
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

unsafe fn is_pen_input(info: &MSLLHOOKSTRUCT) -> bool {
    let mut src = INPUT_MESSAGE_SOURCE::default();
    if GetCurrentInputMessageSource(&mut src).is_ok() {
        return src.deviceType == IMDT_PEN;
    }
    (info.dwExtraInfo & 0xFFFFFF00) == 0xFF515700
}

unsafe extern "system" fn mouse_hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
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
            if GLOBAL_REGISTRY.is_empty() {
                return CallNextHookEx(
                    HHOOK(MOUSE_HOOK.load(Ordering::SeqCst) as *mut _),
                    code,
                    wparam,
                    lparam,
                );
            }

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

            let root = GetAncestor(hit, GA_ROOT);
            if root.0.is_null() {
                return CallNextHookEx(
                    HHOOK(MOUSE_HOOK.load(Ordering::SeqCst) as *mut _),
                    code,
                    wparam,
                    lparam,
                );
            }

            let root_val = root.0 as isize;
            if let Some(state) = GLOBAL_REGISTRY.get(&root_val) {
                if state.mouse_passthrough != state.pen_passthrough {
                    let is_pen = match get_input_mode() {
                        InputMode::Auto => is_pen_input(&info),
                        InputMode::ForceMouse => false,
                        InputMode::ForcePen => true,
                    };
                    let passthrough = if is_pen {
                        state.pen_passthrough
                    } else {
                        state.mouse_passthrough
                    };
                    set_hwnd_mouse_transparent(root, passthrough);
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
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = egui::Color32::from_rgb(18, 18, 18);
        visuals.widgets.noninteractive.bg_fill = egui::Color32::from_rgb(24, 24, 24);
        visuals.widgets.inactive.bg_fill = egui::Color32::from_rgb(32, 32, 32);
        visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(45, 45, 45);
        visuals.widgets.active.bg_fill = egui::Color32::from_rgb(55, 55, 55);
        visuals.selection.bg_fill = egui::Color32::from_rgb(0, 150, 255);
        visuals.window_rounding = egui::Rounding::same(8.0);
        cc.egui_ctx.set_visuals(visuals);
        let _ = EGUI_CTX.set(cc.egui_ctx.clone());

        if let Ok(handle) = cc.window_handle() {
            if let RawWindowHandle::Win32(h) = handle.as_raw() {
                ROOT_HWND.store(h.hwnd.get(), Ordering::Relaxed);
            }
        }

        Self {
            should_exit: false,
            hotkey_editor_open: false,
            hotkey_draft: load_or_create_hotkey_config(),
            hotkey_message: None,
            hotkey_capture: None,
            hotkey_priority_ui: current_priority_flag(),
        }
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
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                ui.heading(egui::RichText::new("TransGlass").color(egui::Color32::from_rgb(0, 150, 255)).strong().size(22.0));
                ui.label(egui::RichText::new(format!("输入模式：{}", input_mode_label())).small().weak());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button(" 🗙 隐藏 ").clicked() {
                        hide_root_window();
                    }
                    ui.menu_button(" ☰ ", |ui| {
                        ui.menu_button("设置", |ui| {
                            if ui.button("快捷键").clicked() {
                                self.open_hotkey_editor();
                                ui.close_menu();
                            }
                        });
                    });
                });
            });
            ui.add_space(10.0);
            ui.separator();
            ui.add_space(10.0);

            // 配置/热键问题提示（以前这些错误是静默吞掉的）
            let issues = snapshot_issues();
            if !issues.is_empty() {
                egui::Frame::none()
                    .fill(egui::Color32::from_rgb(60, 40, 10))
                    .inner_margin(egui::Margin::same(6.0))
                    .rounding(4.0)
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new(format!("⚠ 有 {} 条配置/热键问题", issues.len()))
                                .color(egui::Color32::from_rgb(255, 190, 80))
                                .strong()
                                .small(),
                        );
                        for msg in issues.iter().take(4) {
                            ui.label(
                                egui::RichText::new(format!("• {msg}"))
                                    .color(egui::Color32::from_rgb(255, 200, 120))
                                    .small(),
                            );
                        }
                        if issues.len() > 4 {
                            ui.label(
                                egui::RichText::new(format!("…还有 {} 条，详见 --self-test 报告", issues.len() - 4))
                                    .small()
                                    .weak(),
                            );
                        }
                    });
                ui.add_space(8.0);
            }

            // 热键优先开关：开启后用低级键盘钩子强占组合键
            let mut priority = self.hotkey_priority_ui;
            let resp = ui
                .checkbox(&mut priority, "快捷键优先（强占组合键）")
                .on_hover_text(
                    "开启：用低级键盘钩子抢在其它程序前面拦截已配置的组合键，\n\
                     即使这个键被别的软件占用，TransGlass 也能收到。\n\
                     关闭：使用系统热键注册，先到先得——被占用就没有反应。",
                );
            if resp.changed() {
                self.hotkey_priority_ui = priority;
                match save_priority_flag(priority) {
                    Ok(()) => {
                        self.hotkey_message = Some(if priority {
                            "已开启快捷键优先（强占组合键）".to_string()
                        } else {
                            "已关闭快捷键优先，改用系统热键注册".to_string()
                        })
                    }
                    Err(e) => push_issue(format!("保存快捷键优先设置失败: {e}")),
                }
            }
            ui.add_space(6.0);

            // 正在管理的窗口
            ui.label(egui::RichText::new("已调节的窗口").strong().color(egui::Color32::LIGHT_GRAY));
            ui.add_space(5.0);

            egui::ScrollArea::vertical()
                .max_height(250.0)
                .auto_shrink([false; 2])
                .show(ui, |ui| {
                    if GLOBAL_REGISTRY.is_empty() {
                        ui.vertical_centered(|ui| {
                            ui.add_space(60.0);
                            ui.label(egui::RichText::new("暂无管理记录\n使用热键开始管理").weak());
                        });
                    }

                    let entries: Vec<(isize, WindowState)> = GLOBAL_REGISTRY
                        .iter()
                        .map(|kv| (*kv.key(), kv.value().clone()))
                        .collect();

                    let mut to_restore: Vec<isize> = Vec::new();
                    let mut changes: Vec<PendingChange> = Vec::new();

                    for (hwnd_val, state) in entries {
                        if unsafe { is_own_hwnd(HWND(hwnd_val as *mut _)) } {
                            continue;
                        }
                        if unsafe { is_shell_hwnd(HWND(hwnd_val as *mut _)) } {
                            to_restore.push(hwnd_val);
                            continue;
                        }

                        ui.add_space(4.0);
                        ui.group(|ui| {
                            ui.vertical(|ui| {
                                ui.horizontal(|ui| {
                                    ui.add(egui::Label::new(egui::RichText::new(&state.title).strong().size(14.0)).truncate());
                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        if ui.button("还原").clicked() {
                                            to_restore.push(hwnd_val);
                                        }
                                    });
                                });

                                ui.add_space(6.0);
                                ui.horizontal(|ui| {
                                    ui.label("透明度:");
                                    let mut alpha_f32 = state.current_alpha as f32;
                                    let slider = egui::Slider::new(&mut alpha_f32, 30.0..=255.0)
                                        .show_value(false)
                                        .trailing_fill(true);
                                    if ui.add(slider).changed() {
                                        changes.push(PendingChange {
                                            hwnd_val,
                                            alpha: Some(alpha_f32 as u8),
                                            topmost: None,
                                            mouse_passthrough: None,
                                            pen_passthrough: None,
                                        });
                                    }
                                    ui.add_space(10.0);
                                    let mut topmost = state.user_pref_topmost;
                                    if ui.checkbox(&mut topmost, "置顶").changed() {
                                        changes.push(PendingChange {
                                            hwnd_val,
                                            alpha: None,
                                            topmost: Some(topmost),
                                            mouse_passthrough: None,
                                            pen_passthrough: None,
                                        });
                                    }
                                    ui.add_space(10.0);
                                    let mut mouse_passthrough = state.mouse_passthrough;
                                    if ui.checkbox(&mut mouse_passthrough, "鼠标点透").changed() {
                                        changes.push(PendingChange {
                                            hwnd_val,
                                            alpha: None,
                                            topmost: None,
                                            mouse_passthrough: Some(mouse_passthrough),
                                            pen_passthrough: None,
                                        });
                                    }
                                    ui.add_space(10.0);
                                    let mut pen_passthrough = state.pen_passthrough;
                                    if ui.checkbox(&mut pen_passthrough, "笔点透").changed() {
                                        changes.push(PendingChange {
                                            hwnd_val,
                                            alpha: None,
                                            topmost: None,
                                            mouse_passthrough: None,
                                            pen_passthrough: Some(pen_passthrough),
                                        });
                                    }
                                });
                            });
                        });
                    }

                    for hwnd_val in to_restore {
                        unsafe { restore_window(HWND(hwnd_val as *mut _)); }
                    }

                    for c in changes {
                        let mut apply_alpha: Option<u8> = None;
                        let mut apply_top: Option<bool> = None;
                        let mut apply_mouse: Option<bool> = None;
                        let mut apply_pen: Option<bool> = None;
                        if let Some(mut state) = GLOBAL_REGISTRY.get_mut(&c.hwnd_val) {
                            if let Some(a) = c.alpha {
                                state.current_alpha = a;
                            }
                            if let Some(t) = c.topmost {
                                state.user_pref_topmost = t;
                            }
                            if let Some(m) = c.mouse_passthrough {
                                state.mouse_passthrough = m;
                            }
                            if let Some(p) = c.pen_passthrough {
                                state.pen_passthrough = p;
                            }
                            apply_alpha = Some(state.current_alpha);
                            apply_top = Some(state.user_pref_topmost);
                            apply_mouse = Some(state.mouse_passthrough);
                            apply_pen = Some(state.pen_passthrough);
                        }
                        if let (Some(a), Some(t), Some(m), Some(p)) = (apply_alpha, apply_top, apply_mouse, apply_pen) {
                            unsafe {
                                let _ = apply_transparency_to_hwnd(HWND(c.hwnd_val as *mut _), a, t, m, p);
                            }
                        }
                    }
                });

            ui.add_space(15.0);
            ui.separator();
            ui.add_space(10.0);

            // 底部控制
            ui.horizontal(|ui| {
                if ui.button("♻ 全部还原").clicked() {
                    unsafe { restore_all_windows(); }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("🚀 检查更新").clicked() {
                        thread::spawn(|| { let _ = run_self_update(); });
                    }
                });
            });

            ui.add_space(12.0);
            ui.group(|ui| {
                ui.vertical_centered(|ui| {
                    ui.label(egui::RichText::new("快捷键提示").strong().size(12.0));
                    ui.label(egui::RichText::new("快捷键可在  ☰  → 设置 → 快捷键  中修改").small().weak());
                });
            });
        });

        if self.hotkey_editor_open {
            let defaults = default_config();
            let mut open = self.hotkey_editor_open;
            egui::Window::new("快捷键设置")
                .open(&mut open)
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    let path = get_config_path();
                    ui.label(format!("配置文件：{}", path.display()));
                    ui.add_space(8.0);

                    egui::Grid::new("hotkey_editor_grid")
                        .striped(true)
                        .show(ui, |ui| {
                            ui.label("");
                            ui.label("修饰键");
                            ui.label("按键");
                            ui.label("");
                            ui.end_row();

                            ui.label("增加透明度");
                            ui.text_edit_singleline(&mut self.hotkey_draft.increase.modifiers);
                            ui.text_edit_singleline(&mut self.hotkey_draft.increase.key);
                            if ui.button("录入").clicked() {
                                self.hotkey_capture = Some(HotkeyCaptureTarget::Increase);
                                self.hotkey_message = Some("请按下组合键".into());
                            }
                            ui.end_row();

                            ui.label("减少透明度");
                            ui.text_edit_singleline(&mut self.hotkey_draft.decrease.modifiers);
                            ui.text_edit_singleline(&mut self.hotkey_draft.decrease.key);
                            if ui.button("录入").clicked() {
                                self.hotkey_capture = Some(HotkeyCaptureTarget::Decrease);
                                self.hotkey_message = Some("请按下组合键".into());
                            }
                            ui.end_row();

                            ui.label("切换置顶");
                            ui.text_edit_singleline(&mut self.hotkey_draft.toggle_top.modifiers);
                            ui.text_edit_singleline(&mut self.hotkey_draft.toggle_top.key);
                            if ui.button("录入").clicked() {
                                self.hotkey_capture = Some(HotkeyCaptureTarget::ToggleTop);
                                self.hotkey_message = Some("请按下组合键".into());
                            }
                            ui.end_row();

                            let mut enable_mouse = self.hotkey_draft.toggle_click_through.is_some();
                            ui.horizontal(|ui| {
                                ui.checkbox(&mut enable_mouse, "鼠标点透");
                            });
                            if enable_mouse && self.hotkey_draft.toggle_click_through.is_none() {
                                self.hotkey_draft.toggle_click_through = defaults.toggle_click_through.clone();
                            }
                            if !enable_mouse {
                                self.hotkey_draft.toggle_click_through = None;
                            }
                            let mut mouse_mods = self
                                .hotkey_draft
                                .toggle_click_through
                                .clone()
                                .unwrap_or_else(|| defaults.toggle_click_through.clone().unwrap())
                                .modifiers;
                            let mut mouse_key = self
                                .hotkey_draft
                                .toggle_click_through
                                .clone()
                                .unwrap_or_else(|| defaults.toggle_click_through.clone().unwrap())
                                .key;
                            ui.add_enabled(enable_mouse, egui::TextEdit::singleline(&mut mouse_mods));
                            ui.add_enabled(enable_mouse, egui::TextEdit::singleline(&mut mouse_key));
                            if ui.add_enabled(enable_mouse, egui::Button::new("录入")).clicked() {
                                self.hotkey_capture = Some(HotkeyCaptureTarget::ToggleMouse);
                                self.hotkey_message = Some("请按下组合键".into());
                            }
                            if enable_mouse {
                                self.hotkey_draft.toggle_click_through = Some(HotkeySpec {
                                    modifiers: mouse_mods,
                                    key: mouse_key,
                                });
                            }
                            ui.end_row();

                            let mut enable_pen = self.hotkey_draft.toggle_pen_passthrough.is_some();
                            ui.horizontal(|ui| {
                                ui.checkbox(&mut enable_pen, "笔点透");
                            });
                            if enable_pen && self.hotkey_draft.toggle_pen_passthrough.is_none() {
                                self.hotkey_draft.toggle_pen_passthrough = defaults.toggle_pen_passthrough.clone();
                            }
                            if !enable_pen {
                                self.hotkey_draft.toggle_pen_passthrough = None;
                            }
                            let mut pen_mods = self
                                .hotkey_draft
                                .toggle_pen_passthrough
                                .clone()
                                .unwrap_or_else(|| defaults.toggle_pen_passthrough.clone().unwrap())
                                .modifiers;
                            let mut pen_key = self
                                .hotkey_draft
                                .toggle_pen_passthrough
                                .clone()
                                .unwrap_or_else(|| defaults.toggle_pen_passthrough.clone().unwrap())
                                .key;
                            ui.add_enabled(enable_pen, egui::TextEdit::singleline(&mut pen_mods));
                            ui.add_enabled(enable_pen, egui::TextEdit::singleline(&mut pen_key));
                            if ui.add_enabled(enable_pen, egui::Button::new("录入")).clicked() {
                                self.hotkey_capture = Some(HotkeyCaptureTarget::TogglePen);
                                self.hotkey_message = Some("请按下组合键".into());
                            }
                            if enable_pen {
                                self.hotkey_draft.toggle_pen_passthrough = Some(HotkeySpec {
                                    modifiers: pen_mods,
                                    key: pen_key,
                                });
                            }
                            ui.end_row();

                            let mut enable_mode = self.hotkey_draft.input_mode_cycle.is_some();
                            ui.horizontal(|ui| {
                                ui.checkbox(&mut enable_mode, "输入模式切换");
                            });
                            if enable_mode && self.hotkey_draft.input_mode_cycle.is_none() {
                                self.hotkey_draft.input_mode_cycle = defaults.input_mode_cycle.clone();
                            }
                            if !enable_mode {
                                self.hotkey_draft.input_mode_cycle = None;
                            }
                            let mut mode_mods = self
                                .hotkey_draft
                                .input_mode_cycle
                                .clone()
                                .unwrap_or_else(|| defaults.input_mode_cycle.clone().unwrap())
                                .modifiers;
                            let mut mode_key = self
                                .hotkey_draft
                                .input_mode_cycle
                                .clone()
                                .unwrap_or_else(|| defaults.input_mode_cycle.clone().unwrap())
                                .key;
                            ui.add_enabled(enable_mode, egui::TextEdit::singleline(&mut mode_mods));
                            ui.add_enabled(enable_mode, egui::TextEdit::singleline(&mut mode_key));
                            if ui.add_enabled(enable_mode, egui::Button::new("录入")).clicked() {
                                self.hotkey_capture = Some(HotkeyCaptureTarget::CycleInputMode);
                                self.hotkey_message = Some("请按下组合键".into());
                            }
                            if enable_mode {
                                self.hotkey_draft.input_mode_cycle = Some(HotkeySpec {
                                    modifiers: mode_mods,
                                    key: mode_key,
                                });
                            }
                            ui.end_row();

                            ui.label("还原当前窗口");
                            ui.text_edit_singleline(&mut self.hotkey_draft.reset_current.modifiers);
                            ui.text_edit_singleline(&mut self.hotkey_draft.reset_current.key);
                            if ui.button("录入").clicked() {
                                self.hotkey_capture = Some(HotkeyCaptureTarget::ResetCurrent);
                                self.hotkey_message = Some("请按下组合键".into());
                            }
                            ui.end_row();

                            ui.label("还原所有窗口");
                            ui.text_edit_singleline(&mut self.hotkey_draft.reset_all.modifiers);
                            ui.text_edit_singleline(&mut self.hotkey_draft.reset_all.key);
                            if ui.button("录入").clicked() {
                                self.hotkey_capture = Some(HotkeyCaptureTarget::ResetAll);
                                self.hotkey_message = Some("请按下组合键".into());
                            }
                            ui.end_row();

                            ui.label("检查更新");
                            ui.text_edit_singleline(&mut self.hotkey_draft.update.modifiers);
                            ui.text_edit_singleline(&mut self.hotkey_draft.update.key);
                            if ui.button("录入").clicked() {
                                self.hotkey_capture = Some(HotkeyCaptureTarget::Update);
                                self.hotkey_message = Some("请按下组合键".into());
                            }
                            ui.end_row();

                            let mut enable_reload = self.hotkey_draft.reload.is_some();
                            ui.horizontal(|ui| {
                                ui.checkbox(&mut enable_reload, "重载配置");
                            });
                            if enable_reload && self.hotkey_draft.reload.is_none() {
                                self.hotkey_draft.reload = defaults.reload.clone();
                            }
                            if !enable_reload {
                                self.hotkey_draft.reload = None;
                            }
                            let mut reload_mods = self
                                .hotkey_draft
                                .reload
                                .clone()
                                .unwrap_or_else(|| defaults.reload.clone().unwrap())
                                .modifiers;
                            let mut reload_key = self
                                .hotkey_draft
                                .reload
                                .clone()
                                .unwrap_or_else(|| defaults.reload.clone().unwrap())
                                .key;
                            ui.add_enabled(enable_reload, egui::TextEdit::singleline(&mut reload_mods));
                            ui.add_enabled(enable_reload, egui::TextEdit::singleline(&mut reload_key));
                            if ui.add_enabled(enable_reload, egui::Button::new("录入")).clicked() {
                                self.hotkey_capture = Some(HotkeyCaptureTarget::Reload);
                                self.hotkey_message = Some("请按下组合键".into());
                            }
                            if enable_reload {
                                self.hotkey_draft.reload = Some(HotkeySpec {
                                    modifiers: reload_mods,
                                    key: reload_key,
                                });
                            }
                            ui.end_row();
                        });

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
    spawn_quit_listener();

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

    let _tray_icon = create_tray_icon();

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

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([320.0, 450.0])
            .with_title("TransGlass 控制面板")
            .with_visible(true),
        run_and_return: false,
        ..Default::default()
    };

    eframe::run_native(
        "TransGlass 控制面板",
        options,
        Box::new(|cc| Ok(Box::new(TransGlassApp::new(cc)))),
    )
}

fn run_self_update() -> Result<(), Box<dyn std::error::Error>> {
    let current = env!("CARGO_PKG_VERSION");
    let _ = Update::configure()
        .repo_owner("railgun-1145")
        .repo_name("TransGlass")
        .bin_name("transglass")
        .show_download_progress(true)
        .current_version(current)
        .build()?
        .update()?;
    Ok(())
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
        increase: HotkeySpec {
            modifiers: "ALT".into(),
            key: "Z".into(),
        },
        decrease: HotkeySpec {
            modifiers: "ALT".into(),
            key: "X".into(),
        },
        toggle_top: HotkeySpec {
            modifiers: "ALT".into(),
            key: "T".into(),
        },
        toggle_click_through: Some(HotkeySpec {
            modifiers: "ALT".into(),
            key: "P".into(),
        }),
        toggle_pen_passthrough: Some(HotkeySpec {
            modifiers: "ALT+SHIFT".into(),
            key: "P".into(),
        }),
        input_mode_cycle: Some(HotkeySpec {
            modifiers: "ALT+SHIFT".into(),
            key: "M".into(),
        }),
        reset_current: HotkeySpec {
            modifiers: "ALT".into(),
            key: "R".into(),
        },
        reset_all: HotkeySpec {
            modifiers: "ALT+SHIFT".into(),
            key: "R".into(),
        },
        update: HotkeySpec {
            modifiers: "ALT".into(),
            key: "U".into(),
        },
        reload: Some(HotkeySpec {
            modifiers: "ALT+SHIFT".into(),
            key: "C".into(),
        }),
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

pub(crate) fn push_issue(msg: impl Into<String>) {
    let msg = msg.into();
    if let Ok(mut issues) = RUNTIME_ISSUES.write() {
        if !issues.iter().any(|m| m == &msg) {
            issues.push(msg);
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
        issues.clear();
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
    let toggle_click = cfg.toggle_click_through.clone().unwrap_or(HotkeySpec {
        modifiers: "ALT".into(),
        key: "P".into(),
    });
    let toggle_pen = cfg.toggle_pen_passthrough.clone().unwrap_or(HotkeySpec {
        modifiers: "ALT+SHIFT".into(),
        key: "P".into(),
    });
    let reload = cfg.reload.clone().unwrap_or(HotkeySpec {
        modifiers: "ALT+SHIFT".into(),
        key: "C".into(),
    });

    let mut slots = vec![
        (1, "增加透明度", cfg.increase.clone()),
        (2, "减少透明度", cfg.decrease.clone()),
        (3, "切换置顶", cfg.toggle_top.clone()),
        (4, "切换鼠标点透", toggle_click),
        (5, "切换笔点透", toggle_pen),
        (6, "还原当前窗口", cfg.reset_current.clone()),
        (7, "还原全部窗口", cfg.reset_all.clone()),
        (8, "检查更新", cfg.update.clone()),
        (9, "重载热键配置", reload),
    ];
    if let Some(spec) = cfg.input_mode_cycle.as_ref() {
        slots.push((10, "切换输入模式", spec.clone()));
    }
    slots
}

unsafe fn bind_hotkeys(cfg: &HotkeyConfig) {
    for (id, name, spec) in hotkey_slots(cfg) {
        try_register_hotkey(id, &spec, name);
    }
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
