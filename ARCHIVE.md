# TransGlass 项目归档 / 上下文总结

> 归档时间：2026-11-28　·　对应版本：**v2.16.0**
> 这篇是给"人"和"AI 助手"共用的：换成新对话、换台机器，读完这份就能接上手。

---

## 1. 项目概览

| 项 | 内容 |
| :--- | :--- |
| 是什么 | Windows 窗口透明化工具：**逐窗透明度 / 置顶 / 鼠标·笔分别点透**，配一个 MC 风格纸娃娃悬浮球当遥控器 |
| 仓库 | <https://github.com/railgun-1145/TransGlass>（分支 `unified`，推送目标 `master`） |
| 发布 | <https://github.com/railgun-1145/TransGlass/releases> |
| 教程 | [docs/使用教程.md](docs/使用教程.md)（软件里「帮助」按钮直达） |
| 当前版本 | **v2.16.0** |
| 技术栈 | Rust 1.94 + eframe/egui 0.28 + tray-icon 0.19 + windows 0.58 + image 0.25 |
| 形态 | 单 exe、免登录、本地运行；Inno Setup 安装包（安装路径可选） |
| 部署目录 | `E:\新建文件夹\TransGlass\`（开发机上的实际运行位置） |
| 自检 | `transglass.exe --self-test` → 7/10 通过、**0 失败**、3 警告（正常） |

---

## 2. 代码结构

```
src/main.rs      ~6000 行：Win32 窗口控制 / 托盘 / 热键 / 悬浮球 / egui 主界面 都在这里
src/doll.rs      MC 3D 纸娃娃软件光栅器（6 个 box + 皮肤 UV + 背面剔除 + 画家算法 + z-buffer + 3×3 超采样 + 超采样域 1px 描边）
src/selector.rs  点透会话：Alt+Tab 式全屏窗口选择器（分层窗口 + GDI）、临时关点透、提顶、退出恢复
src/regions.rs   自由形状点透选区：套索编辑器（每选区可反向/调透明度）+ 洞内点击穿透 + 真·透明洞覆盖层（下层快照）
src/selection.rs 选择可视化：当前控制目标一圈选择框 + 可点击标签（全屏置顶点击穿透，150ms 跟随，4 秒自动收起）；标记/颜色/选中管理卡片
build.rs         embed-manifest（清单）+ embed-resource（exe 图标 icon.ico）
TransGlass.iss   Inno Setup 安装脚本
docs/            使用教程 + 截图
icon.png/.ico    那块 MC 玻璃（程序图标、托盘图标、安装包图标都用它）
skins/*.png      9 个皮肤（alex/ari/efe/kai/makena/noor/steve/sunny/zuri）
```

### 2.1 关键子系统

| 子系统 | 实现要点 |
| :--- | :--- |
| 窗口透明 | `WS_EX_LAYERED` + `SetLayeredWindowAttributes(LWA_ALPHA)`；改前存 `original_ex_style`，还原时整份恢复 |
| 点透 | `WS_EX_TRANSPARENT`；鼠标/笔分开靠**低级鼠标钩子**按设备判定（`is_pen_input` + RawInput 上报） |
| 热键 | 普通模式：`RegisterHotKey`；优先模式：`WH_KEYBOARD_LL` 抢键后 `PostThreadMessage(WM_HOTKEY)`；**鼠标键热键**由 `WH_MOUSE_LL` 派发 |
| 点透会话 | `selector.rs`：默认 Alt+反引号 唤出全屏选择器（窗口缩略图预览，再按热键切下一个）；选中后临时摘 `WS_EX_TRANSPARENT`、提到最上层、激活（可拖动），Esc/再按热键按会话前快照恢复 z 序 + 最小化 + 点透 |
| 点透选区 | `regions.rs`：窗口卡片右键 → 套索编辑器画多个自由形状；**每个选区可单独反向、单独调 0–100% 透明度**；按客户区归一化保存；低级鼠标钩子在洞内临时加 `WS_EX_TRANSPARENT`、出洞恢复；真·透明洞用点击穿透的置顶覆盖层贴「下层快照」；整窗点透的窗口上不生效 |
| 选择可视化 / 悬浮球气泡 + 蒙版 | 全屏选择框已移除；悬浮球弹 MC 气泡：球窗口上方不压面板，显示 3.2s + 0.6s 渐隐，重复点重新弹出；每个封闭选区 = 一个独立蒙版，可堆多个；右侧「蒙版列表」逐条眼睛/反向/透明度/删除；「新建蒙版」追加，「编辑当前蒙版」编辑当前条 |
| 输入设备 | RawInput 枚举（鼠标 0x01/0x02、笔 0x0D/0x02、触摸 0x0D/0x04）+ `RIDEV_INPUTSINK` 旁听线程，按设备判断笔/鼠标 |
| 悬浮球 | 独立分层窗口（`UpdateLayeredWindow`）+ CPU 渲染；展开面板是 **MC 背包/设置界面风格**（`#C6C6C6` 底 + 黑框 + 斜面 + `#8B8B8B` 按钮） |
| 纸娃娃 | `doll.rs` 直接写 BGRA 到内存位图；朝向跟光标，yaw 限幅 ±62°，永不露背面 |
| 主界面 | egui 浅色主题（QQ 音乐风：纯白 + `#F7F7F7` 侧栏 + `#2FC27C` 绿 + 字号 20/14/12） |
| 标题栏 | **无边框窗口 + 自绘**：玻璃图标 + 标题 + 「帮助」+ 最小化/最大化/关闭；拖动=StartDrag、双击=最大化、边缘 6px=BeginResize |
| 托盘 | 左键=打开主界面、右键=菜单（唤出悬浮球 / 全部还原 / 退出） |
| 开机自启 | `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` 下的 `TransGlass` 值 |

---

## 3. 当前功能清单（v2.16.0）

**窗口控制**：逐窗透明度（滑条/热键）、置顶、鼠标点透、笔点透、全部还原、右键卡片菜单。
**悬浮球**：左键展开/收起、双击纸娃娃开主界面、拖动移动、面板 4 按钮（置顶/穿透/模式/还原）+ 透明度滑条、标题栏 MC 风格 ×（关闭）、**显示当前目标软件名【Photoshop】**、**最近输入（鼠标/笔）**、**模式按钮直接显示当前模式（自动/鼠标/笔）**、**右下「操作中」红字表示当前目标正被 Alt 会话操作**。
**主界面**：窗口卡片网格（缩略图 + `【软件名】标题` + 透明滑条 + 置顶）、虚拟桌面胶囊筛选、缩略图预览（帧率可调）、小窗模式、设置页（5 分类）。最小化窗口保留最后一帧并标「已最小化」，不再变成「无预览」。
**设置**：快捷键（战争雷霆式绑定，支持鼠标键）、输入（模式 + 鼠标/笔/数位板三下拉）、窗口与预览、悬浮球与纸娃娃（皮肤/模型/大小/待机动画/显示开关/双击开关）、维护（自检/更新/**开机自启动**/配置目录）。
**兼容**：PS 家族（photoshop/illustrator/aftereffects/lightroom/indesign/premiere）、画世界类（huashijie/artworld/painter/画世界）、OBS/VLC/PotPlayer/Office —— 改完 alpha 追加 `DwmFlush` + `RedrawWindow`，避免留旧帧。
**热键冲突提示**：注册失败（1409）时直接点名最可能的占用软件（实测 ALT+R / ALT+SHIFT+R = NVIDIA App 的游戏内叠加层），并给出「快捷键优先」/换绑两条路，不再只甩 Win32 错误码。
**点透会话**：默认 Alt+反引号 唤出 Alt+Tab 式全屏选择器（窗口缩略图预览；←/→/Tab 或再按热键切下一个，Enter/点击选中，Esc 取消）；选中后临时关闭该窗口的点透、提到最上层并激活，可正常拖动/操作；Esc 或再按热键结束，恢复点透、最小化状态和原 z 序。热键可在「设置 → 快捷键 → 选择/控制窗口」改。
**点透选区（自由形状）**：窗口卡片右键 →「编辑自由形状选区…」→ 在窗口客户区上拖动画多边形；`Enter` 保存、`Backspace/Delete` 删、`Esc` 取消；`Tab` 选圈、`I` 逐圈反向、`[` `]` 调该圈透明度（0–100%）。**每个选区可以单独反向**：正常 = 圈内穿透；反向 = 圈内保留、圈外整片穿透。真·透明洞用置顶且点击穿透的覆盖层贴「下层画面快照」，按洞 mask 与该圈透明度混合（0% 只点透不视觉透，100% 完全盖住目标内容显示下层）。快照是静态的，下方变了右键「刷新透明洞画面」重新抓；覆盖层只在该窗口前台时显示，窗口移动/缩放后先隐藏，刷新后再出现。需要窗口没有开整窗点透；选区只保存在本次运行，重启要重画。
**选择可视化 / 悬浮球气泡**：当前目标窗口外圈出现选择框和「已选中 · 标记」标签，4 秒自动收起（标签/右键「显示选中框」可再开）。左侧窗口列表在选中的窗口下挂一个子项「选中控制 · 标记」：点开展开为窗口卡片（缩略图/状态/透明度），再点收起；卡片里可「选中并显示/编辑选区/清空」。卡片和左侧窗口的右键菜单都有「选中控制」子菜单：标记文字/颜色、显示选中框、逐圈反向/透明度/删除、编辑/清空/刷新透明洞。关闭被控制的窗口后自动清理目标/选区/标记/选择框，不再残留选中界面。全屏选择框已移除，改成悬浮球弹 MC 风格小对话气泡（已选中标记/窗口/蒙版数/右键提示）：气泡放在球窗口上方不压面板，显示 3.2s + 0.6s 渐隐，重复点会重新弹出。每个封闭选区算一个独立蒙版，可以堆多个；右侧「蒙版列表」逐条管理，「新建蒙版」追加一个新的，「编辑当前蒙版」编辑当前那一条。主界面右侧 PS 风格「选区 / 点透」面板：选区图层（眼睛启用/停用、反向、透明度、删除）、属性、从面板选中的圈打开编辑器、新建/清空/刷新、窗口状态；最小化主程序时区域编辑器覆盖层自动隐藏，恢复后自动回来，不再残留。**右键悬浮球**仍会选中球下面那个窗口并弹面板。

