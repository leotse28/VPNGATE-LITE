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

## 11. 2026-11-07 晚 v3.5 这一批改动（抓取带运营商、API 超时 30 秒）

背景：Leo 在 v3.4 上实测（`vpngate-gui-3.log`）：启动在新文件夹、没有 config.toml，前置代理未启用，加载内嵌列表；前置代理测试通过（Google 2670 ms，vpngate.net 6955 ms）；23:23:00 刷新时“官方 API 拉取超时（15 秒内无响应）；已改用网页抓取（100 个节点）”——整列运营商全空。已确诊：抓取路径的节点 isp 和 name 都是空字符串（`parse_web_page` 建节点时 `isp: String::new()`、`name: String::new()`，只对 28 个内嵌节点用 curated 表补 isp），而 GUI 那一格是 isp 为空时退到 name、都空就空白——所以整列全空，渲染本身没问题。另外两个日志事实：① v3.3 的 15 秒超时按设计工作了（没再挂 85 秒）；② vpn<数字> 节点并非全死——23:23:37 `vpn359610091.opengw.net:995` 连上了（虚拟 IP 10.211.1.62）。

36. **抓取带运营商（新增 `row_cell_bodies` + `row_operator_name`，`parse_web_page` 里每行取一次填 `isp`）。** 先弄清了中继列表页的表格结构：Wayback Machine 的 CDX API 与快照页在浏览器里都返回 403，改用浏览器直接读了 www.vpngate.net/cn/ 的实时页面（2026-11-07 15:29 UTC 抓取的副本）。表是 10 列：国家/地区、DDNS 主机名 IP 地址（ISP 主机名）、VPN 会话数、线路质量、SSL-VPN、L2TP/IPsec、OpenVPN、MS-SSTP、**志愿者操作员的名字（+ 操作员的消息）**、总分。结论：页面**没有**单独的 ISP/电信运营商列——ISP 信息只有 DDNS 格括号里的 ISP 主机名（如 `(public-vpn-14-01.vpngate.v4.open.ad.jp)`），只在部分行出现、且是主机名不是名字，所以不用它；运营商文本的来源是第 9 列“志愿者操作员的名字”，与 API 的 Operator（cols[12]）同一字段（同一节点两边值一致，如 `By Daiyuu Nobori, Japan. Academic Use Only.` 对应 Operator "Daiyuu Nobori"、Message "Japan. Academic Use Only."），填进抓取节点的 `isp` 与 API 路径在 GUI 运营商列里显示的值一致，不是硬编。写法：`row_operator_name` 找 stripped 文本以 "By " 开头的格，取 "By " 之后、第一个 ", "（消息分隔）、" E-mail:" 或 ' "'（问候引用）之前的一段并 trim；找不到、格为空或名字为空都返回空串，绝不 panic、不影响现有的主机名/国家解析与去重/端口逻辑。内嵌 28 节点的 curated 补 isp 逻辑不变（只在 isp 为空时补，所以 curated 的真实 ISP 名仍优先于抓取的运营商名，与 API 路径一致）。新增测试：`row_operator_name_edges`（带消息、带 E-mail+问候、无该格、"By " 为空、"Bye" 不误匹配）与 `parse_web_page_extracts_operator`（按实时页面拼的 10 列 fixture：Daiyuu Nobori / MOODLE-SERVER's owner / Japan VPN 三种写法、空运营商格、3 列无运营商格的行、重复主机去重+端口升级不受影响）。

37. **API 超时 15 秒 → 30 秒（网页抓取保持 15 秒）。** `FETCH_TIER_TIMEOUT` 拆成 `API_FETCH_TIMEOUT`（30 秒）与 `WEB_FETCH_TIMEOUT`（15 秒）；`fetch_nodes_with_fallback_timeout` 改为两个超时参数，各档超时文案用各自的秒数（"官方 API 拉取超时（30 秒内无响应）" / "网页抓取超时（15 秒内无响应）"）。理由：API 是更全的数据源（带 Operator、Message、分数、延迟、速度），经 Leo 的前置代理访问它本来就慢（前置代理测试里 vpngate.net 一站就要约 7 秒；v3.2 无超时预算时 API 是能拉到的），15 秒太紧、动不动就掉到抓取档。最坏情况 API 30 秒 + 网页 15 秒 = 45 秒，仍远好于以前挂 85 秒。黑洞代理测试改传两个 2 秒超时（断言不变：两档各跑满超时、总耗时 3–8 秒、两档都报“超时”）。

38. **版本 0.3.4 → 0.3.5**（`Cargo.toml`；User-Agent 与关于窗口仍读 `CARGO_PKG_VERSION`）。README / WINDOWS.md 同步更新（抓取的运营商来源、API 30 秒 / 网页 15 秒）。

本批测试（本 VM，`cargo test`，CARGO_EXIT=0）：lib 32 项（原 30 + 运营商提取 2 项）、GUI 23 项、集成 3 项、CLI 0 项，全过，共 58 项。8 个忽略的测试用 `cargo test --bins -- --ignored <名字>` 逐个跑过，全过（与 v3.4 相同，无回归）。
**仍未在真节点上验证（实话）**：与 v3.3/v3.4 相同，本 VM 出口拦截 vpngate.net / *.opengw.net 的 TLS；新加的抓取运营商与 API 30 秒超时须 Leo 在 Windows 机器上确认——打开新版确认启动列表有运营商，点刷新节点（API 现在最多等 30 秒），确认刷新后的列表运营商也不空，再看日志里这次走的是 API 还是抓取。

---

## 12. 2026-11-08 凌晨 v3.6 这一批改动（探测全部 + 当前人数/线路质量两列 + 四列可点排序）

背景：Leo 在 v3.5 的基础上提了四点，并进同一版（版本仍是 0.3.6）：① 一键探测全部节点；② 节点列表加两列信息；③ 两列可点表头排序；④ “延迟/状态”列也可点表头排序。表头文字以 Leo 最后的规定为准：新加的两列必须是“当前人数”与“线路质量”，不许缩写成“连接人数”、不许加括号或后缀（悬停提示可以写全称）。

39. **探测全部（新增 `Busy::ProbingAll` + `GuiEvent::ProbeAllProgress` / `ProbeAllResult` / `ProbeAllDone` + `probe_all_flow` + `start_probe_all` / `stop_probe_all`）。** “探测全部”按钮放在操作区、“探测”按钮旁边（96×34），列表为空或正忙时不可用；点一下按 `display_indices()` 的当前显示顺序（排序后按排好的顺序、没排序时按拉取顺序）、一个一个顺序探测全部节点，不并行：一次只测一个，走现有的单节点探测流程（`probe_any`，隧道超时不变）；每个节点的结果一出来就写进它那行的延迟/状态格（复用单节点探测的状态写法，正在测的那格显示“探测中…”），列表实时更新。进度与取消：状态栏/日志实时显示进度（如“正在探测全部：12/95（public-vpn-192）…”）；扫的过程中按钮变成“停止”（点过一次后禁用、悬停提示“正在停止，等当前节点测完…”），点一下在当前节点测完后停下（`probe_all_stop: Arc<AtomicBool>`，后台任务每测一个节点前后都查一次），已出的结果保留；扫完（或中途停止）后日志写一行汇总——完成时“探测全部完成：95 个节点，12 个可用，83 个失败。”、停止时“探测全部已停止：已测 40/95，8 个可用，Y 个失败，Z 个未测。”、前置代理不通时“探测全部已停止：前置代理不通。已测 d/N，…，Z 个未测。”并弹一次横幅指名代理。前置代理中途断了也提前停（`ProbeFailure::ProxyDown` 时任务直接收尾）。守卫：复用现有 `busy` 机制——全量探测期间 `busy = ProbingAll` 置位，单节点探测/连接/刷新都不能开始（按钮禁用、快捷键守卫也拦）；回车连接、Ctrl+R 探测被忙守卫拦住，上下键移选中照常（只动选中、不触发动作）。扫的过程中每节点只写一行简洁日志、不弹逐个失败横幅（83 个失败横幅会刷屏）。新增测试：单元 `probe_all_targets_follow_display_order`（拉取顺序 + 国家 A-Z 排序后的顺序 + 空列表）、`probe_all_events_drive_progress_and_stop`（开始守卫、进度/结果事件驱动、扫的过程中不弹横幅、停止/完成/代理不通三档汇总文案）、异步 `probe_all_flow_stops_after_stop_request`（4 个黑洞目标、2 秒超时，第一个结果后置停止位，断言只出 1–2 个结果、已停、耗时 < 6 秒而非 ~8 秒全扫）；kittest `probe_all_button_starts_sweep_and_stop_halts_it`（点按钮后 busy 置位、状态含 0/3、首行“探测中…”、方向键照常移选中、点“停止”后 busy 清除、汇总写进日志）。kittest 里 `harness.run()` 的坑：忙时 spinner 每帧请求重绘，`run()` 超过 max_steps 报错，改用 `run_steps(3)`（这次先踩到、已修）。

