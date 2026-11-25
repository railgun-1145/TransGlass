# TransGlass 项目时间线（三仓库合一）

> 本文档把 GitHub 上的三个仓库 + 两个本地目录拼成一条真实时间线，并记录合并方式。
> 合并后的分支：`unified`（树 = 0.1.1，历史 = 全部四条线）。

## TL;DR

| 问题 | 结论 |
| :--- | :--- |
| 哪个仓库最新？ | `TransGlass` 仓库的 `main` 分支（**0.1.1**，2026-05-20），也就是你实际安装的那版 |
| `TransGlass-cursor-` 是什么？ | 4/10 17:39 的**历史快照**（0.1.0，505 行），还留着 Python 原型 |
| `transglass-trae-cn` 是什么？ | 4/13 的**中途快照**（0.2.0），从 master 线的 `cd9678e` 抄出来又改了 43 行 |
| 0.2.0（master）能用吗？ | 原样**编译不过**（缺 `HAS_ACTIVE_DRAG` 声明，5 处 E0425），补一行才能过 |
| trae 那版能编译吗？ | **不能**，AI 加的单实例代码有类型错误，3 个编译错误 |
| 现在该用哪份？ | `unified` 分支，树 = 0.1.1，已用当前工具链编译通过 |

## 1. 五个位置的角色

| 位置 | 类型 | 内容 |
| :--- | :--- | :--- |
| `github.com/railgun-1145/TransGlass` | 主仓库 | 两条**互不相关**的历史：`master`（0.2.0 重写线）与 `main`（0.1.x 发布线） |
| `github.com/railgun-1145/TransGlass-cursor-` | 存档 | 4/10 的 master 线快照 + 3 个 Python 原型 |
| `github.com/railgun-1145/transglass-trae-cn` | 分叉 | 4/13 由 Trae 接管后的一份拷贝 |
| `E:\git ku\TransGlass` | 本地工作副本 | master 线 4/28 版本 + 6/15 补的一行声明（未提交） |
| `E:\新建文件夹\TransGlass` | 安装目录 | 0.1.1 安装态，注册表登记 `TransGlass 版本 0.1.1`，安装日期 20260601（开始菜单快捷方式指向这里） |

## 2. 时间线

### 阶段 A — Python 原型（4/10 13:21 之前）

项目起点不是 Rust：`132cc49 Initial commit of VRC Migrator and Transparency Tool`。
两个 Python 工具（`vrc_migrator.py`、`transparency_tool.py`、`sort_simulation.py`）后来被 Rust 版取代，只在 `TransGlass-cursor-` 仓库里保留了下来。

### 阶段 B — Rust 重写与 GUI 成型（4/10 13:55 – 17:39）

| 时间 | 提交 | 内容 |
| :--- | :--- | :--- |
| 13:55 | `1914f33` / `1da7987` | Rust 项目初始化、Win32 依赖 |
| 13:57–14:00 | `5db9cc1` `3da0d0c` `3be432c` | 窗口事件钩子、全局热键、透明度微调、置顶 |
| 14:18–14:27 | `e069f86` `d5f8dad` `0ba4c70` | 修类型错误、自更新热键、内嵌 UAC/PerMonitorV2 manifest |
| 14:41–14:45 | `6dc2346` `b73bb53` | 热键 JSON 化 + 热重载、修自更新仓库名 |
| **17:05 / 17:28** | `fd2f99e` `8c9bd3a` | **推送 `TransGlass-cursor-`（存档当时的 505 行快照，Cargo 0.1.0）** |
| 17:37 | `7a2afa1` | 专业 GUI 面板、托盘、HWND 安全修复 |
| 17:39 | `ec960f8` | 面板样式打磨 —— **cursor 仓库的 Rust 部分就是这一刻的原样快照**（blob `b76f2753fc`） |

### 阶段 C — master 线打磨（4/10 18:52 – 4/12 13:52）

| 时间 | 提交 | 内容 |
| :--- | :--- | :--- |
| **18:52** | `170c393` | **main 线从这里另起一个根**（托盘无响应 / 隐藏时 CPU 高占用） |
| 19:05 | `6daec92` | 整体替换为当前构建，删掉 Python 残留 |
| 19:32–19:47 | `ddde1f8` `4a066d6` `52d79bf` | 安装包中文化、UAC、卸载器、精简代码 |
| 4/11 15:03–20:20 | `cd95dd4` `7e9d271` `5557a57` | 点透 + 数位板 XButton、鼠标/笔穿透分离 |
| 4/12 13:52 | `cd9678e` | 多显示器、钩子阻塞、拖拽状态泄漏 |

