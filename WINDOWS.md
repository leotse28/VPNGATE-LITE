# Windows 10 x64 说明

目标平台：**Windows 10 x64**。命令行版和图形界面版都是按这个平台做的，
Linux 版只用来在本机验证编译和逻辑，不作为交付物。

## 方式一：直接用编好的程序（推荐）

```
target\x86_64-pc-windows-gnu\release\vpngate-gui.exe      （约 37 MB，图形界面）
target\x86_64-pc-windows-gnu\release\vpngate-client.exe   （约 10 MB，命令行版）
```

双击 `vpngate-gui.exe` 就能打开，不用装 Rust。注意两点：

- 第一次打开时 Windows Defender SmartScreen 可能会拦（程序没有代码签名），
  点“更多信息”→“仍要运行”即可。
- 默认只绑 `127.0.0.1:10808`，不会弹防火墙；只有勾了“允许局域网连接”
  （绑 `0.0.0.0`）时才可能弹防火墙提示。

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

按这个顺序试，任何一步的报错都原样发回来：

1. 双击能打开窗口，中文正常显示（程序会优先用 `C:\Windows\Fonts\msyh.ttc`
   微软雅黑，依次后备 `msyhbd.ttc`、`simsun.ttc`、`simhei.ttf`；这几个都是
   Windows 自带的字体，不用另外装）。
2. 节点列表能拉到（启动后自动从官方 API 拉，拉不到就显示内嵌的 28 个节点）。
3. 选一条节点，先点“探测”——先试日本 NTT `kanratown.opengw.net:992`，
   再点“连接”，最后点“测试隧道”，generate_204 回 204 才算通。
4. 浏览器把代理设为 SOCKS5 `127.0.0.1:10808`（勾选远程 DNS），打开 Google
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
- 高级设置里有 **“保存设置”** 按钮：点一下，当前窗口里的设置（前置代理、本地端口、
  允许局域网、系统代理开关）就写到 `.exe` 旁边的 `config.toml`，下次打开还在。
  不点保存，改的设置只在本次运行有效。高级设置下面会显示当前配置文件的路径。
- `nodes_cache.json`：上次成功从官方 API 拉到的节点列表。官方 API 拉不到时
  （经常是网络被挡），先显示这份缓存（列表上会写“缓存列表，约 X 小时前保存”），
  没有缓存才显示内嵌的 28 个节点。

整个文件夹拷到别的电脑、别的目录都能直接用，配置和缓存跟着文件夹走。
