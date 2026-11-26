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

### 4.3 GUI 渲染
`egui` 每 100ms 轮询一次注册表状态，确保界面上的滑动条与热键调节同步。

**视觉样式（对齐 QQ 音乐那种"白底 + 浅灰 + 一个绿"的克制风）**：`SURFACE/BG_BASE` 纯白、`BG_SIDEBAR #F7F7F7`、`BORDER #EEEEEE`、`TEXT_PRIMARY #1A1A1A`、`TEXT_SECONDARY #8A8A8A`、强调色 `ACCENT #2FC27C` 和淡底 `ACCENT_SOFT #E9F9F1`、悬停 `HOVER_FILL #F5F5F5`、错误 `#E64B4B`。控件圆角 8、卡片 12、窗口 12；卡片 1px 描边 + 很轻的投影；弹窗统一 `popup_shadow`。字号 20/14/12（标题/正文/辅助），标题走粗体家族 `font_title()`。

**两个自绘组件**（`nav_row` / `chip`）：驱动 App 的导航行和桌面胶囊。都用 `animate_bool_with_time` 做 120~160ms 的过渡——悬停淡入 `HOVER_FILL`，选中淡入 `ACCENT_SOFT`，导航行左侧还有一条 3px 强调条；文字用 `lerp_color` 从 `TEXT_PRIMARY` 渐变到 `ACCENT`。卡片悬停则是描边/投影随 `anim` 渐变（`lerp_color(BORDER, #9FC8D6, anim)`）。设置页打开/切分类时 `ui.set_opacity()` 做 150ms 淡入（`settings_fade` 记时间戳）。

**图标**：任务栏/标题栏用 `ViewportBuilder::with_icon()` 读程序目录的 `icon.png`（那块 MC 玻璃）；exe 文件图标靠 `build.rs` 里 `embed_resource::compile("app_icon.rc")` 嵌入 `icon.ico`（由 icon.png 生成的多尺寸 ico），这样资源管理器/快捷方式也是同一块玻璃。

**字体**：优先 `C:\Windows\Fonts\msyh.ttc`（微软雅黑），失败才退回 simhei/simsun（黑体在小字号下又硬又挤，以前放第一优先所以界面像"代码注释"）。另外单独加载 `msyhbd.ttc` 注册成 `FontFamily::Name("bold")`，只给 `Heading` / `Button` 用，正文保持常规字重——这是 egui 里做"真粗体"的唯一办法（`RichText::strong()` 只改颜色不加粗）。

**标题栏**：`with_decorations(false)` 无边框窗口，标题栏自己画（`draw_titlebar`）：左侧 `icon.png` 纹理 + 「TransGlass 控制面板」，右侧自绘 最小化/最大化(还原)/关闭 三个按钮（`titlebar_button`，悬停才出底色、关闭悬停变红）；左 62% 区域 `ui.interact(...)` 拖动 → `ViewportCommand::StartDrag`，双击 → 切换 `Maximized`；关闭按钮走 `hide_root_window()`（和系统关闭一致：隐藏到托盘）。
无边框后原生缩放边框也没了，`handle_window_resize()` 在窗口边缘 6px 内按指针位置发 `ViewportCommand::BeginResize(dir)` 并设置对应光标。窗口尺寸切换（小窗/大界面）用 `resize_root_window()`（`SetWindowPos`）而不是 `InnerSize`——无边框窗口尺寸就是客户区尺寸，`InnerSize` 在这台机器上只生效了一半（宽度没变）。注意窗口尺寸同步那段必须放在 `settings_open` 分支**之前**，否则小窗里点设置切到设置页时不会变大。

**窗口按钮尺寸**：小窗模式下头部是「设置 / 还原」两个按钮，`还原` 只是把 `mini_mode = false`；`设置` 会同时把 `mini_mode = false` 再打开设置页，所以永远是「大界面 + 设置页」。

**无边框的窗口描边**：`rect_stroke` 在 0.5 偏移下会被抗锯齿糊掉（只有右边和下边能看到），改成在 Foreground 层用 `rect_filled` 画四条 1px 实心线，颜色 `#B0B0B0`，四边都实心可见。

**悬浮球开关**：`BALL_SHOWN` 记当前是否显示，`set_ball_visible(on)`（关 = 收起面板 + `ShowWindow(SW_HIDE)`，开 = `ball_render` + `SHOWNA`）。设置里「显示悬浮球」写 `UiState.ball_visible` 并在启动时应用；托盘右键 / 菜单项「唤出悬浮球」也会调它。**右键点悬浮球不再关球**（关球改成面板标题栏右上角的 MC 风格 `×`，命中码 6）。

