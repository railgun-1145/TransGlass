# TransGlass 技术溯源与架构大纲

本文档旨在为 TransGlass 项目提供完整的技术溯源，确保在不同环境下编译时能够准确还原所有功能逻辑。

## 1. 项目核心元数据
- **项目名称**: TransGlass
- **核心定位**: 轻量级 Windows 窗口透明度与置顶管理工具
- **源码路径**: `E:\git_ku\TransGlass` (推荐避免路径空格)
- **开发语言**: Rust (Edition 2021)
- **目标平台**: Windows 10/11 (x64)
- **开源地址**: https://github.com/railgun-1145/TransGlass

## 2. 依赖溯源 (Dependency Graph)
项目主要依赖以下 Rust Crate，确保在编译时版本一致：
- `windows`: 直接交互 Win32 API (HWND, Styles, Events)。
- `eframe/egui`: 构建硬件加速的现代化 GUI 面板。
- `dashmap`: 线程安全的窗口状态并发注册表。
- `tray-icon`: 系统托盘与上下文菜单支持。
- `self_update`: 实现基于 GitHub Releases 的自动更新。
- `serde/serde_json`: 配置文件 (`transglass_hotkeys.json`) 的序列化。

## 3. 文件功能映射 (Source Mapping)
- `src/main.rs`: **主程序入口**。包含热键监听循环、GUI 生命周期管理、托盘事件处理及自更新逻辑。
- `src/doll.rs`: **MC 3D 纸娃娃渲染器**。纯 CPU 光栅化，用皮肤 UV 拼出 6 个盒子、透视投影 + 超采样抗锯齿，输出 BGRA 直通 alpha 位图。
- `src/selftest.rs`: **自检**。`--self-test` / `--strict`，真实环境探测配置、热键、钩子、权限、DPI。
- `src/bin/dollpreview.rs`: **开发用预览工具**（可选构建）。把纸娃娃按任意角度渲染成 PNG，调 UV/姿态时用。
- `Cargo.toml`: **构建配置文件**。定义了项目版本、编译优化参数及上述所有依赖项。
- `transglass.manifest`: **Windows 清单文件**。强制程序请求管理员权限 (UAC) 并声明 DPI 感知 (防止 4K 屏模糊)。
- `build.rs`: **编译脚本**。在构建过程中自动将清单文件嵌入生成的 `.exe` 中。
- `TransGlass.iss`: **安装包脚本**。使用 Inno Setup 生成标准安装/卸载程序。

## 4. 核心逻辑溯源
### 4.1 窗口注册表 (Window Registry)
所有对系统窗口的修改均通过 `GLOBAL_REGISTRY` (基于 `DashMap`) 进行追踪。
- **记录内容**: 原始 `WS_EX_STYLE`、原始置顶状态、当前透明度、用户偏好。
- **还原保证**: 程序退出或窗口销毁时，读取注册表数据并调用 `SetWindowLongW` 恢复原始样式。

### 4.2 热键处理循环
程序在独立线程中运行 Win32 消息循环 (`GetMessageW`)，监听由 `RegisterHotKey` 注册的系统级全局热键。

注册失败（`GetLastError() == 1409`）时，`try_register_hotkey` 会调 `hotkey_owner_hint()`：用 Toolhelp 枚举当前进程，按「已知软件默认组合键」表（NVIDIA Overlay 的 ALT+R / ALT+SHIFT+R / ALT+Z、AMD Software、微信、QQ、Snipaste）点名最可能的占用者；查不到就写"另一个程序占用，Windows 不提供占用者信息"。提示统一引导「设置 → 快捷键 → 快捷键优先」（`WH_KEYBOARD_LL` 抢在对方前面）或换绑，不再向用户甩 Win32 错误码。

### 4.3 GUI 渲染
`egui` 每 100ms 轮询一次注册表状态，确保界面上的滑动条与热键调节同步。

**窗口缩略图**：`thumbnail()` 按 `preview_fps` 节流调用 `capture_window()`（`PrintWindow` + `PW_RENDERFULLCONTENT`），纹理和抓取时间缓存在 `self.thumbs`。**最小化时不要再抓**——最小化窗口的 rect 只有 160×28，`PrintWindow` 只会返回黑帧（CEF 的 KOOK/Edge 实测），直接返回缓存里的上一帧；抓取失败也回退旧帧，别把卡片打回「无预览」。卡片/列表用 `IsIconic` 标「已最小化」。启动时就已经最小化的窗口没有历史帧，只能显示「已最小化」占位。