40. **当前人数 / 线路质量两列（新增 `first_number` + `row_sessions` + `row_line_quality`，`parse_web_page` 每行取一次填 `sessions` / `speed_mbps` / `ping_ms`；GUI 新增 `sessions_cell` / `quality_cell` / `has_listed_stats`，两列固定宽 64 / 88 px，不进 `col_widths` 数组）。** 列顺序：状态（∞）| 国家/地区 | 运营商 | 当前人数 | 线路质量 | 主机:端口 | 延迟/状态。数据来源：API 节点用 `Node` 里已有的 sessions（cols[7] NumVpnSessions）、speed_mbps（cols[4]）、ping_ms（cols[3]）直接显示；抓取节点从中继列表页第 3 列“VPN 会话数”（如“285 会话 14 天”→ 取“会话”前的整数）与第 4 列“线路质量”（如“261.83 Mbps Ping: 8 ms …”→ 速度取“Mbps”前的数、四舍五入到整 Mbps，Ping 取“Ping:”后的数）取——动手前用浏览器再核对了一次实时页面（2026-11-07 16:15 UTC），10 列位置与 v3.5 一致，会话数格如“301 会话 14 天 累计 35,948,866 用户”、线路质量格如“261.83 Mbps Ping: 8 ms 2,006,591.21 GB 日志策略: 两周”；取不到、解析不出数字就记 0（未知），绝不 panic、不影响现有的主机名/国家/运营商解析与去重/端口逻辑。显示规则：三者全为 0 即无数据，两格都显示“—”（内嵌 28 个节点）；否则按实际值显示（0 会话的 API 节点显示“0人”）；格式是“47人”与“572M@3ms”。列宽：新列固定宽度（与 28 px 状态列同一做法），所以老 `config.toml` 的 4 元 `node_col_widths` 照常加载、不丢设置（`col_widths` roundtrip 测试未动）；分隔线映射改成 country(1)→widths[0]、ISP(2)→widths[1]、host:port(5)→widths[2]。行元组从 7 元扩到 9 元（多出 sessions/quality 两个格）。新增测试：`row_sessions_and_line_quality_edges`（“会话”前的整数、千分位逗号、空格、无“会话”不算、速度小数四舍五入、1,024.5 Mbps、无 Ping 仍取速度、空格、无对应格）与 `parse_web_page_extracts_sessions_and_quality`（复用 v3.5 的 10 列 fixture：285/262/8、70/195/11、127/149/5、68/670/12、3 列行全 0 且主机/国家解析不受影响）；GUI 单元 `sessions_and_quality_cells_format`（API 节点“47人”/“572M@3ms”、内嵌节点“—”/“—”、有数据 0 会话“0人”）。

41. **四列可点排序（`country_sort: Option<SortDir>` 泛化为 `sort: Option<(SortCol, SortDir)>`，新增 `SortCol` + `toggle_sort` + `quality_key` + `probe_ms: HashMap<String, u64>` + `latency_key`）。** 可排序的列：国家/地区、当前人数、线路质量、延迟/状态；状态（∞）、运营商、主机:端口不排序。同一时间只按一列排：点某列的表头就按那列排、之前那列的箭头清除，再点同一列反向；点不同列时方向回到该列的第一次点击方向（`toggle_sort`，纯函数、可单元测试）。排的是整行：复用现有的 `display_indices` 机制（行按显示顺序取，选中仍存节点下标、不错位），不另起一套；刷新后对新列表重新应用当前排序（与 v3.3 国家排序的行为一致）。方向规则：当前人数第一次点从少到多（▲，负载最轻的在前）、再点从多到少（▼）；线路质量第一次点按速度从快到慢（▼，速度为主键、Ping 为次键：速度相同时 Ping 小的在前）、再点反向（速度从慢到快，Ping 大的在前），无数据节点（三值全 0）在排序里视为最小——速度升序时排最前、降序时排最后，不混进有数据节点的中间（实现：`quality_key` = (有数据, 速度, Reverse(Ping))，整个 key 反向即得反向顺序）；延迟/状态第一次点：① 已测过且可用的节点按延迟毫秒数从小到大、② 未测的节点（“未探测”）、③ 失败的节点，正在探测中的节点（“探测中…”）按未测的那组排，再点同一列整个顺序反过来（失败的在前、未测的居中、已测可用的按毫秒从大到小在后）。延迟排序不从显示字符串里反解析毫秒数：探测成功时把毫秒数另外存一份（`probe_ms`，与 `probes` 并行的映射；单节点探测与探测全部的成功事件里写、开始探测与失败事件里清，刷新时与 `probes` 一样按主机名保留）；分类依据是探测结果的状态：有毫秒数（`probe_ms` 有该主机）的为可用组、“未探测”（`probes` 无该主机）/“探测中…”为未测组、其余失败文案（“失败”/“代理不通”）为失败组（`latency_key` = (组, 毫秒)，正在探测中的即使旧毫秒没清干净也强制按未测组——双保险）。表头悬停提示：当前人数“点击按连接人数排序”、线路质量“点击按线路质量排序（速度优先）”、延迟/状态“点击按延迟排序：已测的按毫秒从小到大，未测的居中，失败的最后”。旧测试随字段改名更新（`country_sort_orders_by_country_then_host`、`country_sort_survives_refresh`、`probe_all_targets_follow_display_order`、`move_selection_follows_display_order_and_stops_at_ends`、kittest `country_header_click_toggles_sort`，行为不变）。新增测试：单元 `sessions_sort_orders_few_first_then_many`（0/5/47/301 升降序）、`quality_sort_orders_by_speed_then_ping_with_no_data_at_the_ends`（500/10、500/30、100/5、无数据：降序 [500/10, 500/30, 100/5, 无数据]、升序反过来）、`toggle_sort_resets_direction_when_switching_columns`（切列回到新列默认值）、`latency_sort_orders_available_then_unprobed_then_failed`（30/30/120 毫秒升序 + 平局按主机名、探测中按未测组、失败最后、反向全倒过来）、`probe_ms_tracks_probe_events`（成功写毫秒、扫的进度事件清掉、失败不留毫秒、再成功重写）；kittest `sort_headers_toggle_and_switch_clears_arrow`（点当前人数表头两次方向切换、点线路质量表头切换、切到国家列后箭头只留在国家列——用 `query_by_label` 断言其余两列的 ▲/▼ 都不存在）、`latency_header_click_toggles_sort`（点延迟表头两次方向切换；560×420 下延迟列默认在可视区外，测试里把可调列宽临时收窄到 [50, 60, 80, 50] 才点得到）。

42. **版本 0.3.5 → 0.3.6**（`Cargo.toml`；User-Agent 与关于窗口仍读 `CARGO_PKG_VERSION`）。README / WINDOWS.md 同步更新（探测全部、两列、四个排序方向）。

渲染检查：`cargo test --bin vpngate-gui -- --ignored render_for_review` 通过，截图在 `/tmp/ui-review-*.png`（项目根目录的旧截图未动）。目视结论：800×600 深/浅两主题 7 列全显（国家/地区、运营商、当前人数、线路质量、主机:端口、延迟/状态，内嵌节点两新格都是“—”，延迟/状态“未探测”完整可见，无截断）；560×420 时左半列正常、横向滚动条出现、延迟/状态列要横向滚动一下才能看到（与 v3.3 相同，是设计行为——7 列总宽 788 px 超过 560 px 窗口）。列宽取舍：当前人数 64 px（表头“当前人数”4 字约 60 px，刚好放下）、线路质量 88 px（“572M@3ms”约 56 px、“1234M@123ms”约 77 px，放得下）——800×600 下刚好全显，未改默认列宽。

本批测试（本 VM，`cargo test`，CARGO_EXIT=0）：lib 34 项（原 32 + 解析会话数/线路质量 2 项）、GUI 32 项（原 26 + 排序/格子/探测毫秒 6 项）、集成 3 项、CLI 0 项，全过，共 69 项。11 个忽略的测试用 `cargo test --bin vpngate-gui -- --ignored` 一次跑过，全过（原 9 个无回归 + 新增 `sort_headers_toggle_and_switch_clears_arrow` 与 `latency_header_click_toggles_sort` 一次通过；`probe_all_button_starts_sweep_and_stop_halts_it` 曾因忙时 `run()` 不 settle 失败一次，改成 `run_steps(3)` 后通过）。
**仍未在真节点上验证（实话）**：与 v3.3–v3.5 相同，本 VM 出口拦截 vpngate.net / *.opengw.net 的 TLS；新加的探测全部、两列显示、四列排序须 Leo 在 Windows 机器上确认——打开新版：刷新出节点列表后点“探测全部”，看进度推进、点“停止”能停下、扫完看汇总（全量扫 95 个节点要花几分钟，死节点每个约一个超时，属正常）；检查 API/抓取节点两新列显示“47人”/“572M@3ms”、内嵌 28 个显示“—”；点四个可排序的表头，确认方向与箭头、整行跟着动。

## 13. 2026-11-08 凌晨 v3.7 这一批改动（混合端口 SOCKS5+HTTP + 局域网同端口 + 速度单位修 + 六列可调列宽 + 批量选中/删除/恢复）

背景：Leo 看了 NekoBox 的 Mixed (SOCKS+HTTP) 端口设置，要求本地代理也学这种混合形式（一个端口同时提供 SOCKS5 与 HTTP，局域网共享用同一个端口）；同批还报了线路质量速度数字过大的 bug（77570260M），并补了三列可调列宽与批量选中/删除/恢复。版本 0.3.6 → 0.3.7。

