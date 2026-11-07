# vpngate-client（Rust 版 VPNGate 客户端）

一个最小的 Rust 客户端：连 VPNGate 的志愿者节点（MS-SSTP + PPP），
在本地开一个 SOCKS5 端口（默认 `127.0.0.1:10818`），浏览器/应用连这个端口就能走隧道上网。
没有图形界面、没有 TUN 虚拟网卡、没有 OpenVPN，只有下面这几个功能。

> 背景：之前用 Cloudflare Worker + VPNGate 的方案（Worker 里用 JS 跑 SSTP/PPP），
> 在 NekoBox 里显示“已连接”却打不开网页。这个客户端把同一套协议搬到本机 Rust 实现，
> 直接连节点，少一跳，也方便排错。

## 功能

1. **节点列表**：从 VPNGate 官方 API（`https://www.vpngate.net/api/iphone/`）自动拉取；
   拉不到时改抓公开 relay 列表页（`https://www.vpngate.net/cn/`）解析节点表，
   再不行显示缓存、最后显示内嵌的 28 个节点（带已知的 SSTP 端口和备用节点）。
   也可以手动输入 `host:port`。成功拉到一次后会把列表存到程序旁边的 `nodes_cache.json`，
   下次拉不到时先显示这份缓存（列表上会写“缓存列表，约 X 小时前保存”），
   不会只剩一个干巴巴的报错。注意：API 和网页抓取都走同一个连接——如果你的网络
   直连打不开 vpngate.net，要先开前置 SOCKS5 代理（例如连上 Cloady 节点的 NekoBox），
   拉列表、DoH、连节点都会走它。
2. **前置 SOCKS5 代理（可选）**：配置后，连 VPNGate 节点、拉列表、DoH 解析都先走这个代理；不配置就直连。
3. **本地 SOCKS5**：隧道起来后在本地监听，TCP 连接经隧道里手写的用户态 TCP 协议栈转发。
   默认只听 `127.0.0.1:10818`（本地端口）；`--allow-lan` 会额外再听 `0.0.0.0:10809`
   （局域网端口），两个端口可以设成不一样的（配置 `[listen] local_port` / `lan_port`），
   局域网设备也能用（相当于 V2Ray 的“允许局域网连接”）。
4. **系统代理（可选，默认关闭）**：`--system-proxy` 会把系统代理指到本地端口，退出（Ctrl-C）时还原。
   Windows 用注册表、macOS 用 `networksetup`、Linux 用 `gsettings`（GNOME）；没有 `gsettings` 时打印 `export` 命令让你自己设置。
5. **Rust + tokio + clap + TOML 配置**，依赖尽量少。

## 编译

需要 Rust 1.75+（已在 Rust 1.99.1 上验证）。

```bash
cd vpngate-rust-client
cargo build --release
# 二进制在 target/release/vpngate-client
```

首次编译需要联网下载 crates（tokio、rustls、clap 等）；编好后可以离线用。
没有 `Cargo.lock` 之外的问题：如果你的机器上 `cargo build` 下载不到 crates，
把报错贴出来——这通常是网络问题，不是代码问题。

## 配置（便携版）

程序是便携的：所有文件都放在程序旁边，不写用户配置目录、不写注册表
（唯一的例外是“设置系统代理”那个开关本身——它改的是系统代理，关程序时自动还原）。
整个文件夹拷到别的电脑、别的目录都能直接用，配置和缓存跟着文件夹走。

`config.toml` 的查找顺序：命令行 `--config` 指定的 → 程序旁边的 `config.toml` →
工作目录下的 `config.toml`；都没有就用默认值。

复制一份配置示例到程序旁边：

```bash
cp config.example.toml config.toml
```

`config.toml` 各项（都有默认值，命令行会覆盖它）：

```toml
[front_proxy]
enabled = false
addr = "127.0.0.1:1080"   # 前置 SOCKS5 地址

[listen]
addr = "127.0.0.1"
local_port = 10818         # 本地端口（始终监听）
lan_port = 10809           # 局域网端口（allow_lan 时额外监听；不能和本地端口相同）
allow_lan = false          # true = 额外绑 0.0.0.0:lan_port

[system_proxy]
enabled = false
# service = "Wi-Fi"        # macOS 网络服务名

[tunnel]
timeout_secs = 10          # SSTP 建连 / TCP 握手超时（秒）
```

## 用法