### 阶段 D — Trae 分叉（4/13 14:00）

`transglass-trae-cn` 只有一次提交 `46f0d50`，内容是 `cd9678e` 的拷贝 + 43 行改动（3 个 hunk），版本号写成 0.2.0。
详见第 4 节的问题清单。

### 阶段 E — 两条线并行（4/27 – 5/20）

| 时间 | 线 | 提交 | 内容 |
| :--- | :--- | :--- | :--- |
| 4/27 23:37 | master | `12e498d` | 覆盖式「最终优化」 |
| 4/28 00:46 | master | `4c4462e` | **0.2.0 大改版**：热键诊断、多显示器、CI；但漏了 `HAS_ACTIVE_DRAG` 声明 → 编译失败 |
| 5/20 20:00 | main | `d9c11b5` | 点透/热键/安装器更新 |
| 5/20 20:12 | main | `569386e` | 鼠标点透延迟修复 |
| 5/20 20:22 | main | `0b3feb3` | 避免桌面/任务栏被加入管理列表 |
| 5/20 20:39 | main | `a67c860` | 打包 0.1.0 |
| 5/20 21:16 | main | `6a48510` | CI 自动发 Release |
| 5/20 21:22 | main | `d016acc` | 版本号 → 0.1.1 |
| 5/20 21:25 | main | `07c9a10` | 锁文件更新（`v0.1.0` / `v0.1.1` 两个 tag 都在这里） |
| 6/01 | — | — | 你安装了 0.1.1（`E:\新建文件夹\TransGlass`） |
| 6/15 | 本地 | 未提交 | 在 `E:\git ku` 给 master 补上 `HAS_ACTIVE_DRAG` 声明 |

### 阶段 F — 合并（本次）

`unified` 分支 = `main`(0.1.1) 的树 + 三个 `-s ours` 合并，把 master / trae / cursor 全部挂成祖先：

```
unified
├─ merge cursor (8c9bd3a, 4/10 快照)
├─ merge trae   (46f0d50, 4/13 快照)
├─ merge master (4c4462e, 4/28 · 0.2.0)
└─ main 07c9a10 (v0.1.1, 5/20) ← 树取自这里
```

## 3. 两条实现线的功能矩阵

| 能力 | 0.1.1（main，5/20） | 0.2.0（master，4/28） |
| :--- | :---: | :---: |
| 三态输入模式 `INPUT_MODE` + `Alt+Shift+M` | ✅ | ❌ |
| 桌面/任务栏过滤（`Progman`/`WorkerW`/`Shell_TrayWnd`） | ✅ | ❌ |
| 单实例互斥体 + 聚焦已运行窗口 | ✅ 正确实现 | ❌ 没有 |
| 热键诊断自检（`run_test!` ×35） | ❌ | ✅ |
| `SendInput` 事件转发（鼠标/笔分离穿透） | ❌ | ✅ |
| `WS_EX_TRANSPARENT` 双开点透 | ✅ | ✅ |
| 自动发 Release 的 CI（`release.yml`） | ✅ | ❌（只有构建 artifact） |
| 代码规模 | 1776 行 | 1441 行 |
| 当前工具链编译 | ✅ | ❌（补 1 行才过，见 `bd2ee41`） |

## 4. `transglass-trae-cn` 的问题清单（有证据）

基线 = `cd9678e`（4/12），差异 = `+28 / −15`，共 3 处：

### B1 单实例检查写了但编译不过（致命）

```rust
let mutex = windows::Win32::System::Threading::CreateMutexW(None, true, "TransGlass");
```

`lpname` 参数要 `PCWSTR`，传了 `&str`。`cargo check` 实测：

```
error[E0271]: type mismatch resolving `<&str as TypeKind>::TypeKind == CopyType`
error[E0277]: the trait bound `&str: Param<PCWSTR, CopyType>` is not satisfied   (×2)
error: could not compile `transglass` (bin "transglass") due to 3 previous errors
```