43. **混合端口 SOCKS5+HTTP（`src/socks5.rs` 全改写：`serve_socks5` → `serve_mixed` + `classify_first_byte` + HTTP 处理）。** 本地代理服务器在同一个端口（默认 10818）上同时接受两种协议：连接开头字节 `0x05` 走现有 SOCKS5 处理（CONNECT，走隧道，协议处理未动）；ASCII 字母开头（CONNECT、GET、POST、HEAD 等 HTTP 方法）走新增的 HTTP 代理处理；其他开头字节静默关闭连接、不 panic。HTTP 支持：CONNECT 方法（回 `HTTP/1.1 200 Connection Established` 后双向转发原始字节，HTTPS 走这条路）与绝对地址形式的普通 HTTP 请求（`GET/POST http://host:port/path`——经隧道转发给目标、把响应原样传回）。分流用 `TcpStream::peek`（不消耗，处理函数仍能看到完整流）。HTTP 细节：读头用 `read_http_headers`（读到 `\r\n\r\n`，64 KiB 上限，返回头字节 + 必须原样转发的剩余字节——绝不丢包内数据；泛型 `AsyncRead`，可用 `tokio::io::duplex` 单元测试）；`parse_http_request_line` / `parse_authority`（host/host:port/user@host:port；IPv6 字面量拒绝——隧道只支持 IPv4，与 SOCKS5 一致）；`http_target`（CONNECT → authority 默认 443；绝对 `http://` → host 默认 80；origin-form → Host 头默认 80；其他 scheme → 400）；`build_forward_head`（改写成 origin-form，丢掉 Proxy-Authorization/Proxy-Connection/Connection/Keep-Alive，强制 `Connection: close`）；`forward_http_body`（Content-Length 流式转发、chunked 分帧经 `PrefixedReader`，都封顶 64 MiB）。隧道打不开 → `502 Bad Gateway`；请求畸形 → `400 Bad Request`；空头/畸形头 → 静默关闭。无 HTTP 认证（忽略 Proxy-Authorization）；普通 HTTP 每次响应后关闭连接，不做 keep-alive 复用。新增测试：单元 10 项（分流、请求行/authority/target/path 边界、转发头丢弃、头切分剩余字节、EOF 报错、Content-Length 体、chunked 体——全过，修了 2 个 warning：未用的 `TunnelStream` 导入、`handle_mixed_conn` 里多余的 `mut`）；集成 `mixed_proxy_http_methods_against_mock`（mock SSTP 隧道：CONNECT 200 后透传数据拿到 mock 204、绝对地址 GET 拿到 204、带 Content-Length 的 POST 拿到 204）与既有 SOCKS5 端到端（改走 `serve_mixed`，仍通）。

44. **合并端口、取消单独的局域网端口（`src/config.rs`：`Listen` 去掉 `lan_port` + `default_lan_port()`；`listen_addrs()` 改回 `Vec<String>`（不再 `Result`））。** 混合服务器平时只监听 `127.0.0.1:<本地端口>`；打开“允许局域网连接”后，同一个端口再监听 `0.0.0.0:<本地端口>`（同一端口、全部网卡），不再另开端口。关键点：`allow_lan` 时 `listen_addrs()` 只返回 `["0.0.0.0:{port}"]`——通配符绑定本来就服务 loopback，127.0.0.1 与 0.0.0.0 同端口双绑会 EADDRINUSE；显示用 `Listen::display_addrs()`（`"127.0.0.1:10818"` 或 `"127.0.0.1:10818 ＋ 0.0.0.0:10818"`），系统代理目标用 `Listen::loopback_addr()` / 自由函数 `loopback_of(addr)`（把开头的 `0.0.0.0:` 换成 `127.0.0.1:`）。GUI 去掉局域网端口输入框，开关改成“允许局域网分享（混合端口同时监听 0.0.0.0）”；连接成功文案改成“本地混合代理（SOCKS5+HTTP）127.0.0.1:10818”，开局域网时再加“＋ 局域网 0.0.0.0:10818”；系统代理开关仍指本地端口，不受影响。CLI 的 `connect` 同样改成混合服务器（`serve_mixed`、日志文案同步改、`--listen` 覆盖时显示即所给地址、系统代理用 `loopback_of(addrs[0])`）。旧配置兼容：serde 忽略未知字段，老 `config.toml` 里的 `lan_port = 10809` 加载时忽略、不报错，保存时不再写出。新增测试：`listen_addrs_local_always_lan_same_port`（本机/局域网地址 + 显示/loopback）、`old_lan_port_key_is_ignored_not_an_error`（旧配置加载、保存时丢掉该键）、`old_port_key_still_parses_as_local_port`；GUI 测试 `listen_addrs_local_plus_lan_same_port`（局域网 → `["0.0.0.0:10818"]`、显示含 ＋、不再有“端口不能设成一样”的报错）与 `current_config_roundtrips_settings`（去掉 lan_port 断言）；集成 `wildcard_bind_serves_loopback_against_mock`（0.0.0.0:0 绑定后经 127.0.0.1 连上、SOCKS5 问候有应答）。

45. **速度单位修（`src/vpngate.rs`：`parse_api_csv`）。** 官方 API 的 Speed 字段（cols[4]）单位是比特每秒（bps），不是 Mbps——77,570,260 bps ≈ 77.6 Mbps；之前原样存进 `speed_mbps`，线路质量显示成 77570260M。修复：`speed_mbps = (bps + 500_000) / 1_000_000`（四舍五入到整 Mbps；77,570,260 bps → 78 Mbps）。Ping（cols[3]，单位就是 ms）与会话数（cols[7]，纯计数）不动。抓取路径不受影响（页面给的本来就是 "261.83 Mbps"，已四舍五入到整 Mbps）。副作用一起修好：修之前 API 节点的速度是抓取节点的约 100 万倍，线路质量排序时前面全是 API 节点——现在是同口径比较。CLI 的 `list` 输出走同一字段，随这次修复一起好，不用另改。新增测试：`parse_api_csv_sample` 更新（fixture Speed=100 bps → 0 Mbps）、`parse_api_csv_speed_is_bps_not_mbps`（77,570,260 bps → 78 Mbps、四舍五入边界 500,000→1、499,999→0、1,500,000→2）；GUI 单元 `api_speed_bps_converts_to_mbps_in_quality_cell`（真实量级行显示 `"78M@12ms"`、会话数 `"47人"`）与 `quality_sort_compares_api_and_scrape_speeds_like_for_like`（API 78 Mbps vs 抓取 262 Mbps：降序抓取在前、升序 API 在前）。

46. **六列可调列宽（`node_col_widths` 从 4 元扩到 6 元 + 分隔线映射改对）。** 现状是当前人数（64 px）与线路质量（88 px）固定宽、主机:端口虽在 `col_widths` 里但 Leo 反映拖不动——这次三列都要真正能拖。可调列变成 6 列（国家/地区、运营商、当前人数、线路质量、主机:端口、延迟/状态），状态（∞）列保持固定 28 px。`col_widths` 顺序改成显示顺序 [国家、运营商、当前人数、线路质量、主机:端口、延迟/状态]，默认 [90, 130, 64, 88, 190, 90]（总宽不变，默认布局不变）。分隔线：每列可调列（除最后一列外）后面都有一条可拖的分隔线（40–400 px，拖完自动保存）；逐列核对过映射——国家(1)→cw[0]、运营商(2)→cw[1]、当前人数(3)→cw[2]、线路质量(4)→cw[3]、主机:端口(5)→cw[4]，拖哪条就改哪列的宽，不错位；延迟/状态是最后一列可调列，按老做法它后面没有分隔线（它的宽仍存在 cw[5] 里，但拖不动——文档里写明）。旧配置迁移：自定义反序列化 `de_col_widths` 同时接受 4 元与 6 元——4 元按位置映射旧值（国家、运营商、主机:端口、延迟/状态 → 新下标 0、1、4、5），新加的两列（当前人数、线路质量）取默认宽（64/88）；其他长度（手改过的异常数组）回退到默认值、不断掉整个配置加载；新配置保存时写 6 元。新增测试：配置 `ui_col_widths_migrates_old_4_element_config`（4 元旧配置加载后四列旧值不变、两新列为默认值；保存写 6 元、重载不变）与 `ui_col_widths_unexpected_length_falls_back_to_default`（1/3/5/7 元都回退默认值、不报错）；GUI `col_widths_roundtrip_through_config` 更新为 6 元；kittest `column_separator_drag_widens_column` 改写：800 px 窗口（560 px 下主机:端口分隔线 x≈689 在可视区外、抓不到；从右往左依次拖——拖动只影响右边的列，未拖的分隔线位置不动）、依次拖主机:端口、线路质量、当前人数、运营商、国家/地区五条分隔线各 +40 px，断言对应列宽变了（≥30 px）、其他五列宽不动、且 `current_config` → `apply_config` 重载后还在。