---

## 4. 版本历史（本次会话 1.4.2 → 2.16.0）

| 版本 | 关键改动 |
| :--- | :--- |
| 1.4.2 | MC 面板加标题栏/分隔线/深色槽外框；展开改 12 步 ease-out 淡入 + 右滑 |
| 1.5.0 | 待机浮动/摆臂可关（都关掉后静置不重绘）；设置改全屏 + 5 分类 |
| 1.6.0 | 玻璃图标（窗口 + exe，`embed-resource`）；驱动 App 风格：胶囊/导航行/卡片悬停/弹窗投影；设置页淡入 |
| 1.7.0 | 字体换微软雅黑 + **真粗体**（`msyhbd.ttc` 注册 bold family）；头部去 emoji；快捷键编辑内嵌 |
| 1.8.0 | QQ 音乐配色与字号（20/14/12）、卡片标题用雅黑 Bold |
| 1.9.0 | 设置改为主界面内一页；输入设备三下拉 + RawInput 按设备判定笔/鼠标 |
| 2.0.0 | 鼠标键也能当热键（战争雷霆式录入）；删除「鼠标侧键」栏 |
| 2.1.0 | 绑定界面改战争雷霆式：点格录入、右键/清除解绑、冲突标红、空键=未绑定 |
| 2.2.0 | 保存并应用/恢复默认挪到设置页底部右侧 + 状态就地显示 |
| 2.3.0 | **无边框 + 自绘标题栏**；小窗放右下、全部还原放右上；小窗点设置自动回大界；去「检查更新」热键 |
| 2.4.0 | 窗口 1px 描边；显示悬浮球开关；MC × 关球；右键不再关球；托盘唤出；人物居中 + 双击开主界面 |
| 2.5.0 | 悬浮球显示目标窗口；**开机自启动**；PS/画世界兼容重绘；安装包补资源 + SetupIconFile |
| 2.6.0 | 卡片/悬浮球显示**软件名**；修纸娃娃抽畜（水平不跟包围盒）；PS 2023 实测通过 |
| 2.7.0 | 软件名用【】包、去百分比、面板加「最近输入」、软件名缓存（性能）、**帮助按钮 + 图文教程**、兼容列表扩充、D 盘压缩包 |
| 2.7.1 | 「帮助」挪到最小化左边 |
| 2.7.2 | 纸娃娃垂直也改为只按舞台中心（彻底去掉包围盒平移）；静置重绘 0.12s |
| 2.7.3 | 待机浮动回到亚像素定位（取整会变 1px 台阶） |
| 2.7.4 | 纸娃娃描边改到超采样域（去掉半透明灰圈 + 双层描边）；热键冲突提示点名占用软件与解决办法 |
| 2.7.5 | 描边前清掉轮廓外的孤立覆盖飞点，修后脑勺外的"黑刺" |
| 2.7.6 | 最小化窗口保留最后一帧预览（采集失败也回退旧帧）+ 「已最小化」标注 |
| 2.8.0 | **点透会话**：Alt+反引号 全屏选窗口、临时关点透、提顶/拖动、Esc 恢复 z 序/最小化/点透 |
| 2.8.1 | 选择器加窗口缩略图预览 + 再按热键切下一个；悬浮球模式按钮显示当前模式、右下「操作中」红字 |
| 2.9.0 | **自由形状点透选区**：每窗口多个套索多边形，洞内点击穿透到下层；卡片右键编辑/清空 |
| 2.10.0 | 选区**逐圈反向 + 0–100% 透明度**；真·透明洞覆盖层（下层快照）；刷新透明洞画面 |
| 2.11.0 | **选择可视化**：绿框 + 已选中标签；右键悬浮球选窗口并主动弹控制；卡片右键选中并显示控制；Alt 会话弹球 |
| 2.12.0 | 选择框 4 秒自动收起/可点击关闭；主界面「选中控制」卡片堆叠：标记颜色、选区反向/透明度/删除、窗口参数全可操作 |
| 2.13.0 | 选中控制移到右键子菜单；左侧窗口树子项展开窗口卡片；关闭目标自动清理；顶部选中控制 section 移除 |
| 2.14.0 | 最小化时区域编辑器残留修复；右侧 PS 风格选区面板（图层/眼睛/属性/选中圈编辑）；选区 enabled 眼睛 |
| 2.15.0 | 选择框移除；悬浮球 MC 风格对话气泡；一次一个封闭选区（新建选择替换）；右侧面板单选区 |
| 2.16.0 | 气泡移到球窗口上方（不压面板）+ 3.2s 显示/0.6s 渐隐 + 重复点重弹；每个封闭选区=一个独立蒙版，可堆多个；「新建蒙版」追加 |

