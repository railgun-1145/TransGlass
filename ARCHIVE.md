# TransGlass 项目归档 / 上下文总结

> 归档时间：2026-11-28　·　对应版本：**v2.21.1**
> 这篇是给"人"和"AI 助手"共用的：换成新对话、换台机器，读完这份就能接上手。

---

## 1. 项目概览

| 项 | 内容 |
| :--- | :--- |
| 是什么 | Windows 窗口透明化工具：**逐窗透明度 / 置顶 / 鼠标·笔分别点透**，配一个 MC 风格纸娃娃悬浮球当遥控器 |
| 仓库 | <https://github.com/railgun-1145/TransGlass>（分支 `unified`，推送目标 `master`） |
| 发布 | <https://github.com/railgun-1145/TransGlass/releases> |
| 教程 | [docs/使用教程.md](docs/使用教程.md)（软件里「帮助」按钮直达） |
| 当前版本 | **v2.21.1** |
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
src/regions.rs   自由形状点透选区：窗口上直接画（矩形/椭圆/自由 + 确定）＋ 洞内点击穿透 ＋ 真·透明洞覆盖层（下层快照，窗口动洞跟着动）
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
| 点透选区 | `regions.rs`：**在目标窗口上直接画**（微信截图/画图式覆盖层：矩形 / 椭圆 / 自由 + 重画 / 取消 / 确定，Enter/Esc 快捷键），点确定生成一个蒙版；按客户区归一化保存；低级鼠标钩子在洞内临时加 `WS_EX_TRANSPARENT`、出洞恢复；**新蒙版默认透明度 0% = 只点透、不改外观**（视觉洞要手动拉透明度）；真·透明洞用点击穿透的置顶覆盖层贴「下层快照」；窗口移动/缩放后洞跟着窗口走（`follow_visual` 重抓）；整窗点透的窗口上不生效 |
| 选择可视化 / 悬浮球气泡 + 蒙版 | 悬浮球 MC 气泡放在球正上方（距球 6px），**1.8s + 0.5s 渐隐**，重复点重弹；**气泡可点击**（点一下打开主界面）；**气泡与球面板互斥**：选中窗口只弹气泡，点球开面板时气泡隐藏，面板展开时不弹气泡；拖动悬浮球/主界面时立刻收起气泡；每个窗口多个独立蒙版，右侧「蒙版列表」每行带小预览 |
| 输入设备 | RawInput 枚举（鼠标 0x01/0x02、笔 0x0D/0x02、触摸 0x0D/0x04）+ `RIDEV_INPUTSINK` 旁听线程，按设备判断笔/鼠标 |
| 悬浮球 | 独立分层窗口（`UpdateLayeredWindow`）+ CPU 渲染；展开面板是 **MC 背包/设置界面风格**（`#C6C6C6` 底 + 黑框 + 斜面 + `#8B8B8B` 按钮） |
| 纸娃娃 | `doll.rs` 直接写 BGRA 到内存位图；朝向跟光标，yaw 限幅 ±62°，永不露背面 |
| 主界面 | egui 浅色主题（QQ 音乐风：纯白 + `#F7F7F7` 侧栏 + `#2FC27C` 绿 + 字号 20/14/12） |
| 标题栏 | **无边框窗口 + 自绘**：玻璃图标 + 标题 + 「帮助」+ 最小化/最大化/关闭；拖动=StartDrag、双击=最大化、边缘 6px=BeginResize |
| 托盘 | 左键=打开主界面、右键=菜单（唤出悬浮球 / 全部还原 / 退出） |
| 开机自启 | `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` 下的 `TransGlass` 值 |

---

## 3. 当前功能清单（v2.17.0）