47. **批量选中、删除选中、恢复已删除（GUI 选中模型改成 `selected_set: BTreeSet<usize>` + 锚点 `selected: Option<usize>` + 持久屏蔽列表 `Config::blocked_hosts`）。** 选中：单击只选该行；Ctrl+单击（macOS 上 Cmd+单击，`modifiers.ctrl || modifiers.command`）切换该行进出选中集合；Shift+单击选中从锚点（上一次单击的行）到这行的整个范围（按显示顺序——`display_indices()`，排序后按排好的顺序）。所有选中的行都用现有的选中高亮（靛蓝底白字）；锚点不一定在集合里（Ctrl+单击可以把它 toggle 出去），高亮跟集合走，锚点行的跟随滚动与可见性记录仍按锚点。方向键保持 v3.4 的行为：上下移动锚点一行，同时把选中收拢为只有锚点那一行。回车连接、Ctrl+R 探测、“探测”/“连接”按钮一律作用于锚点行（没有锚点时与原来一样不可用——`shortcut_action` 与 `can_act` 本来就按 `selected` 判断，未动）。删除：操作区加一行“删除选中 (N)”按钮（96×34，有选中且不忙时可用）；点一下把选中的节点从当前列表里删掉（按下标从大到小删，下标不移位），并把它们的主机名（补全后的完整主机名，`blocked_key` = `complete_ddns_host`，裸标签 "public-vpn-192" 与完整名匹配同一条）记进持久屏蔽列表（`config.toml` 顶层 `blocked_hosts` 数组，exe 旁边、自动保存）；屏蔽过滤在每次列表加载时都生效——`GuiEvent::Nodes` 处理（API 刷新、网页抓取、缓存、内嵌 fallback 都走这条事件）与启动 `fallback_nodes()` 两处都过一遍 `apply_blocklist`（缓存文件本身仍存完整列表，加载时再过滤）。已连接的节点不能删：如果选中里有它，跳过它并在日志写“已连接的节点不能删除，已跳过 X”。删完清空选中与锚点；被删主机的探测结果（`probes` / `probe_ms` 里的条目）一并清理。忙时（探测/连接/刷新/探测全部进行中）不许删除（`can_delete`：选中非空且 `busy.is_none()`，与刷新一样的 busy 守卫）。恢复：屏蔽列表非空时，在删除按钮旁边显示“恢复已删除 (N)”按钮；点一下清除 `config.toml` 里的屏蔽记录，并把本会话里删掉的节点（`deleted_nodes: Vec<Node>`，内存里留一份被删节点的完整数据）插回当前列表（追加在末尾）；如果程序重启过、内存里没有被删节点了，就只清屏蔽记录并在日志写“已清除删除记录，下次刷新后节点恢复”。恢复后也清空选中。手动添加节点时（列表头部插入，下标整体移一位）选中同步换成只有新行，避免旧集合指到错行。新增测试：单元 `click_row_selection_model`（单击/Ctrl+单击/Shift+单击范围正反向/锚点移动/显示顺序范围）、`arrow_keys_collapse_multi_select_to_anchor`（方向键收拢）、`delete_selected_removes_and_blocks`（移除、屏蔽持久化、配置 roundtrip、探测结果清理、选中清空）、`delete_selected_skips_connected_node` + `delete_selected_logs_connected_skip`（跳过已连接节点与日志文案——`delete_selected_with_connected` 把 connected host 作为参数传入，免得单元测试要造一个真隧道）、`delete_selected_blocked_when_busy`（忙时禁用与 no-op、空选中禁用）、`list_load_filters_blocked_hosts`（加载时过滤 + 裸标签匹配完整屏蔽项）、`restore_deleted_puts_nodes_back`（插回节点、清屏蔽、日志）、`restore_without_session_copies_only_clears_blocklist`（重启后只清记录、下次刷新恢复）；配置 `blocked_hosts_default_empty_and_roundtrip`（默认空、老配置无该键不报错、保存重载）；kittest `ctrl_click_multi_select_and_delete_button`（点 日本行、Ctrl+点 美国行 → 集合 {0,1}、点“删除选中 (2)”剩 德国行、屏蔽 2 个、点“恢复已删除 (2)”三行回来——kittest 里 Ctrl 的做法：`ModifiersChanged(ctrl)` 事件要在 press/release 前各排一次，因为 egui 只从 `ModifiersChanged` 事件更新 `modifiers` 状态，`event_modifiers` 末尾自带的复位会在同一帧里把它清掉，不能用）。

48. **顺手修（不在任务里，但不修全量测试过不了）：文件日志轮转竞态。** `FileLogger::rotate` 原来把 `.old` 写到固定名 `vpngate-gui.log.old`（与日志同目录），并行跑的两个 filelog 测试共用 `/tmp/vpngate-gui.log.old`，一个测试的 cleanup 会删掉另一个正在断言的 `.old` 文件——`rotates_after_max_bytes_and_keeps_old` 在全量运行时约一半概率失败、单独跑总过。修法：新增 `filelog::old_log_path(path)`（日志路径后追加 `.old`；真实日志仍是 `vpngate-gui.log.old`，名字不变，测试文件各自有唯一的 `.old`），`rotate` 与三处测试里的 `.old` 定位都改走它。修完后全量跑三遍稳定通过。

49. **版本 0.3.6 → 0.3.7**（`Cargo.toml`；User-Agent 与关于窗口仍读 `CARGO_PKG_VERSION`）。README / WINDOWS.md / `config.example.toml` 同步更新（混合端口、局域网同端口、10809 取消与旧配置兼容、六列可调、批量删除/恢复、速度单位）。

渲染检查：`cargo test --bin vpngate-gui -- --ignored render_for_review` 通过，截图在 `/tmp/ui-review-*.png`（项目根目录的旧截图未动）。目视结论与 v3.6 相同：800×600 深/浅两主题 7 列全显，默认列宽总宽未变（788 px）；新增的删除/恢复按钮行在操作区第二行，800×600 下完整可见。

本批测试（本 VM，`cargo test`，CARGO_EXIT=0）：lib 49 项（原 34 + 混合 HTTP 单元 10 项 + 速度 2 项 + 列宽迁移 2 项 + 屏蔽 1 项）、GUI 43 项（原 32 + 速度 GUI 2 项 + 列宽 roundtrip 更新 + 选中/删除/恢复 8 项 + 监听地址 1 项）、集成 5 项（原 3 + HTTP 方法 1 项 + 通配符绑定 1 项）、CLI 0 项，全过，共 97 项。12 个忽略的测试用 `cargo test --bin vpngate-gui -- --ignored` 一次跑过，全过（原 11 个无回归 + 新增 `ctrl_click_multi_select_and_delete_button` 与改写的 `column_separator_drag_widens_column` 一次通过）。
**仍未在真节点上验证（实话）**：与 v3.3–v3.6 相同，本 VM 出口拦截 vpngate.net / *.opengw.net 的 TLS；新加的混合端口（SOCKS5 与 HTTP 两种方式）、局域网同端口、删除/恢复、六列拖拽须 Leo 在 Windows 机器上确认——打开新版：连上一个节点，浏览器代理先按 SOCKS5 127.0.0.1:10818 试、再改成 HTTP 127.0.0.1:10818 试，确认两种方式都能上网；打开局域网共享，确认只用 10818 一个端口、局域网设备连 10818 也能用；选几个节点点“删除选中”，确认刷新后不回来、再点“恢复已删除”回来；拖当前人数、线路质量、主机:端口三列后面的分隔线，确认对应列宽变了、其他列不动、重启后还在。

## 14. 2026-11-08 凌晨 v3.8 这一批改动（Shift+单击范围选择修复 + 混合代理连接日志 + CONNECT 透传补强）

背景：Leo 在 Windows 10 真机上报了 v3.7 的两个问题——Shift+单击选范围没反应（Ctrl+单击多选能用），以及 Shift 选中再点“删除选中”删不掉；同时报了局域网现象：同一条已连接的节点，本机 127.0.0.1:10818 上 X 打不开、YouTube 页面能开但视频放不了，局域网 10818（另一台设备）上 ping 能通但 X 和 YouTube 页面都打不开。版本 0.3.7 → 0.3.8。

50. **Shift+单击范围选择失效（`src/bin/vpngate-gui.rs`：按下时锁存修饰键 + 两条调用路径互斥）。** 根因是修饰键只在松开那一帧读：行点击在松开帧完成（`clicked()` 与容错点击都在松开帧触发），用户先松 Shift 再松鼠标左键，松开帧读到的 `shift` 就是 false，Shift+单击退化成普通单击、只选中那一行——kittest 复现：`shift_click_survives_early_shift_release` 在修前失败（集合 `{2}` 而不是 `{0,1,2}`）。Ctrl 只是因为他松 Ctrl 的时机晚，才显得能用。修法：按下那一刻就把修饰键锁存进新字段 `row_press_mods`（与 `row_press_pos` 放一起，松开后清除），点击用的修饰键由纯函数 `effective_click_mods` 算——按下时锁存的 OR 松开帧的（Shift 中途按下、按下松开同一帧都覆盖；Cmd 算 Ctrl）；`click_row` 本身不用改（无锚点时 Shift+单击已有回退：只选该行并设为锚点）。第二个隐患一起修：`clicked()` 与容错点击原来是两条独立的 `if`，漂移 < 6 px 时同一帧各调一次 `click_row`——Shift 范围会被第二次调用收拢成单行，Ctrl 会被 toggle 两次抵消；现在改成互斥（`clicked()` 优先，容错点击只在它没触发时走）。对照实验证实了这两条路的关系：关掉 `clicked()` 后 Ctrl 测试照样通过、关掉容错点击后 Ctrl 与行点击容差测试都失败——kittest 里选中完全由容错点击驱动，`clicked()` 对这些行不触发（应与 ScrollArea 的交互有关）；真机上恰好只有一条路生效（Ctrl 对 Leo 能用），但代码不再依赖这一点。锚点语义不变：每次点击都移动锚点（v3.7 已定设计，`click_row` 注释与单元测试都这样断言），所以连续两次 Shift+单击是第二次从新锚点起算范围，不是 Explorer 式固定锚点——测试初版按 Explorer 语义写错了期望，已按实际设计改回。新增测试：单元 `effective_click_mods_latches_press_time_modifiers`（按下有 Shift/松开无、按下无/松开有、两端都有、两端都无、无锁存回退、Ctrl/Cmd、Ctrl+Shift）；kittest `shift_click_selects_range`（范围正向/锚点移动/普通单击收拢/方向键收拢）、`shift_click_without_anchor_selects_single_row`（删掉选中清掉锚点后 Shift+单击只选单行）、`shift_click_survives_early_shift_release`（先松 Shift 再松鼠标，范围还在）。

