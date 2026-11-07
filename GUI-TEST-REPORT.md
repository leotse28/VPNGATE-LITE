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

---

## 6. 2026-11-07 晚 v3 这一批改动（问题 9–13 + TLS 系统证书库）

针对 Leo 在 Windows 上试用 v2 后的反馈，一次性改了 6 点（源码已推 GitHub main，见下）：

9. **主面板加了外层纵向滚动。** 原来只有节点列表和日志各自有滚动区；窗口矮时高级设置的下半部分和日志滚不出来。现在 CentralPanel 整个包在一个纵向 ScrollArea 里（`id_salt("central")`，滚动条 AlwaysVisible），节点列表和日志保留各自的滚动区。注意：`scroll_offset(1e6)` 这样写是错的——egui 对显式设置的 offset 不做钳制，内容会直接滚出视图外变成空白；正确做法是 `stick_to_bottom(true)`（渲染测试的“滚到底”截图就是这么做的）。
10. **高级设置里加了“测试前置代理”按钮。** 点一下就用当前前置代理配置 HTTPS GET `https://www.google.com` 和 `https://www.vpngate.net`，逐个测，延迟和通/不通写进日志和状态栏。这是 SOCKS5 连通性，不是 ICMP ping。失败分两种，文案不一样：前置代理本身连不上（SOCKS5 建连失败 / 握手失败 / 认证被拒）vs 代理能连上、目标站经它打不开（代理侧 CONNECT 被拒、TLS/HTTP 阶段失败、超时）。前置代理没开时按钮是灰的，并提示“先勾选使用前置 SOCKS5 代理，这个测试需要前置代理开着”；地址为空或正忙时也一样禁用并说明原因。失败文案里带原始错误（例如 `TLS handshake to www.google.com failed: invalid peer certificate: UnknownIssuer`），不只写“失败”，方便下一步区分是证书问题还是防火墙拦了没签名的 exe。
11. **探测走前置代理的可见性（不改路由）。** 探测本来就走前置代理（probe → connect → `connector.connect_tcp`，开着时 TCP 先走 SOCKS5，和刷新、连接同一条链）。改的是可见性：探测开始时状态/日志写“经前置代理 192.168.236.197:10808”；失败分“前置代理连不上”（先做一次到代理端口的直连 TCP 检查，3 秒超时，连不上就定这个原因）和“节点不通”（代理端口能连上）。探测失败的错误文案会点明：刷新、探测、连接都走这条代理，代理不通时会一起失败，并提示先点“测试前置代理”确认。Leo 当时探测失败和 API/网页失败是同一个原因——前置代理到不了 vpngate.net——失败文案就是为了让这个因果关系看得出来。
12. **节点列表列宽可调。** 原来是固定列的 Grid，国家/运营商还截断显示（12/14 字）。现在表头每列后面有一条 6 px 分隔线（ResizeColumn 光标），拖动改该列宽度（40–400 px），宽度记在设置 `[ui] node_col_widths` 里，跟着自动保存走 `config.toml`（默认 `[90, 130, 190, 90]`）。拖宽后截断放宽（单元格用 `Label::truncate()` 按当前列宽截断，配合横向滚动看全）。修了实现时发现的两个真问题：`allocate_ui_with_layout` 会按子内容的实际宽度推进父布局（不是按给定的宽度），短标签会把分隔线和下一列往左拉、表头和数据列错位，所以表头和数据行都改成按宽度数组算出的精确矩形放置（`ui.put` + `ui.interact`，分隔线用显式稳定 id `("node_col_sep", i)`）；数据行原来每列只加了一份间隔，现在和表头一样是对称的“间隔 + 分隔线 + 间隔”。
13. **本地代理默认端口 10808 → 10818。** `config.rs` 里 `default_local_port()` 返回 10818（CLI 和 GUI 的默认值都从这来），`GuiApp::default()` 的 `local_port` 也是 10818，`config.example.toml`、README、WINDOWS.md 里的默认 10808 一并改掉。已经存过 `config.toml` 的用户不受影响（存过的值优先于默认值）；局域网端口默认值 10809 不变。Leo 的前置代理地址 192.168.236.197:10808 是他自己的代理端口，不改。
14. **Verified TLS 再加载系统证书库（`rustls-native-certs` 0.8.5，新依赖，Cargo.toml + Cargo.lock 都已进仓库）。** 背景：浏览器 + SwitchyOmega 走前置代理能打开 vpngate.net/cn/，GUI 刷新两档都失败；已核实刷新、探测走的都是前置代理，排除了路由问题。剩下最可能的系统性差别是证书校验——Chrome 信任 Windows 系统证书库，程序的 verified TLS 只信任 Mozilla 根证书，如果中间有自签 CA 做拦截，浏览器放行、程序会拒绝。改法：`verified_tls_config()` 里 Mozilla 根证书之外再加载系统证书库（Windows 上读系统库，其他平台没有就只留 Mozilla 根），让程序信任的东西和浏览器一致。只改 `vpngate.net` / DoH 这种 verify=true 的校验，节点那边的 accept-any 不动（`accept_any_tls_config` 没改）。本 VM 上验证了机制：`/usr/local/share/ca-certificates/hatch-egress-ca.crt` 就是出口拦截用的 CA，`openssl s_client` 校验返回 0（ok）；改完后 CLI `list` 的报错从证书类变成了 `TLS handshake to www.vpngate.net failed: received corrupt message of type InvalidContentType`——证书校验已通过，剩下的是 VM 出口中间件根本不完成 TLS 握手（客户端侧修不了，Leo 的真实网络上没有这个拦截）。

