//! 自检 / 诊断模块（`transglass.exe --self-test`）。
//!
//! 为什么要有这个模块：
//! 0.2.0 分支里的 `run_code_review()` 与 `run_ui_sandbox_test()` 是"安慰剂"——
//! 35 处 `run_test!` 的条件参数全部写死为 `true`，打印出来的 ✅ 不代表任何检测结果，
//! 而且它在 `main()` 里无条件执行，随后立刻 `FreeConsole()`，用户根本看不到。
//!
//! 这里保留那套"分组 + 逐项 PASS/FAIL + 汇总"的输出形态，但每一项都换成真实探测：
//! 配置文件能不能解析、热键键名是否合法、是否重复、能否真的 `RegisterHotKey`、
//! 低级鼠标钩子能否装上、单实例互斥体是否被占用、是否以管理员运行、DPI 感知级别、
//! 显示器数量、程序目录是否可写。
//!
//! 结果同时写到控制台（若从 cmd/PowerShell 启动）和程序目录下的
//! `transglass_selftest.log`，并用退出码表达结果（0 = 全过，1 = 有失败项）。

use std::fmt::Write as _;
use std::fs;

use windows::core::w;
use windows::Win32::Foundation::*;
use windows::Win32::Security::{
    GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
};
use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
use windows::Win32::System::Threading::*;
use windows::Win32::UI::HiDpi::GetProcessDpiAwareness;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::{get_config_path, parse_modifiers, parse_vk, HotkeyConfig, HotkeySpec};

/// 单实例互斥体名，必须与 `acquire_single_instance()` 里的一致。
const SINGLE_INSTANCE_NAME: windows::core::PCWSTR =
    w!("Local\\TransGlass_SingleInstance_C1E0B6B4-18A0-4D5D-9D7A-2C5DFD4D0A2F");

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