51. **Shift 选中→删除端到端（kittest 全流程）。** Leo 明确“Shift 选中再点删除，节点删不掉”——要按端到端验收，两种可能都要覆盖：(a) Shift+单击根本没选中（第 50 条的修饰键时机问题）；(b) 高亮有了、但删除按钮读到的集合是空的或不对。代码核对：(b) 在代码里不存在——高亮、删除按钮的数量标签、`can_delete`、`delete_selected_except` 全读同一个 `selected_set`，删除按下标从大到小删并记 `blocked_hosts`，没有第二份状态；所以 (a) 是根因，第 50 条的锁存就是修。新增 kittest（与第 50 条的三个测试共用 `click_row_label` / `shift_click_row_label` / `click_button_label` 三个辅助函数）：`shift_click_then_delete_removes_range`（普通单击 日本设锚点 → Shift+单击 法国 → 断言集合 {0,1,2,3}、删除按钮标签“删除选中 (4)”→ 点删除 → 只剩 英国行、blocked_hosts 有 4 个主机、选中清空）、`shift_click_first_then_delete_removes_single_row`（无锚点 Shift+单击 德国 → 只选该行 → 删除 → 剩两行、屏蔽里有它）、`shift_click_early_release_then_delete_removes_range`（与第一条同流程，但 Shift+单击时先松 Shift 再松鼠标——Leo 实际做的手势）。反向验证：临时关掉锁存（只读松开帧修饰键）重跑，恰好两个先松 Shift 的测试（`shift_click_survives_early_shift_release`、`shift_click_early_release_then_delete_removes_range`）与单元测试失败，其余通过——证明这批测试真的守住了这个修，不是空转；验证完已恢复锁存。

52. **混合代理连接日志（`src/socks5.rs`：`ConnLog` + `src/bin/vpngate-gui.rs`：`GuiEvent::ConnLog`）。** 每接受一个连接记一行：来源地址（127.0.0.1 还是局域网 IP）、嗅探到的协议（SOCKS5 / HTTP CONNECT / 普通 HTTP / 未知关闭）、CONNECT/SOCKS5 的目标主机（只记主机名，不记路径与内容），失败另有一行写原因。格式如“混合代理连接 192.168.1.5:54321：SOCKS5 → example.com:443”、“混合代理连接 127.0.0.1:54321：HTTP CONNECT → x.com:443”、“混合代理连接 192.168.1.5:54321 失败：隧道 TCP 到 … 失败：…”。实现：lib 加 `pub type ConnLog = tokio::sync::mpsc::UnboundedSender<String>` 与 `emit_conn_log`（有 sink 发通道、无 sink 走 stderr，CLI 行为不变）；`serve_mixed` 多一个 `Option<ConnLog>` 参数，`handle_mixed_conn` / `handle_socks5_conn` / `handle_http_conn` 透传；空连接（未发送数据）与未知首字节也记一行（未知首字节还记 0x 值）；HTTP 请求头未读完/请求行畸形/目标无法解析各记一行（400 场景）；隧道打不开时 serve 循环记失败行。GUI：`connect_flow` 接 `Option<ConnLog>` 传给 `serve_mixed`；`start_connect` 的任务里建无界通道、连接成功后起转发任务（`GuiEvent::ConnLog` → `log_line`，进日志面板 + `vpngate-gui.log`），转发任务的 handle 放进 `ActiveConnection.serve`，断开时随其他 serve 任务一起 abort。日志绝不记密码与请求体（CONNECT 行只有主机名）。用途就是给 Leo 的局域网问题定位：正常用一次、把日志发来——日志里完全没有局域网设备的地址 = 连接根本没到程序；有 SOCKS5 → 目标 = 到了，死在隧道/节点那头。

53. **HTTP CONNECT 透传补强（只读审查 + mock 回显测试，不改代理行为）。** Leo 的新信息：局域网设备用的是 SOCKS5，不是 HTTP——所以局域网打不开与 HTTP CONNECT 无关，CONNECT 洗脱嫌疑；CONNECT 全握手测试保留（有益无害）。审查结论（`src/socks5.rs` 只读）：CONNECT 在回 `200 Connection Established` 之后是纯 `copy_bidirectional` 字节透传，TLS 握手原样过，与目标站无关；`http_target` 对 CONNECT 就是 authority 本身（默认 443）；请求头上限 64 KiB（浏览器 CONNECT 头约 1 KiB，不会触顶）；局域网绑定（`listen_addrs` → `0.0.0.0:10818`）与 127.0.0.1 没有代码层面的差别——同一个监听、同一个 `serve_mixed`、同一个 handler，来源地址只影响日志里的 peer 字段。X/YouTube 的症状与已知结论一致：隧道只走 TCP，QUIC（UDP）过不去，YouTube/X 大量用 QUIC（本机页面能开、视频放不了；X 打不开），加上学术出口节点的过滤；“探测 OK”只说明 TCP 995 端口能完成 TLS 握手，不说明出口过滤与 UDP。新增测试：mock（`tests/mock_sstp.py`）加了带状态的 ECHO 模式——某条流的 payload 以 `ECHO:` 开头后，该连接后续各段都原样回显（不走 204），这样测试能证明二进制数据原样往返（只回 204 的话，数据被改坏也看不出来）；既有测试从不发 `ECHO:` 开头的 payload，行为不变。集成 `connect_relay_carries_tls_client_hello_against_mock`（通配符 0.0.0.0:0 绑定、经 127.0.0.1 连）：SOCKS5 问候 + CONNECT example.com:443 + `ECHO:socks5-echo` 原样回来；HTTP CONNECT 200 后发 `ECHO:` + 52 字节伪造 TLS ClientHello（含 0x00 字节）原样回来；裸 0x16 字节直发代理端口被关闭（注：服务端 `peek` 不消耗该字节、随后关闭时接收缓冲里还有未读数据，内核回的是 RST 而不是干净 FIN——TCP 的正常行为，测试接受两种结果）；再收连接日志断言 SOCKS5 / HTTP CONNECT 两行都带 peer 127.0.0.1 与目标主机、未知协议行也在。

54. **版本 0.3.7 → 0.3.8**（`Cargo.toml`；User-Agent 与关于窗口仍读 `CARGO_PKG_VERSION`）。README 多选一条加了 Shift 可靠性说明；WINDOWS.md 新增“局域网排障”一节（见下）。

**局域网排障（WINDOWS.md 新增一节，要点）**：第一条就是防火墙——开局域网分享时没有弹过提示不等于放行了：127.0.0.1 不受防火墙入站规则管（本机能通），局域网进来的 TCP 10818 没有放行规则会被拦，表现为“局域网设备什么都打不开、但 ping 通（ICMP 另算）”，与 Leo 的现象完全吻合。去“Windows Defender 防火墙 → 允许应用通过防火墙”找 vpngate-gui.exe，确认“专用”已勾；没有这一项就手动添加放行；不要教他关防火墙。第二条看 v3.8 连接日志定位（无局域网地址 = 没到程序；有 SOCKS5 → 目标 = 到了；有失败行 = 按原因查）。第三条局域网设备上关 QUIC 再试（chrome://flags 关 Experimental QUIC protocol）。第四条局域网设备代理要填本机 IP 不是 127.0.0.1。Leo 明确不想再做纯 HTTP 站测试——不再给他布置测试，定位靠连接日志，他正常用一次、把日志发来就行。

渲染检查：`cargo test --bin vpngate-gui -- --ignored render_for_review` 通过（在 18 个忽略测试一次跑过里），截图在 `/tmp/ui-review-*.png`（项目根目录的旧截图未动）。目视结论与 v3.7 相同。

本批测试（本 VM，`cargo test`，CARGO_EXIT=0）：lib 49 项（与 v3.7 相同，无 lib 新增测试）、GUI 44 项（原 43 + `effective_click_mods` 单元测试 1 项）、集成 6 项（原 5 + `connect_relay_carries_tls_client_hello_against_mock` 1 项）、CLI 0 项，全过，共 99 项。18 个忽略的测试用 `cargo test --bin vpngate-gui -- --ignored` 一次跑过，全过（原 12 个无回归 + 新增 6 个 Shift 测试：`shift_click_selects_range`、`shift_click_without_anchor_selects_single_row`、`shift_click_survives_early_shift_release`、`shift_click_then_delete_removes_range`、`shift_click_first_then_delete_removes_single_row`、`shift_click_early_release_then_delete_removes_range`）。
**仍未在真节点上验证（实话）**：与 v3.3–v3.7 相同，本 VM 出口拦截 vpngate.net / *.opengw.net 的 TLS；新加的 Shift 选中→删除、连接日志、CONNECT 透传须 Leo 在 Windows 机器上确认——打开新版：单击一行、Shift+单击另一行看范围高亮、点“删除选中”看行消失且刷新后不回来；正常用一次、把 vpngate-gui.log 发来，看连接行里有没有局域网设备的地址；局域网问题先按 WINDOWS.md“局域网排障”第 1 条查防火墙放行。