本批测试（本 VM，`cargo test`，CARGO_EXIT=0）：lib 18 项、GUI 15 项（+3 个忽略的：渲染测试、列宽拖拽测试、拖拽冒烟测试）、集成 1 项，全过；新增测试：系统证书库叠加 Mozilla 根（`verified_roots_add_system_store_on_top_of_mozilla`）、前置代理测试事件的双目标上报与原始 TLS 错误进日志、探测失败分代理/节点两类（`classify_probe_failure_splits_proxy_from_node`、死代理/活代理端口的 `probe_flow` 异步测试）、列宽经配置往返、kittest 列分隔线拖拽（+40 px 后列宽 90→130）。另修了一个测试脚手架 bug：`fake_socks5` 原来只接受一个连接，`test_url_through_proxy` 里先做的 TCP 预检会把它耗掉，导致拒绝 CONNECT 的用例被误判为代理不通；现在改成循环接受、每连接一个任务。
渲染检查：`cargo test --bin vpngate-gui -- --ignored render_for_review` 通过，截图 `ui-review-dark.png` / `ui-review-light.png` / `ui-review-dark-zoom150.png`（560×420，双主题 + 150% 缩放）加 `ui-review-dark-scrolled.png` / `ui-review-light-scrolled.png`（滚到底：高级设置尾部、“测试前置代理”按钮、保存行、日志都可见；外层滚动条在右侧画出）。注意：本次 VM 曾被替换过，`mesa-vulkan-drivers`（lavapipe）和 mingw 交叉工具链都是重装后才跑通的渲染和 Windows 构建。
`cargo build --release` 与 `cargo build --release --target x86_64-pc-windows-gnu` 均通过；Windows 版 `vpngate-gui.exe`（PE32+ GUI，39,172,702 字节）、`vpngate-client.exe`（PE32+ console，10,438,168 字节），图标仍在（`.rsrc` 段）。Linux 版 `vpngate-client` 实测：无参数打印中文说明（默认 127.0.0.1:10818）；`list` 在 API/抓取都连不上时正确落到内嵌 28 节点并写明两次失败原因（见上条 TLS 说明）。
**仍未在真节点上验证**：本 VM 出口拦截 vpngate.net / *.opengw.net 的 TLS，真实节点连接、抓取、双端口同时监听都须 Leo 在 Windows 机器上试；新加的“测试前置代理”按钮也要在他机器上对 192.168.236.197:10808 点一次。

---

## 7. 2026-11-07 晚 v3.1 这一批改动（文件日志 + Windows 上 verified TLS 改走 SChannel）

背景：Leo 在 v3 上点“测试前置代理”的结果是代理本身能连上，但 Google 超时、vpngate.net 报 `TLS handshake to www.vpngate.net failed: received fatal alert: HandshakeFailure`；API 与网页刷新同样握手失败。v3 加的系统证书库没有解决，说明原因不在信任库，而在 TLS 握手本身——rustls 的 ClientHello 特征与浏览器不同，被中间设备拒绝（浏览器走同一代理能打开 vpngate.net/cn/）。Leo 同时要求：程序要自动记日志文件，不然出了问题没法调。

