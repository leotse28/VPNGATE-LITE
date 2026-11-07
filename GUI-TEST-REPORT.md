# GUI-TEST-REPORT — vpngate-gui（桌面版）

日期：2026-11-07。测试机：本 VM（Linux x86_64，Rust 1.99.1）。
**目标平台：Windows 10 x64**——Linux 版只用来在本机验证编译和逻辑，
交付物是 Windows 版（见 WINDOWS.md）。
**先说结论：图形界面编出来了，两个二进制都通过，逻辑测试全过，**
**对本地模拟 SSTP 服务器的端到端也通了，界面还在 560×420 最小窗口**
**下做了双主题渲染检查，中文显示正常；但真实 VPNGate 节点在本 VM**
**上连不上（出口拦截 TLS，和命令行版一样），没有宣称任何一次真实**
**节点 GUI 测试通过。**

## 1. 做了什么

- CLI 的模块（`sstp` / `socks5` / `vpngate` / `doh` / `net` / `sysproxy` /
  `config` / `util`）抽成共享库 `src/lib.rs`，CLI（`vpngate-client`）和
  GUI（`vpngate-gui`）共用同一套隧道代码，没有重写协议。
- GUI 是 eframe/egui 0.36，`impl eframe::App` 用 `fn ui`（0.36 的新 API，
  不是旧的 `fn update`）。窗口 800×600，最小 560×420，主题跟随系统。
- 唯一的小改动在库里：隧道 writer 任务在最后一个引用释放后调
  `shutdown()` 发 TLS close_notify，让服务端关闭、读任务退出——
  这样 GUI 里点“断开连接”后不会留一个挂住的连接。协议本身没动。
- 结构：顶部菜单栏（文件 > 退出，帮助 > 关于），中间节点列表
  （国家/地区、运营商、主机、端口、延迟/状态），连接/断开是主按钮
  （选中态颜色），探测和测试隧道在旁边；允许局域网、系统代理和两
  个端口输入都在主面板上（见 §5），高级设置收在折叠栏里
  （手动添加节点、前置 SOCKS5、保存设置）；
  底部状态栏显示连接状态、虚拟 IP、本地地址，错误用错误色显示并
  写明下一步做什么。长任务（拉列表、探测、连接、测试）都在后台
  tokio 任务里跑，界面不卡，忙时显示 Spinner。
- 中文字体：egui 自带字体没有中文，程序启动时在系统里找中文字体
  （Windows 微软雅黑，macOS 苹方，Linux Noto Sans CJK），找不到会
  在窗口顶部提示。不自带字体文件。
- `cargo tree` 检查过：egui 只有一个版本 0.36.2（`cargo tree -i egui`
  验证），没有混版本。`cargo tree --duplicates` 里剩下的重复项只是
  无关的间接依赖（toml、getrandom 等），不是 egui。

## 2. 构建与测试

- `cargo check`：lib、CLI、GUI 均通过（中途修过 2 处：缺
  `use eframe::egui;`、事件通道的借用冲突；1 个死代码警告已清）。
- `cargo build --release`：通过。`target/release/vpngate-gui`（33 MB）、
  `target/release/vpngate-client`（7.0 MB，CLI 不变，仍能编）。
- `cargo test --release`：**全部通过**——lib 8 个、GUI 单元测试 8 个、
  集成测试 1 个；渲染测试 1 个已忽略（只在需要时手动跑，见第 3 节）。

## 2a. 2026-11-07 晚：便携版改动（v0.1.0 之后）

按 Leo 在 Windows 上实测的反馈改了三处：

- **便携配置**：`config.toml` 改为优先读程序旁边的（`--config` → exe 旁 → 工作目录），
  高级设置里加了“保存设置”按钮，点一下写到 exe 旁的 `config.toml`，下次打开还在；
  下面还显示当前配置文件的路径。不写 `%APPDATA%`、不写注册表，整个文件夹拷走即用。
- **节点缓存**：官方 API 拉成功一次后，列表存到 exe 旁的 `nodes_cache.json`；
  再刷新失败时先显示缓存（列表上写“缓存列表，约 X 小时前保存”），没有缓存才显示内嵌 28 个，
  不再只剩一个“官方 API 失败”。CLI 的 `list` 同样走这条兜底链。
- **按钮间距**：`item_spacing` 10×8、`button_padding` 12×6，连接/探测/测试隧道/断开
  四个主按钮固定 96×34，刷新节点 92×32；560×420 最小窗口下重新渲染检查过（见第 3 节），
  按钮不再挤在一起。