**窗口控制**：逐窗透明度（滑条/热键）、置顶、鼠标点透、笔点透、全部还原、右键卡片菜单。
**悬浮球**：左键展开/收起、双击纸娃娃开主界面、拖动移动、面板 4 按钮（置顶/穿透/模式/还原）+ 透明度滑条、标题栏 MC 风格 ×（关闭）、**显示当前目标软件名【Photoshop】**、**最近输入（鼠标/笔）**、**模式按钮直接显示当前模式（自动/鼠标/笔）**、**右下「操作中」红字表示当前目标正被 Alt 会话操作**。
**主界面**：窗口卡片网格（缩略图 + `【软件名】标题` + 透明滑条 + 置顶）、虚拟桌面胶囊筛选、缩略图预览（帧率可调）、小窗模式、设置页（5 分类）。最小化窗口保留最后一帧并标「已最小化」，不再变成「无预览」。
**设置**：快捷键（战争雷霆式绑定，支持鼠标键）、输入（模式 + 鼠标/笔/数位板三下拉）、窗口与预览、悬浮球与纸娃娃（皮肤/模型/大小/待机动画/显示开关/双击开关）、维护（自检/更新/**开机自启动**/配置目录）。
**兼容**：PS 家族（photoshop/illustrator/aftereffects/lightroom/indesign/premiere）、画世界类（huashijie/artworld/painter/画世界）、OBS/VLC/PotPlayer/Office —— 改完 alpha 追加 `DwmFlush` + `RedrawWindow`，避免留旧帧。
**热键冲突提示**：注册失败（1409）时直接点名最可能的占用软件（实测 ALT+R / ALT+SHIFT+R = NVIDIA App 的游戏内叠加层），并给出「快捷键优先」/换绑两条路，不再只甩 Win32 错误码。
**点透会话**：默认 Alt+反引号 唤出 Alt+Tab 式全屏选择器（窗口缩略图预览；←/→/Tab 或再按热键切下一个，Enter/点击选中，Esc 取消）；选中后临时关闭该窗口的点透、提到最上层并激活，可正常拖动/操作；Esc 或再按热键结束，恢复点透、最小化状态和原 z 序。热键可在「设置 → 快捷键 → 选择/控制窗口」改。
**点透选区（窗口上直接画）**：右侧「蒙版 / 点透」面板只做**预览**，实际画选区是点「**在窗口上画选区**」（或右键「选中控制 → 在窗口上画选区」）→ 目标窗口上出现压暗覆盖层 + 顶部工具栏：**矩形 / 椭圆 / 自由** 三种工具，拖出封闭图形（框内高亮、框外压暗，像微信截图），点「**确定**」生成一个蒙版（Enter 确定、Esc 取消、Backspace/Delete 重画）。生成后回到面板：上方缩略图预览 + 蒙版列表，每行一张 56×36 小预览，点行选中（大图金框），行内可单独**眼睛 / 反向 / 0–100% 透明度 / 删除**。**新蒙版默认透明度 0%：只做点击穿透、不改窗口外观**——视觉洞贴的是下层快照，默认打开会让下层窗口文字叠在本窗口文字上，看起来像"文字错位"；想要视觉洞，在列表里把透明度拉起来即可（快照在创建时已抓好，不用重抓）。**窗口拖动/缩放时洞跟着窗口走**（`follow_visual` 重抓）；抓下层快照时会临时藏起主界面，所以洞里不会出现 TransGlass 自己的界面。需要窗口没有开整窗点透；选区只保存在本次运行，重启要重画。
**选择可视化 / 悬浮球气泡**：当前目标窗口外圈出现选择框和「已选中 · 标记」标签的旧方案已换成悬浮球气泡。左侧窗口列表在选中的窗口下挂一个子项「选中控制 · 标记」：点开展开为窗口卡片（缩略图/状态/透明度），再点收起；卡片里可「选中并显示/在窗口上画选区/清空」。卡片和左侧窗口的右键菜单都有「选中控制」子菜单：标记文字/颜色、显示选中框、逐圈反向/透明度/删除、在窗口上画选区/清空/刷新透明洞。关闭被控制的窗口后自动清理目标/选区/标记/选择框，不再残留选中界面。全屏选择框已移除，改成悬浮球弹 MC 网易版聊天框气泡（已选中标记/窗口/蒙版数/点这里打开主界面）：深色半透明底 + 像素边框 + 白字暗影 + 像素尾巴，放在球正上方（距球 6px），**显示 1.8s + 0.5s 渐隐**，重复点会重新弹出；**点气泡直接打开主界面**；**拖动悬浮球或主界面时立刻收起气泡**；气泡和球面板互斥。每个窗口可以有多个独立蒙版，右侧面板逐条管理。**右键悬浮球**仍会选中球下面那个窗口并弹面板。


---