**纸娃娃居中 + 双击**：纸娃娃先 `doll::render` 到一张透明图层，再按返回的包围盒把整块合成到主缓冲——中心按 `t` 从「球的位置」插值到「舞台中心」，所以展开/收起时人物是滑进去的，展开后视觉上水平垂直都居中。`BALL_DOLL_RECT` 存的是合成后的矩形（命中测试用）。`WM_LBUTTONUP` 命中 9（纸娃娃）时判断双击：`DOLL_LAST_CLICK`/`DOLL_LAST_POS` 记上次时间与位置，350ms 内且位置接近就算双击 → `show_root_window()`，开关是 `DOLL_DBLCLICK`（设置里的「双击纸娃娃打开主界面」）。

**快捷键编辑**：不再单开二级窗口（`hotkey_editor_open` 已删），直接内嵌在设置页「快捷键」分类里，一行一个功能：勾选 / 名称 / 修饰键 / 按键 / 录入。

**鼠标键热键（战争雷霆式）**：键位词表里除了键盘键，还有 `鼠标左键/右键/中键/侧键1/侧键2/滚轮上/滚轮下`，`parse_vk` 分别映射到 VK 0x01/02/04/05/06 和自定义码 0x20A/0x20B。`try_register_hotkey` 遇到鼠标键直接返回成功（`RegisterHotKey` 注册不了），改由 `mouse_hook_proc` 派发：按 `WM_*BUTTONDOWN`/`WM_MOUSEWHEEL` 算出 vk，和 `current_hotkey_mods()` 一起在 `HOTKEY_MATCH` 里查，命中就 `PostThreadMessageW(WM_HOTKEY)` 走同一条处理链；侧键/中键吞掉，左/右键和滚轮放行。校验规则：鼠标键可以不带修饰键，但左/右键必须配修饰键。旧的 `mouse.xbutton1/2` 字段保留兼容，保存时若槽位已占用对应侧键会自动清空，避免一次按键触发两次。

**输入设备（数位板适配）**：`list_input_devices()` 用 RawInput 枚举鼠标（usage 0x01/0x02）、笔（0x0D/0x02）、触摸（0x0D/0x04），标签取路径里的 VID/PID；设置页「输入」有三个下拉（鼠标设备 / 笔设备 / 数位板），选择存 `UiState.input_mouse/input_pen/input_tablet`（设备路径）并写进 `DEV_MOUSE/DEV_PEN/DEV_TABLET`。
`spawn_raw_input_tracker()` 起一个消息窗口，`RegisterRawInputDevices`（`RIDEV_INPUTSINK`，只旁听、不抢别人的输入）监听 `WM_INPUT`，按设备反查类别和路径 → 选定设备命中笔/数位板就置 `LAST_RAW_PEN=1`、命中鼠标置 0，并记时间戳。钩子热路径的 `effective_is_pen()` 先看 150ms 内有没有 RawInput 上报，有就按设备判断，否则退回 `dwExtraInfo & 0xFFFFFF00 == 0xFF515700` 的老启发式——这样 Wacom/Huion 把笔上报成普通鼠标时也能正确走「笔点透」。

**设置页**：不再弹窗口，`settings_open` 时主界面直接切成设置页（`settings_page_ui`：底部「← 返回 / 设置 / 版本」+ 左侧 `SETTINGS_TABS` 五个分类 + 右侧 `settings_page_*` 内容）。注意主界面的 `CentralPanel` 也被 `if !self.settings_open` 包住了，否则两个 CentralPanel 会叠在一起画。

### 4.4 悬浮球：分层窗口与 3D 纸娃娃
悬浮球是一个 `WS_EX_LAYERED | WS_EX_NOACTIVATE` 的 Win32 窗口，每帧在内存 DIB 上重画后用 `UpdateLayeredWindow(ULW_ALPHA)` 贴到桌面。要点：

- **必须预乘 alpha**：`BLENDFUNCTION.AlphaFormat = AC_SRC_ALPHA` 要求逐像素预乘，否则抗锯齿边缘会偏亮（深色壁纸上一圈白边）。`ball_render()` 末尾统一预乘。
- **DIB 是 BGRA**：所有像素写入都按 B、G、R、A 顺序（`doll.rs` 的合成函数同样如此）。
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
