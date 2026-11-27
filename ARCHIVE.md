# TransGlass 项目归档 / 上下文总结

> 归档时间：2026-11-28　·　对应版本：**v2.7.5**
> 这篇是给"人"和"AI 助手"共用的：换成新对话、换台机器，读完这份就能接上手。

---

## 1. 项目概览

| 项 | 内容 |
| :--- | :--- |
| 是什么 | Windows 窗口透明化工具：**逐窗透明度 / 置顶 / 鼠标·笔分别点透**，配一个 MC 风格纸娃娃悬浮球当遥控器 |
| 仓库 | <https://github.com/railgun-1145/TransGlass>（分支 `unified`，推送目标 `master`） |
| 发布 | <https://github.com/railgun-1145/TransGlass/releases> |
| 教程 | [docs/使用教程.md](docs/使用教程.md)（软件里「帮助」按钮直达） |
| 当前版本 | **v2.7.5** |
| 技术栈 | Rust 1.94 + eframe/egui 0.28 + tray-icon 0.19 + windows 0.58 + image 0.25 |
| 形态 | 单 exe、免登录、本地运行；Inno Setup 安装包（安装路径可选） |
| 部署目录 | `E:\新建文件夹\TransGlass\`（开发机上的实际运行位置） |
| 自检 | `transglass.exe --self-test` → 7/10 通过、**0 失败**、3 警告（正常） |

---

## 2. 代码结构

```
src/main.rs      ~6000 行：Win32 窗口控制 / 托盘 / 热键 / 悬浮球 / egui 主界面 都在这里
src/doll.rs      MC 3D 纸娃娃软件光栅器（6 个 box + 皮肤 UV + 背面剔除 + 画家算法 + z-buffer + 3×3 超采样 + 超采样域 1px 描边）
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
| 输入设备 | RawInput 枚举（鼠标 0x01/0x02、笔 0x0D/0x02、触摸 0x0D/0x04）+ `RIDEV_INPUTSINK` 旁听线程，按设备判断笔/鼠标 |
| 悬浮球 | 独立分层窗口（`UpdateLayeredWindow`）+ CPU 渲染；展开面板是 **MC 背包/设置界面风格**（`#C6C6C6` 底 + 黑框 + 斜面 + `#8B8B8B` 按钮） |
| 纸娃娃 | `doll.rs` 直接写 BGRA 到内存位图；朝向跟光标，yaw 限幅 ±62°，永不露背面 |
| 主界面 | egui 浅色主题（QQ 音乐风：纯白 + `#F7F7F7` 侧栏 + `#2FC27C` 绿 + 字号 20/14/12） |
| 标题栏 | **无边框窗口 + 自绘**：玻璃图标 + 标题 + 「帮助」+ 最小化/最大化/关闭；拖动=StartDrag、双击=最大化、边缘 6px=BeginResize |
| 托盘 | 左键=打开主界面、右键=菜单（唤出悬浮球 / 全部还原 / 退出） |
| 开机自启 | `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` 下的 `TransGlass` 值 |

---

## 3. 当前功能清单（v2.7.5）

**窗口控制**：逐窗透明度（滑条/热键）、置顶、鼠标点透、笔点透、全部还原、右键卡片菜单。
**悬浮球**：左键展开/收起、双击纸娃娃开主界面、拖动移动、面板 4 按钮（置顶/穿透/模式/还原）+ 透明度滑条、标题栏 MC 风格 ×（关闭）、**显示当前目标软件名【Photoshop】**、**最近输入（鼠标/笔）**。
**主界面**：窗口卡片网格（缩略图 + `【软件名】标题` + 透明滑条 + 置顶）、虚拟桌面胶囊筛选、缩略图预览（帧率可调）、小窗模式、设置页（5 分类）。
**设置**：快捷键（战争雷霆式绑定，支持鼠标键）、输入（模式 + 鼠标/笔/数位板三下拉）、窗口与预览、悬浮球与纸娃娃（皮肤/模型/大小/待机动画/显示开关/双击开关）、维护（自检/更新/**开机自启动**/配置目录）。
**兼容**：PS 家族（photoshop/illustrator/aftereffects/lightroom/indesign/premiere）、画世界类（huashijie/artworld/painter/画世界）、OBS/VLC/PotPlayer/Office —— 改完 alpha 追加 `DwmFlush` + `RedrawWindow`，避免留旧帧。
**热键冲突提示**：注册失败（1409）时直接点名最可能的占用软件（实测 ALT+R / ALT+SHIFT+R = NVIDIA App 的游戏内叠加层），并给出「快捷键优先」/换绑两条路，不再只甩 Win32 错误码。

---

## 4. 版本历史（本次会话 1.4.2 → 2.7.5）

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
- [ ] 缩略图预览对 GPU 应用（PS/AE）截图有时是黑帧（PrintWindow 限制），可考虑改 DXGI 抓取

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