**视觉样式（对齐 QQ 音乐那种"白底 + 浅灰 + 一个绿"的克制风）**：`SURFACE/BG_BASE` 纯白、`BG_SIDEBAR #F7F7F7`、`BORDER #EEEEEE`、`TEXT_PRIMARY #1A1A1A`、`TEXT_SECONDARY #8A8A8A`、强调色 `ACCENT #2FC27C` 和淡底 `ACCENT_SOFT #E9F9F1`、悬停 `HOVER_FILL #F5F5F5`、错误 `#E64B4B`。控件圆角 8、卡片 12、窗口 12；卡片 1px 描边 + 很轻的投影；弹窗统一 `popup_shadow`。字号 20/14/12（标题/正文/辅助），标题走粗体家族 `font_title()`。

**两个自绘组件**（`nav_row` / `chip`）：驱动 App 的导航行和桌面胶囊。都用 `animate_bool_with_time` 做 120~160ms 的过渡——悬停淡入 `HOVER_FILL`，选中淡入 `ACCENT_SOFT`，导航行左侧还有一条 3px 强调条；文字用 `lerp_color` 从 `TEXT_PRIMARY` 渐变到 `ACCENT`。卡片悬停则是描边/投影随 `anim` 渐变（`lerp_color(BORDER, #9FC8D6, anim)`）。设置页打开/切分类时 `ui.set_opacity()` 做 150ms 淡入（`settings_fade` 记时间戳）。

**图标**：任务栏/标题栏用 `ViewportBuilder::with_icon()` 读程序目录的 `icon.png`（那块 MC 玻璃）；exe 文件图标靠 `build.rs` 里 `embed_resource::compile("app_icon.rc")` 嵌入 `icon.ico`（由 icon.png 生成的多尺寸 ico），这样资源管理器/快捷方式也是同一块玻璃。

**字体**：优先 `C:\Windows\Fonts\msyh.ttc`（微软雅黑），失败才退回 simhei/simsun（黑体在小字号下又硬又挤，以前放第一优先所以界面像"代码注释"）。另外单独加载 `msyhbd.ttc` 注册成 `FontFamily::Name("bold")`，只给 `Heading` / `Button` 用，正文保持常规字重——这是 egui 里做"真粗体"的唯一办法（`RichText::strong()` 只改颜色不加粗）。

**标题栏**：`with_decorations(false)` 无边框窗口，标题栏自己画（`draw_titlebar`）：左侧 `icon.png` 纹理 + 「TransGlass 控制面板」，右侧自绘 最小化/最大化(还原)/关闭 三个按钮（`titlebar_button`，悬停才出底色、关闭悬停变红）；左 62% 区域 `ui.interact(...)` 拖动 → `ViewportCommand::StartDrag`，双击 → 切换 `Maximized`；关闭按钮走 `hide_root_window()`（和系统关闭一致：隐藏到托盘）。
无边框后原生缩放边框也没了，`handle_window_resize()` 在窗口边缘 6px 内按指针位置发 `ViewportCommand::BeginResize(dir)` 并设置对应光标。窗口尺寸切换（小窗/大界面）用 `resize_root_window()`（`SetWindowPos`）而不是 `InnerSize`——无边框窗口尺寸就是客户区尺寸，`InnerSize` 在这台机器上只生效了一半（宽度没变）。注意窗口尺寸同步那段必须放在 `settings_open` 分支**之前**，否则小窗里点设置切到设置页时不会变大。

**窗口按钮尺寸**：小窗模式下头部是「设置 / 还原」两个按钮，`还原` 只是把 `mini_mode = false`；`设置` 会同时把 `mini_mode = false` 再打开设置页，所以永远是「大界面 + 设置页」。

**无边框的窗口描边**：`rect_stroke` 在 0.5 偏移下会被抗锯齿糊掉（只有右边和下边能看到），改成在 Foreground 层用 `rect_filled` 画四条 1px 实心线，颜色 `#B0B0B0`，四边都实心可见。

