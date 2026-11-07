# vpngate-rust-client — 工作笔记（防 VM 重启丢失）

## 状态
- 2026-11-07 ~12:05 UTC：项目骨架建立。REA skill 已读：SoftEther 开源，按路由规则不用反编译，只做源码/协议移植；REA MCP 不可用。
- 协议参考：`~/workspace/custom-skills/cf-sstp-home-broadband-skill/worker/worker_stealth_sub.js`（openTun/establish、makeTcp、tunConn、probeGw）与 `scripts/probe_vpngate_sstp.py`。
- `nodes.json`：已从 Worker CFG 解出 28 个节点（host/port/country/isp/backups），含 44 条 backups。
- crates.io 在线可用（tokio 1.54.1 已验证可下载编译）；VM 出口到 vpngate.net 被拦截（curl HTTP 000），活节点实测预计在 VM 上失败，需在 Leo 本机复测。
- SSTP 端口：官方 API 不含 SSTP 端口；已确认的可靠来源只有内嵌的 28 节点表 + 手动输入 + 候选端口探测 [443, 992, 995]。

## 架构
- `src/main.rs` CLI（clap）：list / probe / connect / test
- `src/config.rs` TOML 配置
- `src/vpngate.rs` API 拉取 + 内嵌节点合并
- `src/net.rs` 连接器（直连 / 前置 SOCKS5）+ 简易 HTTPS GET（rustls，校验 / 不校验两种）
- `src/doh.rs` DoH 解析（1.1.1.1 / cloudflare-dns.com）+ 系统 DNS 兜底
- `src/sstp.rs` SSTP+PPP 建隧道 + 用户态 TCP（TunnelStream 实现 AsyncRead/AsyncWrite）
- `src/socks5.rs` 前置 SOCKS5 客户端 + 本地 SOCKS5 服务端
- `src/sysproxy.rs` 系统代理设置/还原（Windows reg / macOS networksetup / Linux gsettings 或打印 export）

## 测试结果（2026-11-07 ~04:30 UTC）
- cargo build --release 通过；cargo test 4/4 通过。
- 模拟 SSTP 服务器（/tmp/mock_sstp.py，不在交付物内）：probe ALIVE vip 10.0.0.2 (128ms)；
  test generate_204 PASS；SOCKS5 + curl 回 204；--allow-lan 绑 0.0.0.0:10808；
  --system-proxy 在无 schema 的 VM 上按设计降级为 export 提示。
- 真实 VPNGate 节点在 VM 上测不了：出口拦截 TLS（InvalidContentType），list/probe
  原样报错已记入 TEST-REPORT.md。Leo 需在本机跑 test/connect 复测。
- 期间修过：ReadBuf put_u8 编译错；3 处单元测试期望值；模拟器回包源 IP bug（客户端校验正确）；
  sysproxy gsettings 有程序无 schema 时降级为 export 提示。