---

## 5. 踩过的坑（都是硬结论，别再试错）

1. **分层窗口像素格式**：`UpdateLayeredWindow` 要 **BGRA + 预乘 alpha**；`doll.rs` 必须写 B,G,R 顺序否则红蓝互换。
2. **GDI 文字**：要画两遍（`#3F3F3F` 阴影 +1,+1）；字体质量必须 `ANTIALIASED`，用 ClearType 会在 alpha 补齐后出现彩虹边。
3. **启动 GUI**：`Start-Process` 会卡住 pwsh，用 `cmd /c start "" <exe>`；退出只能用 `--quit`，**禁止 `Stop-Process -Force` / `taskkill /F`**（每次会留一个幽灵托盘图标，得重启 explorer 才清）。
4. **PowerShell 改源码**：`Get-Content/Set-Content` 会把 UTF-8 中文搞坏，必须用编辑工具或 Python（`io.open(..., newline="")` 保留 CRLF）。
5. **.NET 静态调用**：`[System.IO.File]::ReadAllText('相对路径')` 是按**进程工作目录**解析，不是 PowerShell 的 `cd` —— 一定用绝对路径（曾因此在 `E:\TransGlass` 造出过 3 字节假 Cargo.toml）。
6. **egui 粗体**：`RichText::strong()` 只改颜色；真粗体要把 `msyhbd.ttc` 注册成 `FontFamily::Name("bold")` 并只给标题用（给 Button 用会让整页复选框都变粗）。
7. **无边框窗口**：`ViewportCommand::InnerSize` 在这台机器只生效一半（宽度不变）→ 改用 `SetWindowPos`；小窗↔大界面的尺寸同步必须放在 `settings_open` 分支**之前**，否则小窗点设置不会放大。
8. **1px 描边**：`rect_stroke` 在 0.5 偏移下会被抗锯齿糊掉（只有右/下边可见）→ 用 Foreground 层画四条 1px `rect_filled`。
9. **windows-rs 返回值**：`Reg*` 系列返回 `WIN32_ERROR`（只能 `.is_err()` + 读 `.0`，没有 `map_err`）；`RedrawWindow`/`RDW_*` 在 `Win32::Graphics::Gdi` 而不是 WindowsAndMessaging。
10. **深色标题栏**：这台 Win10 19045 上 `DWMWA_USE_IMMERSIVE_DARK_MODE=0` 无效、`DWMWA_CAPTION_COLOR` 返回 `E_INVALIDARG` → 最终走无边框自绘。
11. **纸娃娃抖动**：`doll::render` 内部就是以 `(cx,cy)` 为模型中心，再用**逐帧包围盒二次居中**会把 ±1~2px 的 bbox 波动放大成"原地抽畜"，还会把待机浮动抵消 → 位置只按「舞台中心 − 球心」平移。
12. **安装包漏资源**：`[Files]` 只写了 exe 和 manifest 时，装完会没有玻璃图标/皮肤 → 补 `icon.png/icon.ico/ball_glass.png/skin.png/skins\*.png` + `SetupIconFile`。
13. **旧卸载器图标**：老版本装出来的 `unins000.exe` 是 Inno 默认图标；不改安装的话，把开始菜单快捷方式的 `IconLocation` 指到 `transglass.exe,0` 再 `ie4uinit -show` 刷缓存。
14. **性能坑**：窗口列表里查进程名（`OpenProcess`+`QueryFullProcessImageNameW`）必须缓存（现为 3s / 96 条），否则每帧每张卡都查一次。
15. **热键 1409**：`RegisterHotKey` 失败时 Windows 只给 1409，**没有任何接口能查出占用者**（API 只告诉你失败，查不出是谁）。实测 `ALT+R`、`ALT+SHIFT+R` 被 **NVIDIA App 的游戏内叠加层**（`NVIDIA Overlay.exe`）默认占着：前者开关性能统计、后者轮换指标。现在按「进程在跑 + 默认键位」表提示占用者（NVIDIA/AMD/微信/QQ/Snipaste），并引导「快捷键优先」或换绑；优先模式用 `WH_KEYBOARD_LL` 抢在对方前面生效（代价是那个键对对方失效）。UI 里不要再写 `Win32 1409` 这种码。
16. **纸娃娃描边**：旧写法是在降采样后的 mask 上取 8 邻域最大值、`alpha = 0.42 × 邻居覆盖`，结果是一条 1~2px、透明度忽高忽低的半透明灰圈，缩到 3px/皮肤像素时就是"毛边"；`ball_render` 末尾还又套了一层 1px 深色描边。现在描边在**超采样域**膨胀覆盖遮罩（半径 = ss 子采样 = 1 目标像素）、跟模型一起降采样，`alpha = 0.85`，并删掉 `ball_render` 里那一层。单帧多约 0.5ms（30fps 下约 +1.5% 单核）。
17. **纸娃娃孤立飞点**：光栅化 / 皮肤纹理在面边界取整时，偶尔会在轮廓外留下 1~2 个子采样的覆盖点。3×3 超采样下几乎看不见，但 1px 描边会沿它膨胀一圈，放大成后脑勺外面的"黑刺"（调淡描边没用，飞点还在）。修法：描边前对超采样覆盖做 4 邻域连通块标记，**小于 12 个子采样**的块直接清零（`doll.rs` 的 `MIN_COMPONENT`）。每帧约 +0.5–1ms（30fps 下约 +2% 单核）。
18. **最小化窗口的缩略图**：最小化后 `GetWindowRect` 只剩 160×28，`PrintWindow` 只给黑帧（CEF 的 KOOK/Edge 实测）。别在最小化时重抓，直接返回 `self.thumbs` 里的上一帧；采集失败也保留旧帧，别回退成「无预览」。卡片/列表用 `IsIconic` 标「已最小化」。注意：**启动时已经最小化**的窗口没有历史帧，只能显示「已最小化」占位；恢复一下再最小化就有图。
19. **选择器不要"失焦即取消"**：`AttachThreadInput` + `SetForegroundWindow` 抢前台时会收到一次瞬时的 `WM_ACTIVATE(WA_INACTIVE)`。如果在 `WM_ACTIVATE` 里 `cancel_chooser()`，刚显示的选择器会立刻被隐藏（表现为前台是 `TransGlassSelector`、`IsWindowVisible` 却是 false）。失焦不取消，靠 Esc / 再按热键 / 点空白退出。
20. **会话 z 序还原**：选中前记录"上方最近的**同层级**可见窗口"（跳过隐藏窗口和另一个 topmost 分组），结束时 `SetWindowPos` 插回它下面。非 topmost 窗口如果以悬浮球（topmost）为锚点，会被一起顶成 topmost（实测 Notepad 会话结束后 `WS_EX_TOPMOST` 多了一位）。原来最小化的窗口结束时要 `SW_MINIMIZE` 回去。
21. **单设备点透**：会话选中的窗口要临时摘 `WS_EX_TRANSPARENT`，同时 `mouse_hook_proc` 必须跳过它的单设备翻转（`selector::is_session_window`），否则下一次鼠标移动又把透明加回去，选中后还是点不动。结束调 `reset_passthrough_tracking()`，让钩子按当前设备重新判一次。
22. **选择器初选**：唤出时记录光标位置，移动 <6px 不改选中；否则光标刚好压在卡片上会把初始选中（前台窗口）改掉（实测开了选择器就跳到 KOOK）。
23. **选择器缩略图**：用 `capture_window`（`PrintWindow`）在打开选择器时给**非最小化**窗口抓一张，存 BGRA 到 `Preview`，卡片按比例最近邻缩放绘制；最小化/GPU 黑帧退回类图标。抓取是同步的，窗口多时打开会慢几百 ms，**不要每帧抓**。
24. **悬浮球状态刷新**：模式按钮 / 「操作中」状态变了要让 ticker 重画（`BALL_DIRTY` + `ball_mark_dirty`），否则待机浮动和摆臂都关掉时面板会一直显示旧状态；会话选中后要把 `TARGET_HWND` 切到选中窗口（`set_target_window`），否则面板判不出「操作中」。
25. **点透选区别用 `SetWindowRgn`**：用窗口区域挖洞会把窗口**画面也裁掉**（洞变透明/黑），不是"只让输入穿透"。正确做法是低级鼠标钩子按坐标临时给窗口加 `WS_EX_TRANSPARENT`：鼠标进洞窗口让出命中测试，点击自然落到下层；出洞立刻恢复。视觉不裁剪，窗口内容还在。
26. **选区钩子快路径**：`mouse_hook_proc` 里有两条 fast-path（面板/悬浮球不可见且无单设备点透、注册表为空）会提前 `CallNextHookEx`。加了选区后必须把 `regions::has_any()` 放进条件，否则画了选区钩子根本走不到 enforcement（实测完全不生效）。
27. **`UpdateLayeredWindow` 0x8007001F**：在 Update 之前就把 mem DC 里的 DIB `SelectObject` 回旧位图，DC 里已经不是这张 DIB，Update 会报 `HRESULT(0x8007001F)`（设备不工作），窗口可见但内容全透明。必须**先 Update 再取消 DIB 选中**；另外窗口创建时别用 0×0，先 `SetWindowPos` 定尺寸。这两个都踩过。
28. **选区坐标/编辑器**：多边形按**客户区归一化 0..1**保存（不是屏幕坐标），窗口移动/缩放后仍对准；编辑器覆盖客户区屏幕矩形，目标最小化时先 `SW_RESTORE`、结束再 `SW_MINIMIZE`，并记录上方同层级窗口做 z 序还原。客户区为 0/离屏时直接放弃。
29. **真·透明洞的视觉限制**：任意窗口做不了逐像素 alpha，别去改目标窗口的层。做法是**置顶 + `WS_EX_TRANSPARENT` 的覆盖层**：在洞区域贴上「下层画面快照」、按选择透明度写 alpha；目标是前台时覆盖层显示，点击穿透照旧。快照是静态的（捕获时 `SW_HIDE` 目标 → BitBlt 屏幕 → `SW_SHOW`），下方内容变了要「刷新透明洞画面」；窗口移动/缩放后快照位置不对，先隐藏，等刷新。覆盖层必须 `WS_EX_NOACTIVATE`，否则会抢焦点。
30. **逐圈反向**：每圈 mask = 圈内（正常）或客户区补集（反向）；点击穿透 = 所有 mask 的并集（与透明度无关）；视觉 alpha = 包含该点的圈里最大的透明度。编辑器里 `Tab` 选圈、`I` 反向、`[` `]` 调透明度；反向圈画蓝、正常圈画绿、选中圈加金框。键盘事件在编辑器窗口有焦点时用 `WM_KEYDOWN`；用 `keybd_event` 发虚拟键（scan=0）在部分环境不稳，测试时直接 `PostMessage(WM_KEYDOWN)` 更可靠。
31. **选择框**：`selection.rs` 用**全屏置顶 + WS_EX_TRANSPARENT + WS_EX_NOACTIVATE** 的分层窗口画 2px 绿框和标签，绝不能让目标窗口失焦；150ms 定时器读 `TARGET_HWND`，只在目标/矩形/软件名变化时重画（全屏 DIB 8MB，别每帧画）。目标关闭/最小化/矩形为空就 `ShowWindow(SW_HIDE)`。选择框是**置顶覆盖**：目标在后面时框也会显示在其他窗口上——这是有意的"选中指示"，如果嫌打扰可以改成目标前台才显示。
32. **右键悬浮球选窗口**：`WindowFromPoint` 在球中心拿到的是悬浮球自己（own）；要沿 `GW_HWNDNEXT` 往下走，跳过 own/shell，检查矩形确实包含该点，再 `GetAncestor(GA_ROOT)` 取顶层；找到就 `select_window_for_control`（切目标 + 弹面板），找不到就只弹当前目标的面板。不要吞掉目标窗口自己的右键菜单——球上的右键不传给下面。卡片右键的「选中并显示控制」和 Alt 会话走同一个 `select_window_for_control`。
33. **选择框不要做成永久置顶**：4 秒自动收起；标签是独立 topmost 小窗（`WM_LBUTTONUP` 设置 `ENABLED=false`），主界面「显示选中框」或卡片「选中并显示」可重新打开。目标窗口自己如果是 topmost，会盖住主界面/标签，auto-hide 是兜底；不要为了关框去 `SetWindowRgn` 或吞输入。
35. **选中控制移到右键 + 左侧树子项**：顶部「选中控制」section 已移除；卡片/左侧窗口的右键菜单加「选中控制」子菜单（`selection_controls_ui`）。左侧 `nav_windows` 在 TARGET/有选区的窗口下挂 `CollapsingHeader` 子项，展开直接调 `self.window_card`（就是第二张图那种卡片），再点收起；`sel_set` 在 `nav_windows` 里一次算出（target + regions），别放进别的函数。
36. **关闭目标自动清理**：`selection_cleanup()` 在 `refresh_window_list` 开头每帧跑：TARGET_HWND 无效就置 0、`regions::prune` 清无效窗口的选区和 hole style、`selection::prune` 清标记；视觉洞/选择框随之隐藏，左侧子项消失。不清理的话窗口都关了选中界面还在。
37. **最小化时编辑器残留**：区域编辑器是全屏置顶覆盖层；主窗口最小化/隐藏后它还在。`editor_proc` 加 `WM_TIMER`（`begin_edit` 里 `SetTimer(overlay, 2, 200, None)`，`end_edit` 里 `KillTimer`），每 200ms 查 `crate::main_window_handle()`：`IsIconic` 或 `!IsWindowVisible` 就 `ShowWindow(SW_HIDE)`，恢复就 `SW_SHOW` + 前台。
38. **右侧 PS 风格选区面板**：`SidePanel::right("ps_panel")` → `ps_selection_panel`，状态在 `App::ps_selected`；`Region` 新增 `enabled`（图层眼睛），停用后 `region_contains` 返回 false、视觉洞也不画；`update_region` 增加 `enabled: Option<bool>`。`open_editor_at(hwnd, Some(idx))` 用 `PENDING_SELECTED` 从面板选中的圈打开编辑器。拖动/滑条只走 `WM_VISUAL_MASK`，别重抓下层快照。
39. **选择框换成悬浮球 MC 气泡**：全屏选择框删了（`selection::init` 不再调用），目标变化时 `ball_bubble()` 给 `BUBBLE_HWND` 发 `WM_BALL_BUBBLE`；气泡是独立的 `WS_EX_LAYERED|TRANSPARENT|TOOLWINDOW|TOPMOST|NOACTIVATE` 窗口，`bubble_render` 用现有 `draw_mc_panel/mc_fill/draw_mc_label` 画 #C6C6C6 面板 + 像素尾巴，3.5s `WM_TIMER` 收起。尾巴在底部中心，气泡放在球上方（`by-42-BUBBLE_H`）。
40. **一次一个封闭选区**：区域编辑器 `on_up` 不再 push，直接 `st.regions = vec![new]`（替换），所以每窗口最多 1 个圈；面板/右键「新建选择」先 `regions::clear` 再开编辑器，「编辑当前选区」按当前圈打开。类似蒙版：一次一个，新建即替换。
41. **气泡位置/渐隐/重弹**：`bubble_pos()` 把气泡放在球窗口上方（`BALL_POS_Y - BUBBLE_H - 4`），不再压住面板；`bubble_show` 记 `BUBBLE_SHOWN_AT`，`WM_TIMER` 30ms 一帧：3200ms 内保持，3200..3800 用 `SourceConstantAlpha` 渐隐，3800 隐藏；重复点走同一个 `WM_BALL_BUBBLE` → `bubble_show` 重置时间，所以会重新弹。
42. **每个封闭选区=一个独立蒙版**：编辑器 `on_up` 回到 `st.regions.push(...)`，可以堆多个；面板/右键「新建蒙版」只开编辑器（不 clear），新画的追加；「编辑当前蒙版」按选中条打开；右侧「蒙版列表」逐条眼睛/反向/透明度/删除，点透/视觉仍取所有启用蒙版的并集。
34. **选中控制卡片**：主界面 `selection_control_section` 列 `TARGET_HWND` + 所有有选区的窗口；卡片用 `CollapsingHeader` 堆叠，可展开/收起。选区反向/透明度/删除走 `regions::update_region/remove_region` → `WM_VISUAL_MASK` **只重画覆盖层 mask**，不重新抓下层快照（否则每拖一下滑条就 BitBlt/闪）。标记存在 `selection.rs::MARKS`（hwnd → 文字/颜色），`set_mark` 清 `LAST` 让选择框重画；卡片标题和选择框标签都显示标记。