**悬浮球开关**：`BALL_SHOWN` 记当前是否显示，`set_ball_visible(on)`（关 = 收起面板 + `ShowWindow(SW_HIDE)`，开 = `ball_render` + `SHOWNA`）。设置里「显示悬浮球」写 `UiState.ball_visible` 并在启动时应用；托盘右键 / 菜单项「唤出悬浮球」也会调它。**右键点悬浮球不再关球**（关球改成面板标题栏右上角的 MC 风格 `×`，命中码 6）。

**纸娃娃居中 + 双击**：纸娃娃先 `doll::render` 到一张透明图层，再按返回的包围盒把整块合成到主缓冲——中心按 `t` 从「球的位置」插值到「舞台中心」，所以展开/收起时人物是滑进去的，展开后视觉上水平垂直都居中。`BALL_DOLL_RECT` 存的是合成后的矩形（命中测试用）。`WM_LBUTTONUP` 命中 9（纸娃娃）时判断双击：`DOLL_LAST_CLICK`/`DOLL_LAST_POS` 记上次时间与位置，350ms 内且位置接近（横 ≤14px、纵 ≤40px）就算双击 → `show_root_window()`，开关是 `DOLL_DBLCLICK`（设置里的「双击纸娃娃打开主界面」）。

**目标窗口显示**：面板左下 `target_row`（176..194）是一个 MC 凹槽，里面用 `draw_mc_label` 画 `ball_target()` 的标题，超过 11 个字截断加 `…`，没有目标显示「未选中窗口」。

**开机自启动**：`HKCU\Software\Microsoft\Windows\CurrentVersion\Run` 下的 `TransGlass` 值（带引号的 exe 路径）。`autostart_enabled()` 查存在性、`set_autostart()` 写/删；设置 → 维护里的复选框直接反映真实注册表状态。注意 windows-rs 的 `Reg*` 返回 `WIN32_ERROR` 而不是 `Result`，只能 `.is_err()` + 读 `.0`，没有 `map_err`。

**重型应用兼容（PS 家族 / 画世界）**：`compat_app_name(hwnd)` 按进程 exe 名匹配（photoshop / illustrator / aftereffects / lightroom / indesign / premiere / painter / huashijie / artworld / 画世界），命中后在 `apply_transparency_to_hwnd` 里改完 alpha 额外做一次 `DwmFlush()` + `RedrawWindow(RDW_INVALIDATE|RDW_FRAME|RDW_UPDATENOW|RDW_ALLCHILDREN)`——这些 GPU 合成的应用不强制重画会留旧帧，要等用户动鼠标才更新。`RedrawWindow` 和 `RDW_*` 在 `Win32::Graphics::Gdi` 下，不在 WindowsAndMessaging。

**快捷键编辑**：不再单开二级窗口（`hotkey_editor_open` 已删），直接内嵌在设置页「快捷键」分类里，一行一个功能：勾选 / 名称 / 修饰键 / 按键 / 录入。

**鼠标键热键（战争雷霆式）**：键位词表里除了键盘键，还有 `鼠标左键/右键/中键/侧键1/侧键2/滚轮上/滚轮下`，`parse_vk` 分别映射到 VK 0x01/02/04/05/06 和自定义码 0x20A/0x20B。`try_register_hotkey` 遇到鼠标键直接返回成功（`RegisterHotKey` 注册不了），改由 `mouse_hook_proc` 派发：按 `WM_*BUTTONDOWN`/`WM_MOUSEWHEEL` 算出 vk，和 `current_hotkey_mods()` 一起在 `HOTKEY_MATCH` 里查，命中就 `PostThreadMessageW(WM_HOTKEY)` 走同一条处理链；侧键/中键吞掉，左/右键和滚轮放行。校验规则：鼠标键可以不带修饰键，但左/右键必须配修饰键。旧的 `mouse.xbutton1/2` 字段保留兼容，保存时若槽位已占用对应侧键会自动清空，避免一次按键触发两次。