同时还有两个隐患：`CreateMutexW` 返回 `Result<HANDLE>` 后立刻调 `GetLastError()` 不可靠（应在调用后立刻判断 `ERROR_ALREADY_EXISTS`，且要处理 `Err` 分支），`std::mem::forget(mutex)` 忘掉的是 `Result` 而不是句柄。

**结论：这个仓库提交进去的代码根本编译不出来。**

### B2 顺手删掉了「自身窗口」旁路（行为回退）

`mouse_hook_proc` 里 27 行被整段删除：`WindowFromPoint` → `is_own_hwnd` → 非置顶时用 `SC_MOVE|HTCAPTION` 拖动面板、其余情况把事件直通给下层。
后果：单开「鼠标点透」时点击 TransGlass 面板会被转发给下层窗口，面板也拖不动。

### B3 把 `debug.log` 提交进了仓库

内容还不是本程序的日志，是搜狗输入法的 `SGRenderLog` 记录。

### 修复现状

B1、B2 在 5/20 的 0.1.1 里已经被重写覆盖：`acquire_single_instance()` 用 `w!("Local\\TransGlass_SingleInstance_…")` 正确建互斥体，并 `focus_existing_instance_window()`；`is_own_hwnd` 直通逻辑也保留了。所以合并后的 `unified` 树不含这些 bug，trae 分支只作历史留档。

## 5. 合并结果与怎么用

- 分支：`unified`（当前工作树，领先 `origin/main` 29 个提交）
- 构建：`cargo build --release` ✅（2m22s，产物 14,105,088 字节，与 `E:\daima\002` 那份 0.1.1 构建同尺寸）
- 产物已归档：`E:\TransGlass\artifacts\transglass-0.1.1-unified.exe`
- master 线补丁保留在 `fix/master-build`（提交 `bd2ee41`），归档构建：`artifacts\transglass-0.2.0-has_active_drag_fix.exe`

常用命令：

```bash
git -C E:\TransGlass\TransGlass log --graph --all --oneline    # 看合一后的谱系
git checkout master        # 看 0.2.0 线（需 bd2ee41）
git checkout unified       # 回到 0.1.1 主线
```

## 6. 移植清单的进展（2026-11-26 更新）

| 项 | 状态 |
| :--- | :--- |
| 自检（原计划搬 0.2.0 的 `run_test!` 那套） | ✅ 已做，但**没有照搬**：0.2.0 那 35 处断言的条件全是字面量 `true`（假测试），且在主程序 `main()` 里无条件执行后被 `FreeConsole()` 吞掉。改为真实探测并重写成 `--self-test`：配置解析、键名合法性、重复绑定、逐槽位 `RegisterHotKey`、`WH_MOUSE_LL` / `WH_KEYBOARD_LL`、单实例、权限、DPI、显示器、目录可写；支持 `--strict` 当门禁 |
| `SendInput` 转发 + `ExtraInfo` 笔识别 | ⏳ 未做。与 0.1.1 的 `INPUT_MODE` 三态是两套思路，要先定取舍 |
| CI 合并 | ⏳ 未做（`release.yml` 与 `windows-build.yml` 仍分开） |

## 7. 2026-11-26 这一轮新增

- **`--self-test` / `--strict`**：真实环境自检，输出 PASS/WARN/FAIL + 日志 + 退出码
- **热键优先模式**：`"hotkey_priority": true` 时改用 `WH_KEYBOARD_LL` 强占组合键，被别的程序占用也能生效；面板上有开关
- **`--quit`**：让已运行实例优雅退出（具名事件），不再"点一次多一个进程"
- **`run_and_return: true`**：修掉 eframe 关窗时直接 `process::exit` 导致 `TrayIcon` 永不 drop、每次退出漏一个托盘图标的问题
- **不再静默失败**：热键注册失败 / 配置解析失败都会进面板横幅；配置写坏会先备份成 `.broken-<时间戳>.bak` 再重建
- **`parse_vk` 扩展**：支持 F1–F24、方向键、SPACE/TAB/ENTER/ESC/DEL/INS/HOME/END/PGUP/PGDN 及常用符号键，非法键名会明确报错

这些改动在 `feat/self-test` 分支，已合入 `unified`。