## 4. 版本历史（本次会话 1.4.2 → 2.17.0）

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
| 2.17.0 | 气泡贴近球（6px）；气泡与球面板互斥：选中只弹气泡，点球开面板气泡隐藏，面板展开不弹气泡 |
| 2.18.0 | **编辑器改 PS 风格直出**：右侧面板顶部目标缩略图直接圈蒙版（完成/取消，完成即生效，去掉全屏覆盖层入口）；蒙版列表行内 56×36 小预览并与大缩略图联动；气泡改 MC 网易版聊天框（深色半透明底 + 像素边框 + 白字暗影 + 像素尾巴，1.8s + 0.5s 渐隐，拖动球/主界面立刻收起）；窗口动洞跟着动（follow_visual） |
| 2.19.0 | **选区改回窗口上直接画**（微信截图/画图式）：压暗覆盖层 + 矩形/椭圆/自由工具 + 重画/取消/确定（Enter/Esc），确定生成一个蒙版；右侧面板只做预览；**新蒙版默认透明度 0%（只点透不改外观）**，修掉"透明处文字和本窗口文字错位"；抓下层快照时藏起主界面；气泡**可点击打开主界面**；面板加滚动条 |
| 2.19.1 | **修左侧黑缝 + 压缩 UI**：长标题把侧栏内容撑宽（内容 478 > 面板 280），中央面板被迫右移、中间留一条黑缝——改成截断树/标题、侧栏固定 280 灰底、窗口 clear_color 改白色兜底；右侧面板去掉底部大按钮，清空/刷新挪到「蒙版列表」标题行，提示压到两行内，标题单行截断；编辑器工具栏文字改非抗锯齿 + 选中工具改浅绿底深绿字 |
| 2.19.2 | **左右侧栏恢复可拖**：左栏 240–420（默认 280）、右栏 220–420（默认 256），都可以拖分隔线调宽度；树子项标题截断到 10 字、窗口行 18 字，保证拖到最窄也不撑宽、不出黑缝；右侧面板内容加 `set_max_width` 防溢出 |
| 2.19.3 | **修蒙版点透不生效（蒙版不是空壳了）**：`WS_EX_TRANSPARENT` 只有在窗口本身是 layered 时才真的让鼠标穿过去——进洞时给没 layered 的窗口临时补 `WS_EX_LAYERED + SetLayeredWindowAttributes(当前alpha)`，出洞撤销；洞里左键/右键现在都会落到下层窗口；另外洞里点击不再把控制目标切到下层窗口 |
| 2.19.4 | **拉蒙版透明度能立刻看到效果**：视觉洞原来只在目标窗口是前台时显示，面板在前台拉滑块时啥也看不到（现在洞内点击又会穿到下层，点不回目标）。改成「目标前台，或主界面在前台且没压在目标窗口上」都显示；`follow_visual` 用 `crate::root_hwnd()` 判断主界面矩形是否和目标客户区相交 |
| 2.19.5 | **选别的窗口不再让洞消失、拖动不再闪**：显示条件从"谁在前台"改成**遮挡判断**（在洞里取采样点 `WindowFromPoint`，顶层不是目标/不是自己的覆盖层就藏）——切到别的窗口时洞还在，主界面/别的窗口压住洞时自动藏；拖动/缩放时只更新 rect 并藏覆盖层，连续两次定时器都不动才重抓快照，避免每帧藏/显目标窗口导致的闪烁 |
| 2.19.6 | **拖动/松手彻底不闪、洞内容对齐**：视觉洞改成抓**整块虚拟屏幕**的下层快照（去掉目标、主界面、覆盖层后 BitBlt 一次），之后窗口拖动/缩放只是从这张图里按新位置裁一块重画——不再重抓、不再藏目标窗口，松手不会闪，洞里的内容也严格对齐屏幕坐标。`draw_visual` 按 `(屏幕坐标 - 快照原点)` 取像素；`follow_visual` 发现 rect 变了就更新 rect + 重画，不重抓 |
| 2.19.7 | **鼠标移进洞里洞不再消失**：`enforce_at` 在鼠标进洞时会把目标临时设成 `WS_EX_TRANSPARENT`，这时 `WindowFromPoint` 会跳过目标返回下层窗口，遮挡判断误以为"被挡住"→ 洞被隐藏（看着像"放上去就复原"）。修法：`follow_visual` 先看 `HOLE_STYLES` 里有没有目标，正在强制点透时跳过遮挡判断 |
| 2.19.8 | **洞跟窗口更紧**：视觉洞定时器 200ms → 50ms，拖动窗口时洞按当前 rect 从整屏快照重裁，跟手很多（依然不重抓、不闪）；配合 2.19.7 的悬停修复，鼠标在洞里也不会再"复原" |
| 2.19.9 | **透明度真正作用在窗口自己身上**：洞里不再只是"把下层快照按 alpha 盖上去"，而是用 PrintWindow 抓一张窗口客户区画面，按 `窗口×(1-透明度) + 下层×透明度` 混合成不透明像素——0% 无变化、100% 完全透出下层，半透明时看到的是"窗口自己的画面变淡"，不再依赖窗口实时的像素层 |
| 2.20.0 | **视觉洞改成真挖洞（SetWindowRgn）**：透明度 > 0 的蒙版直接把窗口 region 按洞剪掉——洞里的下层画面是**实时**的、自动跟着窗口走、点击天然落到下层，彻底告别快照/错位/不跟随/悬停复原。0 < 透明度 < 100 时再用 PrintWindow 的窗口画面按 (1-透明度) 叠在实时下层上，做出"窗口这块变淡"的效果；100% 就是纯洞。退出时 `restore_all_holes()` 把窗口恢复成矩形 |
| 2.20.1 | **半透明不再"真消失"**：2.20.0 把透明度拉到 100% 时窗口画面整个消失（用户要的是"半透明"）。现在覆盖层在 0<透明度≤100 都显示，窗口画面**保底 30% 不透明度**——拉到 100% 也是"很透但还看得见上层"。另外窗口画面改用**屏幕截取**兜底（Chromium/KOOK 这类 PrintWindow 抓不到），并在挖洞前先抓，保证覆盖层有内容可叠 |
| 2.20.2 | **回退挖洞，恢复半透明**：2.20.0/2.20.1 的 SetWindowRgn 在部分窗口（微信/Chromium）上要么不生效、要么把窗口画面挖没了，出现"有蒙版不显示"。恢复 2.19.9 的"窗口画面 + 下层快照"混合方案：窗口画面改成**屏幕截取**（Chromium/KOOK 也能抓到），下层最多压到 70%，窗口画面至少保留 30%——任何透明度下都是半透明，不会消失；0% 仍然只点透不改外观 |
| 2.20.3 | **洞不再盖到上层页面**：覆盖层的遮挡判断之前用 `WindowFromPoint`，会命中我们自己 topmost 的覆盖层（以为“目标没被挡”），于是洞会画在目标上方的其他窗口（DSH/微信）上面。改成按 z 序从最上面往下找「包含该点的、不是我们进程的窗口」，不是目标就藏起覆盖层——目标被别的页面盖住时不会再把洞画上去 |
| 2.20.4 | **洞跟随提到 60fps**：视觉洞定时器 50ms → 16ms，拖动时覆盖层跟手很多，减少「留在原地的重影」；但下层画面仍是静态快照，窗口移到没截过的区域/下层变化时内容会旧——彻底解决要上 GPU 实时采集（DXGI Desktop Duplication / DWM 缩略图） |
| 2.21.0 | **GPU 实时光洞（DWM 缩略图）**：新增 `gpu_hole` 模块——置顶无边框覆盖层 + `SetWindowRgn` 只留洞区 + `DwmRegisterThumbnail` 把**目标窗口下面那扇窗口**实时贴进洞里，透明度直接用缩略图 opacity，全部由 DWM 在 GPU 上合成（不再 CPU 逐像素）。缩略图注册失败/下层是桌面时自动回退到 2.20.4 的 CPU 快照方案 |
| 2.21.1 | **洞里浮出自己的面板**：下层快照抓屏时只在「主界面矩形与目标客户区相交」时才藏主界面；但目标 hwnd 可能和面板显示的不是同一个（微信多窗口），相交判断漏掉，主界面被拍进快照，洞里就浮出一块 TransGlass 自己的 UI。改成抓快照前**无条件隐藏主界面** |

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
41. **气泡位置/渐隐/重弹**：`bubble_pos()` 把气泡放在球窗口上方（`BALL_POS_Y - BUBBLE_H - 4`），不再压住面板；`bubble_show` 记 `BUBBLE_SHOWN_AT`，`WM_TIMER` 30ms 一帧：**1800ms 内保持，1800..2300 用 `SourceConstantAlpha` 渐隐，2300 隐藏**；重复点走同一个 `WM_BALL_BUBBLE` → `bubble_show` 重置时间，所以会重新弹。**拖球（`ball_proc` 的 `WM_MOUSEMOVE` + `BALL_DRAGGING`/`BALL_MOVED`）和拖主界面标题栏（`draw_titlebar` 的 `resp.dragged()`）都直接 `bubble_hide()`**，别让置顶气泡挡在拖动路径上。
42. **每个封闭选区=一个独立蒙版**：编辑器 `on_up` 回到 `st.regions.push(...)`，可以堆多个；面板/右键「新建蒙版」只开编辑器（不 clear），新画的追加；「编辑当前蒙版」按选中条打开；右侧「蒙版列表」逐条眼睛/反向/透明度/删除，点透/视觉仍取所有启用蒙版的并集。
43. **气泡贴近+与球面板互斥**：`bubble_pos` y = 球中心 - 42 - BUBBLE_H - 6（球正上方 6px）；`select_window_for_control` 不再 `ball_popup_panel()`，选中只弹气泡；`ball_set_expanded(true)` 里 `bubble_hide()`；`bubble_show` 开头 `BALL_EXPANDED` 直接 return。三者保证气泡/面板只显示一个。
44. **窗口上画选区（2.19.0）**：编辑器还是 `regions.rs` 的覆盖层线程 + `EditState`，但交互换成"画图式"：顶部工具栏（矩形 / 椭圆 / 自由 / 重画 / 取消 / 确定）在 `bar_layout()` 里算矩形、`hit_bar()` 命中；`on_down` 分工具栏/画布两条路，画布拖出形状存 `pending`（归一化），点「确定」→`confirm_edit` 追加 Region →`end_edit(save=true)`→`set_regions`（**同步等透明洞抓完快照**再恢复主界面）。`begin_edit` 会先 `crate::hide_root_for_overlay()` 把主界面藏起来：画布干净，下层快照也不会把主界面拍进去。旧的「面板缩略图上直接圈」实现（`ps_points`/`ps_draw`）已删掉，面板只剩预览。**GDI 文字在分层窗口上必须补 alpha**：`draw_editor` 画完工具栏后要把工具栏整行的 alpha 置 255，否则 GDI 写的文字像素 alpha=0，在 `UpdateLayeredWindow` 里变成"透明的字"（实测按钮文字只剩白糊）。工具栏按钮的 `fill_rect` 都是不透明底，补 alpha 是安全的。
45. **窗动洞动**：`follow_visual()` 以前发现目标窗口矩形变化就 `ShowWindow(SW_HIDE)` 等用户刷新；2.18.0 改成直接 `refresh_visual(target)`（重抓下层快照 + 重画覆盖层）。注意 `refresh_visual` 内部会 `SW_HIDE` 目标再 BitBlt 屏幕，所以拖动窗口时覆盖层会跟着抖一帧——目前接受；如果以后要做实时跟随，得改成"只挪覆盖层位置 + 用旧快照"，别再每帧抓屏。
46. **命中缓存把面板点击算给别的窗口**：`hit_root_at` 有一份 `HIT_CACHE`（root hwnd + 矩形），鼠标钩子里按"点在缓存矩形内"直接返回。2.18.0 右侧面板要频繁点主界面，结果上次点过的大窗口（铺满屏幕的 PS/浏览器）矩形把面板上的点击也算成它，`TARGET_HWND` 被改走、画的蒙版跑到别的窗口上（实测目标突然跳成后面的浏览器）。修法：`hit_root_at_ex(x, y, use_cache)`，**点击落点走 `hit_root_at_fresh` 不吃缓存**，鼠标移动仍用缓存省 WindowFromPoint。
47. **洞里的文字和本窗口文字"错位"**：透明洞贴的是**下层画面快照**，如果下层是别的窗口（哪怕只是浏览器/聊天框），洞内就会透出它的文字，叠在本窗口文字上，看起来就是"文字错位"。另外抓快照时如果 TransGlass 主界面压在目标上，主界面也会被拍进洞里。两处一起修：① `capture_visual` 抓之前，若主界面与目标客户区有交集就先 `SW_HIDE`，抓完再恢复（不交集就不动，避免目标拖动时反复闪）；② **新蒙版默认透明度 0%**（只点透、不改外观），要视觉洞才手动拉透明度（`apply_regions` 即使透明度 0 也先抓一张快照存着，之后拉滑块只走 `WM_VISUAL_MASK` 不重抓）。
48. **气泡点了没反应**：气泡以前是 `WS_EX_TRANSPARENT` 的纯提示层，点它当然没反应。2.19.0 去掉 `WS_EX_TRANSPARENT`、`bubble_proc` 处理 `WM_LBUTTONUP` → `show_root_window()` + `bubble_hide()`，鼠标悬停换成手型；保留 `WS_EX_NOACTIVATE`，点气泡不抢焦点，只把主界面叫出来。第三行提示文字也从"右键悬浮球管理"改成"点这里打开主界面"。
49. **侧栏长标题把面板撑宽 → 中间一条黑缝**：`SidePanel::left("nav_panel")` 里如果有超宽内容（比如「选中控制 · 完整窗口标题」的 `CollapsingHeader`，窗口标题一长就到 470+），egui 会把面板的 response rect 撑到内容宽度，下一个 `CentralPanel` 只能从被撑开的 x 开始，于是面板背景和中央内容之间露出窗口的 **clear color（默认透明黑）**，表现为一条几十像素的黑竖条，还白吃掉中央空间。修法：① 长标题截断（树里的子项 10 字、窗口行 18 字）；② 左栏**可拖**但限幅 240–420（默认 280）、灰底；右栏也可拖，限幅 220–420（默认 256）；③ 两个面板内容都 `ui.set_max_width(ui.available_width())`，别反过来把面板撑大；④ `eframe::App::clear_color` 改成白色兜底。**以后往窄侧栏里加内容，都要保证不会把面板撑宽。**
50. **编辑器工具栏白字变浅绿糊块**：13px 中文用 `ANTIALIASED_QUALITY` 时笔画像素是"白+背景绿"的中间色，补 alpha 后就变成不透明的浅绿，看起来像糊块。改成 `NONANTIALIASED_QUALITY`（3），笔画实心；选中的工具按钮也从"绿底白字"改成"浅绿底 + 深绿字 + 绿框"，小字号才看得清。
51. **蒙版点透了但还是点不动（空壳）**：`WS_EX_TRANSPARENT` 单独加在**非 layered** 窗口上并不会让鼠标穿过去——它只对 layered 窗口（`WS_EX_LAYERED` + `SetLayeredWindowAttributes`）生效。整窗点透一直能用是因为 `apply_transparency_to_hwnd` 永远会补 `WS_EX_LAYERED`；而洞的 `enforce_at` 只调 `set_hwnd_mouse_transparent`（只改 TRANSPARENT 位），目标窗口没被调过透明度时就不是 layered，于是洞"看着有、点不动"。修法：新增 `crate::add_hole_passthrough` / `remove_hole_passthrough`，进洞时若窗口没 LAYERED 就补上并 `SetLayeredWindowAttributes(当前 alpha 或 255)`，同时记到 `HOLE_LAYERED`，出洞/清空/失效时撤掉。**验证方法**：两个窗口上下叠，洞内左键应把前台切到下层窗口、右键弹的是下层的菜单；同时 `GetWindowLongW(GWL_EXSTYLE)` 在洞内应看到 `0x20|0x80000`，离开后恢复。另一个坑：洞里点击会命中下层窗口，鼠标钩子的"点击=切换控制目标"要先用 `regions::hole_at()` 排除，否则点洞会把面板目标切到下面那个窗口。
52. **拉透明度看不到效果**：视觉洞的 `follow_visual` 只认 `GetForegroundWindow() == 目标窗口`。在面板里拉滑块时前台是主界面，覆盖层一直藏着；而洞内点击现在又会穿到下层，用户很难把目标窗口点回前台，于是"透明度没用"。修法：加 `crate::root_hwnd()`，显示条件改成 **目标前台 或（主界面前台且主界面矩形与目标客户区不相交）**——面板在旁边时拉滑块立刻能看到洞里的下层画面；主界面压在目标上时仍然隐藏，避免覆盖层糊到面板上。视觉线程本来就有 200ms 的 `WM_TIMER → follow_visual`，所以前台变化后最多 200ms 就会显示/隐藏，不用额外消息。
53. **切窗口洞消失 / 拖动闪烁**：显示条件绑"前台"太脆——选别的窗口（前台变了）洞就没了；而 `follow_visual` 发现目标矩形变了就 `refresh_visual`，`refresh_visual` 每次都会 `SW_HIDE` 目标再 BitBlt，于是拖动时每 200ms 闪一下。修法：① 改**遮挡判断**：在洞里取采样点（先试每个选区包围盒中心，再 5×5 网格），`WindowFromPoint + GetAncestor(GA_ROOT)`，顶层既不是目标也不是自己的 `TransGlassHole` 覆盖层就藏——切窗口不受影响，主界面/别的窗口压上来会正确隐藏。注意覆盖层是 `WS_EX_TRANSPARENT|WS_EX_LAYERED`，本身会被 `WindowFromPoint` 跳过，所以采样点命中的是目标窗口。② 拖动时只记 `MOVE_SEQ` + 更新 `VS.rect` + 藏覆盖层；`WM_TIMER` 里连续两次 `MOVE_SEQ` 没变化（约 400ms 没动）才 `refresh_visual` 重抓一次，拖动过程不再反复藏/显目标。
54. **松手还是闪一下 / 洞内容错位**：`capture_visual` 抓的快照只有「目标客户区」那么大，而且每次都要 `SW_HIDE` 目标 + 睡 80ms + 恢复——松开鼠标触发一次重抓就闪一下；窗口移动后旧快照按新 rect 画，内容还会错位。修法：**抓整块虚拟屏幕**（`SM_XVIRTUALSCREEN` 等四个指标，BitBlt 一次），连同 `origin` 存进 `VS`；`draw_visual` 每个像素取 `(rect.x + x - origin.x, rect.y + y - origin.y)`；`follow_visual` 发现目标 rect 变了只更新 `VS.rect` + `draw_visual` 重裁，**不再重抓、不再藏目标窗口**。代价：快照是整屏（1920×1080 约 8MB），下层画面变了要手动「刷新」；换来了拖动/松手完全无闪、洞内容和屏幕严格对齐。
55. **鼠标一放进洞，洞就消失（"复原"）**：`enforce_at` 在鼠标进洞时给目标加 `WS_EX_TRANSPARENT`，于是 `WindowFromPoint` 跳过目标、返回**下层窗口**；`follow_visual` 的遮挡判断看到"顶层不是目标"就以为被挡住，把覆盖层藏了——表现就是鼠标移上去洞就没了。修法：遮挡判断前先查 `HOLE_STYLES`，目标正在被我们强制点透时不判遮挡（`enforced == true` 直接显示）。**凡是"洞里点击穿透"的样式切换，都不能拿来当遮挡判据。**
56. **洞"留在原地/不跟随/错位/变小"（快照方案的天花板）**：视觉洞一直是"把下层画面贴到洞区"——快照是静态的：窗口一动、下面内容一变，洞里的画面就停在旧位置；半透明时又和实时窗口层叠加，出现重影/大小对不上。2.20.0 换成**真挖洞**：`SetWindowRgn(hwnd, rect - 洞多边形)`（窗口坐标，洞点按客户区归一化坐标 × 客户区尺寸 + 客户区在窗口里的偏移）。于是洞里的下层是实时的、跟着窗口走、点击天然穿透，不需要 `WS_EX_TRANSPARENT` 也就不用再担心悬停复原。只给「透明度 > 0」的蒙版挖洞（0% 仍然只点透、不改外观）；0<透明度<100 时用 PrintWindow 的窗口画面按 (1-透明度) 叠回去；100% 不显示覆盖层。窗口缩放后重挖（`reapply_cuts_if_resized`，50ms 定时器里查尺寸），退出时 `restore_all_holes()` 恢复矩形。**SetWindowRgn 会接管 region 的所有权，不要再 DeleteObject。** 坑：窗口被 SetWindowRgn 后会失去系统阴影/圆角，属于挖洞的固有代价。
57. **透明度拉满后上层画面"真消失"**：2.20.0 在 100% 时完全靠挖洞、覆盖层隐藏，用户要的是"半透明"而不是整块消失。修法：`draw_visual` 的窗口内容不透明度改成 `max((100-透明度)*255/100, 76)`（保底 30%），并且在透明度 > 0（含 100%）时都显示覆盖层——任何设置下都能看到上层画面。另一个坑：KOOK/Chromium 这类窗口 `PrintWindow(PW_CLIENTONLY)` 经常返回黑帧或失败，覆盖层没内容可叠 → 看着像"上层消失"；修法是**挖洞之前**从屏幕 BitBlt 客户区当窗口画面（`capture_screen_client`），`PrintWindow` 只在已经挖洞、不能截屏时兜底。
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
| v2.17.0 | `dist\installer\TransGlass_2.17.0_Setup.exe` | `D:\TransGlass-dist\TransGlass_2.17.0.zip` | ✅ |
| v2.18.0 | `dist\installer\TransGlass_2.18.0_Setup.exe` | `D:\TransGlass-dist\TransGlass_2.18.0.zip` | ✅ |
| v2.19.0 | `dist\installer\TransGlass_2.19.0_Setup.exe` | `D:\TransGlass-dist\TransGlass_2.19.0.zip` | ✅ |
| v2.19.1 | `dist\installer\TransGlass_2.19.1_Setup.exe` | `D:\TransGlass-dist\TransGlass_2.19.1.zip` | ✅ |
| v2.19.2 | `dist\installer\TransGlass_2.19.2_Setup.exe` | `D:\TransGlass-dist\TransGlass_2.19.2.zip` | ✅ |
| v2.19.3 | `dist\installer\TransGlass_2.19.3_Setup.exe` | `D:\TransGlass-dist\TransGlass_2.19.3.zip` | ✅ |
| v2.19.4 | `dist\installer\TransGlass_2.19.4_Setup.exe` | `D:\TransGlass-dist\TransGlass_2.19.4.zip` | ✅ |
| v2.19.5 | `dist\installer\TransGlass_2.19.5_Setup.exe` | `D:\TransGlass-dist\TransGlass_2.19.5.zip` | ✅ |
| v2.19.6 | `dist\installer\TransGlass_2.19.6_Setup.exe` | `D:\TransGlass-dist\TransGlass_2.19.6.zip` | ✅ |
| v2.19.7 | `dist\installer\TransGlass_2.19.7_Setup.exe` | `D:\TransGlass-dist\TransGlass_2.19.7.zip` | ✅ |
| v2.19.8 | `dist\installer\TransGlass_2.19.8_Setup.exe` | `D:\TransGlass-dist\TransGlass_2.19.8.zip` | ✅ |
| v2.19.9 | `dist\installer\TransGlass_2.19.9_Setup.exe` | `D:\TransGlass-dist\TransGlass_2.19.9.zip` | ✅ |
| v2.20.0 | `dist\installer\TransGlass_2.20.0_Setup.exe` | `D:\TransGlass-dist\TransGlass_2.20.0.zip` | ✅ |
| v2.20.1 | `dist\installer\TransGlass_2.20.1_Setup.exe` | `D:\TransGlass-dist\TransGlass_2.20.1.zip` | ✅ |
| v2.20.2 | `dist\installer\TransGlass_2.20.2_Setup.exe` | `D:\TransGlass-dist\TransGlass_2.20.2.zip` | ✅ |
| v2.20.3 | `dist\installer\TransGlass_2.20.3_Setup.exe` | `D:\TransGlass-dist\TransGlass_2.20.3.zip` | ✅ |
| v2.20.4 | `dist\installer\TransGlass_2.20.4_Setup.exe` | `D:\TransGlass-dist\TransGlass_2.20.4.zip` | ✅ |
| v2.21.0 | `dist\installer\TransGlass_2.21.0_Setup.exe` | `D:\TransGlass-dist\TransGlass_2.21.0.zip` | ✅ |
| v2.21.1 | `dist\installer\TransGlass_2.21.1_Setup.exe` | `D:\TransGlass-dist\TransGlass_2.21.1.zip` | ✅ |
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
- [ ] **透明洞快照是静态的**：下层内容变了仍要「刷新透明洞画面」；窗口移动/缩放已改成自动重抓（`follow_visual`），但拖动过程中覆盖层会闪一帧；没有做实时逐帧抓取（实时抓会闪、开销大）
- [ ] 透明洞覆盖层只在该目标窗口前台时显示；多个带选区的窗口同时存在时，视觉覆盖层只跟随最近应用/刷新的那个（点击穿透各窗口独立生效）
- [ ] 选择框是**置顶覆盖**：目标被别的窗口挡住时框仍显示；如果觉得打扰，可以改成「目标前台才显示」
- [ ] 选择框只做 4 秒提示，主界面「显示选中框」/卡片「选中并显示」可再打开；目标本身 topmost 时可能盖住主界面/标签，auto-hide 是兜底
- [ ] 右键悬浮球选的是球正下方最上层的非自身窗口；悬浮球盖在哪个应用上就选哪个（这是设计语义）
- [ ] 卡片右键菜单里的「选中并显示控制」和原有菜单项共用；菜单在个别置顶/覆盖场景下的点击测试受环境影响
- [ ] 「选中控制」卡片列的是当前目标 + 所有带选区的窗口；标记和选区都只存内存，TransGlass 重启后要重新设置
- [ ] 卡片的透明度滑条只改视觉洞 mask，不重新抓下层快照；下方画面变了仍需「刷新透明洞」
- [ ] **窗口上画选区暂不支持拖顶点微调**（画歪了用「重画」重来）、**没有撤销**；快捷键只有 Enter 确定 / Esc 取消 / Backspace 重画
- [ ] **新蒙版默认透明度 0%（只点透、不改外观）**：想要"真·透明洞"得在列表里手动把透明度拉起来；洞里透出的是下层快照（下层是别的窗口就会透出它的文字），这是快照方案的固有限制
- [ ] 透明洞快照是**静态**的，下层内容变了要「刷新透明洞」；目标窗口拖动/缩放已改成自动重抓（`follow_visual`），但拖动过程中覆盖层会闪一帧
- [ ] 蒙版列表的小预览是同一个纹理直接等比缩放，窗口小/缩略图分辨率低时预览比较糊
- [ ] 右侧面板整体套了滚动条，但窗口特别矮时预览 + 列表还是要滚
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
