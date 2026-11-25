//! 自检 / 诊断模块（`transglass.exe --self-test [--strict]`）。
//!
//! 为什么要有这个模块：
//! 0.2.0 分支里的 `run_code_review()` 与 `run_ui_sandbox_test()` 是"安慰剂"——
//! 35 处 `run_test!` 的条件参数全部写死为 `true`，打印出来的 ✅ 不代表任何检测结果，
//! 而且它在 `main()` 里无条件执行，随后立刻 `FreeConsole()`，用户根本看不到。
//!
//! 这里保留那套"分组 + 逐项 PASS/FAIL + 汇总"的输出形态，但每一项都换成真实探测。
//!
//! 探测顺序有讲究：**先查单实例**。主程序在跑的时候，它自己占着所有热键，
//! 此时再去 `RegisterHotKey` 必然全部返回 1409，会被误报成"被别的程序占用"，
//! 所以检测到已有实例就跳过注册探测，而不是给用户一堆假结论。

use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;

use windows::Win32::Foundation::*;
use windows::Win32::Security::{
    GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
};
use windows::Win32::System::Console::{AttachConsole, SetConsoleOutputCP, ATTACH_PARENT_PROCESS};
use windows::Win32::System::Threading::*;
use windows::Win32::UI::HiDpi::GetProcessDpiAwareness;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::{get_config_path, hotkey_slots, parse_modifiers, parse_vk, HotkeyConfig, SINGLE_INSTANCE_NAME};

const CP_UTF8: u32 = 65001;
const ERROR_HOTKEY_ALREADY_REGISTERED: u32 = 1409;

struct Report {
    out: String,
    passed: usize,
    failed: usize,
    warned: usize,
}

impl Report {
    fn new() -> Self {
        Self {
            out: String::new(),
            passed: 0,
            failed: 0,
            warned: 0,
        }
    }

    fn section(&mut self, title: &str) {
        let _ = writeln!(self.out, "\n--- {title} ---");
    }

    fn pass(&mut self, name: &str, detail: impl AsRef<str>) {
        self.passed += 1;
        let _ = writeln!(self.out, "[PASS] {name}: {}", detail.as_ref());
    }

    fn fail(&mut self, name: &str, detail: impl AsRef<str>) {
        self.failed += 1;
        let _ = writeln!(self.out, "[FAIL] {name}: {}", detail.as_ref());
    }

    fn warn(&mut self, name: &str, detail: impl AsRef<str>) {
        self.warned += 1;
        let _ = writeln!(self.out, "[WARN] {name}: {}", detail.as_ref());
    }

    fn info(&mut self, name: &str, detail: impl AsRef<str>) {
        let _ = writeln!(self.out, "[INFO] {name}: {}", detail.as_ref());
    }
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    CallNextHookEx(None, code, wparam, lparam)
}

/// 在独立线程里**先全部注册、再统一注销**。
///
/// 用注册失败（1409）来暴露"两个功能抢同一个键"这种真实冲突——
/// 如果注册完一项就立刻注销，重复绑定会双双 PASS，报告自相矛盾。
fn probe_hotkey_registration(items: Vec<(i32, String, u32, u32)>) -> Vec<(i32, String, bool, u32)> {
    std::thread::spawn(move || {
        let mut results = Vec::new();
        let mut ok_ids = Vec::new();
        // 建消息队列，保证 `RegisterHotKey(None, ...)` 绑定到本线程且行为确定
        let mut msg = MSG::default();
        unsafe {
            let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
            SetLastError(ERROR_SUCCESS);
        }
        for (id, label, mods_bits, vk) in items {
            unsafe {
                let ok = RegisterHotKey(None, id, HOT_KEY_MODIFIERS(mods_bits), vk).is_ok();
                let err = if ok { 0 } else { GetLastError().0 };
                if ok {
                    ok_ids.push(id);
                }
                results.push((id, label, ok, err));
            }
        }
        for id in ok_ids {
            unsafe {
                let _ = UnregisterHotKey(None, id);
            }
        }
        results
    })
    .join()
    .unwrap_or_default()
}

enum Elevation {
    Elevated,
    Normal,
    Unknown(String),
}

fn query_elevation() -> Elevation {
    unsafe {
        let mut token = HANDLE::default();
        if let Err(e) = OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) {
            return Elevation::Unknown(format!("OpenProcessToken 失败: {e}"));
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut len = 0u32;
        let res = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut core::ffi::c_void),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        );
        let _ = CloseHandle(token);
        match res {
            Ok(()) if elevation.TokenIsElevated != 0 => Elevation::Elevated,
            Ok(()) => Elevation::Normal,
            Err(e) => Elevation::Unknown(format!("GetTokenInformation 失败: {e}")),
        }
    }
}

fn log_targets(cfg_path: &std::path::Path) -> Vec<PathBuf> {
    let mut out = vec![cfg_path.with_file_name("transglass_selftest.log")];
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        out.push(PathBuf::from(local).join("TransGlass").join("transglass_selftest.log"));
    }
    out
}