## 15. 2026-11-08 凌晨 v3.9 这一批改动（UDP 中继）

背景：Leo 在手机上用 v2rayNG、SOCKS5 连局域网共享的 10818 端口：节点已连通，v2rayNG 测速能测到真延迟（TCP 进隧道是通的），但手机浏览器上不了网。原因：手机流量里有 UDP——DNS 查域名、QUIC（视频和很多网站走它）；代理当时只转发 TCP，UDP 包到了就悄悄丢掉，没有回音，浏览器每个连接都卡住。本机 Chrome 页面能开是因为它的 QUIC 是直连、不走代理，失败了能很快回落到 TCP；手机经过 v2rayNG，所有流量（包括 UDP）都被收进代理，UDP 一丢就是黑洞。隧道本身能走 UDP（里面传的是 IP 包），只是还没实现。Leo 点了“做 v3.9，加 UDP 中继”。版本 0.3.8 → 0.3.9。

55. **隧道里的 UDP（`src/sstp.rs`：IP 层分流 + `UdpFlow`）。** PPP 层传的是 IP 包，原来只实现了 TCP（`handle_ip_packet` 里 `ip[9] != 6` 就扔掉）。现在按协议号分流（TCP=6、UDP=17、其余忽略）：`udp_frame` 组 IP+UDP 头（校验和算对，算出来是 0 时按 RFC 768 发 0xFFFF），`UdpFlow` 是一条 UDP 会话（虚拟 IP 上的源端口 → 一个目标，无握手，收发走 mpsc 通道；drop 时从分流表里摘掉），`open_udp_flow`（域名经 DoH 解析，与 TCP 同一路）/`open_udp_flow_ip`（已解析的 IP）、`send_udp`、`close_udp_flow`。源端口在 20000–59999 里随机分配（UDP 与 TCP 端口空间分开，只查 UDP 表）。单包上限 1472 字节（1500 字节 PPP MRU 减 20 IP + 8 UDP），超了拒绝、不分片。回包按目标端口匹配到流，只认端口、不认来源 IP（DNS anycast、QUIC 的回包来源 IP 可能与查询目标不一致）。新增单元测试：`udp_frame_checksums_validate`（IP 头与 UDP 校验和重算都为 0、端口与长度字段、payload 原样）、`udp_frame_size_cap`（1472 进、1473 拒）。

56. **SOCKS5 UDP ASSOCIATE（`src/socks5.rs`：`UdpRelay` + 命令 0x03）。** SOCKS5 请求分发原来只认 CONNECT（0x01），其余一律回 0x07；现在 0x03 走 UDP ASSOCIATE：回中继的 UDP 地址（中继绑 0.0.0.0 时回 TCP 连接上看到的本机 IP，0.0.0.0 本身没法用），随后把这条 TCP 连接挂起——关联随它活，断开就清掉该客户端 IP 的所有流（60 秒空闲扫除是兜底）。UDP 中继绑在与 TCP 监听相同的地址（同端口号，TCP 与 UDP 端口号不冲突；127.0.0.1 还是 0.0.0.0 与 TCP 监听一致）；UDP 绑定失败时 TCP 代理照常工作，ASSOCIATE 回 0x07 拒绝（有明确错误行）。数据报处理：解析 SOCKS UDP 头（RSV 必须为 0、FRAG > 0 直接丢弃并文档写明、IPv6 与畸形头丢弃），按（客户端 UDP 地址、目标主机、目标端口）找流或新建流（域名每流只解析一次，不按包解析），payload 经隧道发给目标；回包由每流一个转发任务包上 SOCKS UDP 头（来源 = 目标解析后的 IP:端口）发回客户端。流表（`UdpFlowTable`，纯 bookkeeping、无隧道无 socket，可单独测）：上限 1024 条，满了先逐最久未活动的流；空闲超 60 秒扫除（每 15 秒一扫）；`remove_if_same` 只删仍是同一条的 entry（新流同 key 不会被旧转发任务误删）。日志：每条流新建记一行（来源地址、目标主机:端口、首包字节数），失败（解析/打通）记一行；绝不记 DNS 查询内容与 payload 正文。HTTP 代理没有 UDP，只给 SOCKS5 做。新增单元测试：`parse_socks_udp_edges`（IPv4/域名/FRAG 保留/IPv6 拒/RSV 错拒/截断拒）、`build_socks_udp_roundtrip`、`udp_flow_table_sweep_drops_idle_only`、`udp_flow_table_evicts_oldest_when_full`、`udp_flow_table_remove_client`。

57. **生命周期（`serve_mixed` 的 select 循环 + JoinSet）。** UDP 接收循环没有另起独立任务——它与 TCP accept 跑在同一个 select 里，转发任务挂在同一个任务栈的 JoinSet 上：断开时 abort 混合代理任务，JoinSet 一起掉、转发任务全停、UDP socket 释放，不会漏一个绑着 10818 的 UDP 监听挡住下次连接。转发任务的退出路径：流关闭（扫除/隧道死）时 recv 返回 None，退出并摘掉自己的表项；隧道死后无流量，流会在 60 秒内被扫除兜底。

58. **mock 与集成测试（`tests/mock_sstp.py` + `tests/gui_flow.rs`）。** mock 加了 UDP ECHO 模式：隧道里发来的 UDP 数据报按原样回显（来源换成目标地址，如真 NAT），与 TCP 的 ECHO 模式互不干扰（按 IP 协议号 17/6 分支在先）。新增集成 `udp_flow_echo_against_mock`（隧道层，不经过真实 UDP socket：`open_udp_flow` 到 127.0.0.1:80 发两包都原样回来、域名目标经 DoH 同样回来、2000 字节包被拒绝）和 `socks5_udp_associate_echo_against_mock`（完整 ASSOCIATE：问候 → 命令 0x03 → 回复地址是 127.0.0.1 且端口与 TCP 监听同号 → 发 IPv4 目标与域名目标的数据报都原样回来 → FRAG=1 的包无回复 → 日志里有 UDP ASSOCIATE 行）。**本 VM 的限制（实话）**：这台 VM 的沙箱禁了 UDP `sendto`（连 loopback 都报 EPERM，Python 也一样），所以 ASSOCIATE 测试里数据报往返那段在本 VM 跑不了——测试先探针（绑一个 UDP socket、往自己发一包），不可用时只跳过数据报部分，TCP 握手与日志断言照常跑；隧道层那项不经过真实 UDP socket，在本 VM 全过。mock 只能证明通路，真实 DNS/QUIC 仍需 Leo 在手机上验证（v2rayNG 测速 + 浏览器开 YouTube/X）。

59. **文档与边界（README / WINDOWS.md）。** README“已知限制”那条 UDP 改了：v3.9 起 UDP 走 SOCKS5 UDP ASSOCIATE（手机经 v2rayNG 的 DNS 与 QUIC 都走隧道）；WINDOWS.md“局域网排障”第 3 条同步改了：手机那侧不用再关 QUIC，但 PC 上 Chrome 的 QUIC 是直连、不走代理，UDP 中继管不到它——本机 X 打不开、YouTube 视频放不了仍要靠关 QUIC（chrome://flags 关 Experimental QUIC protocol）。版本 0.3.8 → 0.3.9（`Cargo.toml`；User-Agent 与关于窗口仍读 `CARGO_PKG_VERSION`）。

本批测试（本 VM，`cargo test`，CARGO_EXIT=0）：lib 56 项（原 49 + UDP 7 项：sstp 校验和与上限 2 项 + SOCKS UDP 头 2 项 + 流表 3 项）、GUI 44 项（与 v3.8 相同，无新增）、集成 8 项（原 6 + UDP 隧道层 1 项 + ASSOCIATE 1 项）、CLI 0 项，全过，共 108 项。18 个忽略的测试用 `cargo test --bin vpngate-gui -- --ignored` 一次跑过，全过（与 v3.8 相同，无新增）。
**仍未在真节点上验证（实话）**：与 v3.3–v3.8 相同，本 VM 出口拦截 vpngate.net / *.opengw.net 的 TLS；且本 VM 沙箱禁 UDP sendto，ASSOCIATE 数据报往返只在 mock 层验证过、未在真实 UDP socket 上跑过。须 Leo 在 Windows 机器 + 手机上确认——手机 v2rayNG 走局域网 10818：测速仍有真延迟、浏览器能开 YouTube（视频能放）与 X；本机 Chrome 的 QUIC 仍是直连，X/视频问题仍按关 QUIC 处理。

## 16. 2026-11-08 上午 v3.10 这一批改动（删除 bug 确诊与修复 + 去掉恢复已删除 + DoH 优先 IPv4）

背景：Leo 在 Windows 真机上用 v3.9（日志 `vpngate-gui-5.log`）：UDP 中继已验证通过（手机 DNS 与 QUIC 都走隧道），但删节点删不掉——点“删除选中”后日志显示“已删除 1 个节点”、屏蔽列表也涨了（92→96），选中的行却还留在列表里。v3.8 时删除是好的（他一批删了 89 个），v3.9 的 GUI 删除代码与 v3.8 完全相同。另两件事：Leo 明确不要“恢复已删除”；日志里又出现一次 IPv6 失败（`tunnel TCP to ipv6.icanhazip.com:443 failed: bad resolved IP 2606:4700::6810:b8f1`）。版本 0.3.9 → 0.3.10。