15. **GUI 自动写文件日志（`vpngate-gui.log`，新模块 `src/filelog.rs`）。** 每次启动自动在程序目录（便携目录，和 `config.toml` 同一位置；取不到程序目录时退回 `./vpngate-gui.log`）追加全部日志行，每行带本地时间戳（chrono `Local`）和 `[INFO]` / `[WARN]` / `[ERROR]` 标签。启动时先写会话头：启动时间、版本（`CARGO_PKG_VERSION`，现为 0.3.1）、exe 路径、config 路径、前置代理是否启用及地址——**绝不记录密码**：会话头由纯函数 `session_header_lines(&Config, …)` 生成，它只读 `front_proxy.enabled` / `front_proxy.addr`，模块里根本没有密码参数，测试 `session_header_never_logs_password` 用带密码的 Config 证明头里没有密码和用户名。文件超过 2 MB 时轮转：改名 `vpngate-gui.log.old`（先删掉旧的）再重新开始；改名失败时就地截断；再打不开就停写。打不开文件时界面日志里警告一次，程序照常运行，绝不崩溃。`GuiApp::log_line`（INFO）接 `log_warn` / `log_error`，三者都同时写界面日志和文件日志；新增 `set_error`（设错误横幅 + 按 [ERROR] 记日志），审计了全部 13 处 `self.error = …`：与日志同文的 5 处改成 `set_error` 并去掉重复的 `log_line`，文案不同的 2 处（探测失败两类）保留原 `log_line` 再加 `set_error`，只设横幅没记日志的 4 处（测试结果两类、前置代理测试两类）改成 `set_error`，保存失败两处同样处理；手动添加节点格式错误也按 [WARN] 记一条。启动时界面日志里显示日志文件路径，高级设置里保存设置按钮下面也显示一行“日志文件：…”。CLI 不改（它把所有内容打印到控制台，那就是它的日志）。新增测试 6 项（lib 的 filelog：写入+时间戳格式、轮转、会话头不记密码、打不开不崩溃、头写进文件）+ GUI 1 项（`log_lines_go_to_file_log_with_levels`：三种级别进文件、没接日志器之前不写文件、每行以时间戳开头）。
16. **Windows 上 verified TLS 改走 SChannel（`native-tls` 0.2 + `tokio-native-tls` 0.3，新依赖，均为 `[target.'cfg(windows)'.dependencies]`）。** 适用范围是全部已校验连接：vpngate.net API、/cn/ 抓取、DoH、前置代理测试（都是 `https_get` verify=true），以及隧道里的 Google 检查（CLI `test` 与 GUI“测试隧道”里那次经隧道的 TLS 握手，之前直接调 `verified_tls_config()`——现在统一走新函数 `net::verified_tls_connect(stream, host)`，它对任意已连通的流做已校验握手）。Windows 上这个函数用 SChannel，ClientHello 与浏览器一致，证书库也是 Windows 系统库；非 Windows 保持 rustls + Mozilla 根证书 + 系统证书库（`verified_tls_config` / `verified_roots` 加了 `#[cfg(not(windows))]`，相关测试也一样）。节点 SSTP 的 accept-any TLS 全平台继续用 rustls，不动。两种 TLS 流类型不同，用公开 trait `net::TlsStream`（`AsyncRead + AsyncWrite + Unpin + Send` 的合并超 trait）装箱；`https_get` 里 verify=false 仍走 rustls accept-any。Cargo.toml 版本 0.1.0 → 0.3.1，User-Agent 与关于窗口都改成读 `CARGO_PKG_VERSION`。默认端口不动（本地 10818、局域网 10809），Connector 与前置代理路由不动。注意：加依赖时发现 `dyn AsyncRead + AsyncWrite` 编不过（E0225，trait 对象只能有一个非 auto trait），改成合并超 trait 解决；还发现 CLI 与 GUI 的隧道 Google 检查也调了 `verified_tls_config()`，一并改到新函数，不然 Windows 构建会报找不到函数。

本批测试（本 VM，`cargo test`，CARGO_EXIT=0）：lib 24 项（原 18 + filelog 6 项）、GUI 16 项（原 15 + 文件日志 1 项，+3 个忽略的：渲染测试、列宽拖拽测试、拖拽冒烟测试）、集成 1 项，全过，共 41 项。
渲染检查：`cargo test --bin vpngate-gui -- --ignored render_for_review` 通过，5 张截图已更新（560×420，双主题 + 150% 缩放 + 滚到底两张）；滚到底的浅色截图里能看到保存设置按钮下面新加的“日志文件：未开启”一行（渲染测试不走 `new()`，所以没有真的日志器，显示未开启是正确的）。
`cargo build --release` 与 `cargo build --release --target x86_64-pc-windows-gnu` 均通过，无警告；Windows 版 `vpngate-gui.exe`（PE32+ GUI，39,283,029 字节）、`vpngate-client.exe`（PE32+ console，10,555,909 字节），图标仍在（两 exe 都有 `.rsrc` 段，10,376 字节，与 v3 相同）。Linux 版 `vpngate-client` 实测：无参数打印中文说明（注意：无参数时会等按回车才退出，这是 v2 故意改的，`head` 截断会让它一直等，别误以为卡死）；`list` 在 0.07 秒内完成，API 与抓取都以 `received corrupt message of type InvalidContentType` 失败后正确落到内嵌 28 节点（VM 出口中间件根本不完成 TLS 握手，和 v3 时一样）。
**仍未在真节点上验证（实话）**：本 VM 出口拦截 vpngate.net / *.opengw.net 的 TLS，SChannel 握手是否绕过 Leo 网络上那个 HandshakeFailure 只能在他 Windows 机器上验证——点“测试前置代理”看 Google 与 vpngate.net 是否变通；真节点连接、抓取、双端口同时监听同样须他试；文件日志也要在他机器上确认 `vpngate-gui.log` 出现在 exe 旁边、且里面没有密码。

---

## 8. 2026-11-07 晚 v3.2 这一批改动（运营商列、测试超时、SSTP 走 SChannel、短主机名）

背景：Leo 在 v3.1 上实测（`vpngate-gui.log`）：SChannel 的已校验 HTTPS 已生效——前置代理测试通过（Google HTTP 200 约 2.7 秒，vpngate.net HTTP 302 约 4.4 秒），刷新拉到 95 个节点；但探测与连接仍失败，每个节点都是 `TLS handshake to <host>:995 failed: tls handshake eof`；运营商列全是 "2weeks"；前置代理测试有一次 30 秒以上没反应（地址栏先填成 12.0.0.1，测试中改回 127.0.0.1，开始行与结果行的地址对不上）。

