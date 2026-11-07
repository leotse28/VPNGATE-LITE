# Windows 10 x64 说明

目标平台：**Windows 10 x64**。命令行版和图形界面版都是按这个平台做的，
Linux 版只用来在本机验证编译和逻辑，不作为交付物。

## 方式一：直接用编好的程序（推荐）

源码在 GitHub 仓库的 main 分支；编好的 Windows 程序在仓库的 **Releases**
里下载，里面有这两个文件（放在同一个文件夹里就能用）：

```
vpngate-gui.exe      （约 38 MB，图形界面）
vpngate-client.exe   （约 10 MB，命令行版）
```

双击 `vpngate-gui.exe` 就能打开，不用装 Rust。注意两点：

- 第一次打开时 Windows Defender SmartScreen 可能会拦（程序没有代码签名），
  点“更多信息”→“仍要运行”即可。
- 默认只绑 `127.0.0.1:10818`，不会弹防火墙；只有勾了“允许局域网分享”
  （额外绑 `0.0.0.0:10809`）时才可能弹防火墙提示。
- 别双击 `vpngate-client.exe` 指望出窗口：它是命令行版，没有界面，
  双击只会弹一个黑窗口告诉你去用 `vpngate-gui.exe`，再按回车退出。
  命令行版要在 PowerShell / 命令提示符里运行，见下面的用法。

## 方式二：在自己的 Windows 10 机器上自己编

1. 装 Rust：去 https://rustup.rs 下 `rustup-init.exe`，一路默认装完，
   装完**重新开一个** PowerShell 或命令提示符，让 PATH 生效。
   默认装的是 MSVC 工具链；如果安装器提示缺 C++ 生成工具，按提示装
   Visual Studio 生成工具（"Desktop development with C++"，只要链接器部分）。
   验证：`cargo --version` 和 `rustc --version` 能打印版本号。
   （机器上没有 Visual Studio 时也可以改用 GNU 工具链：
   `rustup toolchain install stable-x86_64-pc-windows-gnu`，再装 MinGW，
   但一般直接用默认的 MSVC 工具链最省事。）
2. 拿到项目文件夹 `vpngate-rust-client`（整个文件夹拷过去）。
3. 进文件夹编译：
   ```powershell
   cd vpngate-rust-client
   cargo build --release
   ```
   第一次编译要联网下载依赖（tokio、rustls、eframe 等），需要几分钟；
   编好后可以离线用。
4. 运行：
   ```powershell
   .\target\release\vpngate-gui.exe
   ```
   命令行版是 `.\target\release\vpngate-client.exe`。

## 在 Windows 上要验证的事

按这个顺序试，任何一步的报错都原样发回来，**并把 `.exe` 旁边的 `vpngate-gui.log`
一起发来**（图形界面每次启动都把全部日志追加写进这个文件，带时间戳和 `[INFO]` /
`[WARN]` / `[ERROR]` 标签，超过 2 MB 会轮转成 `vpngate-gui.log.old`；界面日志只留
最后 100 行，关程序就没了，所以只截图界面日志常常不够定位。文件里绝不记录密码，
启动时先写一段会话头：启动时间、版本号、程序路径、配置文件路径、前置代理是否启用及地址）：

1. 双击能打开窗口，中文正常显示（程序会优先用 `C:\Windows\Fonts\msyh.ttc`
   微软雅黑，依次后备 `msyhbd.ttc`、`simsun.ttc`、`simhei.ttf`；这几个都是
   Windows 自带的字体，不用另外装）。
2. 节点列表能拉到（启动后自动拉：官方 API → 网页抓取 → 缓存 → 内嵌的 28 个节点，
   列表上方会写当前是哪一种来源；直连打不开 vpngate.net 时，先开前置 SOCKS5 代理再刷新）。
   如果两档都失败，先点高级设置里的“测试前置代理”：它走代理测 Google 和 vpngate.net，
   会分清是“前置代理连不上”还是“代理通、目标站不通”，日志里带原始错误。每站最多 10 秒
   （先连一下代理端口，最多 5 秒，含在 10 秒预算内），测的过程中状态栏会显示正在测哪一个站；
   测试用的是点击那一刻填的代理地址，测的过程中改地址不影响本次结果。注意代理本身
   不设账号密码也能用（用户名密码留空就行）；命令行版不走系统代理，别用 cmd 去测，
   就在图形界面里点这个按钮。
3. 选一条节点，先点“探测”——先试日本 NTT `kanratown.opengw.net:992`，
   再点“连接”，最后点“测试隧道”，generate_204 回 204 才算通。
4. 浏览器把代理设为 SOCKS5 `127.0.0.1:10818`（勾选远程 DNS），打开 Google
   试试；或用命令行版对照：
   ```powershell
   .\target\release\vpngate-client.exe test kanratown.opengw.net:992
   ```
5. 如果勾了“设置系统代理”，退出时会自动还原（写注册表
   `HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings`
   的 `ProxyEnable` / `ProxyServer`，退出改回 0）。如果程序被强行杀掉，
   系统代理可能留在开启状态，去 Windows 设置里手动关掉。

## 配置文件（便携版）

程序是便携的：所有文件都放在 `.exe` 旁边，不写 `%APPDATA%`、不写注册表
（唯一的例外是“设置系统代理”那个开关本身——它改的是系统代理，关程序时自动还原）。

- `config.toml`：配置。启动时按这个顺序找：命令行 `--config` 指定的 → `.exe` 旁边的
  `config.toml` → 工作目录下的 `config.toml`；都没有就用默认值。复制
  `config.example.toml` 改名就能用。
- 图形界面里改设置（前置代理、本地端口、局域网端口、允许局域网分享、系统代理开关）**改完就自动保存**
  到 `.exe` 旁边的 `config.toml`，下次打开还在，不用点保存；高级设置里还有个“保存设置”
  按钮，点一下是手动再存一次。下面会显示当前配置文件的路径。
- `nodes_cache.json`：上次成功拉到的节点列表（官方 API 或网页抓取）。官方 API 拉不到时
  （经常是网络被挡），先试网页抓取（`vpngate.net/cn` 节点表），再显示这份缓存
  （列表上会写“缓存列表，约 X 小时前保存”），没有缓存才显示内嵌的 28 个节点。
  注意：API 和网页抓取都走同一个连接——直连打不开 vpngate.net 时，要先开前置 SOCKS5
  代理（例如连上 Cloady 节点的 NekoBox）再点刷新。
- `vpngate-gui.log`：图形界面的文件日志。每次启动自动追加全部日志（信息、警告、错误），
  每行带本地时间戳和级别标签；启动时先写会话头（启动时间、版本号、程序路径、配置文件
  路径、前置代理是否启用及地址，绝不记录密码）；超过 2 MB 轮转成 `vpngate-gui.log.old`。
  打不开文件时界面日志里会警告一次，程序照常运行。报问题时请把这个文件一起发来。
  启动时界面日志里会显示它的完整路径，高级设置里保存设置按钮下面也显示。

整个文件夹拷到别的电脑、别的目录都能直接用，配置和缓存跟着文件夹走。