60. **删除 bug 确诊（`src/bin/vpngate-gui.rs`：`row_press_node` + clip 矩形守卫 + 帧级互斥）。** 先排除了最初的嫌疑——排序下标错位：读代码确认选中集合（`selected_set`）、`display_indices()`（排序后的显示映射）与删除（`delete_selected_except`，从高下标往低删）三处口径一致，全是节点数组下标；还写了两个 kittest（国家排序后点一行删、Ctrl+多选+排序删）都通过，证明简单排序错位不是原因；又证明了 tolerant-click 在列表滚动时只能丢点击、不能错选。真正的机制是两条复现测试钉死的：**(a) tolerant-click 按松开那一刻的行矩形认行**——按下某行、列表在按住期间滚动（滚轮/拖动）、在原位置松开，代码用“当前在松开点下方的行”当选中行，选中的是滚到光标下的另一行，删除就删了那一行、Leo 想删的行留下；**(b) 滚动条本身会“点行”**——`row_press_pos` 是窗口级任意按下都记，行矩形又横跨整个列表宽（含滚动条那 10 px），所以在垂直滚动条轨道上点一下（翻页）也会选中那个高度的行，下一次“删除选中”删的就是它。修复：按下时在行循环里记下按下的节点（`row_press_node`，按下那帧记、松开那帧清、新的按下先清掉旧值防 stale）；tolerant-click 只对 `row_press_node` 记下的那一行开火（仍保留 12 px 预算区分点击与真拖动）；只记落在列表 clip 矩形里的按下——clip 矩形不含滚动条带，滚动条按下不记行、不选行；互斥从“每行内”升级为帧级（`row_clicked`）：`clicked()` 与 tolerant-click 在同一帧里不能各打一行（列表动过之后两条路可能落在不同行上）。“已删除 N 个节点”的 N 本来就已经是实际删掉的数（`delete_selected_except` 返回的 `removed`，不是选中个数），不用改。新增 kittest：`drifted_click_after_scroll_selects_pressed_row`（按住一行、滚轮滚 3 行、原地松开，断言选中的是按下的那行）、`scrollbar_press_does_not_select_a_row`（在滚动条上（800 px 窗口，x=787）按+松，断言一行都没选中）——两条在修前都失败（分别选中了错行 node 7 与 node 2），修后通过。

61. **去掉“恢复已删除”（v3.10）。** 操作区的“恢复已删除 (N)”按钮、`restore_deleted()`、`deleted_nodes` 字段与恢复路径、两个恢复单元测试（`restore_deleted_puts_nodes_back`、`restore_without_session_copies_only_clears_blocklist`）全部删掉，不留死代码；`blocked_hosts` 持久化与加载过滤保留（删除后刷新不回来是删除本身的机制）；`ctrl_click_multi_select_and_delete_button` kittest 末尾改成断言恢复按钮不存在；README 多选一条与 `config.example.toml` 注释同步：想取消屏蔽就改 `config.toml` 里的 `blocked_hosts`，删掉那一行主机名，下次刷新它就回来了。

62. **DoH 优先 IPv4（`src/doh.rs`：`pick_ipv4` + `src/sstp.rs` 报错改写）。** 查清了 IPv6 的漏口：DoH 本身只问 `type=A`，漏在最后的系统解析器回退——`tokio::net::lookup_host` 取 `it.next()`，第一个地址可能是 IPv6（如 `ipv6.icanhazip.com` 的 2606:4700::6810:b8f1），随后 `sstp.rs` 把它当 `Ipv4Addr` 解析失败才报 `bad resolved IP`。修复：抽出纯函数 `pick_ipv4(host, addrs)`——有 IPv4 就取第一个 IPv4（A 与 AAAA 混着回也一样）；只有 IPv6 时返回明白的错（“only IPv6 addresses for …; the tunnel is IPv4-only”）；空结果也给明白的错。`resolve_ipv4` 的回退改走它；`sstp.rs` 两处 `bad resolved IP` 报错改写为“resolved … to …，which is not an IPv4 address (the tunnel is IPv4-only)”（代码里不再有 `bad resolved IP` 的报错，只剩注释里的历史引用）。新增单元测试 3 项：`pick_ipv4_prefers_ipv4_over_ipv6`（IPv6 在前也选 IPv4；多个 IPv4 取第一个）、`pick_ipv4_only_ipv6_is_a_clear_error`、`pick_ipv4_empty_is_a_clear_error`。README“已知限制”加了一条：隧道是纯 IPv4，IPv6-only 的站经隧道打不开是设计边界。

63. **版本 0.3.9 → 0.3.10**（`Cargo.toml`；User-Agent 与关于窗口仍读 `CARGO_PKG_VERSION`）。README 多选一条、已知限制、`config.example.toml` 同步；WINDOWS.md 没有删除/恢复相关内容，不用改。

本批测试（本 VM，`cargo test`，CARGO_EXIT=0）：lib 59 项（原 56 + DoH 3 项）、GUI 42 项（原 44 − 2 个恢复单元测试）、集成 8 项（与 v3.9 相同）、CLI 0 项，全过，共 109 项。22 个忽略的测试用 `cargo test --bin vpngate-gui -- --ignored` 一次跑过，全过（原 18 个无回归 + 新增 4 个：v3.10 前两个排序删除测试 `sorted_list_delete_removes_the_clicked_row`、`sorted_list_ctrl_click_delete_removes_both_rows`，与本批两个复现测试 `drifted_click_after_scroll_selects_pressed_row`、`scrollbar_press_does_not_select_a_row`）。
**仍未在真节点上验证（实话）**：与 v3.3–v3.9 相同，本 VM 出口拦截 vpngate.net / *.opengw.net 的 TLS；删除修复须 Leo 在 Windows 机器上确认——在 85 行列表里滚到下面、点一行“删除选中”，看消失的正是点中的那一行（不是滚动条位置那一行），再删几个看屏蔽列表只涨该删的主机；DoH 优先 IPv4 的逻辑由单元测试覆盖，真实 IPv6-only 目标的报错文案须他在真机上看日志确认。

## 17. 2026-11-08 上午 v3.11 这一批改动（导出选中到剪贴板 + Ctrl+C/Ctrl+V + 文件菜单 + 刷新自动清空屏蔽列表）

背景：三件事合进同一版。(a) Leo 要把选中的节点导出来：先是操作区“导出选中”按钮（07:20），随后加范围（07:23）：Ctrl+C 导出、Ctrl+V 导入、顶栏“文件”菜单四项（导出到剪切板、导出到config、从剪切板导入、从文件config导入）。(b) Leo 报刷新越刷越少（07:28）：删了一些节点后，每次“刷新节点”都只有当前剩下的数量（删到只剩 9 个，每次刷新也只有 8–9 个）。先读代码确诊：刷新是替换列表（不是合并）、`blocked_hosts` 过滤作用在 API 结果上、比对字段是补全后的完整主机名（`complete_ddns_host`，两侧口径一致）、缓存存的是过滤前的原始列表、回退不静默（每条分支都在状态栏与日志里写明来源）。算术排除“过滤多滤了”与“回退到小源”的简单版本——v3.9 那次刷新是 85 个、屏蔽 92 个，屏蔽再涨 4 个也只能到 81，不可能到 8–9；最吻合的是 API + 抓取两档都在失败、回退到了内嵌 28 个（28 − 约 19–20 个已屏蔽 ≈ 8–9）或旧缓存。Leo 随后发来 v3.10 的日志 `vpngate-gui-6.log`（07:36）定论：刷新确实每次都从 API 抓到新列表（07:00:02 那次 96 个），API 列表约 125 个，他那个会话里删了 28+16+6+34+32 = 116 个进屏蔽列表，125 − 116 ≈ 8–10，与每次刷新的 8–10 对得上；8 与 9 之间的浮动是每次新抓的。原因就是屏蔽列表把稳定的 API 列表滤得只剩几个，不是抓取失败。旁证（不用修）：这次运行是从新文件夹起的（启动时屏蔽 0、加载内嵌列表而不是缓存——config.toml 与 nodes_cache.json 又没带过去）；启动那一刻 API 与网页都不可用、1 分钟后刷新又通了，是瞬时问题。(c) 修法 Leo 先后否了/定了两次：先定的“文件”菜单“清空屏蔽列表”（07:36 前已实现）被他否了（07:38），改成他的方案——**刷新时先自动清空屏蔽列表**（本批最终修法）。版本 0.3.10 → 0.3.11。