```bash
# 1. 看节点列表（前 50 条；--country JP 只看日本）
./target/release/vpngate-client list
./target/release/vpngate-client list --country JP --limit 20

# 2. 探活：只做 SSTP 握手，看节点给不给虚拟 IP（不产生真实流量）
./target/release/vpngate-client probe kanratown.opengw.net:992
./target/release/vpngate-client probe 2        # 内嵌列表的第 2 条
./target/release/vpngate-client probe vpn267947139.opengw.net   # 没给端口时试 443/992/995

# 3. 连上并开本地 SOCKS5（按 Ctrl-C 退出）
./target/release/vpngate-client connect kanratown.opengw.net:992
./target/release/vpngate-client connect 2 --allow-lan
./target/release/vpngate-client connect vpn267947139.opengw.net:1663 --system-proxy
./target/release/vpngate-client connect kanratown.opengw.net:992 --front-socks5 127.0.0.1:1080

# 4. 一键测试：建隧道后直接测 generate_204 和 Google HTTPS
./target/release/vpngate-client test kanratown.opengw.net:992
```

### 浏览器/命令行怎么走隧道

SOCKS5 跑起来后（默认 `127.0.0.1:10818`）：

```bash
# curl 走隧道：先测连通性，期望 HTTP/1.1 204 No Content
curl -x socks5h://127.0.0.1:10818 http://www.gstatic.com/generate_204 -i

# 再测 Google
curl -x socks5h://127.0.0.1:10818 https://www.google.com -I
```

浏览器：把代理设为 SOCKS5 `127.0.0.1:10818`（注意选 SOCKS5 并勾选“代理 DNS”/远程 DNS，
否则域名在本机解析）。或者用 `--system-proxy` 让程序帮你设置系统代理。

### 前置 SOCKS5 怎么工作

`--front-socks5 127.0.0.1:1080`（或配置文件里 `front_proxy` 一节）：
程序先连这个 SOCKS5，再通过它连 VPNGate 节点做 TLS/SSTP。
用户名密码只支持在配置文件里填（`front_proxy.username` / `password`）。
不设置时全部直连。

### --allow-lan

默认只听 `127.0.0.1:10818`（本地端口，本机才能用）。加 `--allow-lan` 后额外再听
`0.0.0.0:10809`（局域网端口），同一局域网的手机/电脑可以把代理指到这台机器的 IP:10809；
本地端口照常可用。两个端口在配置 `[listen]` 里分别改（`local_port` / `lan_port`，
默认 10818 / 10809），不能设成一样；端口被占用时连接会直接报错写明是哪个端口。
注意：这会把代理暴露给局域网，只在可信网络里开。

### --system-proxy

默认**不碰**系统代理。加 `--system-proxy` 后：
- Windows：写注册表 `ProxyEnable=1`、`ProxyServer=127.0.0.1:10818`；退出时改回 0。
- macOS：`networksetup -setwebproxy/-setsecurewebproxy`（服务名默认 `Wi-Fi`，可在配置里改）；退出时关掉。
- Linux：`gsettings` 设 GNOME 代理为手动 SOCKS；退出时改回自动。没有 `gsettings` 时只打印 `export` 命令，不伪造成功。

## 图形界面（vpngate-gui）

目标平台：**Windows 10 x64**（见 [WINDOWS.md](WINDOWS.md)）。
除了命令行，还有一个桌面窗口版，功能和命令行完全一样，只是换成点鼠标操作。

```bash
cargo run --release --bin vpngate-gui
# 或者直接跑编好的：./target/release/vpngate-gui
```

**Windows 10 用户**：源码在 GitHub 仓库的 main 分支，编好的 Windows 程序
（`vpngate-gui.exe`、`vpngate-client.exe`）在仓库的 **Releases** 里下载，
双击就能打开，不用装 Rust。只验证了编出来，没在 Windows 上跑过，
第一次打开如果报缺 DLL，把报错发回来。自己编的步骤见 WINDOWS.md。

窗口是 800×600（最小 560×420），中文界面，主题跟随系统。用法：