---

## 6. 构建 / 部署 / 打包 / 发布

```powershell
# 0) 环境（Git 与 cargo 都加到 PATH）
$env:PATH="E:\git\Git\cmd;C:\Windows\System32;C:\Users\railgun\.cargo\bin;$env:PATH"

# 1) 编译
cd E:\TransGlass\TransGlass
cargo build --release

# 2) 部署（先优雅退出，再覆盖，再启动）
$dst='E:\新建文件夹\TransGlass\transglass.exe'
& $dst --quit
for ($i=0;$i -lt 20;$i++){ Start-Sleep -Milliseconds 400; if (-not (Get-Process transglass -ErrorAction SilentlyContinue)) { break } }
Copy-Item target\release\transglass.exe $dst -Force
cmd /c start "" $dst

# 3) 自检（期望 7/10 通过、0 失败、3 警告）
& $dst --self-test

# 4) 打安装包（本机 Inno Setup 路径）
& 'E:\daima\002\.tools\innosetup\ISCC.exe' TransGlass.iss
# 产物：dist\installer\TransGlass_<版本>_Setup.exe

# 5) D 盘备份压缩包
$stage = "D:\TransGlass-dist\TransGlass_<版本>"
New-Item -ItemType Directory -Force -Path $stage | Out-Null
Copy-Item "dist\installer\TransGlass_<版本>_Setup.exe" $stage -Force
Copy-Item docs\使用教程.md $stage -Force
Copy-Item docs\img $stage -Recurse -Force
Compress-Archive -Path $stage -DestinationPath "D:\TransGlass-dist\TransGlass_<版本>.zip" -Force

# 6) 提交 / 推送 / 打标签
git add -A; git commit -F <说明文件>; git push origin unified:master unified; git tag -a v<版本> -m "..."; git push origin v<版本>

# 7) GitHub Release（gh 在 D:\github cli\gh.exe，已登录 railgun-1145）
gh release create v<版本> "dist\installer\TransGlass_<版本>_Setup.exe" "D:\TransGlass-dist\TransGlass_<版本>.zip" --title "TransGlass v<版本>" --notes-file <说明文件>
```