17. **运营商列读错列（`src/vpngate.rs` `parse_api_csv`）。** `isp` 与 `name` 都取了 cols[11]，那是 LogType（现网值就是 "2weeks"），运营商 Operator 在 cols[12]。改成 `isp = cols[12]`；API 节点的 `name` 置空（与网页抓取路径一致，中文展示名只来自 curated 表）。测试用现有的 sample CSV 断言 `isp == "SoftBank BB"` 且 `name` 为空。

18. **前置代理测试加超时与进度。** 原来测试用隧道超时（默认 10 秒），TCP 预检与 HTTPS 各占一份超时、两个站串行，最坏 40 秒才出结果，中间无反馈（Leo 那次 40 秒正好是这个上限）。改成测试专用的每站 10 秒总超时（`FRONT_PROXY_TEST_TIMEOUT`，TCP 预检不超过 5 秒，含在 10 秒预算内——`net::test_url_through_proxy` 现在按 deadline 算，预检预算是纯函数 `front_proxy_precheck_budget`，10 秒总预算给预检 5 秒）；两个站之间发进度事件，状态栏显示“正在测试前置代理… Google / vpngate.net”；点击时把前置代理地址快照下来（`GuiEvent::FrontProxyTest` 新增 `addr` 字段），开始行与全部结果/报错行都用这个快照地址，不再读实时字段；超时按现有分类报不通（“连接前置代理超时”/“请求超时，目标站无响应”）。新增测试：预检预算上限纯函数、黑洞代理（握手成功后永不回应）3 秒总预算内返回且分类为目标站不通、无应答代理地址 8 秒预算内返回且分类为代理不通、快照地址事件、进度事件。

19. **SSTP 的 TLS 在 Windows 上也走 SChannel（本次主修，`src/net.rs` 新增 `accept_any_tls_connect`，`src/sstp.rs` 的 `SstpTunnel::connect` 改调它）。** v3.1 的日志证明：同一条代理上 SChannel 的 HTTPS 能通、rustls 的 SSTP 在握手中途被掐掉（EOF），问题在 TLS 栈/握手特征，不在代理本身。Windows 上用 `native-tls` + `tokio-native-tls`（已是 Windows-only 依赖，无新增依赖），`danger_accept_invalid_certs(true)` + `danger_accept_invalid_hostnames(true)`,保持 accept-any 语义；SNI 仍用目标主机名；非 Windows 继续 rustls accept-any。Connector 与前置代理路由不动，默认端口不动（本地 10818、局域网 10809）。`Cargo.toml` 注释同步更新。验证靠集成测试 `gui_connect_flow_against_mock`（mock SSTP 服务器 + 自签证书，Linux 上走改动后的 rustls accept-any 路径全过）与 Windows 交叉编译（SChannel 路径只编过，没法在本 VM 跑）。

20. **主机名诊断与防御。** 先确认的事实：探测开始行用的 `target_str` 与连接的目标就是传给 `resolve_target` 的完整字符串（显示时才去掉 `.opengw.net` 后缀），但 API 返回的节点里混着不带后缀的短主机名（Leo 日志里的 `public-vpn-192`、`vpn321359264`，甚至 `opengw`），短名的来源在代码里找不到——只做防御，不硬猜。`resolve_target` 里对不含 "." 的裸标签主机名补 ".opengw.net" 后缀（`complete_ddns_host`，公开函数，GUI 的探测/连接开始行也用它拼完整 host:port 再记日志；FQDN 与 IP 不受影响）；网页抓取里把 host 恰好等于 "opengw.net" 本身的条目过滤掉（`valid_ddns_host` 本来就拒掉它，这里是显式防线）。连接开始行原来没有日志，现在补一条（“开始连接 …（经前置代理 …）”），与探测开始行一致。新增测试：短标签补后缀（带端口与不带端口、`opengw`、FQDN/IP 不动、curated 节点端口与备用不变）、网页抓取不产出 "opengw.net"。

21. **版本 0.3.1 → 0.3.2**（`Cargo.toml`；User-Agent 与关于窗口仍读 `CARGO_PKG_VERSION`）。README / WINDOWS.md 同步更新（运营商来源、测试超时、SSTP 走 SChannel）。版本 bump 顺带修了一个测试里的硬编码版本号：`session_header_never_logs_password` 断言头里含 "0.3.1"，改成读 `CARGO_PKG_VERSION`，否则每次升版本都会误报。

本批测试（本 VM，`cargo test`，CARGO_EXIT=0）：lib 29 项（原 24 + vpngate 2 项、net 3 项）、GUI 18 项（原 16 + 快照地址、进度事件，+3 个忽略的：渲染测试、列宽拖拽测试、拖拽冒烟测试）、集成 1 项，全过，共 48 项。

---

## 9. 2026-11-07 晚 v3.3 这一批改动（启动不自动刷新、国家排序、配色、行选区、并行探测、拉取超时、无 config 提示、回车连接、∞ 状态列、Ctrl+R 探测）