**输入设备（数位板适配）**：`list_input_devices()` 用 RawInput 枚举鼠标（usage 0x01/0x02）、笔（0x0D/0x02）、触摸（0x0D/0x04），标签取路径里的 VID/PID；设置页「输入」有三个下拉（鼠标设备 / 笔设备 / 数位板），选择存 `UiState.input_mouse/input_pen/input_tablet`（设备路径）并写进 `DEV_MOUSE/DEV_PEN/DEV_TABLET`。
`spawn_raw_input_tracker()` 起一个消息窗口，`RegisterRawInputDevices`（`RIDEV_INPUTSINK`，只旁听、不抢别人的输入）监听 `WM_INPUT`，按设备反查类别和路径 → 选定设备命中笔/数位板就置 `LAST_RAW_PEN=1`、命中鼠标置 0，并记时间戳。钩子热路径的 `effective_is_pen()` 先看 150ms 内有没有 RawInput 上报，有就按设备判断，否则退回 `dwExtraInfo & 0xFFFFFF00 == 0xFF515700` 的老启发式——这样 Wacom/Huion 把笔上报成普通鼠标时也能正确走「笔点透」。

**设置页**：不再弹窗口，`settings_open` 时主界面直接切成设置页（`settings_page_ui`：底部「← 返回 / 设置 / 版本」+ 左侧 `SETTINGS_TABS` 五个分类 + 右侧 `settings_page_*` 内容）。注意主界面的 `CentralPanel` 也被 `if !self.settings_open` 包住了，否则两个 CentralPanel 会叠在一起画。

### 4.4 悬浮球：分层窗口与 3D 纸娃娃
悬浮球是一个 `WS_EX_LAYERED | WS_EX_NOACTIVATE` 的 Win32 窗口，每帧在内存 DIB 上重画后用 `UpdateLayeredWindow(ULW_ALPHA)` 贴到桌面。要点：

