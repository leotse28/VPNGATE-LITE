# TEST-REPORT — vpngate-client

日期：2026-11-07（UTC 约 04:00–04:30）。测试机：本 VM（Linux，Rust 1.99.1）。
**结论先说：代码编译通过、单元测试全过、对本地模拟 SSTP 服务器的端到端测试全过；
但对真实 VPNGate 节点的测试在本 VM 上做不了——本机出口拦截了到 `*.opengw.net` 和
`vpngate.net` 的 TLS（下面有原样报错）。没有宣称任何一次真实节点测试通过。**

## 1. 编译与单元测试

- `cargo build --release`：通过（Rust 1.99.1，tokio 1.54.1 等 crates 均可从 crates.io 正常下载）。
  中途修过 2 个编译错误（`ReadBuf::put_u8` 不存在，改 `put_slice`）和 4 个 warning（已清掉）。
- `cargo test --release`：**4 passed，0 failed**。
  包括：IPv4/TCP 校验和已知向量（与 Python 独立算出的 `0xB97C` 一致）、UUID 格式、
  VPNGate API CSV 解析、内嵌 28 节点表完整性。
  （测试本身也修过 3 处我手写的期望值错误：校验和期望值、UUID 分组长度、CSV 表头跳过。）

## 2. 对本地模拟 SSTP 服务器的端到端测试（通过）

因为真实节点在 VM 上连不上，我写了一个最小模拟 SoftEther 服务器
（`/tmp/mock_sstp.py`，标准库 Python + openssl 自签证书，**不在交付物里**，
只做测试用）：TLS → `SSTP_DUPLEX_POST` 回 200 → LCP/PAP/IPCP 协商分配虚拟 IP
`10.0.0.2` → 收到 TCP SYN 回 SYN+ACK，收到数据回 canned `HTTP/1.1 204`。

| 测试 | 命令 | 结果 |
|---|---|---|
| 探活 | `probe 127.0.0.1:8443` | **ALIVE，virtual-ip=10.0.0.2，128 ms** |
| 端到端 | `test 127.0.0.1:8443` | **generate_204：PASS**（`HTTP/1.1 204 No Content`） |
| SOCKS5 | `connect 127.0.0.1:8443` + `curl -x socks5h://127.0.0.1:10808 http://example.com/` | **回 204**（经 SOCKS5 → 隧道 → 模拟服务器，全链路通） |
| SOCKS5 | 同上，`curl ... http://www.gstatic.com/generate_204` | **回 204** |
| --allow-lan | `connect 127.0.0.1:8443 --allow-lan` | 监听在 **0.0.0.0:10808** |
| --system-proxy | `connect 127.0.0.1:8443 --system-proxy`（本 VM 无 GNOME schema） | 按设计降级：打印 `export http_proxy=socks5://127.0.0.1:10808` 等三行，不伪造成功；Ctrl-C 后打印还原提示 |
| 退出 | Ctrl-C（SIGINT） | 打印 `Ctrl-C, shutting down...`，干净退出 |

测试中还抓到一个模拟服务器自己的 bug（回包源 IP 写成了客户端虚拟 IP，
客户端按规则丢弃——这反而证明了客户端的源 IP 校验在工作），修掉后才通过。
`test` 里的 Google HTTPS 一步在模拟环境下是**预期失败**：
模拟服务器对 TLS ClientHello 也回 canned 204，rustls 报
`received corrupt message of type InvalidContentType`——这是模拟器的限制，
不是客户端的 bug；真实节点上这一步应该走通（见下面 Leo 本机复测）。

## 3. 对真实 VPNGate 节点的测试（在本 VM 上失败——出口拦截，如实记录）

本 VM 的出口会对到境外特定域名的直连 TLS 做拦截/篡改（之前 Python 探测脚本
报的也是 `TLS wrong version number`；curl 到 `vpngate.net` 是 HTTP 000）。
原样报错：

```
$ ./target/release/vpngate-client list --limit 5
# WARNING: live API fetch failed (TLS handshake to www.vpngate.net failed:
#   received corrupt message of type InvalidContentType); showing bundled curated list only.
# （随后正常显示内嵌 28 节点表，list 的兜底逻辑工作正常）

$ ./target/release/vpngate-client probe kanratown.opengw.net:992
probe kanratown.opengw.net:992 ... dead: TLS handshake to kanratown.opengw.net:992
  failed: received corrupt message of type InvalidContentType
all candidates failed

$ ./target/release/vpngate-client probe 2
# 依次试了 kanratown.opengw.net:992 及其两个备用节点（vpn699884073.opengw.net:1556、
# vpn615046637.opengw.net:1341），三个都是同一个 TLS 报错——说明失败在 VM 出口，
# 不是节点本身的问题。
```

同样原因，DoH 到 `1.1.1.1` / `cloudflare-dns.com` 的直连 TLS 在 VM 上也报
`InvalidContentType`（但 `curl` 走 VM 的 HTTP 代理时 DoH 是通的，说明不是
DoH 服务的问题）。程序里的系统 DNS 兜底（`lookup_host`）在 VM 上是工作的，
所以 SOCKS5 测试里 `example.com` 仍能解析。

## 4. Leo 在自己机器上复测（请按这个顺序跑）

```bash
cd vpngate-rust-client
cargo build --release
./target/release/vpngate-client list --country JP --limit 10
./target/release/vpngate-client probe kanratown.opengw.net:992
./target/release/vpngate-client test kanratown.opengw.net:992
# test 通过的标准：generate_204 回 204，且 google https 那一步拿到 HTTP 响应头。
# 如果 test 不通，先试 probe 再换节点（德国 vpn579127873.opengw.net:443、
# 日本 vpn267947139.opengw.net:1663 都是 11 月 7 日 11:40 探活过的）。
./target/release/vpngate-client connect kanratown.opengw.net:992
# 另开终端：
curl -x socks5h://127.0.0.1:10808 http://www.gstatic.com/generate_204 -i
curl -x socks5h://127.0.0.1:10808 https://www.google.com -I
```

如果 `test` 在 Leo 机器上也出现 `TCP handshake timed out`，请把完整的终端输出
发回来——最可能的原因是节点过载（换节点）或用户态 TCP 协议栈需要补重传，
这是下一步要修的点，不是换个节点表就能解决的。

## 5. 已知未测到的部分

- 真实 VPNGate 节点的 IPCP/LCP 交互（模拟器只覆盖了“教科书式”的回复顺序；
  真实 SoftEther 服务器可能多发 LCP Request / Nak，客户端代码里按 JS 参考
  实现了这些分支，但没在真机上验证过）。
- Windows/macOS 的系统代理分支（本 VM 是 Linux，只验证了 gsettings 降级路径；
  Windows 注册表、macOS networksetup 的命令是按文档写的，没实测过）。
- 长时间运行的稳定性（隧道断线后没有自动重连——需求里没要求，就没做）。
- 大流量/多并发下的表现（模拟器只测了单连接小请求）。