**版本号要同步三处**：`Cargo.toml`、`Cargo.lock`（`name = "transglass"` 那段的 version）、`TransGlass.iss` 的 `#define MyAppVersion`。

---

## 7. 产物与发布记录

| 版本 | 安装包 | D 盘压缩包 | Release |
| :--- | :--- | :--- | :--- |
| v2.5.0 | `dist\installer\TransGlass_2.5.0_Setup.exe` | — | ✅ |
| v2.6.0 | `dist\installer\TransGlass_2.6.0_Setup.exe` | — | ✅ |
| v2.7.0 | `dist\installer\TransGlass_2.7.0_Setup.exe` | `D:\TransGlass-dist\TransGlass_2.7.0.zip` | ✅ |
| v2.7.1 | `dist\installer\TransGlass_2.7.1_Setup.exe` | `D:\TransGlass-dist\TransGlass_2.7.1.zip` | ✅ |
| v2.7.2 | `dist\installer\TransGlass_2.7.2_Setup.exe` | `D:\TransGlass-dist\TransGlass_2.7.2.zip` | ✅ |
| v2.7.3 | `dist\installer\TransGlass_2.7.3_Setup.exe` | `D:\TransGlass-dist\TransGlass_2.7.3.zip` | ✅ |
| v2.7.4 | `dist\installer\TransGlass_2.7.4_Setup.exe` | `D:\TransGlass-dist\TransGlass_2.7.4.zip` | ✅ |
| v2.7.5 | `dist\installer\TransGlass_2.7.5_Setup.exe` | `D:\TransGlass-dist\TransGlass_2.7.5.zip` | ✅ |
| v2.7.6 | `dist\installer\TransGlass_2.7.6_Setup.exe` | `D:\TransGlass-dist\TransGlass_2.7.6.zip` | ✅ |
| v2.8.0 | `dist\installer\TransGlass_2.8.0_Setup.exe` | `D:\TransGlass-dist\TransGlass_2.8.0.zip` | ✅ |
| v2.8.1 | `dist\installer\TransGlass_2.8.1_Setup.exe` | `D:\TransGlass-dist\TransGlass_2.8.1.zip` | ✅ |
| v2.9.0 | `dist\installer\TransGlass_2.9.0_Setup.exe` | `D:\TransGlass-dist\TransGlass_2.9.0.zip` | ✅ |
| v2.10.0 | `dist\installer\TransGlass_2.10.0_Setup.exe` | `D:\TransGlass-dist\TransGlass_2.10.0.zip` | ✅ |
| v2.11.0 | `dist\installer\TransGlass_2.11.0_Setup.exe` | `D:\TransGlass-dist\TransGlass_2.11.0.zip` | ✅ |
| v2.12.0 | dist\installer\TransGlass_2.12.0_Setup.exe | D:\TransGlass-dist\TransGlass_2.12.0.zip | ✅ |
| v2.13.0 | `dist\installer\TransGlass_2.13.0_Setup.exe` | `D:\TransGlass-dist\TransGlass_2.13.0.zip` | ✅ |
| v2.14.0 | `dist\installer\TransGlass_2.14.0_Setup.exe` | `D:\TransGlass-dist\TransGlass_2.14.0.zip` | ✅ |
| v2.15.0 | `dist\installer\TransGlass_2.15.0_Setup.exe` | `D:\TransGlass-dist\TransGlass_2.15.0.zip` | ✅ |
| v2.16.0 | `dist\installer\TransGlass_2.16.0_Setup.exe` | `D:\TransGlass-dist\TransGlass_2.16.0.zip` | ✅ |
> 爱发电没有上传 API，需要人工把同一个 `TransGlass_<版本>.zip` 传上去。