- **节点列表**：启动时直接加载缓存列表（有则用缓存，无则内嵌的 28 个节点），**不自动拉取**——之前启动自动刷新在前置代理没开时会直连 vpngate.net 挂 85 秒。点右上角“刷新节点”才拉取，顺序是官方 API → 网页抓取（vpngate.net relay 列表页）→ 缓存列表 → 内嵌的 28 个节点，列表上方会写当前是哪一种来源；API 档最多 30 秒、网页档最多 15 秒，超时按原有文案报失败。点一行选中，整行都可点（行高 30 px，点行内任意位置都选中，鼠标按住拖动几像素再松开也算点中——之前 22 px 行 + 只有列宽那么窄经常选不中）；选中行是靛蓝底白字。运营商列来自官方 API 的 Operator 字段（之前读错了列，显示的是 LogType，全是 "2weeks"）；网页抓取的节点也从列表页的“志愿者操作员的名字”一列取运营商（与 API 的 Operator 同一字段，如 "Daiyuu Nobori"），抓取的节点没有中文名，运营商空时该格为空；主机名只显示短名（去掉 `.opengw.net` 后缀），完整主机名在悬停提示里。API 有时会返回不带后缀的短主机名（如 `public-vpn-192`），探测/连接时程序会自动补成完整域名再连。国家/地区列可以点表头排序：点一次 A-Z，再点一次 Z-A，来回切换，表头显示 ▲/▼；排序按国家名，同国家内按主机名；默认启动不排序（保持拉取顺序），刷新后对新列表重新应用你选的方向。国家/地区列前面有一列窄的状态列（28 px，表头是空的，不能拖宽）：当前已连接的节点在这一列显示 ∞，其他行留空，未连接时整列为空。探测/连接的候选端口（443/992/995）现在是并行试的，第一个通的获胜，死节点探测不再 3 个端口串行各挂 7 秒、总共约 20 秒，而是一个超时（默认 10 秒）左右就出结果；失败时仍按原有分类报“前置代理连不上”还是“节点不通”。上下方向键 ↑/↓ 可以在列表里切换选中行：到顶/到底停住，不循环；排序后按显示顺序走；选中行会自动滚进可视区（95 个节点的列表里按几下也不会看不见选中行）；忙碌（探测/连接中）时也能移选中行，只是不触发动作。方向键与回车/Ctrl+R 一样的守卫：没有文本框获得焦点、About 窗口没开时才响应。
- **连接 / 断开连接**：选中节点后点“连接”（主按钮），或直接按回车（悬停提示里写着“回车”）；连上后状态栏显示虚拟 IP 和本地 SOCKS5 地址，浏览器/应用连 `127.0.0.1:10818` 即可上网。点“断开连接”或关窗口都会断开。回车只在没有文本框获得焦点时响应（在手动添加节点、端口、前置代理地址等输入框里打字时按回车不会误触发），已在连接中或已有连接时也不触发。
- **探测**：只做 SSTP 握手，看节点给不给虚拟 IP，延迟显示在列表里；或按 Ctrl+R（悬停提示里写着“Ctrl+R”），与回车连接一样的守卫（无文本框焦点、不忙、已选中）。开着前置代理时探测走代理（日志/状态里会写“经前置代理”）；失败分两种：“前置代理连不上”（代理本身不通，刷新、探测、连接会一起失败）和“前置代理能连上，但节点不通”。
- **测试隧道**：连上后点它，测 generate_204 和 Google HTTPS，结果写在日志里。
- **允许局域网分享 / 设置系统代理**：主面板上的两个开关，不用展开高级设置就能看到；前者绑 `0.0.0.0:局域网端口`，后者默认关闭，断开或退出时自动还原，系统代理始终指本地端口。
- **本地端口 / 局域网端口**：主面板上的两个输入（默认 10818 / 10809），和上面两个开关放一起；局域网端口只在勾选局域网分享时可改；端口只在连接时生效，连接中两个都锁住，旁边会注明断开后才能改；两个端口设成一样时连接会报错。
- **高级设置**（折叠着的，默认收起）：手动添加节点（`主机名:端口`，格式不对会在字段旁边报错）；前置 SOCKS5（开关、地址、用户名、密码），旁边有个“测试前置代理”按钮，点一下就走这个代理测 Google 和 vpngate.net 通不通（走 SOCKS5，不是 ping），结果和毫秒数写在日志和状态栏；每站最多 10 秒（先连一下代理端口，最多 5 秒，含在 10 秒预算内），测的过程中状态栏会显示正在测哪一个站；测试用的是点击那一刻填的代理地址，测的过程中改地址不影响本次结果。代理没开时这个按钮是灰的，并提示要先开前置代理。节点列表的表头分隔线可以拖动调列宽，宽度跟着设置一起保存；拖宽后截断放宽，配合横向滚动能看全。窗口太矮时整个主面板可以上下滚动，高级设置和日志都能滚出来。保存设置按钮下面会显示日志文件的路径。

设置说明：图形界面里改的设置（前置代理、本地端口、局域网端口、允许局域网、系统代理开关）改完就**自动保存**到程序旁边的 `config.toml`，下次打开还在，不用点“保存设置”（那个按钮还在，点一下是手动再存一次）。
退出时如果开着系统代理，会自动还原。启动时如果旁边没有 `config.toml`，状态栏/日志会提示一句：“未找到 config.toml：如从旧版升级，把旧文件夹里的 config.toml 与 nodes_cache.json 复制到新版 exe 旁边即可保留设置”——便携式存储设计没变，配置仍在 exe 旁边。