/// 与 `bind_hotkeys()` 保持一致的槽位表：(注册 id, 名称, 配置项)
fn hotkey_slots(cfg: &HotkeyConfig) -> Vec<(i32, &'static str, HotkeySpec)> {
    let click = cfg.toggle_click_through.clone().unwrap_or(HotkeySpec {
        modifiers: "ALT".into(),
        key: "P".into(),
    });
    let pen = cfg.toggle_pen_passthrough.clone().unwrap_or(HotkeySpec {
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
        (4, "切换鼠标点透", click),
        (5, "切换笔点透", pen),
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

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    CallNextHookEx(None, code, wparam, lparam)
}

/// 在独立线程里逐个真实注册热键（`RegisterHotKey` 绑定到调用线程，必须同线程注册/注销）。
fn probe_hotkey_registration(
    items: Vec<(i32, String, u32, u32)>,
) -> Vec<(i32, String, bool, u32)> {
    std::thread::spawn(move || {
        let mut results = Vec::new();
        // 建消息队列，避免某些系统对无队列线程注册热键的行为差异
        let mut msg = MSG::default();
        unsafe {
            let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
        }
        for (id, label, mods_bits, vk) in items {
            unsafe {
                let ok = RegisterHotKey(None, id, HOT_KEY_MODIFIERS(mods_bits), vk).is_ok();
                let err = if ok { 0 } else { GetLastError().0 };
                if ok {
                    let _ = UnregisterHotKey(None, id);
                }
                results.push((id, label, ok, err));
            }
        }
        results
    })
    .join()
    .unwrap_or_default()
}

pub fn run() -> i32 {
    let mut rep = Report::new();

    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "<unknown>".into());
    let _ = writeln!(rep.out, "TransGlass 自检报告");
    let _ = writeln!(rep.out, "版本: v{}", env!("CARGO_PKG_VERSION"));
    let _ = writeln!(rep.out, "程序: {exe}");

    // ---------- 1. 配置文件 ----------
    rep.section("配置文件");
    let cfg_path = get_config_path();
    rep.info("路径", cfg_path.display().to_string());
    let mut cfg = None;
    match fs::read_to_string(&cfg_path) {
        Ok(text) => match serde_json::from_str::<HotkeyConfig>(&text) {
            Ok(c) => {
                rep.pass("解析", format!("{} 字节，JSON 结构正确", text.len()));
                cfg = Some(c);
            }
            Err(e) => rep.fail("解析", format!("JSON 无法解析为热键配置: {e}")),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => rep.warn(
            "读取",
            "配置文件不存在（首次运行或刚换目录，属正常），下面用内置默认值检测",
        ),
        Err(e) => rep.fail("读取", format!("读不到配置文件: {e}")),
    }
    if cfg.is_none() {
        // 缺文件时退回默认值继续跑其余检测，但要明确报失败
        rep.warn("回退", "将使用内置默认热键继续检测（运行主程序会重新生成该文件）");
        cfg = Some(crate::default_config());
    }
    let cfg = cfg.expect("cfg always present");

    // ---------- 2. 热键配置合法性 ----------
    rep.section("热键配置");
    let slots = hotkey_slots(&cfg);
    let mut seen: Vec<(String, &'static str)> = Vec::new();
    let mut bad_keys: Vec<String> = Vec::new();
    let mut duplicates: Vec<String> = Vec::new();
    let mut probe_items: Vec<(i32, String, u32, u32)> = Vec::new();

    for (id, label, spec) in &slots {
        let (mods, vk) = unsafe { (parse_modifiers(&spec.modifiers), parse_vk(&spec.key)) };
        let combo = format!("{}+{}", spec.modifiers.trim().to_uppercase(), spec.key.trim().to_uppercase());
        if vk == 0 {
            bad_keys.push(format!("{label}({combo})"));
        }
        probe_items.push((*id, format!("{label} ({combo})"), mods.0, vk));
        if let Some((_, other)) = seen.iter().find(|(c, _)| c == &combo) {
            duplicates.push(format!("{label} 与 {other} 都是 {combo}"));
        } else {
            seen.push((combo, *label));
        }
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

    // ---------- 3. 热键实际注册 ----------
    rep.section("热键注册");
    let results = probe_hotkey_registration(probe_items);
    if results.is_empty() {
        rep.fail("注册", "探测线程没有返回结果");
    } else {
        for (id, label, ok, err) in &results {
            if *ok {
                rep.pass(label, format!("id={id} 注册成功（已立即注销）"));
            } else if *err == 1409 {
                // ERROR_HOTKEY_ALREADY_REGISTERED
                rep.fail(
                    label,
                    format!("id={id} 组合键已被其它程序占用 (ERROR_HOTKEY_ALREADY_REGISTERED)"),
                );
            } else {
                rep.fail(label, format!("id={id} 注册失败，Win32 错误码 {err}"));
            }
        }
    }

    // ---------- 4. 低级鼠标钩子 ----------
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

    // ---------- 5. 单实例 ----------
    rep.section("单实例");
    unsafe {
        match CreateMutexW(None, true, SINGLE_INSTANCE_NAME) {
            Ok(handle) => {
                let already = GetLastError() == ERROR_ALREADY_EXISTS;
                let _ = CloseHandle(handle);
                if already {
                    rep.warn("互斥体", "已有另一个 TransGlass 在运行");
                } else {
                    rep.pass("互斥体", "当前没有其它实例占用");
                }
            }
            Err(e) => rep.fail("互斥体", format!("创建失败: {e}")),
        }
    }

    // ---------- 6. 运行环境 ----------
    rep.section("运行环境");
    let elevated = unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_ok() {
            let mut elevation = TOKEN_ELEVATION::default();
            let mut len = 0u32;
            let ok = GetTokenInformation(
                token,
                TokenElevation,
                Some(&mut elevation as *mut _ as *mut core::ffi::c_void),
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut len,
            )
            .is_ok();
            let _ = CloseHandle(token);
            ok && elevation.TokenIsElevated != 0
        } else {
            false
        }
    };
    if elevated {
        rep.warn(
            "权限",
            "以管理员身份运行：能操作管理员窗口，但低级鼠标钩子可能被 UIPI 限制",
        );
    } else {
        rep.pass("权限", "普通用户权限：无法操作以管理员运行的窗口（设计如此）");
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
    let code = if rep.failed > 0 { 1 } else { 0 };
    let _ = writeln!(rep.out, "退出码: {code}");

    // 输出：控制台（若父进程有）+ 程序目录日志文件
    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS).is_ok() {
            if let Ok(mut con) = fs::OpenOptions::new().write(true).open("CONOUT$") {
                use std::io::Write as _;
                let _ = con.write_all(rep.out.as_bytes());
                let _ = con.write_all(b"\r\n");
            }
        }
    }
    let log_path = cfg_path.with_file_name("transglass_selftest.log");
    // 带 BOM 写出，避免老版记事本把中文判成 ANSI
    let mut log_body = String::from("\u{FEFF}");
    log_body.push_str(&rep.out);
    if let Err(e) = fs::write(&log_path, log_body.as_bytes()) {
        // 连日志都写不进去时至少让控制台看到
        eprintln!("无法写入 {}: {e}", log_path.display());
        print!("{}", rep.out);
    }

    code
}