---

## 8. 配置与运行数据（安装目录）

| 文件 | 内容 |
| :--- | :--- |
| `transglass_ui.json` | 窗口位置/大小、置顶、预览帧率、皮肤、纸娃娃模型、待机动画开关、悬浮球显示、双击开关、输入设备选择、视图模式 |
| `transglass_hotkeys.json` | 各热键槽位（modifiers + key，鼠标键写成 `鼠标侧键1` 等）+ `hotkey_priority` + 旧版 `mouse` 字段（兼容保留） |
| `hilite.log` | 调试日志（目标窗口、兼容重绘） |
| `transglass_selftest.log` | 自检报告 |
| `skins\*.png` / `skin.png` | 皮肤 |
| `icon.png` / `icon.ico` / `ball_glass.png` | 图标资源（`icon.png` 是那块玻璃，**不要改**） |

**配置结构注意**：`UiState` 新字段必须写成 `Option<T>`，否则老配置反序列化会失败。

---

## 9. 性能基线（单核占用，AMD/Intel 混合测试）

| 场景 | 占用 |
| :--- | :--- |
| 悬浮球窗口隐藏 | ≈ 0（ticker 全跳） |
| 悬浮球显示、静置（0.12s 重绘） | **2–2.5%** |
| 悬浮球 + 鼠标持续移动（30fps） | +9% 左右 |
| 主界面窗口预览（15fps × 每窗 PrintWindow） | **6–12%（真正的 CPU 大头）** |