背景：Leo 在 v3.2 上实测（`vpngate-gui-2.log`）：SChannel 的 SSTP 已生效，`public-vpn-192` 探测可用（虚拟 IP 10.246.159.236）、连接成功（虚拟 IP 10.246.160.14，本地 SOCKS5 127.0.0.1:10818 + 0.0.0.0:10809），`public-vpn-117` 也连上了（虚拟 IP 10.236.4.207）；但 `vpn<数字>` 节点仍全部失败，报错是 SChannel 的 `unexpected EOF during handshake`（不是 rustls 的 `tls handshake eof`，说明握手已走 SChannel、仍被对端掐掉）——`public-vpn-*` 能通、`vpn<数字>` 全灭，最可能的解释是那些志愿者节点本身已死/端口不对/服务没起来，程序修不了死节点；启动时前置代理未启用（新版解压到新文件夹，没有 config.toml），自动刷新直连 vpngate.net 挂了 85 秒（21:27:24 → 21:28:49，os error 10060）；死节点探测要 ~20 秒（3 个端口串行各挂约 7 秒）。Leo 的要求是：启动不要自动刷新、国家列可排序、界面配色高级优雅、节点行不好选、log 再看还有什么可优化的；随后又补了回车连接、∞ 状态列、Ctrl+R 探测三点。

22. **启动不再自动刷新（`GuiApp::new`）。** 原来 `new()` 里调 `start_fetch`，前置代理没开时直连挂 85 秒（且当时 `https_get` 无超时）。现在启动时直接调 `vpngate::fallback_nodes()` 加载缓存列表（有则用缓存，无则内嵌 28 个），不发起任何网络拉取；状态栏与日志写“已加载缓存列表（N 个，约 X 前保存）。点右上角‘刷新节点’获取最新。”（内嵌列表时文案是“已加载内嵌列表（官方 API 与网页都不可用）。点右上角…”）。手动“刷新节点”按钮行为不变（仍调 `start_fetch`）。`Default` 的初始日志行也从“就绪。正在拉取最新节点列表…”改成“就绪。”。

23. **国家/地区列可点击排序（新增 `SortDir` + `display_indices`）。** 点表头一次按 A-Z，再点一次按 Z-A，来回切换；表头显示 ▲/▼ 指示当前方向（未排序时不显示箭头，悬停提示“点击按国家排序（A-Z / Z-A 切换）”）；排序对当前列表生效，刷新后对新列表重新应用（保持用户选的方向）——实现上是 `display_indices()` 每次现算顺序（`country_sort` 只存方向，不存顺序），所以刷新换列表后自动重排；按国家名排，同国家内按主机名排（稳定排序）。默认启动时不排序（`country_sort: None`，保持拉取顺序），只在用户点了表头后才排序。注意：国家名是中文字符串，按 Unicode 码点顺序排（不是拼音），确定性分组、同国家行聚在一起——这个口径写在 `SortDir` 注释与测试里。`selected` 仍存节点下标（不是显示位置），排序后选中、探测、连接都不错位。新增测试：`country_sort_orders_by_country_then_host`（A-Z：德国 < 日本 < 美国，同国家按主机名；Z-A 反过来）、`country_sort_survives_refresh`（刷新换列表后方向保持、重排生效）、kittest `country_header_click_toggles_sort`（点表头两次，方向 A-Z → Z-A，显示顺序跟着变）。

24. **界面配色（新增 `branded_visuals` + `infinity_accent`，`setup_visuals` 里对深/浅两主题分别 `set_visuals_of`）。** 与程序图标（靛蓝 #1E40AF → 青色 #0891B2 渐变）协调的克制配色：主强调色靛蓝（“连接”按钮、选中行底），成功/已连接标记用青色/柔和绿（∞ 用青色），警告用 muted 琥珀，错误用柔和红；深色是深 slate 底 #0F172A + 柔和 slate 面板 #1E293B + 暖白文字 #F1F5F9，浅色是暖灰白底 #F7F6F3 + 墨色文字 #1F2937；按钮、分区标题、状态栏与日志区的层级与间距统一（按钮仍是 15 pt 字、12×6 内边距；行高见下条）。不花哨、不多色混用。对比度按 WCAG 4.5:1 测过并写进测试 `branded_visuals_meet_text_contrast`（正文/面板 ≈13:1 浅 / ≈15:1 深；白字/靛蓝选中底 ≈8.7:1 浅、≈5.1:1 深；∞ 青色/面板 ≈4.9:1 浅、≈10:1 深；错误 ≈5.9:1、警告 ≈4.6:1）。渲染检查：560×420 与 800×600 双主题共 7 张截图（见下）。

25. **节点行不好选（三个原因一起修）。** 查到的原因：(a) 行高只有 22 px；(b) 行的响应矩形只有列宽总和（约 554 px），800 px 窗口下每行右边 ~200 px 是死区，点那里选不中；(c) egui 只把指针移动 < 6 px（`max_click_dist`）的按下-松开算点击，真实鼠标点按时很容易抖过 6 px，抖过之后手势变成滚动区的拖动、点击就丢了。改法：行高 22 → 30 px；行矩形取 `total_w` 与 `ui.available_width()` 的较大值（列宽总和超过视口时仍可横向滚动，不足时行铺满整宽）；再加手动容错——在行循环外记录按下时的指针位置（`row_press_pos`，帧级 `any_pressed`/`interact_pos`），松开时若按下与松开位置相距 < 12 px 且都在同一行矩形内，也选中（`drag_started`/`drag_stopped` 帮不上忙：click  sense 的 widget 永远不报拖拽事件）；选中行有清晰高亮（靛蓝底 + 白字，见下条）。单元格标签是无 sense 的 Label，不会吃点击；排序表头的 `interact` 盖在标签上面。新增 kittest `row_click_selects_and_drag_tolerance_selects`（点美国行选中；按住德国行、竖直拖 8 px 再松开——超过 egui 的 6 px 上限——仍选中），并在该测试末尾渲染一张选中行截图 `ui-review-selected-row.png`（靛蓝满宽底 + 白字，目视确认）。