- **必须预乘 alpha**：`BLENDFUNCTION.AlphaFormat = AC_SRC_ALPHA` 要求逐像素预乘，否则抗锯齿边缘会偏亮（深色壁纸上一圈白边）。`ball_render()` 末尾统一预乘。
- **DIB 是 BGRA**：所有像素写入都按 B、G、R、A 顺序（`doll.rs` 的合成函数同样如此）。
- **1px 描边**：在 `doll::render` 的超采样缓冲上对覆盖遮罩做半径 = `ss` 子采样（正好 1 目标像素）的最大值膨胀，只在模型外的样本铺 `#080A0E`、`alpha 0.85`，然后跟模型一起盒式滤波降采样——边缘和描边共用同一条抗锯齿斜坡。描边前先清掉超采样覆盖里 **小于 12 个子采样**的孤立连通块（4 邻域 flood fill）：光栅化/皮肤纹理边界取整偶尔留下的 1~2 个采样点，在 3×3 下看不见，但描边会沿它膨胀成后脑勺外的"黑刺"。`ball_render` 里**不要**再套第二层描边（两层叠起来就是 2px 半透明灰边）。
- **纸娃娃姿态**：`update_doll_pose()` 读光标相对球心的方向，目标朝向 = `62° × dx/|d|`（正上/正下都是正面，左右饱和到 62°，永远看不到背面）；身体吃 78%、头再补 ≤0.3 rad，俯仰 −0.22…+0.28 rad，三者各自指数平滑。用水平分量而不是 `atan2`，是为了在正下方连续、光标在中垂线上不左右弹。
- **帧率策略**：`spawn_doll_ticker()` 每 33ms 推进一次姿态；光标刚动或姿态未收敛时 ~30fps，静置时降到 ~7fps（窗口隐藏则完全跳过）。实测静置多耗约 2.5% 单核。
- **动画时钟**：用 `OnceLock<Instant>` 计时（`anim_secs()`），不能用 `SystemTime` 毫秒转 f32——数值到 1e13 后 f32 只剩分钟级精度，待机浮动会卡住。
- **点击命中**：`ball_hit()` 除了球半径 34 的圆，还认 `BALL_DOLL_RECT`（`doll::render` 返回的包围盒），所以头/腿伸出圆外的部分也能拖动、也能点。
- **展开面板（MC 背包风格）**：点纸娃娃展开 `ball_panel()` 的布局——顶部标题栏 `TransGlass` + 分隔线，左侧竖排 4 个 MC 按钮（置顶/穿透/模式/还原，`draw_mc_button`：黑框 + 上左高光 + 下右阴影），右侧一块 `draw_mc_stage()` 深色槽底站着纸娃娃（外面套 2px 黑框，像 MC 背包放角色/物品的那格），底部一条透明度滑条（`draw_mc_slider`：凹槽 + 按钮式滑块）。取色按 MC GUI 贴图：背景 `#C6C6C6`、外框纯黑、内侧上左白下右 `#555555`、按钮 `#8B8B8B`。文字用 GDI 画两遍（`#3F3F3F` 阴影 +1,+1），字体质量取 ANTIALIASED——用 ClearType 会带彩色亚像素边，补 alpha 后会变成彩虹边。
- **出现方式**：`t` 同时当透明度用（`mc_fill/mc_frame` 都带 alpha 参数做直通 alpha 合成），12 步 × 16ms 三次方 ease-out，面板淡入 + 从右滑 22px，不再"啪"一下糊在桌面上；文字不能真半透明（GDI 不写 alpha），用颜色往 `#C6C6C6` 靠的方式跟着淡入。
- **穿透按窗口独立**：底部动作栏只有「全部还原 / 设置 / 小窗模式」；鼠标点透、笔点透是每窗口状态，放在窗口的右键菜单（`window_menu`）里，和置顶、透明度一起。全局"一键穿透"已删掉（`set_all_passthrough`）。
- **滑条交互**：`BALL_SLIDER_DRAG` + `SetCapture`，拖动时按 `x` 反算透明度（左 0% = 不透明，右 100% = alpha 40），走 `BallAction::SetAlpha`；滑条位置每帧从注册表读目标的 `current_alpha`，所以永远反映实际值。
- **遥控目标解析**：`ball_target()` 优先用记录的 `TARGET_HWND`，但它可能是自身/系统窗口（历史遗留），会退回前台窗口；再不行就没有目标。`target_alpha_now()` 同源，避免出现"滑条能拖但什么都没变"。
- **悬停高亮**：由 ticker 用 `GetCursorPos` 统一算（`BALL_HOVER`），不依赖 `WM_MOUSEMOVE`——分层窗口在没有捕获时不一定收得到移动消息。
- **支持**：经典（4 宽手臂）/ 纤细 Alex（3 宽）两套 UV，按皮肤自动判定（右臂纹理区最右两列是否为空），可在设置里强制；第二层皮肤（帽子/外套/袖子）按 MC 规则外扩 0.5 / 0.25 像素叠加；HD 皮肤按边长缩放 UV。

### 4.5 点透会话选择器（`selector.rs`）
- **线程/窗口**：独立线程 + 全屏 `WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST` 窗口；`UpdateLayeredWindow` 贴 BGRA 预乘缓冲，主线程只 `PostMessage(WM_APP+0x41)` 触发，不碰 UI。
- **候选窗口**：`EnumWindows` + `IsWindowVisible`（最小化也算）、排除自身/Shell/工具窗/`WS_EX_NOACTIVATE`/被 DWM cloaked（其它虚拟桌面）/过小窗；`GetClassLongPtr` 取类图标，标题 + `cached_app_name`。打开时对非最小化窗口调 `capture_window()`（`PrintWindow`）抓一张 BGRA 缩略图；最小化/黑帧退回类图标。
- **绘制**：整屏压暗（黑 alpha 92）+ 居中 880×340 白面板；5 张卡片（有缩略图的等比缩放铺在 128×80 区域，没有的用大图标；软件名 + 标题 + 「已最小化」），GDI 文字/`DrawIconEx` 画在 32bpp DIB 上，面板区域内 alpha 统一 255；卡片矩形存 `State.cards` 供鼠标命中。
- **状态机**：`IDLE → CHOOSING → ACTIVE`。CHOOSING 时点透不动（全屏层自己吃鼠标），热键再按一次 = `move_selection(1)`（Alt+Tab 手感），Esc 取消；确认后先记会话快照（原 `WS_EX_TRANSPARENT`、是否最小化、上方最近的同层级可见窗口），再摘透明位、`SetWindowPos(HWND_TOP)`、`SetForegroundWindow`、`set_target_window()`（让悬浮球面板显示「操作中」），进入 ACTIVE。
- **退出**：ACTIVE 时 Esc（临时 `WH_KEYBOARD_LL` 钩子 → `PostMessage(WM_APP+0x42)`）或再按热键；按注册表重放样式（`reapply_managed_state`）、`reset_passthrough_tracking()`、插回原 z 序、原来最小化的 `SW_MINIMIZE` 回去。
- **主线程配合**：`mouse_hook_proc` 对 `selector::is_session_window(root)` 跳过单设备点透翻转；热键槽 `HotkeySlot::SelectSession`（默认 ALT+反引号，id 11）→ `selector::toggle()`；悬浮球面板的模式/「操作中」状态用 `BALL_DIRTY` + `ball_mark_dirty()` 触发重画。
- **坑**：`WM_ACTIVATE(WA_INACTIVE)` 在 `AttachThreadInput` 抢前台时会瞬时报一次，不能当取消；非 topmost 窗口的 z 序锚点要跳过 topmost 分组，否则结束会被顶成 topmost；缩略图抓取是同步的，别每帧抓；待机动画全关时状态变化必须靠 `BALL_DIRTY` 重画。