64. **导出选中 + Ctrl+C/Ctrl+V + 文件菜单（`src/bin/vpngate-gui.rs`；新依赖 `rfd 0.18.0`、`arboard 3.7.0`）。** 操作区“删除选中 (N)”旁边加“导出选中 (N)”按钮（同一套守卫 `can_delete()`：没选中或正忙时灰掉）；导出逻辑抽成纯函数 `export_nodes_text(&[Node]) -> String`——按传入顺序（调用方给显示顺序）每行一个 `主机:端口`，主机名用补全后的 DDNS 主机名（与连接 dial 的目标一致），端口用节点自己的端口字段，没有端口的节点只写主机名、不发明端口。剪贴板走 `ctx.copy_text`（egui 自带，fire-and-forget、无失败信号，所以只在可检测的失败——空选中——时报；点完状态栏与日志各写一行“已导出 N 个节点到剪贴板。”）。Ctrl+C 与按钮同一份 `export_selected`；Ctrl+V 从剪贴板导入：按键本身触发（`consume_shortcut` Ctrl+V + 直接 `arboard` 读，空剪贴板有明确提示而不是静默），eframe 在下一帧补发的 `Event::Paste` 用 `drop_next_paste` 标志丢掉（只活一帧），防止一次粘贴导入两次；菜单里的“从剪切板导入”直接读剪贴板，不走按键、不用标志。顶栏菜单栏本来就有（`Panel::top("menu_bar")`，文件/退出、帮助/关于）——“文件”菜单新加四项，各带快捷键显示：导出到剪切板（Ctrl+C）、导出到config（`rfd` 保存对话框，默认 `nodes_export.txt`，与剪贴板完全一致的纯文本格式；对话框取消时日志写明）、从剪切板导入（Ctrl+V）、从文件config导入（`rfd` 打开对话框）。导入解析共用一份：`parse_import_line`（`主机:端口`、只有主机名也接受、空行与 `#` 注释行跳过、端口须 1–65535、主机名只含字母数字/`.`/`-`、裸标签（无点）也接受）与 `import_nodes_text`（按文件顺序追加到列表末尾、选中不动；已在列表里的主机跳过算重复；被导入的主机在 `blocked_hosts` 里就拿掉并写日志；导入后写 `nodes_cache.json`；正忙时拒绝导入——一次未完成的拉取会替换列表、悄悄丢掉刚导入的节点）；结果状态栏与日志写清成功/重复/格式不对各多少个（格式不对的行不让整个导入失败），剪贴板为空或没有可解析的行时明确提示。快捷键只在没有文本框获得焦点且 About 窗口没开时响应（复用回车/Ctrl+R 的焦点守卫，`ctx.text_edit_focused()`）——输入框里打字时 Ctrl+C/Ctrl+V 仍是输入框自己的复制粘贴。新增单元测试 6 项：`export_nodes_text_joins_display_order_with_ports`、`export_nodes_text_bare_host_for_unknown_port`、`export_selected_with_empty_selection_claims_nothing`、`parse_import_line_cases`、`import_nodes_text_adds_dedups_and_unblocks`、`import_nodes_text_empty_reports_nothing`。新增 kittest 5 项：`export_button_copies_selected_rows_in_display_order`（先点德国、再 Ctrl+点日本，断言剪贴板是显示顺序 `a.opengw.net:443\nc.opengw.net:995`）、`export_button_disabled_with_no_selection`、`ctrl_c_exports_selection`（注入 `Event::Copy`）、`ctrl_v_imports_from_clipboard`（注入 `Event::Paste`）、`ctrl_c_ignored_when_text_field_focused`（点手动添加输入框、断言 `text_edit_focused()` 后注入 Copy，断言无 CopyText）。kittest 里读剪贴板命令的办法：按下经 `run()`、只把松开事件排队后单步 `step()` 一次，再读 `harness.output().platform_output.commands` 里的 `CopyText`——`drop_at` 会多排一个 `PointerGone` 事件、普通 `run()` 会沉到复制命令之后的那帧，两种都会让命令消失；菜单项带快捷键时 accesskit 标签里会带快捷键（如“导出到剪切板 Ctrl+C”），kittest 里要用全名查。焦点测试的教训：节点列表高度随窗口缩放（`panel_h - 250`），加高窗口反而把输入框往下推；最后用 800×500 矮窗口 + 滚轮把外层滚动区滚下来再点输入框（点行会抢走输入框焦点，所以先选行、后聚焦输入框）。

65. **刷新自动清空屏蔽列表（`start_fetch` + `clear_blocklist` + `refresh_summary_line`；Leo 07:38 的决定，菜单项方案已撤）。** 语义变化（README / WINDOWS.md 已写明）：删除从此只是“本次会话里隐藏”，刷新永远能回到很多个节点。实现：唯一的刷新入口 `start_fetch`（刷新节点按钮；Ctrl+R 是探测，不走这里）在拉取前先调 `clear_blocklist()`——清空内存里的 `blocked_hosts` 并经 `auto_save_if_dirty()` 写回 `config.toml`（重启也不会复活），清空数存在新字段 `refresh_cleared_blocked` 里；随后照常抓取，Nodes 处理里 `apply_blocklist` 不变（刷新时屏蔽已空所以不过滤；中途删节点——刷新进行中又删了一个——仍会被滤掉，那是正确的），完成时记一行新汇总 `refresh_summary_line(source, cleared, final)`：“刷新完成（官方 API）：已清空屏蔽 116 个，获取到 125 个。”——以前那句“返回 N 个、屏蔽滤掉 M 个”在新语义下不再适用。启动加载（缓存/内嵌）仍按屏蔽列表过滤——只有显式点“刷新节点”才清空。`clear_blocklist()` 从“文件菜单动作”改成返回清空数的纯清理（调用方负责文案）；“清空屏蔽列表”菜单项与它的状态/日志文案已删掉，“文件”菜单仍是之前那四项；删除按钮的悬停提示与删除完成状态也改成新语义（“本次会话里隐藏，刷新后会带回来”），`config.rs` 里 `blocked_hosts` 的文档同步。`start_fetch` 里原来直接 `tokio::spawn`，改成与探测一致的 `spawn_task`（无运行时时 no-op）——这样 kittest 里能点“刷新节点”并观察忙标志与已清空的屏蔽列表，再手工推一个 Nodes 事件进来；生产里 `main` 照常 `rt.enter()`，行为不变。新增单元测试 3 项：`refresh_summary_line_states_source_and_counts`（四种来源的新格式）、`refresh_clears_blocklist_and_restores_full_list`（屏蔽非空时刷新：断言清空数、配置已保存、最终列表含原被屏蔽主机、日志含汇总行）、`curated_status_uses_filtered_count`（内嵌分支状态用过滤后的数，不再硬编码 28）。新增 kittest 1 项：`refresh_clears_blocklist_and_restores_deleted_nodes`（删 2 个、点刷新、断言屏蔽列表为 0 且忙标志置起、手工推 Nodes 事件后被删节点回来且日志含“已清空屏蔽 2 个，获取到 3 个。”；同时断言没有“恢复已删除”按钮、文件菜单里没有“清空屏蔽列表”、四项导入/导出都在）。测试卫生：`clear_blocklist` 单元测试与已有的 `auto_save_if_dirty_persists_front_proxy` 都写同一个 `target/debug/deps/config.toml`，并行跑时互相撕文件（曾出现半截 `node_col_widths` 行导致 TOML 解析失败）——加了测试模块静态锁 `PORTABLE_CONFIG_LOCK`，两处测试各持锁跑完全程；新增的刷新 kittest 也持同一把锁。

66. **版本 0.3.10 → 0.3.11**（只改 `Cargo.toml`；User-Agent、关于窗口、日志头仍读 `CARGO_PKG_VERSION`）。README 多选一条（删除新语义）与导出/导入一条、节点列表一条（刷新前清屏蔽）同步；WINDOWS.md 刷新段同步；`config.example.toml` 没有屏蔽语义相关内容，不用改。

本批测试（本 VM，`cargo test`，CARGO_EXIT=0）：lib 59 项（与 v3.10 相同）、GUI 51 项（原 42 + 新增 9 项单元测试）、集成 8 项（与 v3.10 相同）、CLI 0 项，全过，共 118 项。28 个忽略的测试用 `cargo test --bin vpngate-gui -- --ignored` 一次跑过，全过（原 22 个无回归 + 新增 6 个：上面 5 个导出/导入测试与刷新测试）。为跑 kittest，本 VM 重装了交叉编译与渲染驱动（VM 替换后丢了）：mingw-w64（`gcc-mingw-w64-x86-64-posix 13.2.0`、`binutils-mingw-w64-x86-64 2.41.90`、`mingw-w64 11.0.1`，direct .deb + `dpkg -i`，按 Packages 索引核对版本与依赖；`gcc-mingw-w64-x86-64-posix-runtime` 的包第一次下载被截断，断点续传后才完整）与 `mesa-vulkan-drivers 25.2.8`（lavapipe）。
**仍未在真节点上验证（实话）**：与 v3.3–v3.10 相同，本 VM 出口拦截 vpngate.net / *.opengw.net 的 TLS；须 Leo 在 Windows 真机上确认四件事——(1) 导出：选几个节点点“导出选中”/Ctrl+C，粘贴出来看是不是显示顺序的 `主机:端口`，每行一个；(2) 文件菜单：导出到config 的保存对话框能不能弹出、默认文件名是不是 `nodes_export.txt`，从文件config导入能不能读回；(3) 刷新自动清屏蔽：删几个节点（屏蔽列表涨），点“刷新节点”，看被删的节点是不是跟着完整列表回来了、屏蔽列表是不是 0，日志里是不是“已清空屏蔽 N 个，获取到 K 个。”；(4) v3.10 留下的两项——删除修复（在 85 行列表里滚到下面、点一行“删除选中”，看消失的正是点中的那一行）与 DoH 优先 IPv4 的真实 IPv6-only 目标报错文案（看日志确认）。