> 2.7.5 的孤立飞点清理每帧约 +0.5ms（悬浮球单核占用大约 +1~2%）。

嫌吵就把「窗口与预览 → 预览帧率」调低或设 0；悬浮球待机动画两个都关掉后静置完全不重绘。

---

## 10. 已知问题 / 待办

- [ ] Lightroom Classic 没实测（只验证了 Photoshop 2023；代码走同一套名字匹配）
- [ ] 「检查更新」热键已从绑定表移除，但 `HotkeyConfig.update` 字段仍保留（兼容老配置，不再注册）
- [ ] 旧版 `mouse.xbutton1/2` 行为默认仍生效（XButton1=减透明度、XButton2=加透明度）；在槽位里绑了侧键后保存会自动清空旧字段
- [ ] 无边框窗口没有系统圆角/投影/Alt+Space 菜单（可以做，Win11 可用 DWM 圆角）
- [ ] PS 内置面板（UXP 插件）没做；「帮助」目前跳 GitHub 上的 `docs/使用教程.md`
- [ ] 爱发电发布需人工
- [ ] 纸娃娃按 3× 超采样 + 每帧约 2MB 中间缓冲（col / z-buffer / 描边膨胀）；如还要压性能，可改 thread_local 复用缓冲
- [ ] 缩略图预览对 GPU 应用（PS/AE）截图有时是黑帧（PrintWindow 限制），可考虑改 DXGI 抓取（2.7.6 起会保留上一帧，不会变「无预览」）
- [ ] **透明洞快照是静态的**：下层内容/目标窗口移动后要「刷新透明洞画面」；没有做实时逐帧抓取（实时抓会闪、开销大）
- [ ] 透明洞覆盖层只在该目标窗口前台时显示；多个带选区的窗口同时存在时，视觉覆盖层只跟随最近应用/刷新的那个（点击穿透各窗口独立生效）
- [ ] 选择框是**置顶覆盖**：目标被别的窗口挡住时框仍显示；如果觉得打扰，可以改成「目标前台才显示」
- [ ] 选择框只做 4 秒提示，主界面「显示选中框」/卡片「选中并显示」可再打开；目标本身 topmost 时可能盖住主界面/标签，auto-hide 是兜底
- [ ] 右键悬浮球选的是球正下方最上层的非自身窗口；悬浮球盖在哪个应用上就选哪个（这是设计语义）
- [ ] 卡片右键菜单里的「选中并显示控制」和原有菜单项共用；菜单在个别置顶/覆盖场景下的点击测试受环境影响
- [ ] 「选中控制」卡片列的是当前目标 + 所有带选区的窗口；标记和选区都只存内存，TransGlass 重启后要重新设置
- [ ] 卡片的透明度滑条只改视觉洞 mask，不重新抓下层快照；下方画面变了仍需「刷新透明洞」
- [ ] 选择器里**最小化**的窗口只显示类图标 + 「已最小化」，没有缓存的最后一帧（主界面缩略图缓存是 egui 纹理，选择器线程拿不到）；后面可加全局缩略图缓存
- [ ] 选择器 v1 只覆盖主屏，高 DPI 下 GDI 尺寸没按 DPI 缩放（100% 缩放下正常）；多显示器/高 DPI 可再适配
- [ ] 选择器默认热键 Alt+反引号 可在「设置 → 快捷键 → 选择/控制窗口」改；若被系统/输入法占用，按冲突提示换绑或开快捷键优先