**配色**：跟随系统浅色/深色，但两套都是定制的克制配色，与程序图标（靛蓝 #1E40AF → 青色 #0891B2 渐变）协调：主强调色靛蓝（“连接”按钮、选中行），已连接标记用青色，警告用 muted 琥珀，错误用柔和红；深色是深 slate 底 + 柔和 slate 面板 + 暖白文字，浅色是暖灰白底 + 墨色文字；按钮、分区标题、状态栏与日志区的层级与间距统一过。正文与面板、选中行白字与靛蓝底、∞ 标记与面板的对比度都按 WCAG 4.5:1 测过（测试里有断言）。

**日志文件**：图形界面每次启动都会把全部日志（信息、警告、错误）追加写到程序旁边的
`vpngate-gui.log`，每行带本地时间戳和 `[INFO]` / `[WARN]` / `[ERROR]` 标签；启动时
先写一段会话头（启动时间、版本号、程序路径、配置文件路径、前置代理是否启用及地址——
**绝不记录密码**）。文件超过 2 MB 时自动轮转成 `vpngate-gui.log.old` 再重新开始。
界面里的日志只留最后 100 行、关程序就没了，文件日志才是调错的依据：**报问题时请把
`vpngate-gui.log` 一起发来**，只截图界面日志常常看不到关键的那行。启动时界面日志里
会显示日志文件的完整路径，高级设置里保存设置按钮下面也显示。命令行版不用这个文件，
它把所有内容直接打印到控制台，那就是它的日志。

**中文字体说明**：egui 自带字体没有中文字，所以程序启动时会在系统里找一个中文字体加载。目标平台是 Windows 10 x64，优先找 `C:\Windows\Fonts\msyh.ttc` 微软雅黑，依次后备 `msyhbd.ttc`、`simsun.ttc`、`simhei.ttf`（都是 Windows 自带字体）；macOS 找苹方，Linux 找 Noto Sans CJK。如果你的机器上一个中文字体都没有，中文会显示为方框，窗口顶部会有一行提示——这时装一个 Noto Sans CJK 字体再重启就行。程序不自带字体文件。

**界面检查**：在 560×420 最小窗口与 800×600 下做过深色/浅色双主题渲染检查
（截图在项目根目录 `ui-review-dark.png` / `ui-review-light.png`、
`ui-review-dark-800x600.png` / `ui-review-light-800x600.png`，
150% 缩放一张 `ui-review-dark-zoom150.png`，滚动到底两张
`ui-review-dark-scrolled.png` / `ui-review-light-scrolled.png`，选中行一张
`ui-review-selected-row.png`，∞ 字形两张 `ui-review-infinity-dark.png` /
`ui-review-infinity-light.png`），中文显示正常，五列全部可见（800×600 时；
560×420 时延迟/状态列要横向滚动一下）；150% 缩放时无重叠。
详见 GUI-TEST-REPORT.md。真实 VPNGate 节点仍未在 GUI 里测过，
第一次用先按下面第 4 节的顺序试。

## 已知限制（实话）

- **只有 TCP 能走隧道，UDP 不走。** DNS 解析在程序里用 DoH（1.1.1.1）做，QUIC/UDP 应用、UDP 的 DNS 都会失败或绕开。
  这是协议栈本身的限制，和之前那个 Worker 方案一样。
- **志愿者节点极不稳定。** 节点随时下线、过载；同一个节点上午通、下午就可能不通。
  `probe` 活着只代表能拿到虚拟 IP，不代表从它家能打开 Google——以 `generate_204` 回 204 为准。
- **SSTP 端口没有官方来源。** VPNGate 的官方 API（`api/iphone`）不含 SSTP 端口。
  目前可靠的端口来源只有内嵌的 28 个节点（带已知端口和备用节点）+ 你手动输入 `host:port`；
  API 里不认识的节点只能按候选端口 443/992/995 去试，不保证能试出来。README 的这个限制是如实写的，
  不是没做——官方就没有这个字段。