- 新增测试：配置保存/读取回环、便携路径解析、节点缓存回环、GUI 设置回写——全过（上面已计入总数）。
  - GUI 单元测试：手动节点解析（合法/非法端口、序号越界等）、
    文本截断、节点列表事件（成功更新 / API 失败回落内嵌列表）、探测事件
    （记录延迟 / 失败时给可操作报错）、连接失败与测试结果路径、
    未连接时断开是空操作。
  - 集成测试 `tests/gui_flow.rs`：启动 `tests/mock_sstp.py` 模拟
    服务器，GUI 用的同一套连接流程拿到虚拟 IP 10.0.0.2，
    generate_204 回 204，再经本地 SOCKS5 拿到模拟服务器的 204，
    断开后隧道引用释放。**0.96 秒通过。**
- Windows 10 x64 交叉构建：`cargo build --release --target
  x86_64-pc-windows-gnu`（mingw-gcc 13.2）**通过**：
  `target/x86_64-pc-windows-gnu/release/vpngate-gui.exe`（38 MB，
  PE32+ GUI x86-64）、`vpngate-client.exe`（9.7 MB）。只验证了编出
  来，没在 Windows 上跑过（本 VM 是 Linux，跑不了 .exe）。本次修正后
  （字体后备加了 `simsun.ttc` / `simhei.ttf`）已用最新源码重新编过，
  `.exe` 是 2026-11-07 05:07 的新版。自己在 Windows 上编的步骤见
  WINDOWS.md。

## 3. 界面渲染检查（做过，修过两个真问题）

本 VM 一开始没有显卡：渲染测试先报 `No adapter found`。装了
`mesa-vulkan-drivers`（软件 lavapipe）后，`render_for_review` 测试
跑通，560×420 最小窗口、深色/浅色双主题、150% 缩放各出一张图，
存在项目根目录（`ui-review-dark.png`、`ui-review-light.png`、
`ui-review-dark-zoom150.png`）。检查后修了两个真问题：

1. **中文一开始全是方框**：渲染测试用的构造器没加载字体。修法是
   把字体加载抽成共用函数，测试也走真字体路径。之后中文正常显示
   （本 VM 用的是 Noto Sans CJK；Leo 的 Windows 10 上会优先用微软雅黑
   `msyh.ttc`，依次后备 `msyhbd.ttc`、`simsun.ttc`、`simhei.ttf`）。
   这也证明了 .ttc 字体文件能被 egui 0.36（skrifa 解析器）正常加载。
2. **节点列表横向溢出**：第一版是 5 列，`push_id` 把每一行包成了
   一个格子，行与行横向排布。修法是去掉 `push_id`（按钮 ID 本来
   就是按调用顺序自动分配的，不会冲突），改成 4 列（国家/地区、
   运营商、主机:端口、延迟/状态），国家名截到 12 字、运营商截到
   14 字、主机名去掉 `.opengw.net` 后缀（完整地址放悬停提示里）。
   修完后 560×420 下四列全部可见；150% 缩放时无重叠、无文字裁切，
   只是延迟/状态列要横向滚动一下才能看到（最小窗口 + 放大后的
   正常取舍）。

另外：直接在 Xvfb 下跑 Linux 版 GUI 会报
`Wgpu(CreateSurfaceError)`——还是同一个原因（本 VM 没有可用的
图形后端），不是代码问题；在有显卡的真机上不会这样。

## 4. 没有验证到的事（实话）

- **真实 VPNGate 节点没有在 GUI 里测过。** 本 VM 出口拦截到
  `vpngate.net` / `*.opengw.net` 的 TLS，和命令行版的原因一样。
  GUI 的连接流程和命令行共用同一套代码，模拟服务器端到端已通，
  但真实节点的 IPCP/LCP 交互（真实 SoftEther 服务器可能多发
  LCP Request / Nak）仍未在真节点上验证。
- **Windows 版 .exe 没有在 Windows 上跑过。** 只验证了交叉编译
  通过。第一次在 Windows 10 上双击时如果报缺 DLL，把报错发回来。
- Windows/macOS 的系统代理分支是按文档写的，没实测过；长时运行
  稳定性、断线自动重连（需求里没要求）都没有。

## 4. Leo 在自己机器上（Windows 10 x64）跑

**方式一：直接用编好的 Windows 版。** 文件在：

```
target/x86_64-pc-windows-gnu/release/vpngate-gui.exe   （38 MB）
```