26. **探测/连接候选端口并行（`src/sstp.rs`：`connect` 拆成 `handshake` + `establish`，新增 `connect_any` / `probe_any`）。** `handshake` 只做 TCP + TLS 握手、不建任务，返回的流 drop 即关闭，所以 race 里落选的候选不留残余任务；`establish` 只对获胜者做 SSTP/PPP 建立并建读写任务。`connect_any` 用 `tokio::task::JoinSet` 把 443/992/995 三个握手并行跑在一个总超时（隧道超时）之下，第一个成功的获胜、其余 `abort_all()` 丢弃；全失败时返回最后收到的错误（无任何应答时返回超时错误），总耗时约等于一个超时，而不是三个串行之和。失败分类不变（GUI 的 `probe_flow` / `connect_flow` 改调 `probe_any` / `connect_any`，分类逻辑与文案不动；CLI 的 `connect_first` / `cmd_probe` 同样改走并行路径，打印改成“trying N candidate(s) in parallel …”）。新增集成测试（`tests/gui_flow.rs`，mock SSTP 服务器）：`parallel_candidate_race_is_fast`（两个黑洞端口排在前面、mock 端口在最后，5 秒超时内快速成功，实测 < 3 秒；串行要先在黑洞上烧掉整个超时）、`parallel_candidate_race_all_hanging_returns_within_timeout`（三个黑洞端口，3 秒超时内返回错误，实测约 3 秒）。修测试脚手架：两个用 mock 的测试（原流程测试 + 新 race 测试）现在共用 `MOCK_LOCK`（`tokio::sync::Mutex`，整个测试体持有）串行跑——之前并行跑时后启动的测试会在 `start_mock` 里 pkill 掉先启动的 mock（或先结束的测试在 Drop 里杀掉另一个正在用的 mock），导致原流程测试在 `open_stream` 报 `TCP handshake failed: connection closed/reset`；`start_mock` 也加了“端口已在监听就复用、不 pkill”的检查。

27. **拉取超时收紧（`src/vpngate.rs`：`FETCH_TIER_TIMEOUT` 15 秒）。** `https_get` 本身无超时，`fetch_nodes_with_fallback` 现在把 API 与网页各档包在 `tokio::time::timeout(15s)` 里，超时按现有文案报失败（“官方 API 拉取超时（15 秒内无响应）”/“网页抓取超时（15 秒内无响应）”），刷新最坏约 30 秒、不再挂 85 秒。超时值做成参数 `fetch_nodes_with_fallback_timeout` 供测试用短超时测。新增测试 `fetch_fallback_bounds_black_holed_proxy`（黑洞前置代理：接受 TCP、永不回应，两档各 2 秒超时，实测约 4 秒返回、两档都报“超时”；注意：测试断言来源是内嵌或缓存均可——`target/debug/deps` 下可能留有旧的 `nodes_cache.json`，断言只锁超时边界与错误文案，不锁具体 fallback 档）。

28. **启动无 `config.toml` 时给一句提示（`GuiApp::new`）。** `config_path` 为空时在状态栏/日志写“未找到 config.toml：如从旧版升级，把旧文件夹里的 config.toml 与 nodes_cache.json 复制到新版 exe 旁边即可保留设置。”。便携式存储设计不动（配置仍在 exe 旁边）。

29. **回车连接（新增 `shortcut_action` + `ui()` 顶部的快捷键处理，第 8 项）。** 节点列表里选中一行后按回车触发连接（与“连接”按钮同一条流程 `start_connect`）。守卫：只有在没有文本框获得焦点时才响应（`ctx.text_edit_focused()`，覆盖手动添加节点、端口、前置代理地址等输入框——DragValue 编辑态内部也是 TextEdit，同样被挡住）；已在连接中或已有连接时不触发；忙碌时不触发；About 窗口打开时不触发；没有选中行时不触发。实现细节：回车用 `key_pressed(Enter)`；行本身有焦点时回车也会触发行的 `clicked()`（egui 对有焦点的 click widget 按回车/空格即算点击），那只是重新选中同一行，无害；按钮有焦点时回车会同时激活按钮，但快捷键处理跑在 `ui()` 顶部、先把 busy 置上，按钮的重复触发会被 busy 守卫挡住。为让 kittest 能观测到：`start_probe` / `start_connect` 改调新增的 `spawn_task`（有 tokio runtime 时 `handle.spawn`，无 runtime 时跳过——harness 里没有 runtime，busy 标志保持置位，正好是测试要断言的状态；生产环境 `main` 里 `rt.enter()` 恒在，不受影响）。“连接”按钮的悬停提示注明“回车”。新增测试：纯守卫测试 `shortcut_action_guards`（无选中/文本框焦点/忙碌/About 均挡住，回车优先于 Ctrl+R）+ kittest `keyboard_shortcuts_connect_and_probe`（无选中时回车无动作；选中后回车 → busy=Connecting；再按 Ctrl+R → busy=Probing；注意：busy 后 spinner 持续请求重绘，测试里用 `run_steps(3)` 而不是 `run()`，否则超步数失败）。