---

## 11. 协作约定（来自与开发者的实际协作）

1. **全权代做、直接给结果**：不问"要不要我做"，除非真的二选一有重大代价。
2. **每次改动都要**：改版本号三处 → 编译 → 部署 → `--self-test` → 打安装包 → D 盘 zip → commit → push → tag → `gh release create`。
3. **先验证再宣布**：说"已修复"之前必须有实测（截图/像素值/进程状态/日志）。
4. **UI 语言全中文**，风格：浅色（QQ 音乐风）、MC 风格只用在悬浮球面板、不用 emoji、字体微软雅黑。
5. **不要改 `icon.png`**：那块 MC 玻璃是刻意选的（16px 下像占位符，是"幽灵图标误会"的根源，但设计如此）。
6. **交互参考**：快捷键绑定 = 战争雷霆式（点格录入 / 右键清除 / 冲突标红）；设置 = 主界面内切页而非弹窗。

---

## 12. 上下文恢复指引

新对话接手时按顺序读：

1. 本文件（`ARCHIVE.md`，项目全貌 + 坑）
2. `TECHNICAL_OUTLINE.md`（实现细节，按模块）
3. `src/main.rs` 顶部常量区（热键、悬浮球布局、主题色都在这里）
4. `src/doll.rs` 的 `render()` 签名与 `Pose` 结构
5. `TransGlass.iss`（安装包配置）

快速自检：

```powershell
& 'E:\新建文件夹\TransGlass\transglass.exe' --self-test
Get-Content 'E:\新建文件夹\TransGlass\transglass_selftest.log' -TotalCount 3
```