### 4.6 自由形状点透选区（`regions.rs`）
- **数据**：`Mutex<Vec<(hwnd, Vec<Region>)>>`，`Region { pts, invert, opacity }`；多边形按**客户区归一化 0..1**保存。每圈 `invert=false` 表示圈内是洞，`true` 表示圈内保留、客户区补集是洞；点击穿透 = 所有圈 mask 的并集（与透明度无关）。
- **编辑器**：独立线程 + 客户区大小的分层窗口（`WS_EX_LAYERED|WS_EX_TOOLWINDOW|WS_EX_TOPMOST`）；目标先 `SW_RESTORE`（如果最小化）并 `SetWindowPos(HWND_TOP)` 提顶、记录上方同层级窗口；GDI 画压暗层 + 两行指令条 + 已有区域（正常绿/反向蓝，选中金框）+ 当前套索（橙线）。鼠标收集点；`Enter` 保存、`Backspace/Delete` 删、`Esc` 取消；`Tab` 选圈、`I` 反向、`[` `]` 调该圈透明度。结束隐藏覆盖层、恢复 z 序和最小化。
- **点击穿透**：`mouse_hook_proc` 每条鼠标消息在 `hit_root_at` **之前**调 `regions::enforce_at`；对每个有选区的窗口，鼠标在客户区内且命中任一 `region_contains` 时给窗口加 `WS_EX_TRANSPARENT`，出洞/离开恢复。点透会话窗口 / 整窗点透窗口跳过。
- **真·透明洞**：`regions_visual` 线程维护一个置顶、`WS_EX_TRANSPARENT|WS_EX_NOACTIVATE` 的分层覆盖层；刷新时 `SW_HIDE` 目标 → 屏幕 BitBlt 出客户区下方快照 → `SW_SHOW`；按每像素「包含该点的圈的最大透明度」写快照 alpha。只在该窗口是前台时显示；目标移动/缩放后快照位置不对就隐藏，等刷新。菜单「刷新透明洞画面」重新抓取。
- **入口**：主界面窗口卡片右键菜单 →「编辑自由形状选区…」/「刷新透明洞画面」/「清空选区」，显示当前数量；`regions::open_editor(hwnd)` / `refresh_visual(hwnd)` 通过 `PostMessage` 叫对应线程。
- **坑**：hook 的 fast-path 必须加 `regions::has_any()`，否则无单设备点透/注册表为空时提前返回，选区完全不生效；`UpdateLayeredWindow` 前不能把 DIB 从 mem DC 取消选中（否则 0x8007001F、窗口全透明）；窗口别用 0×0 创建；`SetWindowRgn` 会裁画面，不能用；任意窗口做不了逐像素 alpha，视觉洞只能靠下层快照覆盖层。
- **限制**：快照静态（下方变了要刷新）；覆盖层只跟前台目标；选区只存内存；没有顶点编辑/持久化。