30. **状态列 ∞（第 9 项）。** 国家/地区列前面加一列窄的状态列（固定 28 px，表头是空的，不参与拖动调宽、不进 `col_widths` 配置数组——所以旧 `config.toml` 的 4 元数组照常加载，不会丢设置）。当前已连接（激活）的节点在这一列显示 ∞（U+221E），其他行留空；未连接时整列为空；排序只作用于国家列，这列不参与排序。实现：`connected_node_host()` 从 `conn.node_label`（已补全后缀的 host 或 host:port）取主机名，与每行 `complete_ddns_host(&n.host)` 比对；∞ 用 17 pt 居中放在 28 px 格里（行高 30 px），颜色是主题相关的青色（`infinity_accent`，浅色 #0E7490、深色 #22D3EE，未选中时用青色、选中行时用白字）。字体验证：新增渲染测试 `render_infinity_glyph`（深/浅两张 `ui-review-infinity-dark.png` / `ui-review-infinity-light.png`），17 pt 与 15 pt 的 ∞ 都是正常字形、不是方框/缺字；系统 CJK 字体（微软雅黑等）都带 U+221E。注意：无头渲染里只能验证字形本身，真实连接态下的 ∞ 标记要等 Leo 在 Windows 上连上一个节点后看（`public-vpn-192`）。

31. **Ctrl+R 探测（第 10 项，与第 29 条同一套守卫）。** 选中一行后按 Ctrl+R 直接探测该节点（与“探测”按钮同一条流程 `start_probe`，忙碌时不触发、没有选中行时不触发、文本框获得焦点时不触发）。实现用 `consume_shortcut(KeyboardShortcut::new(Modifiers::COMMAND, Key::R))`（Windows 上 COMMAND 即 Ctrl）。“探测”按钮的悬停提示注明“Ctrl+R”。测试同第 29 条的 kittest（`keyboard_shortcuts_connect_and_probe` 后半段）。

32. **版本 0.3.2 → 0.3.3**（`Cargo.toml`；User-Agent 与关于窗口仍读 `CARGO_PKG_VERSION`）。README / WINDOWS.md 同步更新（启动不自动刷新、国家排序、∞ 状态列、回车/Ctrl+R、行选区、并行探测、拉取超时、无 config 提示、配色）。

本批测试（本 VM，`cargo test`，CARGO_EXIT=0）：lib 30 项（原 29 + 拉取超时黑洞测试 1 项）、GUI 22 项（原 18 + 国家排序 2 项、快捷键守卫 1 项、配色对比度 1 项，+7 个忽略的：渲染测试、∞ 字形渲染、行点击/拖拽容差、国家表头点击、键盘快捷键、列宽拖拽、拖拽冒烟测试）、集成 3 项（原 1 项 + 并行 race 2 项）、CLI 0 项，全过，共 55 项。7 个忽略的测试用 `cargo test --bins -- --ignored <名字>` 逐个跑过，全过（其中键盘快捷键测试修过一次：busy 后 spinner 持续请求重绘，`run()` 超步数，改成 `run_steps(3)`；集成测试修过一次：两个用 mock 的测试并行时互相 pkill，改成 `MOCK_LOCK` 串行）。
渲染检查：`cargo test --bin vpngate-gui -- --ignored render_for_review render_infinity_glyph` 通过，截图 10 张（项目根目录 `ui-review-*.png`，PNG 不进 snapshot 推送）：560×420 与 800×600 双主题各 2 张、150% 缩放 1 张、滚到底 2 张、选中行 1 张（`ui-review-selected-row.png`：德国行靛蓝满宽底 + 白字，清晰可辨）、∞ 字形 2 张。目视结论：深色深 slate 底 + 暖白字、浅色暖灰白底 + 墨色字、“连接”按钮靛蓝白字、行高 30 px、条纹铺满整宽、800×600 时五列全显（560×420 时延迟/状态列要横向滚动一下，与 v3.2 相同）；∞ 在深/浅两主题都是正常字形（青色/深青色），不是方框。`cargo build --release` 与 `cargo build --release --target x86_64-pc-windows-gnu` 均通过；Windows 版 `vpngate-gui.exe`（PE32+ GUI，39,375,906 字节）、`vpngate-client.exe`（PE32+ console，10,641,425 字节），图标仍在（两 exe 都有 `.rsrc` 段，10,376 字节，与 v3.2 相同）。Linux 版 `vpngate-client` 实测：`--version` 打印 0.3.3，无参数打印中文说明并等按回车退出；`list` 在 0.17 秒内完成，API 与抓取都以 `received corrupt message of type InvalidContentType` 失败后正确落到内嵌 28 节点（VM 出口中间件根本不完成 TLS 握手，和 v3.2 时一样）。
**仍未在真节点上验证（实话）**：本 VM 出口拦截 vpngate.net / *.opengw.net 的 TLS，真实节点连接、抓取、双端口同时监听都须 Leo 在 Windows 机器上试；新加的启动行为（不自动刷新 + 无 config 提示）、国家排序、回车/Ctrl+R、∞ 标记、并行探测的提速（死节点 ~20 秒 → 约 10 秒）也要在他机器上确认——先试 `public-vpn-192`（v3.2 已验证能连）。