- **TLS 不校验节点证书。** VPNGate 节点用自签证书，校验必然失败，所以到节点的 TLS 是 accept-any
  （和官方 SoftEther 客户端对 VPNGate 的做法、以及之前的 Python 探测脚本一致），
  校验仍是不校验，只换了握手用的栈：**Windows 上到节点也走系统 TLS 栈（SChannel）**，
  因为 rustls 的 SSTP 握手曾在握手中途被掐掉（`tls handshake eof`），而 SChannel 的 HTTPS
  走同一条前置代理能通——问题在握手特征，不在代理本身；**Linux/macOS 上到节点仍用 rustls**。
  到 `vpngate.net` 和 DoH 的 TLS 是正常校验的，但 Windows 和别的平台走的不是同一套握手：
  **Windows 上用系统 TLS 栈（SChannel）**，握手和浏览器一模一样——因为 rustls 的 ClientHello
  曾经被中间设备直接拒掉（`received fatal alert: HandshakeFailure`），而 Chrome 走同一条
  前置代理却能打开 vpngate.net；**Linux/macOS 上用 rustls**，校验用的根证书是 Mozilla 根证书
  **加上系统证书库**（浏览器信任的东西，程序也信任——如果网络里有自签 CA 做 TLS 拦截，
  浏览器能打开 vpngate.net、程序却报证书错，加系统库就是为了修这个）。TLS 握手失败时日志里
  会写原始错误，不只写“失败”。不要用这个客户端传真正敏感的东西——
  出口本身就是陌生人家的宽带。
- **速度。** 隧道是 TCP 套 TCP（用户态 TCP 再套 TLS），大文件、视频会比较慢；只适合轻量浏览和测试。
- **并发。** 每个 SOCKS5 连接在隧道里是一条独立的用户态 TCP 会话；同时开太多连接会拖慢隧道，
  浏览器一次开几十个连接时可能感觉卡。
- **系统代理还原。** 正常 Ctrl-C 退出会还原；如果程序被强行杀掉（kill -9 / 断电），
  系统代理可能留在开启状态，需要手动关掉（Windows 设置里关代理，macOS 同理）。

## 排错

| 现象 | 可能原因 / 做法 |
|---|---|
| `list` 拉不到 API | 先确认本机能不能打开 https://www.vpngate.net ；打不开就用缓存或内嵌列表（`probe 0`..`probe 27`、`connect <序号>`）。成功拉到一次后会存一份缓存，之后 API 断了也能显示缓存列表 |
| 刷新节点显示“官方 API 不可用” | 官方 API 被挡了（常见）；程序会接着试网页抓取（vpngate.net/cn 节点表），再不行用缓存列表或内嵌列表，不是程序坏了。缓存列表上有“约 X 小时前保存”；想更新它，等网络能打开 vpngate.net 时再点一次刷新；如果直连打不开，先开前置 SOCKS5 代理再刷新 |
| `probe` 全部超时 | 节点死了，或本机到 `*.opengw.net` 的 TLS 被拦截；换节点（先试内嵌列表里日本 NTT、德国 Telekom、日本 SoftBank 那几条）；也可以加 `--front-socks5` 走前置代理 |
| `probe` 活着但 `test` 的 generate_204 不通 | 隧道建起来了，但出口到目标站不通，或 TCP 握手超时；换节点；把 `--timeout` 调大到 15 再试 |
| 浏览器打不开、但 `curl -x socks5h://... generate_204` 通 | 浏览器 DNS 没走代理：确认浏览器代理设的是 SOCKS5 且勾选远程 DNS；或直接用 `--system-proxy` |
| 连上后过几分钟就断 | 志愿者节点掉线或被挤掉；重连换一个节点 |

## 文件

- `src/main.rs` CLI（list / probe / connect / test）
- `src/bin/vpngate-gui.rs` 图形界面（egui/eframe 0.36）
- `src/lib.rs` 共享库：CLI 和 GUI 共用下面这些模块
- `src/sstp.rs` SSTP + PPP + 用户态 TCP（核心，移植自 JS 参考实现）
- `src/socks5.rs` 前置 SOCKS5 客户端 + 本地 SOCKS5 服务端
- `src/net.rs` TCP 连接器 + 最小 HTTPS GET（Windows 上 verified TLS 与节点 SSTP 都走 SChannel，其余平台走 rustls；前置代理测试每站 10 秒总预算）
- `src/filelog.rs` 图形界面的文件日志（`vpngate-gui.log`，带时间戳、级别、2 MB 轮转，绝不记密码）
- `src/doh.rs` DoH 解析、`src/vpngate.rs` 节点列表、`src/sysproxy.rs` 系统代理
- `nodes.json` 内嵌的 28 个节点（已知 SSTP 端口 + 备用节点）
- `config.example.toml` 配置示例、`TEST-REPORT.md` 实测报告、`GUI-TEST-REPORT.md` 图形界面实测报告