双击就能打开，不用装 Rust。第一次打开先看界面能不能正常显示、
中文显不显示、节点列表能不能拉到。

**方式二：自己编（装好 Rust 1.95+ 后）。** 详细步骤见 WINDOWS.md。

```powershell
cd vpngate-rust-client
cargo run --release --bin vpngate-gui
```

先看界面能不能正常打开、中文显不显示、节点列表能不能拉到；
再选一条节点（先试日本 NTT kanratown.opengw.net:992）点“探测”，
再点“连接”，最后点“测试隧道”。如果“测试隧道”里 generate_204
通了但 Google 不通，换一个节点；如果探测就超时，把报错原样发回来。

---

## 5. 2026-11-07 下午这一批改动（问题 1–8）

针对 Leo 在 Windows 上试用后的反馈，一次性改了 8 点（源码已推 GitHub main，见下）：

1. **节点刷新增加网页抓取兜底。** 顺序：官方 API → 抓 `https://www.vpngate.net/cn/`（从节点表里提 `.opengw.net` 主机名和 MS-SSTP 端口，缺端口默认 443）→ 本地 `nodes_cache.json` → 内嵌 28 节点。列表上方显示当前来源（官方 API / 网页抓取 / 缓存 / 内嵌列表）。CLI `list` 也走同一条链。
2. **“允许局域网分享”和“设置系统代理”挪到主面板**，不用展开高级设置就能看到。
3. **设置自动保存。** 改任何设置（前置代理、端口、开关）都会自动写 `config.toml`，退出时也会存；“保存设置”按钮还在，点一下是手动再存一次。
4. 同 2（系统代理开关已在主面板）。
5. **命令行版双击不再闪退。** 无参数启动会打印中文说明（这是命令行版，图形界面是同文件夹的 `vpngate-gui.exe`，附基本用法），等按回车再退出。
6. **节点列表滚动条一直显示。** 只加 `AlwaysVisible` 不够：egui 默认滚动条样式是 floating，静止时不透明度为 0，鼠标不悬停就看不见。改成 `ScrollStyle::solid()` 后，6 px 宽的滚动条（灰色滑块 + 白色轨道）在亮/暗主题下都一直画出来，560×420 渲染像素检查确认（滑块 y≈92–103，轨道 x=546–551 全高）。另外修了一个既有 bug：节点列表和日志两个 ScrollArea 共用默认 ID，同时加了各自的 `id_salt`。
7. **“本地代理端口”挪到主面板**，和两个开关放一起；连接中锁住，旁边注明断开后才能改（端口只在连接时生效）。
8. **本地端口和局域网端口分开设。** 配置 `[listen]` 里 `local_port`（默认 10808，始终监听 `127.0.0.1`）和 `lan_port`（默认 10809，勾选局域网分享后额外监听 `0.0.0.0`），两个 SOCKS5 共用同一条隧道，断开时一起关；系统代理仍指本地端口。旧版 `config.toml` 里的 `port = …` 仍能读（当作 `local_port`）。两个端口设成一样、或端口被占用时，连接会在日志/错误里写明是哪个端口。

本批测试（本 VM，`cargo test`，CARGO_EXIT=0）：lib 13 项、GUI 11 项（+1 个忽略的渲染测试）、集成 1 项，全过；新增测试：抓取解析（合成 HTML）、兜底链优先级、双端口地址计算与等端口报错、旧 `port` 键兼容、GUI 设置往返（含 `lan_port`）。
渲染检查：`cargo test --bin vpngate-gui -- --ignored render_for_review` 通过，截图 `ui-review-dark.png` / `ui-review-light.png` / `ui-review-dark-zoom150.png`（560×420，双主题 + 150% 缩放），主面板两个开关、两个端口输入都可见，滚动条已确认画出。
`cargo build --release` 与 `cargo build --release --target x86_64-pc-windows-gnu` 均通过；Windows 版 `vpngate-gui.exe`（PE32+ GUI，38,950,846 字节）、`vpngate-client.exe`（PE32+ console，10,289,297 字节）。Linux 版 `vpngate-client` 实测：无参数打印中文说明；`list` 在 API/抓取都连不上时正确落到内嵌 28 节点并写明两次失败原因。
**仍未在真节点上验证**：本 VM 出口拦截 vpngate.net / *.opengw.net 的 TLS，真实节点连接、抓取、双端口同时监听都须 Leo 在 Windows 机器上试。