---

## 10. 2026-11-07 晚 v3.4 这一批改动（上下方向键切换选中 + 跟随滚动）

背景：Leo 在 v3.3 的基础上只提了一个新需求：节点列表里用上下方向键切换选中行。范围就这一件，没扩。

33. **上下键切换选中（新增 `move_selection` + `ui()` 顶部的方向键处理，`GuiApp` 新增 `scroll_to_selected` / `node_list_scroll_y` / `selected_row_in_view` 三个字段）。** ↑ 把选中行往上移一行，↓ 往下移一行；到顶/到底停住，不循环。没有选中时 ↓ 选第一行、↑ 选最后一行。移动按显示顺序走（`display_indices()`，排序后按排好的顺序），`selected` 仍存节点下标，不错位。守卫与回车/Ctrl+R 同一套思路（没有文本框获得焦点——↑/↓ 在输入框里是移动光标、About 窗口没开），但**不**要求不忙/未连接：移选中行只动选中、不触发任何动作，所以忙碌（探测/连接中）时也能移。按住方向键会连续移动（egui `key_pressed` 包含按键重复事件）。实现上方向键处理放在 `ui()` 顶部、回车/Ctrl+R 处理的后面；egui 的 ScrollArea 本身不消费方向键（源码里没有 ArrowUp/ArrowDown 的处理），所以不会双重滚动。新增单元测试 `move_selection_follows_display_order_and_stops_at_ends`（无选中时 ↓/↑ 的落点、上下步进、到顶/到底停住且不请求滚动、排序后按显示顺序走、空列表不动）。

34. **跟随滚动（`scroll_to_selected` 一次性请求 + 行循环里的 `ui.scroll_to_rect(row_rect, None)`）。** 方向键移选中后，行循环在渲染选中行时调 `scroll_to_rect`（对齐参数 None：只滚到刚好能看见整行为止，已经在可视区内的行不移动列表——如果用居中对齐，每按一下可见行列表都会跳一下）。目标由最内层的节点列表 ScrollArea 消费（egui 的保存/恢复机制：内层先取走目标，外层中央滚动区看不到它），所以只滚列表、不滚整个主面板。鼠标点击不设这个请求（点的行本来就可见）。`scroll_to_selected` 只在选中真正变化时才置位，到顶/到底按方向键不动选中、也不请求滚动；列表为空时没有行消费它，无害。新增字段 `node_list_scroll_y`（每帧从 ScrollArea 输出的 `state.offset.y` 取，测试用它证明列表真的滚了）与 `selected_row_in_view`（行循环里用 clip rect——即列表的可视区——判断选中行纵向是否完全可见，每帧重算；只看纵向：行是铺满整宽的，560 px 宽时行会横向超出 clip rect，那与是否在屏上无关）。新增 kittest `arrow_keys_move_selection_and_follow_scroll`（560×420、10 个节点，列表约 120 px 高、只放得下约 3 行）：无选中时 ↓ 选第一行、↑ 到顶不动、↓↓↑ 走到第 2 行、连按 8 次 ↓ 走到底（断言 `node_list_scroll_y > 0` 且 `selected_row_in_view` 为真）、到底再按 ↓ 不动、忙碌时 ↑ 仍能移选中、回车在新选中的行上照常触发连接、Ctrl+R 照常触发探测；滚动动画（默认 0.1–0.3 秒）走完才断言，用 `run()` 等它 settle，忙碌段用 `run_steps(3)`（spinner 一直请求重绘，`run()` 不会停）。

35. **版本 0.3.3 → 0.3.4**（`Cargo.toml`；User-Agent 与关于窗口仍读 `CARGO_PKG_VERSION`）。README / WINDOWS.md 同步更新（方向键导航与跟随滚动）。

本批测试（本 VM，`cargo test`，CARGO_EXIT=0）：lib 30 项、GUI 23 项（原 22 + 方向键移动单元测试 1 项）、集成 3 项、CLI 0 项，全过，共 56 项。8 个忽略的测试用 `cargo test --bins -- --ignored <名字>` 逐个跑过，全过（新增 `arrow_keys_move_selection_and_follow_scroll` 一次通过；其余 7 个与 v3.3 相同，无回归）。
**仍未在真节点上验证（实话）**：与 v3.3 相同，本 VM 出口拦截 vpngate.net / *.opengw.net 的 TLS；新加的方向键导航与跟随滚动须 Leo 在 Windows 机器上确认——打开新版，点一行后按 ↓/↑ 试切换，确认选中行跟随滚动、回车仍能连接。