pub fn run(strict: bool) -> i32 {
    let mut rep = Report::new();

    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "<unknown>".into());
    let _ = writeln!(rep.out, "TransGlass 自检报告{}", if strict { "（严格模式）" } else { "" });
    let _ = writeln!(rep.out, "版本: v{}", env!("CARGO_PKG_VERSION"));
    let _ = writeln!(rep.out, "程序: {exe}");

    // ---------- 1. 单实例（必须最先，否则会污染热键探测） ----------
    rep.section("单实例");
    let instance_running = unsafe {
        SetLastError(ERROR_SUCCESS);
        match CreateMutexW(None, true, SINGLE_INSTANCE_NAME) {
            Ok(handle) => {
                let already = GetLastError() == ERROR_ALREADY_EXISTS;
                let _ = CloseHandle(handle);
                already
            }
            Err(e) => {
                rep.fail("互斥体", format!("创建失败: {e}"));
                false
            }
        }
    };
    if instance_running {
        rep.warn("互斥体", "已有另一个 TransGlass 在运行（热键注册探测将被跳过）");
    } else {
        rep.pass("互斥体", "当前没有其它实例占用");
    }

    // ---------- 2. 配置文件 ----------
    rep.section("配置文件");
    let cfg_path = get_config_path();
    rep.info("路径", cfg_path.display().to_string());
    let mut used_default = false;
    let mut cfg = None;
    match fs::read_to_string(&cfg_path) {
        Ok(raw) => {
            // 记事本存 UTF-8 会带 BOM，serde_json 不认，先剥掉再解析
            let text = raw.trim_start_matches('\u{feff}');
            match serde_json::from_str::<HotkeyConfig>(text) {
                Ok(c) => {
                    rep.pass("解析", format!("{} 字节，JSON 结构正确", text.len()));
                    cfg = Some(c);
                }
                Err(e) => {
                    rep.fail("解析", format!("JSON 无法解析为热键配置: {e}"));
                    if raw.starts_with('\u{feff}') {
                        rep.info("提示", "文件带 UTF-8 BOM，已自动剥除后重试");
                    }
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => rep.warn(
            "读取",
            "配置文件不存在（首次运行或刚换目录，属正常），下面用内置默认值检测",
        ),
        Err(e) => rep.fail("读取", format!("读不到配置文件: {e}")),
    }
    if cfg.is_none() {
        used_default = true;
        rep.warn("回退", "使用内置默认热键继续检测（运行主程序会重新生成该文件）");
        cfg = Some(crate::default_config());
    }
    let cfg = cfg.expect("cfg always present");

    // ---------- 3. 热键配置合法性 ----------
    rep.section("热键配置");
    let slots = hotkey_slots(&cfg);
    let mut bad_keys: Vec<String> = Vec::new();
    let mut duplicates: Vec<String> = Vec::new();
    // 去重键用 (modifiers 位, vk) —— 按字符串比会把 "SHIFT+ALT+P" 与 "ALT+SHIFT+P" 当成两个键
    let mut seen: Vec<((u32, u32), &'static str)> = Vec::new();
    let mut probe_items: Vec<(i32, String, u32, u32)> = Vec::new();

    for (id, label, spec) in &slots {
        let (mods, vk) = unsafe { (parse_modifiers(&spec.modifiers), parse_vk(&spec.key)) };
        let combo = format!(
            "{}+{}",
            spec.modifiers.trim().to_uppercase(),
            spec.key.trim().to_uppercase()
        );
        if vk == 0 {
            // 键名不合法时主程序直接跳过注册，这里也不该拿去真实注册：
            // Win32 对 vk=0 的 RegisterHotKey 会返回成功，测了也是假的
            bad_keys.push(format!("{label}({combo})"));
            continue;
        }
        if let Some((_, other)) = seen.iter().find(|(k, _)| *k == (mods.0, vk)) {
            duplicates.push(format!("{label} 与 {other} 都是 {combo}"));
        } else {
            seen.push(((mods.0, vk), *label));
        }
        probe_items.push((*id, format!("{label} ({combo})"), mods.0, vk));
    }

    if bad_keys.is_empty() {
        rep.pass("键名解析", format!("{} 个槽位全部可识别", slots.len()));
    } else {
        rep.fail("键名解析", format!("无法识别的键: {}", bad_keys.join(", ")));
    }
    if duplicates.is_empty() {
        rep.pass("重复绑定", "没有两个功能抢同一个组合键");
    } else {
        rep.fail("重复绑定", duplicates.join("; "));
    }

    // ---------- 4. 热键实际注册 ----------
    rep.section("热键注册");
    if used_default {
        rep.warn("基准", "以下注册结果基于内置默认键位，不是你配置文件里的键位");
    }
    if instance_running {
        rep.warn("跳过", "已有实例在运行，它占着全部热键，注册探测无意义");
    } else if probe_items.is_empty() {
        rep.warn("跳过", "没有键名合法的槽位可测");
    } else {
        for (id, label, ok, err) in probe_hotkey_registration(probe_items) {
            if ok {
                rep.pass(&label, format!("id={id} 注册成功（本次探测已注销）"));
            } else if err == ERROR_HOTKEY_ALREADY_REGISTERED {
                rep.fail(
                    &label,
                    format!("id={id} 组合键已被其它程序占用 (ERROR_HOTKEY_ALREADY_REGISTERED)"),
                );
            } else {
                rep.fail(&label, format!("id={id} 注册失败，Win32 错误码 {err}"));
            }
        }
    }

    // ---------- 5. 低级鼠标钩子 ----------
    rep.section("鼠标钩子");
    unsafe {
        match SetWindowsHookExW(WH_MOUSE_LL, Some(hook_proc), None, 0) {
            Ok(hook) => {
                let _ = UnhookWindowsHookEx(hook);
                rep.pass("WH_MOUSE_LL", "安装并卸载成功（点透功能可用）");
            }
            Err(e) => rep.fail(
                "WH_MOUSE_LL",
                format!("安装失败: {e}（点透会不可用，通常是安全软件拦截）"),
            ),
        }
    }

    // ---------- 6. 运行环境 ----------
    rep.section("运行环境");
    match query_elevation() {
        Elevation::Elevated => rep.warn(
            "权限",
            "以管理员身份运行：能操作管理员窗口，但低级鼠标钩子可能被 UIPI 限制",
        ),
        Elevation::Normal => {
            rep.pass("权限", "普通用户权限：无法操作以管理员运行的窗口（设计如此）")
        }
        Elevation::Unknown(e) => rep.warn("权限", format!("查询失败，无法判断: {e}")),
    }

    unsafe {
        match GetProcessDpiAwareness(None) {
            Ok(awareness) => {
                let label = match awareness.0 {
                    0 => "UNAWARE（4K 屏会糊）",
                    1 => "SYSTEM_DPI_AWARE",
                    2 => "PER_MONITOR_DPI_AWARE（推荐）",
                    _ => "未知",
                };
                if awareness.0 == 2 {
                    rep.pass("DPI 感知", label);
                } else {
                    rep.warn("DPI 感知", label);
                }
            }
            Err(e) => rep.warn("DPI 感知", format!("查询失败: {e}")),
        }

        let monitors = GetSystemMetrics(SM_CMONITORS);
        if monitors >= 1 {
            rep.info("显示器数量", monitors.to_string());
        } else {
            rep.warn("显示器数量", "查询失败（GetSystemMetrics 返回 0）");
        }
    }

    // ---------- 7. 目录写入 ----------
    rep.section("程序目录");
    let probe_path = cfg_path.with_file_name("transglass_selftest_write_probe.tmp");
    match fs::write(&probe_path, b"ok") {
        Ok(()) => {
            let _ = fs::remove_file(&probe_path);
            rep.pass("可写", format!("{} 可写", cfg_path.display()));
        }
        Err(e) => rep.warn("可写", format!("程序目录不可写: {e}（配置改动会丢失）")),
    }

    // ---------- 汇总 ----------
    let total = rep.passed + rep.failed + rep.warned;
    let _ = writeln!(
        rep.out,
        "\n=== 汇总: {} / {} 通过，{} 失败，{} 警告 ===",
        rep.passed, total, rep.failed, rep.warned
    );
    let code = if rep.failed > 0 {
        1
    } else if strict && rep.warned > 0 {
        2
    } else {
        0
    };
    let _ = writeln!(rep.out, "退出码: {code}");

    // 输出：控制台（若父进程有）+ 日志文件（程序目录优先，失败则退到 %LOCALAPPDATA%）
    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS).is_ok() {
            // 父控制台通常是 936 代码页，直接写 UTF-8 会乱码
            let _ = SetConsoleOutputCP(CP_UTF8);
            if let Ok(mut con) = fs::OpenOptions::new().write(true).open("CONOUT$") {
                use std::io::Write as _;
                let _ = con.write_all(rep.out.as_bytes());
                let _ = con.write_all(b"\r\n");
            }
        }
    }

    // 带 BOM 写出，避免老版记事本把中文判成 ANSI
    let mut body = String::from("\u{FEFF}");
    body.push_str(&rep.out);
    let mut written = None;
    for target in log_targets(&cfg_path) {
        if let Some(dir) = target.parent() {
            let _ = fs::create_dir_all(dir);
        }
        match fs::write(&target, body.as_bytes()) {
            Ok(()) => {
                written = Some(target);
                break;
            }
            Err(_) => continue,
        }
    }
    match written {
        Some(p) => {
            unsafe {
                if AttachConsole(ATTACH_PARENT_PROCESS).is_ok() {
                    let _ = SetConsoleOutputCP(CP_UTF8);
                    if let Ok(mut con) = fs::OpenOptions::new().write(true).open("CONOUT$") {
                        use std::io::Write as _;
                        let _ = writeln!(con, "\r\n[报告已写入] {}", p.display());
                    }
                }
            }
        }
        None => {
            // GUI 子系统下 stderr 可能无处可去，这里尽力而为
            eprintln!("无法写入自检日志（程序目录与 %LOCALAPPDATA% 均不可写）");
            print!("{}", rep.out);
        }
    }

    code
}