### 4.7 选择可视化 / 选中控制（`selection.rs`）
- **选择框**：全屏置顶、`WS_EX_LAYERED|WS_EX_TRANSPARENT|WS_EX_TOOLWINDOW|WS_EX_NOACTIVATE` 的分层窗口画 2px 框（颜色取标记色）；150ms 定时器读 `TARGET_HWND`，**只在目标/矩形/标记变化时重画**；4 秒自动收起，目标关闭/最小化隐藏。
- **标签**：独立的 26px 高 topmost 小窗，画「已选中 · 标记 ×」；点击 `WM_LBUTTONUP` 设 `ENABLED=false` 收起。主界面「显示选中框」和卡片「选中并显示」可重新打开。
- **标记**：`MARKS: Mutex<Vec<(hwnd, tag, color)>>`；`mark_for`/`set_mark`。卡片标题、标签文字/颜色都用它；同软件多窗口靠标记区分。
- **右键子菜单**：`selection_controls_ui` 放进 `window_menu` 和左侧窗口行的右键菜单；内容：标记文字/颜色、显示选中框、逐圈反向/透明度/删除、编辑/清空/刷新透明洞。
- **左侧树子项**：`nav_windows` 里对 TARGET/有选区的窗口挂 `CollapsingHeader`，展开调 `self.window_card`（窗口卡片，缩略图/状态/透明度），再点收起；`sel_set` 一次算出。
- **关闭清理**：`selection_cleanup()` 在 `refresh_window_list` 每帧跑，TARGET 无效置 0，`regions::prune`/`selection::prune` 清无效窗口。
- **右侧 PS 面板**：`SidePanel::right("ps_panel")` → `ps_selection_panel(ui, &mut App::ps_selected)`；`Region.enabled` 眼睛；`update_region(hwnd, idx, invert, opacity, enabled)`；`open_editor_at(hwnd, Some(idx))` 用 `PENDING_SELECTED` 按选中圈打开。
- **最小化残留**：区域编辑器 `WM_TIMER` 200ms 查 `main_window_handle()`，主窗口 `IsIconic`/不可见就隐藏覆盖层，恢复再显示；`SetTimer`/`KillTimer` 在 `begin_edit`/`end_edit`。
- **悬浮球气泡**：`BUBBLE_HWND` 独立 layered 窗口，`bubble_render` 用 `draw_mc_panel/mc_fill/draw_mc_label` 画 MC 面板 + 像素尾，`WM_BALL_BUBBLE` 触发，3500ms `WM_TIMER` 收起；`ball_bubble()` 在 `TARGET_HWND` 变化/`set_target_window` 后调用。
- **单选区**：编辑器 `on_up` 用 `st.regions = vec![new]` 替换；面板/右键「新建选择」= `clear` + `open_editor_at`；一次最多 1 个圈。
- **选区联动**：卡片的 `regions::update_region/remove_region` 走 `WM_VISUAL_MASK` 只重画覆盖层 mask，保留已抓的下层快照；编辑/保存才 `refresh_visual` 重新抓。
- **右键悬浮球**：`WindowFromPoint` 在球中心拿到球自己；沿 `GW_HWNDNEXT` 跳过 own/shell，检查矩形包含，`GetAncestor(GA_ROOT)`；找到就 `select_window_for_control`（切目标 + 弹面板），找不到只弹当前面板。
- **坑**：选择框是 transient cue，不要永久置顶；topmost 目标会遮住主界面/标签，auto-hide 兜底；卡片的透明度滑条不能触发重新抓快照；`ball_set_expanded` 在主线程跑 200ms 动画，别重入。

## 5. 构建与打包流程
1. **环境准备**: 安装 Rust 工具链 (MSVC) 及 Inno Setup。
2. **编译源码**: 
   ```bash
   cargo build --release
   ```
3. **分发文件**: 
   所有关键分发文件已汇总至 `E:\daima\002\TransGlass_Distribution` 文件夹。
4. **生成安装包**: 
   运行 Inno Setup 并加载 `E:\daima\002\TransGlass_Distribution\TransGlass.iss`，编译生成安装程序。

## 6. 安全与一致性保证
- **100% 还原**: 程序设计原则是“不留痕迹”，所有样式修改在关闭时必须清除。
- **路径无关性**: `TransGlass.iss` 使用相对路径变量，支持在任何开发目录下直接打包。
