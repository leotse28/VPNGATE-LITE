//! vpngate-gui — desktop GUI for the VPNGate Rust client (egui/eframe 0.36).
//!
//! Same tunnel code as the CLI (`vpngate_client` lib); this binary only
//! adds a window around it. Long work (fetch / probe / connect / test)
//! runs on tokio tasks; the UI polls them through a channel and never
//! blocks.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;
use std::collections::{BTreeSet, HashMap};
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use vpngate_client::config::Config;
use vpngate_client::filelog::{self, FileLogger, LogLevel};
use vpngate_client::net::{self, Connector};
use vpngate_client::sstp::SstpTunnel;
use vpngate_client::sysproxy::SystemProxyGuard;
use vpngate_client::vpngate::{self, Node};

// ---------------------------------------------------------------------------
// Background work
// ---------------------------------------------------------------------------

/// What the UI is waiting for right now (at most one job at a time).
#[derive(Clone, Copy, PartialEq, Eq)]
#[derive(Debug)]
enum Busy {
    Fetching,
    Probing,
    ProbingAll,
    Connecting,
    Testing,
    TestingFrontProxy,
}

/// Which node-list column the list is sorted by. Only the country,
/// sessions (当前人数）, line-quality （线路质量） and latency/status
/// （延迟/状态） columns are sortable; the status (∞), ISP and
/// host:port columns are not. One column at a time: clicking a
/// header sorts by that column and clears the previous column's
/// arrow; clicking the same column again reverses the direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortCol {
    Country,
    Sessions,
    Quality,
    Latency,
}

impl SortCol {
    /// First-click direction for the column — the most useful order:
    /// countries A-Z, fewest sessions first (lightest load), fastest
    /// line first (speed primary, ping secondary), and for latency
    /// the available nodes by ms ascending, unprobed in the middle,
    /// failed last (see `latency_key`).
    fn default_dir(self) -> SortDir {
        match self {
            SortCol::Country => SortDir::Asc,
            SortCol::Sessions => SortDir::Asc,
            SortCol::Quality => SortDir::Desc,
            SortCol::Latency => SortDir::Asc,
        }
    }
}

/// Country-column sort direction, toggled by clicking a sortable
/// header. Chinese country names sort by string (Unicode code-point)
/// order — deterministic, and same-country rows stay grouped — with
/// the hostname as the tie-breaker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortDir {
    Asc,
    Desc,
}

impl SortDir {
    /// Arrow shown in the sorted column's header for the current
    /// direction: ▲ = the column's first-click order, ▼ = reversed.
    fn arrow(self) -> &'static str {
        match self {
            SortDir::Asc => "▲",
            SortDir::Desc => "▼",
        }
    }
}

/// Sort key for the 线路质量 column: (has data, speed, ping
/// reversed). No-data nodes (sessions, speed and ping all zero —
/// the curated 28) count as smallest, so they sort first in
/// ascending order and last in descending order, never mixed in
/// with nodes that have data. Speed is primary, ping secondary:
/// in the ascending key a larger ping sorts later (Reverse), so
/// reversing the whole key (descending, the first click) is
/// fastest-first with smaller ping first on ties, and ascending is
/// slowest-first with larger ping first.
fn quality_key(n: &vpngate::Node) -> (bool, u64, std::cmp::Reverse<u64>) {
    let has_data = n.sessions != 0 || n.speed_mbps != 0 || n.ping_ms != 0;
    (
        has_data,
        n.speed_mbps,
        std::cmp::Reverse(n.ping_ms),
    )
}

/// Does this node carry listed stats (sessions / speed / ping)?
/// The curated 28 have all three zero — no data, not "zero
/// sessions" or "zero speed".
fn has_listed_stats(n: &vpngate::Node) -> bool {
    n.sessions != 0 || n.speed_mbps != 0 || n.ping_ms != 0
}

/// 当前人数 cell: "47人" ("0人" for a node with data but zero
/// sessions); "—" when the node carries no listed stats at all.
fn sessions_cell(n: &vpngate::Node) -> String {
    if has_listed_stats(n) {
        format!("{}人", n.sessions)
    } else {
        "—".to_string()
    }
}

/// 线路质量 cell: "572M@3ms" (speed Mbps @ ping ms); "—" with no data.
fn quality_cell(n: &vpngate::Node) -> String {
    if has_listed_stats(n) {
        format!("{}M@{}ms", n.speed_mbps, n.ping_ms)
    } else {
        "—".to_string()
    }
}

impl Busy {
    fn label(self) -> &'static str {
        match self {
            Busy::Fetching => "正在拉取节点列表…",
            Busy::Probing => "正在探测节点…",
            Busy::ProbingAll => "正在探测全部节点…",
            Busy::Connecting => "正在连接…",
            Busy::Testing => "正在测试隧道…",
            Busy::TestingFrontProxy => "正在测试前置代理…",
        }
    }
}

/// Why a probe failed: the front proxy itself is down, or the proxy
/// is up but the node did not answer. The distinction matters because
/// refresh / probe / connect all share the front proxy — when the
/// proxy is down they fail together for one reason, and swapping
/// nodes will not help.
#[derive(Debug, Clone)]
enum ProbeFailure {
    ProxyDown(String),
    NodeDown(String),
}

/// A live connection: tunnel + the mixed proxy server task(s) + the optional
/// system-proxy guard. Dropping the tunnel Arc closes the TLS stream
/// (the lib's writer task sends close_notify when its channel closes).
/// With LAN sharing on, the one server binds the wildcard address
/// (0.0.0.0:local_port) instead of a second port; the task is
/// aborted on disconnect.
struct ActiveConnection {
    tunnel: Arc<SstpTunnel>,
    serve: Vec<tokio::task::JoinHandle<()>>,
    proxy_guard: Option<SystemProxyGuard>,
    /// Display label, e.g. "127.0.0.1:10818" or
    /// "127.0.0.1:10818 ＋ 0.0.0.0:10818".
    listen_addr: String,
    node_label: String,
    vip: Ipv4Addr,
}

enum GuiEvent {
    Nodes(vpngate::NodeListUpdate),
    Probe {
        host: String,
        result: Result<(Ipv4Addr, Duration), ProbeFailure>,
    },
    /// Probe-all sweep: the task is about to probe `current` (the
    /// (done+1)-th of `total`). The row shows "探测中…" and the
    /// status line names it; the outcome still arrives as
    /// `ProbeAllResult` below.
    ProbeAllProgress {
        current: String,
        done: usize,
        total: usize,
    },
    /// Probe-all sweep: one node's outcome. `done` counts this node.
    ProbeAllResult {
        host: String,
        result: Result<(Ipv4Addr, Duration), ProbeFailure>,
        done: usize,
        total: usize,
    },
    /// Probe-all sweep finished: `stopped` means the user pressed
    /// 停止 (or the front proxy went down, see `proxy_down`)
    /// before the queue ran out; `ok`/`failed` count the nodes
    /// actually probed.
    ProbeAllDone {
        total: usize,
        ok: usize,
        failed: usize,
        stopped: bool,
        proxy_down: bool,
    },
    Connected(Box<ActiveConnection>),
    ConnectFailed(String),
    /// One line from the mixed proxy about an accepted connection
    /// (source address, protocol, target host — see `ConnLog` in
    /// `socks5.rs`). The UI writes it to the log panel + file log,
    /// so a LAN device that never reaches the proxy leaves no line.
    ConnLog(String),
    TestResult {
        gen204: bool,
        google: bool,
        detail: String,
    },
    FrontProxyTest {
        /// Front-proxy address snapshotted when the test was clicked
        /// (the live field may have changed by the time the result
        /// arrives — Leo once typed 12.0.0.1, fixed it mid-test to
        /// 127.0.0.1, and the start line and the result line named
        /// different addresses).
        addr: String,
        google: net::FrontProxyCheck,
        vpngate: net::FrontProxyCheck,
    },
    /// Progress of a running front-proxy test: which site is being
    /// tested right now ("Google" / "vpngate.net"). Updates the
    /// status line only; the full result still comes as
    /// `FrontProxyTest`.
    FrontProxyTestProgress { site: String },
}

/// Build a Connector from the current settings.
fn connector_for(front_enabled: bool, front_addr: &str, user: &str, pass: &str) -> Connector {
    if front_enabled {
        let mut fp = vpngate_client::config::FrontProxy::default();
        fp.enabled = true;
        fp.addr = front_addr.to_string();
        fp.username = if user.is_empty() {
            None
        } else {
            Some(user.to_string())
        };
        fp.password = if pass.is_empty() {
            None
        } else {
            Some(pass.to_string())
        };
        Connector::new(Some(fp))
    } else {
        Connector::new(None)
    }
}

async fn fetch_flow(connector: &Connector) -> vpngate::NodeListUpdate {
    vpngate::fetch_nodes_with_fallback(connector).await
}

async fn probe_flow(
    connector: &Connector,
    target: &str,
    timeout: Duration,
) -> Result<(Ipv4Addr, Duration), ProbeFailure> {
    let candidates = vpngate::resolve_target(target);
    // One race over all candidate ports (first success wins); a dead
    // node costs about one timeout, not one per port in series.
    let last_err = match SstpTunnel::probe_any(connector, &candidates, timeout).await {
        Ok(v) => return Ok(v),
        Err(e) => e,
    };
    // All candidates failed. If a front proxy is configured, find out
    // whether the proxy itself is the problem: a direct TCP connect to
    // the proxy separates "proxy down" from "node down".
    let proxy_tcp = net::check_front_proxy_tcp(connector, Duration::from_secs(3)).await;
    Err(classify_probe_failure(last_err, proxy_tcp))
}

/// Pure half of `probe_flow`'s failure handling, so the split is
/// unit-testable: only a failed direct TCP connect to a configured
/// front proxy blames the proxy; anything else blames the node.
fn classify_probe_failure(last_err: String, proxy_tcp: Option<bool>) -> ProbeFailure {
    match proxy_tcp {
        Some(false) => ProbeFailure::ProxyDown(last_err),
        _ => ProbeFailure::NodeDown(last_err),
    }
}

/// Probe every node in `targets` in order, one at a time (never in
/// parallel — a dead node costs about one timeout, and 95 parallel
/// handshakes would flood the front proxy). `targets` is the
/// (raw host, dialled target) snapshot `probe_all_targets` took at
/// click time. Sends a progress event before each probe and a
/// result event after it, then a done event with the counts. Stops
/// early when `stop` is set — checked before each probe and after
/// each result, so a 停止 click lands after the node in flight —
/// or when a probe fails because the front proxy is down (the rest
/// would fail identically, one timeout each).
async fn probe_all_flow(
    connector: &Connector,
    targets: Vec<(String, String)>,
    timeout: Duration,
    stop: Arc<AtomicBool>,
    tx: mpsc::Sender<GuiEvent>,
    ctx: egui::Context,
) {
    let total = targets.len();
    let mut ok = 0usize;
    let mut failed = 0usize;
    let mut stopped = false;
    let mut proxy_down = false;
    for (i, (host, target)) in targets.into_iter().enumerate() {
        if stop.load(Ordering::Relaxed) {
            stopped = true;
            break;
        }
        let _ = tx.send(GuiEvent::ProbeAllProgress {
            current: host.clone(),
            done: i,
            total,
        });
        ctx.request_repaint();
        let res = probe_flow(connector, &target, timeout).await;
        match &res {
            Ok(_) => ok += 1,
            Err(ProbeFailure::ProxyDown(_)) => {
                failed += 1;
                proxy_down = true;
            }
            Err(ProbeFailure::NodeDown(_)) => failed += 1,
        }
        let _ = tx.send(GuiEvent::ProbeAllResult {
            host,
            result: res,
            done: i + 1,
            total,
        });
        ctx.request_repaint();
        if proxy_down {
            break;
        }
        if stop.load(Ordering::Relaxed) {
            stopped = true;
            break;
        }
    }
    let _ = tx.send(GuiEvent::ProbeAllDone {
        total,
        ok,
        failed,
        stopped,
        proxy_down,
    });
    ctx.request_repaint();
}

/// Test the front proxy itself: HTTPS GET to Google and to vpngate.net
/// through it (SOCKS5 connectivity, not ICMP ping). Runs on a
/// background task; the result distinguishes "the proxy is down" from
/// "the proxy is up but the target does not answer through it".
///
/// `timeout` is the TOTAL budget per site (see
/// `net::test_url_through_proxy`): pre-check + HTTPS together, so two
/// sites take at most 2 × `timeout`. A progress event is sent before
/// each site so the status line shows which one is being tested
/// instead of going silent for the whole run.
async fn front_proxy_test_flow(
    connector: &Connector,
    timeout: Duration,
    tx: &std::sync::mpsc::Sender<GuiEvent>,
) -> (net::FrontProxyCheck, net::FrontProxyCheck) {
    let _ = tx.send(GuiEvent::FrontProxyTestProgress {
        site: "Google".to_string(),
    });
    let google = net::test_url_through_proxy(connector, "https://www.google.com", timeout).await;
    let _ = tx.send(GuiEvent::FrontProxyTestProgress {
        site: "vpngate.net".to_string(),
    });
    let vpngate = net::test_url_through_proxy(connector, "https://www.vpngate.net", timeout).await;
    (google, vpngate)
}

/// Dedicated per-site budget for the front-proxy test (10 s total per
/// site: TCP pre-check at most 5 s, HTTPS gets the rest). This is NOT
/// the tunnel timeout: the tunnel timeout is for SSTP/PPP establish,
/// and a proxy test must not inherit a long one (or a short one) —
/// Leo's v3.1 test once sat silent for 40 s because each leg had its
/// own 10 s timeout, twice over, for two sites.
const FRONT_PROXY_TEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Connect the tunnel, start the local mixed proxy server(s),
/// optionally set the system proxy. Runs on a background task;
/// returns the live connection. `listen_addrs` is the address(es)
/// to actually bind (one mixed server per address); `listen_display`
/// is what the user sees (loopback always, plus the LAN address
/// when sharing is on — a single wildcard bind under the hood).
/// Both servers share the one tunnel, and the system proxy (if on)
/// points at the loopback address, never 0.0.0.0.
async fn connect_flow(
    connector: Connector,
    target: String,
    listen_addrs: Vec<String>,
    listen_display: String,
    timeout: Duration,
    system_proxy: bool,
    service: Option<String>,
    conn_log: Option<vpngate_client::socks5::ConnLog>,
) -> Result<ActiveConnection, String> {
    let candidates = vpngate::resolve_target(&target);
    // One race over all candidate ports (first success wins); a dead
    // node costs about one timeout, not one per port in series.
    let tunnel = SstpTunnel::connect_any(&connector, &candidates, timeout)
        .await
        .map_err(|e| format!("{} 个候选均失败；最后一次：{}", candidates.len(), e))?;
    let vip = tunnel.vip;
    let mut listeners = Vec::new();
    for addr in &listen_addrs {
        let listener = tokio::net::TcpListener::bind(addr).await.map_err(|e| {
            format!(
                "本地混合代理无法监听 {}：{}（端口可能被占用，换一个端口再试）",
                addr, e
            )
        })?;
        listeners.push(listener);
    }
    let mut serve = Vec::new();
    for listener in listeners {
        serve.push(tokio::spawn(vpngate_client::socks5::serve_mixed(
            listener,
            tunnel.clone(),
            timeout,
            conn_log.clone(),
        )));
    }
    let listen_addr = listen_display;
    let mut guard = None;
    if system_proxy {
        // System proxy always points at the loopback address (a
        // 0.0.0.0 bind is not a usable proxy address).
        let sysproxy_addr = vpngate_client::config::loopback_of(&listen_addrs[0]);
        match SystemProxyGuard::set(&sysproxy_addr, service.as_deref()) {
            Ok(g) => guard = Some(g),
            Err(e) => {
                for s in &serve {
                    s.abort();
                }
                return Err(format!("系统代理设置失败：{}（隧道已断开）", e));
            }
        }
    }
    Ok(ActiveConnection {
        tunnel,
        serve,
        proxy_guard: guard,
        listen_addr,
        node_label: target,
        vip,
    })
}

/// End-to-end check through an established tunnel: generate_204 over
/// plain HTTP, then HTTPS to Google (TLS inside the tunnel).
async fn test_flow(tunnel: &Arc<SstpTunnel>, timeout: Duration) -> (bool, bool, String) {
    let mut gen204 = false;
    let mut google = false;
    let mut detail = String::new();
    match tunnel.open_stream("www.gstatic.com", 80, timeout).await {
        Ok(mut s) => {
            let req = b"GET http://www.gstatic.com/generate_204 HTTP/1.1\r\nHost: www.gstatic.com\r\nConnection: close\r\n\r\n";
            if s.write_all(req).await.is_ok() {
                let mut buf = vec![0u8; 4096];
                match tokio::time::timeout(timeout, s.read(&mut buf)).await {
                    Ok(Ok(n)) => {
                        let head = String::from_utf8_lossy(&buf[..n]).to_string();
                        let first = head.lines().next().unwrap_or("").to_string();
                        detail.push_str(&format!("generate_204：{}；", first));
                        gen204 = head.contains("204");
                    }
                    Ok(Err(e)) => detail.push_str(&format!("generate_204 读取失败：{}；", e)),
                    Err(_) => detail.push_str("generate_204 读取超时；"),
                }
            } else {
                detail.push_str("generate_204 发送失败；");
            }
        }
        Err(e) => detail.push_str(&format!("generate_204 建流失败：{}；", e)),
    }
    match tunnel.open_stream("www.google.com", 443, timeout).await {
        Ok(stream) => {
            match tokio::time::timeout(
                timeout,
                net::verified_tls_connect(stream, "www.google.com"),
            )
            .await {
                Ok(Ok(mut tls)) => {
                    let req = format!(
                        "GET / HTTP/1.1\r\nHost: www.google.com\r\nConnection: close\r\nUser-Agent: vpngate-client/{}\r\n\r\n",
                        env!("CARGO_PKG_VERSION")
                    );
                    if tls.write_all(req.as_bytes()).await.is_ok() {
                        let mut buf = vec![0u8; 4096];
                        match tokio::time::timeout(timeout, tls.read(&mut buf)).await {
                            Ok(Ok(n)) => {
                                let head = String::from_utf8_lossy(&buf[..n]).to_string();
                                let first = head.lines().next().unwrap_or("").to_string();
                                detail.push_str(&format!("Google：{}。", first));
                                google = n > 0;
                            }
                            Ok(Err(e)) => detail.push_str(&format!("Google 读取失败：{}。", e)),
                            Err(_) => detail.push_str("Google 读取超时。"),
                        }
                    } else {
                        detail.push_str("Google 发送失败。");
                    }
                }
                Ok(Err(e)) => detail.push_str(&format!("Google TLS 握手失败：{}。", e)),
                Err(_) => detail.push_str("Google TLS 握手超时。"),
            }
        }
        Err(e) => detail.push_str(&format!("Google 建流失败：{}。", e)),
    }
    (gen204, google, detail)
}

/// Place a truncating label at the left of an exact cell rect.
/// (`Ui::put` centres the widget, which would centre every column's
/// text; a node list reads better left-aligned, like the old Grid.)
fn put_cell_label(ui: &mut egui::Ui, rect: egui::Rect, text: impl Into<egui::WidgetText>) {
    ui.scope_builder(
        egui::UiBuilder::new()
            .max_rect(rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
        |ui| {
            ui.add(egui::Label::new(text).truncate());
        },
    );
}

/// One line for one front-proxy test target: reachable + ms, or which
/// side failed (proxy down vs target unreachable through a working proxy).
fn front_proxy_check_line(label: &str, check: &net::FrontProxyCheck) -> String {
    match check {
        net::FrontProxyCheck::Reachable { status, ms } => {
            format!("{}：通（HTTP {}，{} ms）", label, status, ms)
        }
        net::FrontProxyCheck::ProxyUnreachable(detail) => {
            format!("{}：前置代理连不上（{}）", label, detail)
        }
        net::FrontProxyCheck::TargetUnreachable(detail) => {
            format!("{}：代理通、目标站不通（{}）", label, detail)
        }
    }
}

/// Parse a manual node entry: "host:port", a bare hostname, or an index
/// into the curated list. Returns the canonical target string for
/// `resolve_target`, or a validation error to show next to the field.
fn parse_manual_target(text: &str, curated_len: usize) -> Result<String, String> {
    let t = text.trim();
    if t.is_empty() {
        return Err("请输入节点，例如 kanratown.opengw.net:992".to_string());
    }
    if let Some((h, p)) = t.rsplit_once(':') {
        if !h.is_empty() {
            match p.parse::<u16>() {
                Ok(port) if port > 0 => return Ok(t.to_string()),
                _ => return Err("端口必须是 1–65535 的数字".to_string()),
            }
        }
    }
    if let Ok(idx) = t.parse::<usize>() {
        if idx < curated_len {
            return Ok(t.to_string());
        }
        return Err(format!("序号超出范围（内嵌列表共 {} 个）", curated_len));
    }
    // Bare hostname: resolve_target will try the candidate SSTP ports.
    if t.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        && t.contains('.')
    {
        return Ok(t.to_string());
    }
    Err("格式不对：主机名:端口，例如 kanratown.opengw.net:992".to_string())
}

// ---------------------------------------------------------------------------
// CJK font: egui's default fonts have no Chinese glyphs, so load one from
// the OS in the constructor. Tries system paths in order; returns the
// first readable file. Prefers .ttf (single-face) over .ttc collections.
// ---------------------------------------------------------------------------

fn find_cjk_font() -> Option<Vec<u8>> {
    let mut candidates: Vec<std::path::PathBuf> = vec![
        // Windows (target platform: Windows 10 x64 — tried first)
        "C:\\Windows\\Fonts\\msyh.ttc".into(),
        "C:\\Windows\\Fonts\\msyhbd.ttc".into(),
        "C:\\Windows\\Fonts\\simsun.ttc".into(),
        "C:\\Windows\\Fonts\\simhei.ttf".into(),
        // macOS
        "/System/Library/Fonts/PingFang.ttc".into(),
        // Linux (this VM and most distros)
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc".into(),
        "/usr/share/fonts/opentype/noto/NotoSansCJKsc-Regular.ttc".into(),
    ];
    // Walk /usr/share/fonts for any NotoSansCJK* file (depth-limited).
    if let Ok(top) = std::fs::read_dir("/usr/share/fonts") {
        let mut stack: Vec<(std::path::PathBuf, usize)> =
            top.filter_map(|e| e.ok()).map(|e| (e.path(), 1)).collect();
        while let Some((dir, depth)) = stack.pop() {
            if depth > 3 {
                continue;
            }
            if let Ok(rd) = std::fs::read_dir(&dir) {
                for e in rd.flatten() {
                    let path = e.path();
                    if path.is_dir() {
                        stack.push((path, depth + 1));
                    } else if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                        if name.contains("NotoSansCJK")
                            && name.contains("Regular")
                            && (name.ends_with(".ttf") || name.ends_with(".ttc"))
                        {
                            candidates.push(path);
                        }
                    }
                }
            }
        }
    }
    // Prefer .ttf over .ttc (single-face files parse everywhere).
    candidates.sort_by_key(|p| p.extension().map(|e| e != "ttf").unwrap_or(true));
    for path in candidates {
        if let Ok(bytes) = std::fs::read(&path) {
            if !bytes.is_empty() {
                return Some(bytes);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

struct GuiApp {
    config: Config,
    /// Where the config was loaded from (`config.toml` next to the exe
    /// in portable mode); `None` when running on defaults.
    config_path: Option<std::path::PathBuf>,
    nodes: Vec<Node>,
    nodes_source: String,
    /// Anchor row: the last clicked row (an index into `self.nodes`,
    /// not a display position). Enter / Ctrl+R / the 探测 and 连接
    /// buttons act on this row; `None` means nothing is current and
    /// those actions are unavailable, as before.
    selected: Option<usize>,
    /// Multi-select set: node indices selected by click / Ctrl+click
    /// / Shift+click (see `click_row`). This is what 删除选中
    /// deletes, and the row highlight follows this set — not the
    /// anchor alone. The anchor is not necessarily in the set
    /// (Ctrl+click can toggle it out).
    selected_set: BTreeSet<usize>,
    /// Hostnames deleted from the list, kept out of every list load
    /// until restored. Mirrors `Config::blocked_hosts` (canonical
    /// completed hostnames — see `blocked_key`); persisted via
    /// auto-save.
    blocked_hosts: BTreeSet<String>,
    /// Full data of the nodes deleted in this session, so 恢复已删除
    /// can put them back. Session-only: after a restart this is
    /// empty and restore only clears the blocklist (the nodes come
    /// back on the next refresh).
    deleted_nodes: Vec<Node>,
    probes: HashMap<String, String>,
    /// Last successful probe latency per host (ms), parallel to
    /// `probes`: the 延迟/状态 sort reads this number, never
    /// re-parses the "N ms" display string. Set when a probe
    /// succeeds, cleared when a probe starts (the row shows
    /// "探测中…" then) or fails, so it never goes stale.
    probe_ms: HashMap<String, u64>,
    busy: Option<Busy>,
    /// Probe-all sweep state: the shared stop flag the 停止 button
    /// sets (the background task checks it before/after each probe),
    /// and the running counts shown in the progress line. `busy ==
    /// ProbingAll` is the sweep-active flag; the counters are only
    /// meaningful while it is set.
    probe_all_stop: Arc<AtomicBool>,
    probe_all_total: usize,
    probe_all_done: usize,
    probe_all_ok: usize,
    probe_all_failed: usize,
    status: String,
    error: String,
    log: Vec<String>,
    conn: Option<ActiveConnection>,
    // Settings (editable in the advanced section; start from config).
    front_enabled: bool,
    front_addr: String,
    front_user: String,
    front_pass: String,
    local_port: u16,
    allow_lan: bool,
    system_proxy: bool,
    /// Node-list column widths in pixels, in display order: country,
    /// ISP, 当前人数， 线路质量， host:port, latency/status. Draggable
    /// at the header separators (every resizable column but the last
    /// has one); persisted to config.toml with the rest of the
    /// settings via auto-save. The status column (∞ for the connected
    /// node) is a fixed 28 px and is NOT in this array — it has no
    /// separator and is not resizable. Pre-v3.7 configs stored only
    /// 4 widths; those migrate on load (see `de_col_widths` in
    /// config.rs), so old config files keep loading as-is.
    col_widths: [u16; 6],
    /// List sort: `None` = fetch order (startup default), `Some`
    /// = (column, direction) the user picked by clicking a
    /// sortable header （国家/地区 / 当前人数 / 线路质量 / 延迟/状态）.
    /// Clicking a different column restarts at that column's
    /// first-click direction (see `SortCol::default_dir`). Kept
    /// across refreshes; session-only, not persisted to config.toml.
    sort: Option<(SortCol, SortDir)>,
    /// Where the mouse was pressed while selecting a row (`None` when
    /// no press is in flight). egui only counts a click when the
    /// pointer moves under `max_click_dist` (6 px); a real mouse
    /// drifts past that while clicking a row, and the ScrollArea
    /// turns the gesture into a scroll drag — so the row loop also
    /// accepts a press-and-release on the same row within 12 px.
    row_press_pos: Option<egui::Pos2>,
    /// The modifier keys held when the row press started, latched
    /// next to `row_press_pos`. The row click reads its modifiers
    /// on the *release* frame (see `effective_click_mods`), and a
    /// user who lets go of Shift/Ctrl a beat before the mouse
    /// button would lose the gesture there — the press-time latch
    /// is what makes Shift+click select a range reliably.
    row_press_mods: Option<egui::Modifiers>,
    /// One-shot request from the arrow keys: after ↑/↓ moves the
    /// selection, the row loop scrolls the selected row into view
    /// (see the row loop). Only set when the selection actually
    /// changes; cleared when the row loop consumes it. Mouse clicks
    /// never set it — a clicked row is already visible.
    scroll_to_selected: bool,
    /// The node list's current vertical scroll offset (pixels, from
    /// the ScrollArea output each frame). The arrow-key test reads
    /// it to prove the list actually scrolled.
    node_list_scroll_y: f32,
    /// Whether the selected row is fully inside the node list's
    /// visible area vertically this frame (recomputed in the row
    /// loop from the clip rect; `false` when nothing is selected).
    /// The arrow-key test reads it to prove the follow-scroll works.
    selected_row_in_view: bool,
    /// Whether 高级设置 starts open. Production default is closed;
    /// the render test opens it so the review screenshots show the
    /// front-proxy section and the log.
    advanced_open: bool,
    /// Render-test aid: start the outer scroll area at the bottom, so
    /// the review screenshots show the log instead of the node list.
    render_scroll_bottom: bool,
    manual_host: String,
    manual_error: String,
    show_about: bool,
    font_ok: bool,
    /// Last auto-save failure message (to show a repeated failure once).
    last_save_error: String,
    /// File logger (`vpngate-gui.log` next to the exe). `None` until
    /// `init_file_log` runs in `new()`; the logger itself no-ops when
    /// its file could not be opened. `for_tests` apps have none.
    file_logger: Option<FileLogger>,
    /// The log file's path (shown in the UI log at startup and next to
    /// the save button). `Some` even when the file could not be opened.
    log_path: Option<std::path::PathBuf>,
    event_tx: mpsc::Sender<GuiEvent>,
    event_rx: Option<mpsc::Receiver<GuiEvent>>,
}

impl Default for GuiApp {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            config: Config::default(),
            config_path: None,
            nodes: vpngate::curated_nodes(),
            nodes_source: "内嵌列表（28 个已知节点）".to_string(),
            selected: None,
            selected_set: BTreeSet::new(),
            blocked_hosts: BTreeSet::new(),
            deleted_nodes: Vec::new(),
            probes: HashMap::new(),
            probe_ms: HashMap::new(),
            busy: None,
            probe_all_stop: Arc::new(AtomicBool::new(false)),
            probe_all_total: 0,
            probe_all_done: 0,
            probe_all_ok: 0,
            probe_all_failed: 0,
            status: "未连接".to_string(),
            error: String::new(),
            log: vec!["就绪。".to_string()],
            conn: None,
            front_enabled: false,
            front_addr: "127.0.0.1:1080".to_string(),
            front_user: String::new(),
            front_pass: String::new(),
            local_port: 10818,
            allow_lan: false,
            system_proxy: false,
            col_widths: vpngate_client::config::UiSettings::default().node_col_widths,
            sort: None,
            row_press_pos: None,
            row_press_mods: None,
            scroll_to_selected: false,
            node_list_scroll_y: 0.0,
            selected_row_in_view: false,
            advanced_open: false,
            render_scroll_bottom: false,
            manual_host: String::new(),
            manual_error: String::new(),
            show_about: false,
            font_ok: false,
            last_save_error: String::new(),
            file_logger: None,
            log_path: None,
            event_tx: tx,
            event_rx: Some(rx),
        }
    }
}

/// Brand palette, coordinated with the app icon (indigo #1E40AF →
/// cyan #0891B2). Restrained on purpose: one accent (indigo), one
/// success accent (cyan/teal, used for the ∞ connected-node mark),
/// muted amber for warnings, soft red for errors — no multi-colour
/// mixing. Both themes follow the OS setting; these are per-theme
/// overrides on egui's defaults. Contrast checked (WCAG):
/// * light ink #1F2937 on warm off-white #F7F6F3 ≈ 13:1
/// * dark warm-white #F1F5F9 on deep slate #0F172A ≈ 15:1
/// * white on indigo selection #1E40AF ≈ 8.7:1, on #2563EB ≈ 5.1:1
/// * cyan ∞ #0E7490 on #F7F6F3 ≈ 4.9:1, #22D3EE on #0F172A ≈ 10:1
/// * error #B91C1C on #F7F6F3 ≈ 5.9:1, warn #B45309 ≈ 4.6:1
fn branded_visuals(theme: egui::Theme) -> egui::Visuals {
    use egui::Color32;
    let mut v = match theme {
        egui::Theme::Dark => egui::Visuals::dark(),
        egui::Theme::Light => egui::Visuals::light(),
    };
    match theme {
        egui::Theme::Dark => {
            v.override_text_color = Some(Color32::from_rgb(0xF1, 0xF5, 0xF9));
            v.panel_fill = Color32::from_rgb(0x0F, 0x17, 0x2A); // deep slate
            v.window_fill = Color32::from_rgb(0x1E, 0x29, 0x3B); // slate-800
            v.extreme_bg_color = Color32::from_rgb(0x0B, 0x12, 0x20);
            v.faint_bg_color = Color32::from_rgb(0x1E, 0x29, 0x3B);
            v.selection.bg_fill = Color32::from_rgb(0x25, 0x63, 0xEB);
            v.selection.stroke = egui::Stroke::new(1.0, Color32::WHITE);
            v.hyperlink_color = Color32::from_rgb(0x93, 0xC5, 0xFD);
            v.error_fg_color = Color32::from_rgb(0xFC, 0xA5, 0xA5);
            v.warn_fg_color = Color32::from_rgb(0xFC, 0xD3, 0x4D);
            v.widgets.hovered.bg_fill = Color32::from_rgb(0x1E, 0x3A, 0x8A);
            v.widgets.active.bg_fill = Color32::from_rgb(0x1E, 0x40, 0xAF);
            v.widgets.inactive.bg_fill = Color32::from_rgb(0x1E, 0x29, 0x3B);
        }
        egui::Theme::Light => {
            v.override_text_color = Some(Color32::from_rgb(0x1F, 0x29, 0x37)); // ink
            v.panel_fill = Color32::from_rgb(0xF7, 0xF6, 0xF3); // warm off-white
            v.window_fill = Color32::WHITE;
            v.extreme_bg_color = Color32::from_rgb(0xF8, 0xFA, 0xFC);
            v.faint_bg_color = Color32::from_rgb(0xF1, 0xF5, 0xF9);
            v.selection.bg_fill = Color32::from_rgb(0x1E, 0x40, 0xAF);
            v.selection.stroke = egui::Stroke::new(1.0, Color32::WHITE);
            v.hyperlink_color = Color32::from_rgb(0x1E, 0x40, 0xAF);
            v.error_fg_color = Color32::from_rgb(0xB9, 0x1C, 0x1C);
            v.warn_fg_color = Color32::from_rgb(0xB4, 0x53, 0x09);
            v.widgets.hovered.bg_fill = Color32::from_rgb(0xE0, 0xE7, 0xFF);
            v.widgets.active.bg_fill = Color32::from_rgb(0xC7, 0xD2, 0xFE);
            v.widgets.inactive.bg_fill = Color32::WHITE;
        }
    }
    v
}

/// Accent colour for the ∞ connected-node mark (not the only signal —
/// the glyph itself marks the row). Darker cyan on light so the
/// contrast stays above 4.5:1; bright cyan on dark.
fn infinity_accent(dark_mode: bool) -> egui::Color32 {
    if dark_mode {
        egui::Color32::from_rgb(0x22, 0xD3, 0xEE)
    } else {
        egui::Color32::from_rgb(0x0E, 0x74, 0x90)
    }
}

impl GuiApp {
    /// Fonts + text sizes + theme, set up once. Returns whether a CJK
    /// font was found. Shared by `new` and the render test constructor.
    fn setup_visuals(cc: &eframe::CreationContext<'_>) -> bool {
        cc.egui_ctx.set_theme(egui::ThemePreference::System);
        cc.egui_ctx
            .set_visuals_of(egui::Theme::Dark, branded_visuals(egui::Theme::Dark));
        cc.egui_ctx
            .set_visuals_of(egui::Theme::Light, branded_visuals(egui::Theme::Light));
        cc.egui_ctx.all_styles_mut(|style| {
            style.text_styles = [
                (
                    egui::TextStyle::Heading,
                    egui::FontId::new(20.0, egui::FontFamily::Proportional),
                ),
                (
                    egui::TextStyle::Body,
                    egui::FontId::new(15.0, egui::FontFamily::Proportional),
                ),
                (
                    egui::TextStyle::Button,
                    egui::FontId::new(15.0, egui::FontFamily::Proportional),
                ),
                (
                    egui::TextStyle::Monospace,
                    egui::FontId::new(13.0, egui::FontFamily::Monospace),
                ),
                (
                    egui::TextStyle::Small,
                    egui::FontId::new(12.0, egui::FontFamily::Proportional),
                ),
            ]
            .into();
            // Roomier controls: the default 8x4 spacing makes the
            // action buttons look cramped at 15 pt text.
            style.spacing.item_spacing = egui::vec2(10.0, 8.0);
            style.spacing.button_padding = egui::vec2(12.0, 6.0);
        });
        if let Some(bytes) = find_cjk_font() {
            let mut fonts = egui::FontDefinitions::default();
            fonts.font_data.insert(
                "cjk".to_owned(),
                std::sync::Arc::new(egui::FontData::from_owned(bytes)),
            );
            if let Some(fam) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
                fam.insert(0, "cjk".to_owned());
            }
            if let Some(fam) = fonts.families.get_mut(&egui::FontFamily::Monospace) {
                fam.insert(0, "cjk".to_owned());
            }
            cc.egui_ctx.set_fonts(fonts);
            true
        } else {
            false
        }
    }

    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let mut app = Self::default();
        app.font_ok = Self::setup_visuals(cc);
        // Portable config first: the session header in the file log
        // records the config path and the front-proxy setting it loaded.
        // Nothing is read from or written to the user profile.
        let cfg_path = Config::resolve_config_path(None);
        if let Some(p) = &cfg_path {
            if let Ok(cfg) = Config::load(Some(p)) {
                app.apply_config(&cfg);
            }
        }
        app.config_path = cfg_path;
        // File log: from here on every log_line / log_warn / log_error
        // (and every set_error) is also appended to vpngate-gui.log.
        app.init_file_log();
        if !app.font_ok {
            app.log_warn("未找到系统里的中文字体，中文可能显示为方框。".to_string());
        }
        // No config file at all (fresh folder, e.g. a new portable
        // version unzipped next to the old one): say so once, and tell
        // the user how to carry their settings over. The portable
        // storage design is unchanged — config stays next to the exe.
        if app.config_path.is_none() {
            app.log_line(
                "未找到 config.toml：如从旧版升级，把旧文件夹里的 config.toml 与 nodes_cache.json 复制到新版 exe 旁边即可保留设置。"
                    .to_string(),
            );
        }
        // No auto-refresh on startup: a refresh with the front proxy
        // off once hung for 85 s (direct fetch to vpngate.net, no
        // timeout). Load the cached list if there is one, else the
        // bundled 28 — no network at all — and let the user refresh
        // manually. `start_fetch` is still what the 刷新节点 button
        // runs; only the startup call is gone.
        let (nodes, cache_age) = vpngate::fallback_nodes();
        let source = if cache_age.is_some() {
            vpngate::NodeListSource::Cache
        } else {
            vpngate::NodeListSource::Curated
        };
        // The blocklist (loaded from config.toml just above) filters
        // the startup list too, like every other load.
        let nodes = app.apply_blocklist(nodes);
        let n = nodes.len();
        app.nodes = nodes;
        app.nodes_source = source.label(n, cache_age);
        let loaded = format!(
            "已加载{}。点右上角“刷新节点”获取最新。",
            app.nodes_source
        );
        app.status = loaded.clone();
        app.log_line(loaded);
        app
    }

    /// Test-only constructor: no font loading, no config file, no network.
    #[cfg(test)]
    fn for_tests() -> Self {
        Self::default()
    }

    /// Render-test constructor: real fonts and styles, but no config
    /// file and no network fetch (no tokio runtime in the harness).
    #[cfg(test)]
    fn for_render_tests(cc: &eframe::CreationContext<'_>) -> Self {
        let mut app = Self::default();
        app.font_ok = Self::setup_visuals(cc);
        // 高级设置 open: the review shots must show the front-proxy
        // section and its test button, not just the collapsed header.
        app.advanced_open = true;
        // No fetch runs in the harness; don't claim one is starting.
        app.log = vec!["就绪。".to_string()];
        app
    }

    /// Render-test constructor with the outer scroll area at the bottom
    /// (shows the end of 高级设置 and the log).
    #[cfg(test)]
    fn for_render_tests_scrolled(cc: &eframe::CreationContext<'_>) -> Self {
        let mut app = Self::for_render_tests(cc);
        app.render_scroll_bottom = true;
        app
    }

    fn apply_config(&mut self, cfg: &Config) {
        self.config = cfg.clone();
        self.front_enabled = cfg.front_proxy.enabled;
        self.front_addr = cfg.front_proxy.addr.clone();
        self.front_user = cfg.front_proxy.username.clone().unwrap_or_default();
        self.front_pass = cfg.front_proxy.password.clone().unwrap_or_default();
        self.local_port = cfg.listen.local_port;
        self.allow_lan = cfg.listen.allow_lan;
        self.system_proxy = cfg.system_proxy.enabled;
        self.col_widths = cfg.ui.node_col_widths;
        self.blocked_hosts = cfg.blocked_hosts.iter().cloned().collect();
    }

    /// Build the current settings as a Config (for saving).
    fn current_config(&self) -> Config {
        let mut cfg = self.config.clone();
        cfg.front_proxy.enabled = self.front_enabled;
        cfg.front_proxy.addr = self.front_addr.clone();
        cfg.front_proxy.username = if self.front_user.is_empty() {
            None
        } else {
            Some(self.front_user.clone())
        };
        cfg.front_proxy.password = if self.front_pass.is_empty() {
            None
        } else {
            Some(self.front_pass.clone())
        };
        cfg.listen.local_port = self.local_port;
        cfg.listen.allow_lan = self.allow_lan;
        cfg.system_proxy.enabled = self.system_proxy;
        cfg.ui.node_col_widths = self.col_widths;
        // BTreeSet iteration is sorted, so the saved list is in a
        // stable order across runs.
        cfg.blocked_hosts = self.blocked_hosts.iter().cloned().collect();
        cfg
    }

    /// Save the current settings to `config.toml` next to the exe
    /// (portable mode). Reports the result in the log and status line.
    /// With auto-save on, this is normally already done — the button is
    /// an explicit "save again now" that confirms it.
    fn save_settings(&mut self) {
        let Some(path) = Config::portable_config_path() else {
            self.set_error("保存失败：找不到程序所在目录。".to_string());
            return;
        };
        match self.current_config().save(&path) {
            Ok(()) => {
                self.config = self.current_config();
                self.config_path = Some(path.clone());
                self.error.clear();
                self.status = "设置已保存到程序目录下的 config.toml。".to_string();
                self.log_line(format!("设置已保存到 {}。", path.display()));
            }
            Err(e) => {
                self.set_error(format!("保存设置失败：{}。程序目录可能不可写。", e));
            }
        }
    }

    /// Quiet save used by auto-save: writes the file without touching
    /// the status line or log (a save per keystroke would spam both).
    /// A failure is shown once per distinct error message.
    fn save_settings_quiet(&mut self) {
        let Some(path) = Config::portable_config_path() else {
            return;
        };
        match self.current_config().save(&path) {
            Ok(()) => {
                self.config_path = Some(path);
                if !self.last_save_error.is_empty() {
                    self.last_save_error.clear();
                    self.error.clear();
                }
            }
            Err(e) => {
                let msg = format!("自动保存设置失败：{}。程序目录可能不可写。", e);
                if msg != self.last_save_error {
                    self.last_save_error = msg.clone();
                    self.set_error(msg);
                }
            }
        }
    }

    /// If any setting differs from the last saved/loaded config, persist
    /// it now — so closing the app without clicking 保存设置 loses
    /// nothing (front proxy, LAN sharing, system proxy, local port).
    /// Called at the end of every frame and on exit.
    fn auto_save_if_dirty(&mut self) {
        let cfg = self.current_config();
        if cfg != self.config {
            self.config = cfg;
            self.save_settings_quiet();
        }
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(self.config.tunnel.timeout_secs.max(1))
    }

    fn connector(&self) -> Connector {
        connector_for(
            self.front_enabled,
            &self.front_addr,
            &self.front_user,
            &self.front_pass,
        )
    }

    /// Start the file log (`vpngate-gui.log` next to the exe, same
    /// portable dir as config.toml; `./vpngate-gui.log` when the exe
    /// dir is unknown). Writes the session header (start time,
    /// version, exe path, config path, front proxy enabled + address —
    /// never the password) and replays the lines already on screen, so
    /// the file covers the whole run from startup. A file that cannot
    /// be opened warns once in the on-screen log and disables file
    /// logging for this run; the GUI never crashes over it.
    fn init_file_log(&mut self) {
        let path = Config::portable_dir()
            .map(|d| filelog::log_path_for_dir(&d))
            .unwrap_or_else(|| std::path::PathBuf::from("vpngate-gui.log"));
        self.log_path = Some(path.clone());
        let mut logger = FileLogger::new(&path);
        let exe = std::env::current_exe().ok();
        logger.write_header(&self.config, exe.as_deref(), self.config_path.as_deref());
        // Replay what is already on screen (the "就绪…" startup line),
        // so the file covers the run from the start.
        for line in self.log.clone() {
            logger.log(LogLevel::Info, &line);
        }
        if logger.open_failed {
            let err = logger.open_error.clone().unwrap_or_default();
            self.file_logger = Some(logger);
            self.log_warn(format!(
                "日志文件打不开（{}）：{}。本次运行不写文件日志，界面日志不受影响。",
                path.display(),
                err
            ));
        } else {
            self.file_logger = Some(logger);
            self.log_line(format!("日志文件：{}。报问题时请附上它。", path.display()));
        }
    }

    /// Info: on-screen log + file log at [INFO].
    fn log_line(&mut self, line: String) {
        self.log_line_at(LogLevel::Info, line);
    }

    /// Warning: on-screen log + file log at [WARN].
    fn log_warn(&mut self, line: String) {
        self.log_line_at(LogLevel::Warn, line);
    }

    /// Error: on-screen log + file log at [ERROR].
    fn log_error(&mut self, line: String) {
        self.log_line_at(LogLevel::Error, line);
    }

    fn log_line_at(&mut self, level: LogLevel, line: String) {
        if let Some(fl) = self.file_logger.as_mut() {
            fl.log(level, &line);
        }
        self.log.push(line);
        if self.log.len() > 100 {
            let excess = self.log.len() - 100;
            self.log.drain(..excess);
        }
    }

    /// Set the error banner AND log it at [ERROR] (both logs). Use
    /// this instead of assigning `self.error` directly — a bare
    /// assignment is invisible in the file log, which is exactly how
    /// a failure ends up with no trace to debug from.
    fn set_error(&mut self, msg: String) {
        self.error = msg.clone();
        self.log_error(msg);
    }

    fn start_fetch(&mut self, ctx: egui::Context) {
        if self.busy.is_some() {
            return;
        }
        self.busy = Some(Busy::Fetching);
        self.status = "正在拉取节点列表…".to_string();
        let tx = self.event_tx.clone();
        let connector = self.connector();
        tokio::spawn(async move {
            let res = fetch_flow(&connector).await;
            let _ = tx.send(GuiEvent::Nodes(res));
            ctx.request_repaint();
        });
    }

    /// Spawn a background task. No-ops when there is no tokio runtime
    /// (the kittest harness has none): the busy flag stays set and the
    /// caller observes it, which is exactly what the shortcut tests
    /// assert. Production always runs inside the runtime `main`
    /// creates (`rt.enter()`), so this never skips there.
    fn spawn_task<F>(f: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(f);
        }
    }

    fn start_probe(&mut self, ctx: egui::Context) {
        let Some(i) = self.selected else { return };
        let Some(node) = self.nodes.get(i) else {
            return;
        };
        if self.busy.is_some() {
            return;
        }
        let target = node.host.clone();
        let port = node.port;
        // The dialled target (and the start line) uses the completed
        // hostname: a bare DDNS label from the node list
        // ("public-vpn-192") becomes its full FQDN, so the log shows
        // exactly which host:port is being dialled. `target` itself
        // stays raw: it is the probes-map key and the table looks the
        // status up by the node's stored host.
        let target_str = match port {
            Some(p) => format!("{}:{}", vpngate::complete_ddns_host(&target), p),
            None => vpngate::complete_ddns_host(&target),
        };
        self.busy = Some(Busy::Probing);
        // Probing goes through the front proxy when it is on (same
        // chain as refresh/connect), so say so: when the proxy is down,
        // the probe fails for the proxy's sake, not the node's.
        if self.front_enabled {
            self.status = format!("正在探测 {}（经前置代理 {}）…", target_str, self.front_addr);
            self.log_line(format!(
                "开始探测 {}（经前置代理 {}）。",
                target_str, self.front_addr
            ));
        } else {
            self.status = format!("正在探测 {}…", target_str);
        }
        self.probes.insert(target.clone(), "探测中…".to_string());
        // A re-probe must not keep the old latency: until this probe
        // finishes, the node sorts with the unprobed group.
        self.probe_ms.remove(&target);
        let tx = self.event_tx.clone();
        let connector = self.connector();
        let timeout = self.timeout();
        Self::spawn_task(async move {
            let res = probe_flow(&connector, &target_str, timeout).await;
            let _ = tx.send(GuiEvent::Probe {
                host: target,
                result: res,
            });
            ctx.request_repaint();
        });
    }

    /// Snapshot of the nodes to probe, in display order (see
    /// `display_indices`, so a sorted list sweeps in the shown
    /// order): (raw host — the probes-map key — and the dialled
    /// target string, host:port with the DDNS suffix completed, like
    /// `start_probe` builds it). A snapshot, not indices: the list
    /// can gain a manually-added node mid-sweep, which would shift
    /// indices under a stored queue.
    fn probe_all_targets(&self) -> Vec<(String, String)> {
        self.display_indices()
            .into_iter()
            .filter_map(|i| {
                let n = self.nodes.get(i)?;
                let target = match n.port {
                    Some(p) => format!("{}:{}", vpngate::complete_ddns_host(&n.host), p),
                    None => vpngate::complete_ddns_host(&n.host),
                };
                Some((n.host.clone(), target))
            })
            .collect()
    }

    /// Start a probe-all sweep: every node in the list, in display
    /// order, one at a time. Blocked when another job is running or
    /// the list is empty (the button is disabled then too).
    fn start_probe_all(&mut self, ctx: egui::Context) {
        if self.busy.is_some() || self.nodes.is_empty() {
            return;
        }
        let targets = self.probe_all_targets();
        if targets.is_empty() {
            return;
        }
        let total = targets.len();
        self.probe_all_stop.store(false, Ordering::Relaxed);
        self.probe_all_total = total;
        self.probe_all_done = 0;
        self.probe_all_ok = 0;
        self.probe_all_failed = 0;
        self.busy = Some(Busy::ProbingAll);
        self.status = format!("正在探测全部：0/{}…", total);
        self.log_line(format!(
            "开始探测全部：{} 个节点（按当前显示顺序，一个一个探测）。",
            total
        ));
        // The first row shows "探测中…" right away; later rows get
        // it from the ProbeAllProgress event before their probe.
        if let Some((first, _)) = targets.first() {
            self.probes.insert(first.clone(), "探测中…".to_string());
            self.probe_ms.remove(first);
        }
        let tx = self.event_tx.clone();
        let connector = self.connector();
        let timeout = self.timeout();
        let stop = self.probe_all_stop.clone();
        Self::spawn_task(async move {
            probe_all_flow(&connector, targets, timeout, stop, tx, ctx).await;
        });
    }

    /// Ask the running sweep to stop after the node in flight
    /// finishes. The busy flag stays set until the ProbeAllDone
    /// event arrives; results already out are kept.
    fn stop_probe_all(&mut self) {
        if self.busy != Some(Busy::ProbingAll) {
            return;
        }
        self.probe_all_stop.store(true, Ordering::Relaxed);
        self.status = format!(
            "正在停止探测全部（等当前节点测完，已测 {}/{}）…",
            self.probe_all_done, self.probe_all_total
        );
        self.log_line("已请求停止探测全部，当前节点测完后停下。".to_string());
    }

    /// Addresses to actually bind for the next connection, from the
    /// live UI fields: the local port, or the same port on all
    /// interfaces when LAN sharing is on (one mixed server).
    fn listen_addrs(&self) -> Vec<String> {
        self.current_config().listen.listen_addrs()
    }

    /// What the user sees for the next connection's local proxy.
    fn listen_display(&self) -> String {
        self.current_config().listen.display_addrs()
    }

    fn start_connect(&mut self, ctx: egui::Context) {
        let Some(i) = self.selected else { return };
        let Some(node) = self.nodes.get(i) else {
            return;
        };
        if self.busy.is_some() || self.conn.is_some() {
            return;
        }
        let target = match node.port {
            // Complete a bare DDNS label, like start_probe does, so
            // the start line and the connection label show the full
            // host:port actually dialled.
            Some(p) => format!("{}:{}", vpngate::complete_ddns_host(&node.host), p),
            None => vpngate::complete_ddns_host(&node.host),
        };
        let listen_addrs = self.listen_addrs();
        let listen_display = self.listen_display();
        self.busy = Some(Busy::Connecting);
        self.status = format!("正在连接 {}…", target);
        self.error.clear();
        // Log the start line with the full target (host:port when the
        // port is known, bare host otherwise — the candidates are
        // resolved inside connect_flow). The probe start line already
        // logs its target; the connect had none, so a failed connect
        // left no trace of which host:port it actually dialled.
        if self.front_enabled {
            self.log_line(format!(
                "开始连接 {}（经前置代理 {}）。",
                target, self.front_addr
            ));
        } else {
            self.log_line(format!("开始连接 {}。", target));
        }
        let tx = self.event_tx.clone();
        let connector = self.connector();
        let timeout = self.timeout();
        let system_proxy = self.system_proxy;
        let service = self.config.system_proxy.service.clone();
        Self::spawn_task(async move {
            // Per-connection lines from the mixed proxy go through
            // this channel into the GUI log (GuiEvent::ConnLog);
            // the forwarder joins the connection's serve tasks, so
            // disconnecting aborts it too.
            let (conn_tx, mut conn_rx) =
                tokio::sync::mpsc::unbounded_channel::<String>();
            let tx2 = tx.clone();
            match connect_flow(
                connector,
                target.clone(),
                listen_addrs,
                listen_display,
                timeout,
                system_proxy,
                service,
                Some(conn_tx),
            )
            .await
            {
                Ok(mut c) => {
                    c.serve.push(tokio::spawn(async move {
                        while let Some(line) = conn_rx.recv().await {
                            let _ = tx2.send(GuiEvent::ConnLog(line));
                        }
                    }));
                    let _ = tx.send(GuiEvent::Connected(Box::new(c)));
                }
                Err(e) => {
                    let _ = tx.send(GuiEvent::ConnectFailed(e));
                }
            }
            ctx.request_repaint();
        });
    }

    fn start_test(&mut self, ctx: egui::Context) {
        let Some(c) = &self.conn else { return };
        if self.busy.is_some() {
            return;
        }
        self.busy = Some(Busy::Testing);
        self.status = "正在测试隧道（generate_204 与 Google）…".to_string();
        let tx = self.event_tx.clone();
        let tunnel = c.tunnel.clone();
        let timeout = self.timeout();
        tokio::spawn(async move {
            let (gen204, google, detail) = test_flow(&tunnel, timeout).await;
            let _ = tx.send(GuiEvent::TestResult {
                gen204,
                google,
                detail,
            });
            ctx.request_repaint();
        });
    }

    /// Test the front proxy (button next to the front-proxy settings).
    /// Only runs when the front proxy is on — with it off there is
    /// nothing to test and a "direct" result would be misleading.
    /// The address is snapshotted at click time: the start line, the
    /// progress and the result must all name the address the test
    /// actually used, even if the field is edited mid-test.
    fn start_test_front_proxy(&mut self, ctx: egui::Context) {
        if !self.front_enabled || self.busy.is_some() {
            return;
        }
        let addr = self.front_addr.clone();
        self.busy = Some(Busy::TestingFrontProxy);
        self.status = format!("正在测试前置代理 {}…", addr);
        self.log_line(format!(
            "开始测试前置代理 {}（到 Google 与 vpngate.net，每站最多 10 秒）。",
            addr
        ));
        let tx = self.event_tx.clone();
        let connector = self.connector();
        tokio::spawn(async move {
            let (google, vpngate) =
                front_proxy_test_flow(&connector, FRONT_PROXY_TEST_TIMEOUT, &tx).await;
            let _ = tx.send(GuiEvent::FrontProxyTest {
                addr,
                google,
                vpngate,
            });
            ctx.request_repaint();
        });
    }

    fn disconnect(&mut self) {
        if let Some(c) = self.conn.take() {
            for s in c.serve {
                s.abort();
            }
            if let Some(g) = c.proxy_guard {
                g.unset();
            }
            // Dropping the last tunnel Arc closes the TLS stream.
            self.log_line(format!("已断开 {}（虚拟 IP {}）。", c.node_label, c.vip));
        }
        self.status = "未连接".to_string();
        self.error.clear();
    }

    /// Apply one background-task result. Pure state update, no egui calls,
    /// so headless tests can drive it.
    fn apply_event(&mut self, ev: GuiEvent) {
        match ev {
            GuiEvent::Nodes(upd) => {
                self.busy = None;
                // A fresh list from the API or the web scrape is worth
                // caching, so the next failed refresh still has a recent
                // list to show.
                if matches!(
                    upd.source,
                    vpngate::NodeListSource::Api | vpngate::NodeListSource::WebScrape
                ) {
                    let _ = vpngate::save_cached_nodes(&upd.nodes);
                }
                // Deleted hosts never come back: the blocklist filters
                // every load — API, web scrape, cache, embedded.
                // (The cache above still stores the full list; the
                // filter re-applies when the cache is loaded.)
                self.nodes = self.apply_blocklist(upd.nodes);
                let n = self.nodes.len();
                self.nodes_source = upd.source.label(n, upd.cache_age_secs);
                self.clear_selection();
                match upd.source {
                    vpngate::NodeListSource::Api => {
                        self.status = format!("节点列表已更新：{} 个节点。", n);
                        self.log_line(format!("节点列表已更新：{} 个节点。", n));
                    }
                    vpngate::NodeListSource::WebScrape => {
                        self.status = format!("官方 API 不可用，已从网页抓取 {} 个节点。", n);
                        self.log_line(format!(
                            "官方 API 拉取失败：{}；已改用网页抓取（{} 个节点）。",
                            upd.api_error.as_deref().unwrap_or("未知原因"),
                            n
                        ));
                    }
                    vpngate::NodeListSource::Cache => {
                        self.status =
                            format!("官方 API 与网页都不可用，已显示缓存列表（{} 个）。", n);
                        self.log_line(format!(
                            "官方 API 拉取失败：{}；网页抓取失败：{}；已改用缓存列表（{} 个）。",
                            upd.api_error.as_deref().unwrap_or("未知原因"),
                            upd.web_error.as_deref().unwrap_or("未知原因"),
                            n
                        ));
                    }
                    vpngate::NodeListSource::Curated => {
                        self.status =
                            "官方 API 与网页都不可用，显示内嵌列表（28 个）。".to_string();
                        self.log_line(format!(
                            "官方 API 拉取失败：{}；网页抓取失败：{}；已改用内嵌列表。",
                            upd.api_error.as_deref().unwrap_or("未知原因"),
                            upd.web_error.as_deref().unwrap_or("未知原因")
                        ));
                    }
                }
            }
            GuiEvent::Probe { host, result } => {
                self.busy = None;
                match result {
                    Ok((vip, ms)) => {
                        self.probes
                            .insert(host.clone(), format!("{} ms", ms.as_millis()));
                        self.probe_ms
                            .insert(host.clone(), ms.as_millis() as u64);
                        self.status =
                            format!("{} 可用：虚拟 IP {}，{} ms。", host, vip, ms.as_millis());
                        self.log_line(format!(
                            "探测 {}：可用，虚拟 IP {}，{} ms。",
                            host,
                            vip,
                            ms.as_millis()
                        ));
                    }
                    Err(ProbeFailure::ProxyDown(e)) => {
                        self.probes.insert(host.clone(), "代理不通".to_string());
                        self.probe_ms.remove(&host);
                        self.status = "探测失败：前置代理连不上。".to_string();
                        self.set_error(format!(
                            "探测 {} 失败：前置代理 {} 连不上（{}）。节点还没连上——问题在代理不在节点：刷新、探测、连接都走这条代理，代理不通时会一起失败，换节点没用。先点高级设置里的“测试前置代理”确认。",
                            host, self.front_addr, e
                        ));
                        self.log_line(format!(
                            "探测 {}：失败：前置代理 {} 连不上（{}）。提示：刷新节点、探测、连接都走前置代理，代理不通时它们会一起失败。",
                            host, self.front_addr, e
                        ));
                    }
                    Err(ProbeFailure::NodeDown(e)) => {
                        self.probes.insert(host.clone(), "失败".to_string());
                        self.probe_ms.remove(&host);
                        self.status = format!("{} 探测失败。", host);
                        if self.front_enabled {
                            self.set_error(format!(
                                "探测 {} 失败：前置代理 {} 能连上，但节点不通（{}）。换一个节点再试。",
                                host, self.front_addr, e
                            ));
                            self.log_line(format!(
                                "探测 {}：失败：前置代理 {} 能连上，节点不通（{}）。",
                                host, self.front_addr, e
                            ));
                        } else {
                            self.set_error(format!("探测 {} 失败：{}。换一个节点再试。", host, e));
                        }
                    }
                }
            }
            GuiEvent::ProbeAllProgress {
                current,
                done,
                total,
            } => {
                // A sweep in flight: the row being probed shows
                // "探测中…" (same cell text as a single probe) and
                // the status line names it. A stray progress event
                // with no sweep running is ignored.
                if self.busy != Some(Busy::ProbingAll) {
                    return;
                }
                self.probe_all_done = done;
                self.probe_all_total = total;
                self.probes.insert(current.clone(), "探测中…".to_string());
                self.probe_ms.remove(&current);
                self.status = format!("正在探测全部：{}/{}（{}）…", done, total, current);
            }
            GuiEvent::ProbeAllResult {
                host,
                result,
                done,
                total,
            } => {
                // Same cell updates as a single probe, but no
                // per-node error banner: 83 failures would spam the
                // banner and the file log with the long advisory
                // text, and the summary at the end is the right
                // place for the outcome. One concise log line per
                // node keeps the trace without the noise.
                if self.busy != Some(Busy::ProbingAll) {
                    return;
                }
                self.probe_all_done = done;
                self.probe_all_total = total;
                match result {
                    Ok((vip, ms)) => {
                        self.probe_all_ok += 1;
                        self.probes
                            .insert(host.clone(), format!("{} ms", ms.as_millis()));
                        self.probe_ms
                            .insert(host.clone(), ms.as_millis() as u64);
                        self.status = format!(
                            "正在探测全部：{}/{}（{} 可用，{} ms）…",
                            done, total, host, ms.as_millis()
                        );
                        self.log_line(format!(
                            "探测全部 {}/{}：{} 可用，虚拟 IP {}，{} ms。",
                            done, total, host, vip, ms.as_millis()
                        ));
                    }
                    Err(ProbeFailure::ProxyDown(e)) => {
                        self.probe_all_failed += 1;
                        self.probes.insert(host.clone(), "代理不通".to_string());
                        self.probe_ms.remove(&host);
                        self.status = format!(
                            "正在探测全部：{}/{}（{} 代理不通）…",
                            done, total, host
                        );
                        self.log_line(format!(
                            "探测全部 {}/{}：{} 失败：前置代理不通（{}）。",
                            done, total, host, e
                        ));
                    }
                    Err(ProbeFailure::NodeDown(e)) => {
                        self.probe_all_failed += 1;
                        self.probes.insert(host.clone(), "失败".to_string());
                        self.probe_ms.remove(&host);
                        self.status = format!(
                            "正在探测全部：{}/{}（{} 失败）…",
                            done, total, host
                        );
                        self.log_line(format!(
                            "探测全部 {}/{}：{} 失败（{}）。",
                            done, total, host, e
                        ));
                    }
                }
            }
            GuiEvent::ProbeAllDone {
                total,
                ok,
                failed,
                stopped,
                proxy_down,
            } => {
                self.busy = None;
                self.probe_all_ok = ok;
                self.probe_all_failed = failed;
                self.probe_all_done = ok + failed;
                self.probe_all_total = total;
                let skipped = total.saturating_sub(ok + failed);
                if proxy_down {
                    self.status = format!(
                        "探测全部已停止：前置代理不通，已测 {}/{}，{} 个可用，{} 个失败。",
                        ok + failed, total, ok, failed
                    );
                    self.set_error(format!(
                        "探测全部已停止：前置代理 {} 不通，剩下的 {} 个节点没测（已测 {}/{}，{} 个可用）。先点高级设置里的“测试前置代理”确认。",
                        self.front_addr, skipped, ok + failed, total, ok
                    ));
                    self.log_line(format!(
                        "探测全部已停止：前置代理不通。已测 {}/{}，{} 个可用，{} 个失败，{} 个未测。",
                        ok + failed, total, ok, failed, skipped
                    ));
                } else if stopped {
                    self.status = format!(
                        "探测全部已停止：已测 {}/{}，{} 个可用，{} 个失败。",
                        ok + failed, total, ok, failed
                    );
                    self.log_line(format!(
                        "探测全部已停止：已测 {}/{}，{} 个可用，{} 个失败，{} 个未测。",
                        ok + failed, total, ok, failed, skipped
                    ));
                } else {
                    self.status = format!(
                        "探测全部完成：{} 个节点，{} 个可用，{} 个失败。",
                        total, ok, failed
                    );
                    self.log_line(format!(
                        "探测全部完成：{} 个节点，{} 个可用，{} 个失败。",
                        total, ok, failed
                    ));
                }
            }
            GuiEvent::Connected(c) => {
                self.busy = None;
                self.status = format!("已连接 {}，虚拟 IP {}。", c.node_label, c.vip);
                self.error.clear();
                self.log_line(format!(
                    "已连接 {}，虚拟 IP {}，本地混合代理（SOCKS5+HTTP）{}。浏览器/应用连这个端口即可上网。",
                    c.node_label, c.vip, c.listen_addr
                ));
                self.conn = Some(*c);
            }
            GuiEvent::ConnectFailed(e) => {
                self.busy = None;
                self.status = "连接失败。".to_string();
                self.set_error(format!("连接失败：{}。换一个节点，或检查前置 SOCKS5 设置。", e));
            }
            GuiEvent::ConnLog(line) => {
                self.log_line(line);
            }
            GuiEvent::TestResult {
                gen204,
                google,
                detail,
            } => {
                self.busy = None;
                self.status = if gen204 && google {
                    "测试通过：generate_204 与 Google 都通了。".to_string()
                } else if gen204 {
                    "测试部分通过：generate_204 通了，Google 不通。".to_string()
                } else {
                    "测试未通过：generate_204 不通。".to_string()
                };
                self.log_line(format!(
                    "隧道测试：{}（generate_204 {}，Google {}）。",
                    detail, gen204, google
                ));
                if !gen204 {
                    self.set_error(format!("测试未通过：{} 换一个节点再试。", detail));
                } else if !google {
                    self.set_error(format!(
                        "Google 不通：{} 可能是出口被挡，换一个节点再试。",
                        detail
                    ));
                } else {
                    self.error.clear();
                }
            }
            GuiEvent::FrontProxyTestProgress { site } => {
                // Which site is being tested right now; the busy flag
                // stays set until the FrontProxyTest result arrives.
                self.status = format!("正在测试前置代理… {}", site);
            }
            GuiEvent::FrontProxyTest {
                addr,
                google,
                vpngate,
            } => {
                self.busy = None;
                let g = front_proxy_check_line("Google", &google);
                let v = front_proxy_check_line("vpngate.net", &vpngate);
                self.log_line(format!("前置代理测试：{}；{}。", g, v));
                let proxy_detail = match &google {
                    net::FrontProxyCheck::ProxyUnreachable(d) => Some(d.clone()),
                    _ => match &vpngate {
                        net::FrontProxyCheck::ProxyUnreachable(d) => Some(d.clone()),
                        _ => None,
                    },
                };
                let mut target_down: Vec<(&str, String)> = Vec::new();
                if let net::FrontProxyCheck::TargetUnreachable(d) = &google {
                    target_down.push(("Google", d.clone()));
                }
                if let net::FrontProxyCheck::TargetUnreachable(d) = &vpngate {
                    target_down.push(("vpngate.net", d.clone()));
                }
                if let Some(d) = proxy_detail {
                    self.status = "前置代理测试：前置代理连不上。".to_string();
                    self.set_error(format!(
                        "前置代理 {} 连不上（{}）。刷新、探测、连接都走这条代理，代理不通时会一起失败：检查前置代理（NekoBox）开没开、地址对不对。",
                        addr, d
                    ));
                } else if target_down.is_empty() {
                    self.status = format!("前置代理测试：{}；{}。", g, v);
                    self.error.clear();
                } else {
                    let labels: Vec<&str> = target_down.iter().map(|(l, _)| *l).collect();
                    self.status = format!("前置代理测试：前置代理通，{}不通。", labels.join("、"));
                    self.set_error(format!(
                        "前置代理能连上，但 {} 通过它打不开（{}）。刷新节点要走 vpngate.net，这个不通就拉不到新节点；Google 不通则是出口的问题。",
                        labels.join("、"),
                        target_down[0].1
                    ));
                }
            }
        }
    }

    fn drain_events(&mut self) {
        let mut events = Vec::new();
        if let Some(rx) = &self.event_rx {
            while let Ok(ev) = rx.try_recv() {
                events.push(ev);
            }
        }
        for ev in events {
            self.apply_event(ev);
        }
    }

    fn selected_node_label(&self) -> Option<String> {
        self.nodes.get(self.selected?).map(|n| match n.port {
            Some(p) => format!("{}:{}", n.host, p),
            None => n.host.clone(),
        })
    }

    /// Node indices in display order: fetch order by default, or
    /// sorted by the column the user picked (see `sort`) — the
    /// whole row follows the sort key, and `selected` still stores
    /// a node index, so sorting never misaligns the selection. A
    /// refresh replaces `self.nodes` and the sort re-applies
    /// automatically, since the order is computed here, not stored.
    fn display_indices(&self) -> Vec<usize> {
        let mut idx: Vec<usize> = (0..self.nodes.len()).collect();
        if let Some((col, dir)) = self.sort {
            idx.sort_by(|&a, &b| {
                let (na, nb) = (&self.nodes[a], &self.nodes[b]);
                let ord = match col {
                    SortCol::Country => na
                        .country
                        .cmp(&nb.country)
                        .then_with(|| na.host.cmp(&nb.host)),
                    SortCol::Sessions => na
                        .sessions
                        .cmp(&nb.sessions)
                        .then_with(|| na.host.cmp(&nb.host)),
                    SortCol::Quality => quality_key(na)
                        .cmp(&quality_key(nb))
                        .then_with(|| na.host.cmp(&nb.host)),
                    SortCol::Latency => self
                        .latency_key(na)
                        .cmp(&self.latency_key(nb))
                        .then_with(|| na.host.cmp(&nb.host)),
                };
                match dir {
                    SortDir::Asc => ord,
                    SortDir::Desc => ord.reverse(),
                }
            });
        }
        idx
    }

    /// Click a sortable column header: the same column again
    /// reverses the direction; a different column starts at that
    /// column's first-click direction (see `SortCol::default_dir`).
    fn toggle_sort(&mut self, col: SortCol) {
        self.sort = Some(match self.sort {
            Some((c, dir)) if c == col => (
                col,
                match dir {
                    SortDir::Asc => SortDir::Desc,
                    SortDir::Desc => SortDir::Asc,
                },
            ),
            _ => (col, col.default_dir()),
        });
    }

    /// Sort key for the 延迟/状态 column: (group, ms). Group 0 =
    /// probed and available, ordered by latency ms ascending;
    /// group 1 = unprobed ("未探测" — no entry — or "探测中…");
    /// group 2 = failed ("失败" / "代理不通"). Reversing the whole
    /// key (second click) puts failures first, unprobed in the
    /// middle, available by ms descending last. The ms comes from
    /// `probe_ms`, stored when a probe succeeds — never re-parsed
    /// from the display string.
    fn latency_key(&self, n: &vpngate::Node) -> (u8, u64) {
        // A node being probed right now sorts with the unprobed
        // group even if an earlier probe left a ms behind (the
        // start path clears `probe_ms`, so this is belt-and-braces).
        if self.probes.get(&n.host).map(|s| s.as_str()) == Some("探测中…") {
            return (1, 0);
        }
        match self.probe_ms.get(&n.host) {
            Some(&ms) => (0, ms),
            None => match self.probes.get(&n.host) {
                None => (1, 0),
                Some(_) => (2, 0),
            },
        }
    }

    /// Canonical key for the blocklist: the completed DDNS hostname
    /// (a bare API label like "public-vpn-192" becomes
    /// "public-vpn-192.opengw.net"), so a node matches its block
    /// entry no matter which source listed it — and so the
    /// connected-node check compares like with like.
    fn blocked_key(host: &str) -> String {
        vpngate::complete_ddns_host(host)
    }

    /// Host of the currently connected node, if any (the `node_label`
    /// is the target string, "host" or "host:port", with the DDNS
    /// suffix already completed). The status column shows ∞ for the
    /// node whose completed host matches this.
    fn connected_node_host(&self) -> Option<String> {
        let label = self.conn.as_ref()?.node_label.clone();
        let host = match label.rsplit_once(':') {
            Some((h, p))
                if !h.is_empty() && p.chars().all(|c| c.is_ascii_digit()) =>
            {
                h.to_string()
            }
            _ => label,
        };
        Some(host)
    }

    /// Move the selection one row up (`delta < 0`) or down
    /// (`delta > 0`) in display order (see `display_indices`, so a
    /// sorted list moves in the shown order). Stops at the ends —
    /// no wrap-around. With no selection yet, ↓ picks the first row
    /// and ↑ picks the last. Sets `scroll_to_selected` when the
    /// selection actually changes, so the row loop scrolls it into
    /// view; pressing ↑ at the top (or ↓ at the bottom) changes
    /// nothing and requests no scroll. Moving the selection starts
    /// nothing, so callers may move it while busy or connected.
    fn move_selection(&mut self, delta: isize) {
        let order = self.display_indices();
        if order.is_empty() {
            return;
        }
        let pos = self
            .selected
            .and_then(|s| order.iter().position(|&i| i == s));
        let new_pos = match pos {
            Some(p) => (p as isize + delta).clamp(0, order.len() as isize - 1) as usize,
            None => {
                if delta > 0 {
                    0
                } else {
                    order.len() - 1
                }
            }
        };
        if Some(order[new_pos]) != self.selected {
            self.selected = Some(order[new_pos]);
            // Arrow keys keep the v3.4 behaviour: the multi-select
            // collapses to just the row the anchor moved to.
            self.selected_set = BTreeSet::from([order[new_pos]]);
            self.scroll_to_selected = true;
        }
    }

    /// Clear both halves of the selection: the anchor and the
    /// multi-select set (used after a delete, a restore, and a list
    /// reload — indices from the old list are meaningless there).
    fn clear_selection(&mut self) {
        self.selected = None;
        self.selected_set.clear();
    }

    /// The modifiers a row click counts with: the ones held when
    /// the press started (`press_mods`, latched in `row_press_mods`)
    /// OR the ones held now (the release frame). A press and a
    /// release usually land in different frames, and a user who
    /// lets go of Shift/Ctrl a beat before the mouse button would
    /// lose the gesture if only the release frame were read — that
    /// is exactly the v3.7 Shift+click failure (the range came out
    /// as a plain single-row click). The OR also covers Shift
    /// pressed mid-click and a press+release that land in one
    /// frame. `command` counts as Ctrl (Cmd on macOS, Ctrl
    /// elsewhere), like the row code before it.
    fn effective_click_mods(
        press_mods: Option<egui::Modifiers>,
        current: egui::Modifiers,
    ) -> (bool, bool) {
        let press = press_mods.unwrap_or_default();
        let ctrl = press.ctrl || press.command || current.ctrl || current.command;
        let shift = press.shift || current.shift;
        (ctrl, shift)
    }

    /// Click selection for a row (`node_idx` is an index into
    /// `self.nodes`, not a display position). Plain click selects
    /// only that row; Ctrl/Cmd+click toggles it in or out of the
    /// multi-select set; Shift+click selects the whole range from
    /// the anchor to this row in display order. Every click moves
    /// the anchor to the clicked row — the anchor is the row Enter /
    /// Ctrl+R / 探测 / 连接 act on, while 删除选中 deletes the set.
    fn click_row(&mut self, node_idx: usize, ctrl: bool, shift: bool) {
        if shift {
            if let Some(anchor) = self.selected {
                let order = self.display_indices();
                match (
                    order.iter().position(|&i| i == anchor),
                    order.iter().position(|&i| i == node_idx),
                ) {
                    (Some(a), Some(b)) => {
                        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
                        self.selected_set =
                            order[lo..=hi].iter().copied().collect();
                    }
                    // Anchor or clicked row is gone (the list was
                    // reloaded between clicks): fall back to
                    // selecting just this row.
                    _ => self.selected_set = BTreeSet::from([node_idx]),
                }
            } else {
                self.selected_set = BTreeSet::from([node_idx]);
            }
        } else if ctrl {
            if !self.selected_set.remove(&node_idx) {
                self.selected_set.insert(node_idx);
            }
        } else {
            self.selected_set = BTreeSet::from([node_idx]);
        }
        self.selected = Some(node_idx);
    }

    /// Whether 删除选中 may run: a non-empty selection and no job
    /// running (probe / connect / refresh / probe-all all set
    /// `busy` — the same guard as the 刷新节点 button). Deleting
    /// mid-sweep would change the list under the running probe-all.
    fn can_delete(&self) -> bool {
        !self.selected_set.is_empty() && self.busy.is_none()
    }

    /// Delete the multi-selected nodes. The connected node is never
    /// deleted — if it is selected it is skipped and logged.
    /// Deleted hosts go on the persistent blocklist (so a refresh
    /// never brings them back) and their full data is kept in
    /// `deleted_nodes` for this session's restore. Probe results
    /// for deleted hosts are cleaned up, and the selection is
    /// cleared.
    fn delete_selected(&mut self) {
        if !self.can_delete() {
            return;
        }
        let connected = self.connected_node_host();
        self.delete_selected_with_connected(connected.as_deref());
    }

    /// `delete_selected` with the connected host supplied directly
    /// (the live path passes `connected_node_host()`): the list
    /// mutation plus the status/log reporting, so the connected-node
    /// skip wording is unit-testable without a live tunnel.
    fn delete_selected_with_connected(&mut self, connected_host: Option<&str>) {
        let (removed, skipped) = self.delete_selected_except(connected_host);
        self.clear_selection();
        if skipped > 0 {
            self.log_line(format!("已连接的节点不能删除，已跳过 {}。", skipped));
        }
        if removed > 0 {
            self.status = format!(
                "已删除 {} 个节点（已记入屏蔽列表，刷新后也不会回来）。",
                removed
            );
            self.log_line(format!(
                "已删除 {} 个节点：屏蔽列表现在有 {} 个主机。",
                removed,
                self.blocked_hosts.len()
            ));
        }
    }

    /// Core of `delete_selected`, split out so the connected-node
    /// skip is unit-testable without a live tunnel:
    /// `connected_host` is the completed hostname of the connected
    /// node, if any. Returns (removed, skipped).
    fn delete_selected_except(&mut self, connected_host: Option<&str>) -> (usize, usize) {
        let mut removed = 0usize;
        let mut skipped = 0usize;
        // Remove highest index first so earlier indices stay valid.
        let mut idxs: Vec<usize> = self.selected_set.iter().copied().collect();
        idxs.sort_unstable_by(|a, b| b.cmp(a));
        let mut kept: Vec<Node> = Vec::new();
        for i in idxs {
            let Some(node) = self.nodes.get(i) else {
                continue;
            };
            let key = Self::blocked_key(&node.host);
            if connected_host == Some(key.as_str()) {
                skipped += 1;
                continue;
            }
            let node = self.nodes.remove(i);
            self.probes.remove(&node.host);
            self.probe_ms.remove(&node.host);
            self.blocked_hosts.insert(key);
            kept.push(node);
            removed += 1;
        }
        self.deleted_nodes.extend(kept);
        (removed, skipped)
    }

    /// Clear the blocklist and, if this session deleted nodes, put
    /// their full data back into the current list. After a restart
    /// there is nothing in memory to put back — then only the block
    /// record is cleared and the nodes return on the next refresh.
    /// Clears the selection either way.
    fn restore_deleted(&mut self) {
        if self.blocked_hosts.is_empty() {
            return;
        }
        let n_blocked = self.blocked_hosts.len();
        self.blocked_hosts.clear();
        let restored = std::mem::take(&mut self.deleted_nodes);
        let n_restored = restored.len();
        self.nodes.extend(restored);
        self.clear_selection();
        if n_restored > 0 {
            self.status = format!("已恢复 {} 个节点。", n_restored);
            self.log_line(format!(
                "已恢复 {} 个节点，屏蔽列表已清空（原有 {} 个主机）。",
                n_restored, n_blocked
            ));
        } else {
            self.status = "已清除删除记录，下次刷新后节点恢复。".to_string();
            self.log_line("已清除删除记录，下次刷新后节点恢复。".to_string());
        }
    }

    /// Drop the blocked hosts from a freshly loaded list. Applied on
    /// every list load — API refresh, web scrape, cache, embedded
    /// fallback — so a deleted node never comes back until restored.
    fn apply_blocklist(&self, nodes: Vec<Node>) -> Vec<Node> {
        if self.blocked_hosts.is_empty() {
            return nodes;
        }
        nodes
            .into_iter()
            .filter(|n| !self.blocked_hosts.contains(&Self::blocked_key(&n.host)))
            .collect()
    }

    /// Keyboard shortcuts (Enter = connect, Ctrl+R = probe): the pure
    /// guard half, unit-testable. `enter` / `ctrl_r` are whether the
    /// key was pressed this frame; `text_edit_focused` is whether a
    /// text field (manual-add, port, front-proxy address…) has focus
    /// — typing there must never trigger a connect or a probe.
    fn shortcut_action(
        &self,
        enter: bool,
        ctrl_r: bool,
        text_edit_focused: bool,
    ) -> Option<ShortcutAction> {
        if text_edit_focused
            || self.show_about
            || self.conn.is_some()
            || self.busy.is_some()
            || self.selected.is_none()
        {
            return None;
        }
        if enter {
            Some(ShortcutAction::Connect)
        } else if ctrl_r {
            Some(ShortcutAction::Probe)
        } else {
            None
        }
    }
}

/// What a keyboard shortcut asks for (see `shortcut_action`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShortcutAction {
    Connect,
    Probe,
}

// ---------------------------------------------------------------------------
// eframe App
// ---------------------------------------------------------------------------

impl eframe::App for GuiApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Background results first.
        self.drain_events();

        // Escape closes the About window.
        if self.show_about && ui.ctx().input(|i| i.key_pressed(egui::Key::Escape)) {
            self.show_about = false;
        }
        // Keyboard shortcuts on the node list: Enter connects the
        // selected node, Ctrl+R probes it (same flows as the buttons).
        // `text_edit_focused` covers the manual-add, port and
        // front-proxy fields — typing there must not trigger anything.
        // A focused row also fires its own `clicked()` on Enter (egui
        // activates focused click widgets with Enter/Space); that only
        // re-selects the same row, and the busy guard below blocks a
        // second connect/probe from a focused button in the same frame.
        let ctx = ui.ctx().clone();
        let enter = ctx.input(|i| i.key_pressed(egui::Key::Enter));
        let ctrl_r = ctx.input_mut(|i| {
            i.consume_shortcut(&egui::KeyboardShortcut::new(
                egui::Modifiers::COMMAND,
                egui::Key::R,
            ))
        });
        let text_focused = ctx.text_edit_focused();
        match self.shortcut_action(enter, ctrl_r, text_focused) {
            Some(ShortcutAction::Connect) => self.start_connect(ctx.clone()),
            Some(ShortcutAction::Probe) => self.start_probe(ctx.clone()),
            None => {}
        }
        // Arrow keys move the selection in the node list (same guard
        // family as the shortcuts above: no text field focused —
        // ↑/↓ there move the cursor — and the About window closed).
        // Unlike Enter/Ctrl+R, moving the selection is allowed while
        // busy or connected: it only changes which row is selected,
        // it starts nothing. Holding a key repeats (egui
        // `key_pressed` includes repeat events). The row loop
        // scrolls the newly selected row into view (one-shot
        // `scroll_to_selected`).
        if !text_focused && !self.show_about {
            if ctx.input(|i| i.key_pressed(egui::Key::ArrowUp)) {
                self.move_selection(-1);
            } else if ctx.input(|i| i.key_pressed(egui::Key::ArrowDown)) {
                self.move_selection(1);
            }
        }
        // Quitting while connected disconnects first (no confirm needed).
        if ui.ctx().input(|i| i.viewport().close_requested()) {
            self.disconnect();
            // Persist any unsaved setting change before the window goes.
            self.auto_save_if_dirty();
        }

        egui::Panel::top("menu_bar").show(ui, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("文件", |ui| {
                    if ui.button("退出").clicked() {
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
                ui.menu_button("帮助", |ui| {
                    if ui.button("关于").clicked() {
                        self.show_about = true;
                        ui.close();
                    }
                });
            });
        });

        egui::Panel::bottom("status_bar").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(&self.status);
                if let Some(c) = &self.conn {
                    ui.separator();
                    ui.label(format!("虚拟 IP {} · 本地 {}", c.vip, c.listen_addr));
                }
            });
            if !self.error.is_empty() {
                ui.label(egui::RichText::new(&self.error).color(ui.visuals().error_fg_color));
            }
        });

        egui::CentralPanel::default().show(ui, |ui| {
            // egui's default scroll style is "floating": the bar is fully
            // transparent (dormant opacity 0) until the mouse hovers the
            // list, so "always visible" alone still shows nothing at rest.
            // Solid bars are always drawn (6 px, full opacity) in both
            // themes, which is what the node list needs.
            ui.style_mut().spacing.scroll = egui::style::ScrollStyle::solid();
            // The panel is only 420 px tall at minimum size while the
            // content below the node list (advanced settings, log) needs
            // more room: one outer vertical scroll area wraps everything
            // so nothing is cut off without a way to reach it. The node
            // list and the log keep their own fixed-height scroll areas
            // inside it. `panel_h` is captured before entering the
            // scroll area (inside it the available height is unbounded).
            let panel_h = ui.available_height();
            // Test-only: pin the scroll to the bottom so the render test
            // can screenshot the end of 高级设置 and the log. (An
            // explicit scroll_offset(1e6) does NOT clamp — egui applies
            // it raw and the content scrolls out of view entirely, so
            // stick_to_bottom is the right tool here.)
            let outer = egui::ScrollArea::vertical()
                .id_salt("central")
                .auto_shrink([false, false])
                .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysVisible)
                .stick_to_bottom(self.render_scroll_bottom);
            outer.show(ui, |ui| {
            if !self.font_ok {
                ui.label(
                    egui::RichText::new("未找到系统里的中文字体，中文可能显示为方框。")
                        .color(ui.visuals().warn_fg_color),
                );
            }
            // Toolbar: heading, source, refresh.
            ui.horizontal(|ui| {
                ui.heading("节点");
                ui.label(egui::RichText::new(&self.nodes_source).small());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let can_refresh = self.busy.is_none();
                    if ui
                        .add_enabled(
                            can_refresh,
                            egui::Button::new("刷新节点").min_size(egui::vec2(92.0, 32.0)),
                        )
                        .on_disabled_hover_text("正在忙，请稍候")
                        .clicked()
                    {
                        self.start_fetch(ui.ctx().clone());
                    }
                    if let Some(b) = self.busy {
                        ui.spinner();
                        if b == Busy::ProbingAll {
                            ui.label(format!(
                                "正在探测全部：{}/{}…",
                                self.probe_all_done, self.probe_all_total
                            ));
                        } else {
                            ui.label(b.label());
                        }
                    }
                });
            });
            ui.separator();

            // Node list (7 columns: a fixed 28 px status column with ∞
            // for the connected node, then country / ISP / 当前人数 /
            // 线路质量 / host:port / latency-status; host:port merged
            // so the grid fits a 560 px window — hostnames alone are
            // ~200 px wide).
            // Each row: (node index, status icon, country, isp,
            // sessions cell, quality cell, host:port shown, status,
            // host:port full). Rows are built in display order (see
            // display_indices): fetch order by default, or sorted by
            // the column the user clicked. `selected` still stores a
            // node index.
            let connected_host = self.connected_node_host();
            let rows: Vec<(
                usize,
                String,
                String,
                String,
                String,
                String,
                String,
                String,
                String,
            )> = self.display_indices()
                    .into_iter()
                    .map(|idx| {
                        let n = &self.nodes[idx];
                        let status = self
                            .probes
                            .get(&n.host)
                            .cloned()
                            .unwrap_or_else(|| "未探测".to_string());
                        let full = match n.port {
                            Some(p) => format!("{}:{}", n.host, p),
                            None => n.host.clone(),
                        };
                        let status_icon =
                            if connected_host.as_deref().is_some_and(|h| {
                                vpngate::complete_ddns_host(&n.host) == h
                            }) {
                                "∞".to_string()
                            } else {
                                String::new()
                            };
                        (
                            idx,
                            status_icon,
                            // Full text here: the cells truncate with "…"
                            // to the current column width (see below), so
                            // widening a column shows more instead of a
                            // fixed 12/14-character cut.
                            n.country.clone(),
                            if n.isp.is_empty() { n.name.clone() } else { n.isp.clone() },
                            sessions_cell(n),
                            quality_cell(n),
                            // All curated hosts end in .opengw.net; drop
                            // the suffix for display (full host is in
                            // the tooltip).
                            match n.port {
                                Some(p) => format!(
                                    "{}:{}",
                                    n.host.strip_suffix(".opengw.net").unwrap_or(&n.host),
                                    p
                                ),
                                None => n
                                    .host
                                    .strip_suffix(".opengw.net")
                                    .unwrap_or(&n.host)
                                    .to_string(),
                            },
                            status,
                            full,
                        )
                    })
                    .collect();
            let list_h = (panel_h - 250.0).max(120.0);
            // Recomputed in the row loop below; false when there is
            // no list or no selection (see the row loop).
            self.selected_row_in_view = false;
            if rows.is_empty() {
                ui.label("还没有节点。点右上角“刷新节点”，或在下面“高级设置”里手动添加。");
            } else {
                let list_out = egui::ScrollArea::both()
                    .id_salt("node_list")
                    .max_height(list_h)
                    .auto_shrink([false, false])
                    // Always show the scroll bars: with 28+ nodes the
                    // list overflows at 560x420, and a bar that only
                    // appears while scrolling is easy to miss entirely.
                    .scroll_bar_visibility(
                        egui::scroll_area::ScrollBarVisibility::AlwaysVisible,
                    )
                    .show(ui, |ui| {
                        // Tighter gaps so the columns fit a 560 px
                        // window at the default widths.
                        ui.spacing_mut().item_spacing = egui::vec2(6.0, 4.0);
                        // Status column (∞) is fixed 28 px and never
                        // resizable; the other six columns （国家/地
                        // 区、运营商、当前人数、线路质量、主机:端口、
                        // 延迟/状态） all come from `col_widths`
                        // (config `node_col_widths`, 6 elements).
                        const STATUS_COL_W: f32 = 28.0;
                        let widths: [f32; 7] = [
                            STATUS_COL_W,
                            self.col_widths[0] as f32,
                            self.col_widths[1] as f32,
                            self.col_widths[2] as f32,
                            self.col_widths[3] as f32,
                            self.col_widths[4] as f32,
                            self.col_widths[5] as f32,
                        ];
                        // 30 px rows: 22 px was easy to miss, and a
                        // click that drifts a few pixels is lost to the
                        // scroll drag (see the tolerant-click note in
                        // the row loop below).
                        let row_h = 30.0;
                        // One row is 7 cells + 6 six-pixel separators +
                        // the 12 gaps between the 13 items. The status
                        // column has no separator (its gap is the same
                        // width, so the columns stay aligned).
                        let handle_w = 6.0f32;
                        let gap = ui.spacing().item_spacing.x;
                        let total_w =
                            widths.iter().sum::<f32>() + 6.0 * handle_w + 12.0 * gap;
                        // Rows span the full available width, not just
                        // the columns: at 800 px the old 554 px rows
                        // left ~200 px of dead space on the right of
                        // every row, and clicks there selected nothing.
                        let row_w = total_w.max(ui.available_width());
                        // Manual click tolerance: record the press
                        // position (frame-level) so the row loop can
                        // accept a press-and-release on the same row
                        // within 12 px — egui's own click needs under
                        // 6 px of movement (max_click_dist), and a real
                        // mouse easily drifts past that.
                        let (pressed, released, ptr_pos) = ui.input(|i| {
                            (
                                i.pointer.any_pressed(),
                                i.pointer.any_released(),
                                i.pointer.interact_pos(),
                            )
                        });
                        // Modifier keys for the row clicks below:
                        // Ctrl/Cmd+click toggles a row in the
                        // multi-select set, Shift+click selects a
                        // range from the anchor. `command` is Cmd on
                        // macOS and Ctrl elsewhere, so Ctrl+click works
                        // on both. The press-time modifiers are latched
                        // next to the press position (see
                        // `row_press_mods`): the click is completed on
                        // the release frame, and reading the modifiers
                        // only there loses the gesture when the user
                        // lets go of Shift/Ctrl a beat before the mouse
                        // button (the v3.7 Shift+click failure).
                        let click_mods = ui.input(|i| i.modifiers);
                        if pressed {
                            self.row_press_pos = ptr_pos;
                            self.row_press_mods = Some(click_mods);
                        }
                        let (click_ctrl, click_shift) = Self::effective_click_mods(
                            self.row_press_mods,
                            click_mods,
                        );
                        if pressed {
                            self.row_press_pos = ptr_pos;
                        }
                        // Header row: labels plus a draggable separator
                        // after each resizable column but the last.
                        // Dragging resizes that column (40–400 px); the
                        // width is remembered in the settings and
                        // auto-saved to config.toml. The status column
                        // (index 0) has an empty header and no
                        // separator; the country （当前人数 / 线路质量 /
                        // 延迟/状态） columns (indices 1, 3, 4, 6) are
                        // clickable to sort (see below).
                        // NOTE: the cells are placed at exact rects (like
                        // the data rows below), not via
                        // allocate_ui_with_layout: that advances the parent
                        // by the child's *content* size, so a short label
                        // would pull the separator and the next column left
                        // of its intended position, and the header would
                        // drift out of line with the data columns.
                        // Handles get explicit stable ids (not auto ids).
                        let (header_rect, _) = ui.allocate_exact_size(
                            egui::vec2(row_w, row_h),
                            egui::Sense::hover(),
                        );
                        // The arrow sits on the one column currently
                        // sorted (see `sort`); the other sortable
                        // headers show their bare label.
                        let sort_arrow = |col: SortCol| match self.sort {
                            Some((c, dir)) if c == col => format!(" {}", dir.arrow()),
                            _ => String::new(),
                        };
                        let country_label = format!("国家/地区{}", sort_arrow(SortCol::Country));
                        let sessions_label =
                            format!("当前人数{}", sort_arrow(SortCol::Sessions));
                        let quality_label =
                            format!("线路质量{}", sort_arrow(SortCol::Quality));
                        let latency_label =
                            format!("延迟/状态{}", sort_arrow(SortCol::Latency));
                        let header_labels = [
                            "",
                            country_label.as_str(),
                            "运营商",
                            sessions_label.as_str(),
                            quality_label.as_str(),
                            "主机:端口",
                            latency_label.as_str(),
                        ];
                        let mut x = header_rect.left();
                        for (i, label) in header_labels.iter().enumerate() {
                            let cell_rect = egui::Rect::from_min_size(
                                egui::pos2(x, header_rect.top()),
                                egui::vec2(widths[i], row_h),
                            );
                            put_cell_label(
                                ui,
                                cell_rect,
                                egui::RichText::new(*label).strong(),
                            );
                            // Sortable headers: country (1), 当前人数
                            // (3), 线路质量 (4), 延迟/状态 (6). The
                            // label is drawn first and the interact
                            // sits on top of it (a Label has no sense,
                            // so it cannot eat the click). Clicking a
                            // header sorts by that column (first click
                            // = that column's most useful order);
                            // clicking it again reverses.
                            let sort_col = match i {
                                1 => Some(SortCol::Country),
                                3 => Some(SortCol::Sessions),
                                4 => Some(SortCol::Quality),
                                6 => Some(SortCol::Latency),
                                _ => None,
                            };
                            if let Some(col) = sort_col {
                                let tip = match col {
                                    SortCol::Country => {
                                        "点击按国家排序（A-Z / Z-A 切换）"
                                    }
                                    SortCol::Sessions => "点击按连接人数排序",
                                    SortCol::Quality => {
                                        "点击按线路质量排序（速度优先）"
                                    }
                                    SortCol::Latency => {
                                        "点击按延迟排序：已测的按毫秒从小到大，未测的居中，失败的最后"
                                    }
                                };
                                let resp = ui
                                    .interact(
                                        cell_rect,
                                        egui::Id::new(("sort_col", i)),
                                        egui::Sense::click(),
                                    )
                                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                                    .on_hover_text(tip);
                                if resp.clicked() {
                                    self.toggle_sort(col);
                                }
                            }
                            x += widths[i];
                            if i < 6 {
                                x += gap;
                                // Separator after a resizable column
                                // only, and it resizes that column:
                                // country (1) -> col_widths[0], ISP
                                // (2) -> col_widths[1], 当前人数
                                // (3) -> col_widths[2], 线路质量
                                // (4) -> col_widths[3], host:port
                                // (5) -> col_widths[4]. The status
                                // column keeps the same gap but no
                                // handle, and the last column
                                // (延迟/状态， 6) has none — so it is
                                // the one resizable column that cannot
                                // be dragged (its width is still
                                // stored in col_widths[5]).
                                let sep_col = match i {
                                    1 => Some(0usize),
                                    2 => Some(1usize),
                                    3 => Some(2usize),
                                    4 => Some(3usize),
                                    5 => Some(4usize),
                                    _ => None,
                                };
                                if let Some(wi) = sep_col {
                                    let handle_rect = egui::Rect::from_min_size(
                                        egui::pos2(x, header_rect.top()),
                                        egui::vec2(handle_w, row_h),
                                    );
                                    let resp = ui.interact(
                                        handle_rect,
                                        egui::Id::new(("node_col_sep", wi)),
                                        egui::Sense::drag(),
                                    );
                                    let active = resp.hovered() || resp.dragged();
                                    // Apply the per-frame delta while
                                    // dragging, and also on release: a
                                    // drag whose pointer only moved in the
                                    // release event still carries its delta
                                    // then (kittest teleports this way).
                                    if resp.dragged() || resp.drag_stopped() {
                                        self.col_widths[wi] =
                                            (self.col_widths[wi] as f32
                                                + resp.drag_delta().x)
                                                .clamp(40.0, 400.0)
                                                as u16;
                                    }
                                    let stroke = if active {
                                        ui.visuals().widgets.active.bg_stroke
                                    } else {
                                        ui.visuals().widgets.noninteractive.bg_stroke
                                    };
                                    ui.painter().vline(
                                        handle_rect.center().x,
                                        handle_rect.y_range(),
                                        stroke,
                                    );
                                    resp.on_hover_cursor(egui::CursorIcon::ResizeColumn);
                                }
                                x += handle_w + gap;
                            }
                        }
                        // Data rows: whole-row click selects; text
                        // truncates with "…" to the current column width,
                        // and widening a column shows more of it (the
                        // horizontal scroll bar reveals the rest).
                        // The row rect spans the full available width
                        // (row_w, not just total_w) so clicks on the
                        // empty right part of a row still select it.
                        for (
                            display_i,
                            (node_idx, status_icon, country, isp, sessions, quality, hostport, status, full),
                        ) in rows.iter().enumerate()
                        {
                            let (row_rect, row_resp) = ui.allocate_exact_size(
                                egui::vec2(row_w, row_h),
                                egui::Sense::click(),
                            );
                            // Highlight follows the multi-select set
                            // (see `click_row`): every row in the set
                            // gets the indigo fill and white text.
                            // The arrow-key follow-scroll and the
                            // visibility bookkeeping below still track
                            // the anchor row only (`self.selected`).
                            let selected = self.selected_set.contains(node_idx);
                            let is_anchor = self.selected == Some(*node_idx);
                            // Arrow-key follow-scroll: a one-shot
                            // request from move_selection. Align None
                            // scrolls only as far as needed to bring
                            // the row into view (already-visible rows
                            // don't move the list). The target is
                            // consumed by this (innermost) ScrollArea.
                            if self.scroll_to_selected && is_anchor {
                                ui.scroll_to_rect(row_rect, None);
                                self.scroll_to_selected = false;
                            }
                            // Visibility bookkeeping for the arrow-key
                            // test: the clip rect is this list's
                            // viewport. Vertical only — rows span the
                            // full width and can run past the clip
                            // rect horizontally (the list scrolls
                            // sideways at 560 px), which says nothing
                            // about whether the row is on screen.
                            // Recomputed every frame (the scroll
                            // target above only takes effect after
                            // this frame's show).
                            if is_anchor {
                                let clip = ui.clip_rect();
                                self.selected_row_in_view = row_rect.top() >= clip.top()
                                    && row_rect.bottom() <= clip.bottom();
                            }
                            let fill = if selected {
                                ui.visuals().selection.bg_fill
                            } else if row_resp.hovered() {
                                ui.visuals().widgets.hovered.bg_fill
                            } else if display_i % 2 == 0 {
                                ui.visuals().faint_bg_color
                            } else {
                                egui::Color32::TRANSPARENT
                            };
                            ui.painter().rect_filled(row_rect, 0.0, fill);
                            // Selected-row text goes white (the indigo
                            // fill would swallow the default ink text).
                            let sel_text = ui.visuals().selection.stroke.color;
                            let cell_text = |t: &str| {
                                if selected {
                                    egui::RichText::new(t).color(sel_text)
                                } else {
                                    egui::RichText::new(t)
                                }
                            };
                            // Place each cell label at its exact rect
                            // (egui 0.36 has no allocate_ui_at_rect).
                            let cells =
                                [status_icon, country, isp, sessions, quality, hostport, status];
                            let mut x = row_rect.left();
                            for (j, text) in cells.iter().enumerate() {
                                let cell_rect = egui::Rect::from_min_size(
                                    egui::pos2(x, row_rect.top()),
                                    egui::vec2(widths[j], row_h),
                                );
                                if j == 0 {
                                    // Status column: the ∞ mark for the
                                    // connected node, centred in the
                                    // 28 px cell, slightly larger than
                                    // body text so the glyph reads at
                                    // 30 px row height. (The glyph's
                                    // rendering is verified by the
                                    // render_infinity_glyph test — the
                                    // system CJK font must carry U+221E.)
                                    let accent =
                                        infinity_accent(ui.visuals().dark_mode);
                                    let color = if selected { sel_text } else { accent };
                                    ui.put(
                                        cell_rect,
                                        egui::Label::new(
                                            egui::RichText::new(text.as_str())
                                                .size(17.0)
                                                .color(color),
                                        )
                                        .halign(egui::Align::Center),
                                    );
                                } else {
                                    put_cell_label(ui, cell_rect, cell_text(text));
                                }
                                x += widths[j];
                                if j < 6 {
                                    // Gap, separator, gap — the same
                                    // symmetric spacing as the header row.
                                    x += gap + handle_w + gap;
                                }
                            }
                            // Full host:port on hover (the column shows it
                            // without the .opengw.net suffix).
                            let row_resp = row_resp
                                .on_hover_text(full)
                                .on_hover_cursor(egui::CursorIcon::PointingHand);
                            // Click selection (see `click_row`): plain
                            // click selects only this row, Ctrl/Cmd
                            // toggles it in the multi-select set,
                            // Shift selects a range from the anchor.
                            // The two paths below are exclusive: egui's
                            // own click and the tolerant click below
                            // both fire on the release frame for a
                            // press that barely moved, and calling
                            // `click_row` twice would undo the gesture
                            // (a Shift range would collapse back to the
                            // clicked row, a Ctrl toggle would toggle
                            // back out). `clicked()` wins when it fires;
                            // the tolerant path covers the 6–12 px drift
                            // egui's click misses.
                            let clicked = row_resp.clicked();
                            if clicked {
                                self.click_row(*node_idx, click_ctrl, click_shift);
                            } else if released {
                                if let (Some(origin), Some(pos)) =
                                    (self.row_press_pos, ptr_pos)
                                {
                                    if origin.distance(pos) < 12.0
                                        && row_rect.contains(origin)
                                        && row_rect.contains(pos)
                                    {
                                        self.click_row(*node_idx, click_ctrl, click_shift);
                                    }
                                }
                            }
                        }
                        if released {
                            self.row_press_pos = None;
                            self.row_press_mods = None;
                        }
                    });
                // The list's current scroll offset (the arrow-key
                // test reads it to prove the list actually scrolled).
                self.node_list_scroll_y = list_out.state.offset.y;
            }

            // Primary action row (roomy buttons: 96x34).
            ui.horizontal(|ui| {
                let connected = self.conn.is_some();
                let can_act = self.busy.is_none() && self.selected.is_some();
                let btn_size = egui::vec2(96.0, 34.0);
                if connected {
                    if ui
                        .add(egui::Button::new("断开连接").min_size(btn_size))
                        .clicked()
                    {
                        self.disconnect();
                    }
                } else {
                    let selection = ui.visuals().selection;
                    let connect = egui::Button::new(
                        egui::RichText::new("连接").color(selection.stroke.color),
                    )
                    .fill(selection.bg_fill)
                    .min_size(btn_size);
                    if ui
                        .add_enabled(can_act, connect)
                        .on_hover_text("连接选中节点（回车）")
                        .on_disabled_hover_text(if self.selected.is_none() {
                            "请先在上面选择一个节点"
                        } else {
                            "正在忙，请稍候"
                        })
                        .clicked()
                    {
                        self.start_connect(ui.ctx().clone());
                    }
                }
                if ui
                    .add_enabled(can_act, egui::Button::new("探测").min_size(btn_size))
                    .on_hover_text("探测选中节点（Ctrl+R）")
                    .on_disabled_hover_text(if self.selected.is_none() {
                        "请先在上面选择一个节点"
                    } else {
                        "正在忙，请稍候"
                    })
                    .clicked()
                {
                    self.start_probe(ui.ctx().clone());
                }
                // Probe-all: sweeps the whole list in display order,
                // one node at a time. While a sweep runs the same
                // button becomes 停止 (the sweep stops after the
                // node in flight; results already out are kept).
                // No selection needed — it probes everything.
                if self.busy == Some(Busy::ProbingAll) {
                    let stop_requested = self.probe_all_stop.load(Ordering::Relaxed);
                    if ui
                        .add_enabled(
                            !stop_requested,
                            egui::Button::new("停止").min_size(btn_size),
                        )
                        .on_hover_text("在当前节点探测结束后停下，已出的结果保留")
                        .on_disabled_hover_text("正在停止，等当前节点测完…")
                        .clicked()
                    {
                        self.stop_probe_all();
                    }
                } else {
                    let can_probe_all = self.busy.is_none() && !self.nodes.is_empty();
                    if ui
                        .add_enabled(
                            can_probe_all,
                            egui::Button::new("探测全部").min_size(btn_size),
                        )
                        .on_hover_text("按当前显示顺序，一个一个探测全部节点")
                        .on_disabled_hover_text(if self.nodes.is_empty() {
                            "列表里还没有节点"
                        } else {
                            "正在忙，请稍候"
                        })
                        .clicked()
                    {
                        self.start_probe_all(ui.ctx().clone());
                    }
                }
                if ui
                    .add_enabled(
                        connected && self.busy.is_none(),
                        egui::Button::new("测试隧道").min_size(btn_size),
                    )
                    .on_disabled_hover_text("连接后才能测试隧道")
                    .clicked()
                {
                    self.start_test(ui.ctx().clone());
                }
                match self.selected_node_label() {
                    Some(label) => ui.label(format!("已选：{}", label)),
                    None => ui.label("未选择节点"),
                };
            });
            // Batch delete: removes the multi-selected rows from the
            // list and records their hostnames on the persistent
            // blocklist, so a refresh never brings them back. Busy
            // jobs (probe / connect / refresh / probe-all) disable
            // it — same guard as 刷新节点. The restore button only
            // shows while the blocklist is non-empty.
            ui.horizontal(|ui| {
                let btn_size = egui::vec2(96.0, 34.0);
                let n_sel = self.selected_set.len();
                let delete_label = if n_sel > 0 {
                    format!("删除选中 ({})", n_sel)
                } else {
                    "删除选中".to_string()
                };
                if ui
                    .add_enabled(
                        self.can_delete(),
                        egui::Button::new(delete_label).min_size(btn_size),
                    )
                    .on_hover_text("删除选中的节点，并记入屏蔽列表（刷新后也不会回来）")
                    .on_disabled_hover_text(if n_sel == 0 {
                        "请先选择要删除的节点（单击选一行，Ctrl+单击多选）"
                    } else {
                        "正在忙，请稍候"
                    })
                    .clicked()
                {
                    self.delete_selected();
                }
                if !self.blocked_hosts.is_empty() {
                    let n_blocked = self.blocked_hosts.len();
                    if ui
                        .add(
                            egui::Button::new(format!("恢复已删除 ({})", n_blocked))
                                .min_size(btn_size),
                        )
                        .on_hover_text("清除屏蔽记录，并把本会话里删掉的节点放回列表")
                        .clicked()
                    {
                        self.restore_deleted();
                    }
                }
                ui.label("Ctrl+单击多选，Shift+单击选一个范围。");
            });
            // 常用开关和端口放主面板上，不用展开高级设置也能看到；改动会自动保存。
            // 本地端口只在连接时生效，连接中锁住；局域网分享用同一个
            // 混合端口（SOCKS5+HTTP），不再另开端口。
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.allow_lan, "允许局域网分享（混合端口同时监听 0.0.0.0）")
                    .on_hover_text("开启后，局域网里的其他设备也能连这个本地混合代理端口——同一个端口，不用再多占一个");
                ui.checkbox(&mut self.system_proxy, "设置系统代理（断开/退出时还原）")
                    .on_hover_text("开启后，系统代理会指到本地端口；关闭或退出时自动还原。");
            });
            ui.horizontal(|ui| {
                let connected = self.conn.is_some();
                ui.label("本地端口：");
                ui.add_enabled(
                    !connected,
                    egui::DragValue::new(&mut self.local_port).range(1..=65535),
                )
                .on_disabled_hover_text("连接中，断开后才能改端口");
                if connected {
                    ui.label("连接中，断开后才能改。");
                } else if self.allow_lan {
                    ui.label(format!(
                        "局域网设备连本机 IP:{}（同一混合端口）。",
                        self.local_port
                    ));
                } else {
                    ui.label("（勾选局域网分享后，局域网设备连本机 IP 用同一端口）");
                }
            });
            ui.separator();

            // Advanced settings.
            egui::CollapsingHeader::new("高级设置")
                .default_open(self.advanced_open)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("手动添加节点：");
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut self.manual_host)
                                .hint_text("主机名:端口，例如 kanratown.opengw.net:992")
                                .desired_width(280.0),
                        );
                        if ui.button("添加").clicked() || resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                            match parse_manual_target(&self.manual_host, vpngate::curated_nodes().len()) {
                                Ok(target) => {
                                    self.manual_error.clear();
                                    let (host, port) = match target.rsplit_once(':') {
                                        Some((h, p)) => (h.to_string(), p.parse::<u16>().ok()),
                                        None => (target.clone(), None),
                                    };
                                    let node = Node {
                                        host,
                                        port,
                                        country: "手动".to_string(),
                                        country_code: String::new(),
                                        isp: "手动添加".to_string(),
                                        name: target.clone(),
                                        score: 0,
                                        ping_ms: 0,
                                        speed_mbps: 0,
                                        sessions: 0,
                                        ip: String::new(),
                                        backups: Vec::new(),
                                        port_source: "manual".to_string(),
                                        source: "manual".to_string(),
                                    };
                                    self.nodes.insert(0, node);
                                    // The insert shifts every old index
                                    // by one, so the old multi-select
                                    // would point at the wrong rows:
                                    // replace it with just the new row,
                                    // like the anchor.
                                    self.selected = Some(0);
                                    self.selected_set = BTreeSet::from([0]);
                                    self.log_line(format!("手动添加节点：{}。", target));
                                    self.manual_host.clear();
                                }
                                Err(e) => {
                                    self.log_warn(format!("手动添加节点失败：{}。", e));
                                    self.manual_error = e;
                                }
                            }
                        }
                    });
                    if !self.manual_error.is_empty() {
                        ui.label(
                            egui::RichText::new(&self.manual_error)
                                .color(ui.visuals().error_fg_color),
                        );
                    }
                    ui.separator();
                    ui.checkbox(&mut self.front_enabled, "使用前置 SOCKS5 代理（连节点前先走它）");
                    ui.add_enabled_ui(self.front_enabled, |ui| {
                        egui::Grid::new("front_proxy_grid")
                            .num_columns(2)
                            .show(ui, |ui| {
                                ui.label("代理地址：");
                                ui.text_edit_singleline(&mut self.front_addr);
                                ui.end_row();
                                ui.label("用户名：");
                                ui.text_edit_singleline(&mut self.front_user);
                                ui.end_row();
                                ui.label("密码：");
                                ui.add(egui::TextEdit::singleline(&mut self.front_pass).password(true));
                                ui.end_row();
                            });
                    });
                    // Test the front proxy itself: with it off there is
                    // nothing to test, so the button stays disabled with
                    // a hint instead of running a misleading "direct" test.
                    ui.horizontal(|ui| {
                        let can_test = self.front_enabled
                            && !self.front_addr.trim().is_empty()
                            && self.busy.is_none();
                        if ui
                            .add_enabled(
                                can_test,
                                egui::Button::new("测试前置代理").min_size(egui::vec2(96.0, 32.0)),
                            )
                            .on_disabled_hover_text(if !self.front_enabled {
                                "先勾选“使用前置 SOCKS5 代理”，这个测试需要前置代理开着"
                            } else if self.front_addr.trim().is_empty() {
                                "先填代理地址"
                            } else {
                                "正在忙，请稍候"
                            })
                            .clicked()
                        {
                            self.start_test_front_proxy(ui.ctx().clone());
                        }
                        ui.label("测代理到 Google、vpngate.net（走 SOCKS5）。");
                    });
                    ui.separator();
                    ui.separator();
                    // Portable save: settings go to config.toml next to
                    // the exe, so the folder stays self-contained.
                    ui.horizontal(|ui| {
                        if ui
                            .add(
                                egui::Button::new("保存设置").min_size(egui::vec2(96.0, 32.0)),
                            )
                            .on_hover_text("设置改动会自动保存到程序目录下的 config.toml；点一下是手动再存一次")
                            .clicked()
                        {
                            self.save_settings();
                        }
                        match &self.config_path {
                            Some(p) => ui.label(
                                egui::RichText::new(format!("配置文件：{}", p.display())).small(),
                            ),
                            None => ui.label(
                                egui::RichText::new("配置文件：未保存（当前是默认设置）").small(),
                            ),
                        };
                    });
                    // Log file location: next to the exe, same folder as
                    // config.toml. Shown here so Leo can find it when
                    // reporting a problem (startup also logs the path).
                    match &self.log_path {
                        Some(p) => ui.label(
                            egui::RichText::new(format!("日志文件：{}", p.display())).small(),
                        ),
                        None => ui.label(
                            egui::RichText::new("日志文件：未开启").small(),
                        ),
                    };
                });
            ui.separator();

            // Log.
            ui.label("日志");
            let log_text = self.log.join("\n");
            egui::ScrollArea::vertical()
                .id_salt("log")
                .max_height(90.0)
                .auto_shrink([false, false])
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    ui.add(
                        egui::TextEdit::multiline(&mut log_text.as_str())
                            .font(egui::TextStyle::Monospace)
                            .desired_width(f32::INFINITY),
                    );
                });
            });
        });

        // Settings auto-save: any change made this frame (front proxy,
        // LAN sharing, system proxy, local port) is persisted now, so
        // closing without clicking 保存设置 loses nothing.
        self.auto_save_if_dirty();

        if self.show_about {
            egui::Window::new("关于 VPNGate 客户端")
                .collapsible(false)
                .resizable(false)
                .show(ui.ctx(), |ui| {
                    ui.label(format!(
                        "VPNGate 客户端 {}（Rust / egui）",
                        env!("CARGO_PKG_VERSION")
                    ));
                    ui.label("连 VPNGate 志愿者节点（MS-SSTP + PPP），本地开一个 SOCKS5 端口。");
                    ui.label("没有虚拟网卡，不改系统路由；系统代理默认关闭。");
                    ui.label("节点是陌生人家的宽带，随时可能下线，仅供试用。");
                    if ui.button("关闭").clicked() {
                        self.show_about = false;
                    }
                });
        }
    }
}

fn main() -> eframe::Result {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let _guard = rt.enter(); // lets tokio::spawn find the runtime
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("VPNGate 客户端")
            .with_inner_size([800.0, 600.0])
            .with_min_inner_size([560.0, 420.0]),
        ..Default::default()
    };
    eframe::run_native(
        "vpngate-gui",
        options,
        Box::new(|cc| Ok(Box::new(GuiApp::new(cc)))),
    )
}

// ---------------------------------------------------------------------------
// Tests (headless; no window, no GPU)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn node(host: &str) -> Node {
        Node {
            host: host.to_string(),
            port: Some(992),
            country: "日本".to_string(),
            country_code: "JP".to_string(),
            isp: "NTT".to_string(),
            name: host.to_string(),
            score: 100,
            ping_ms: 10,
            speed_mbps: 50,
            sessions: 1,
            ip: "1.2.3.4".to_string(),
            backups: vec![],
            port_source: "curated".to_string(),
            source: "curated".to_string(),
        }
    }

    #[test]
    fn manual_target_parsing() {
        assert_eq!(
            parse_manual_target("kanratown.opengw.net:992", 28).unwrap(),
            "kanratown.opengw.net:992"
        );
        assert_eq!(
            parse_manual_target("  vpn1.opengw.net:443  ", 28).unwrap(),
            "vpn1.opengw.net:443"
        );
        assert_eq!(parse_manual_target("3", 28).unwrap(), "3");
        assert_eq!(
            parse_manual_target("kanratown.opengw.net", 28).unwrap(),
            "kanratown.opengw.net"
        );
        assert!(parse_manual_target("", 28).is_err());
        assert!(parse_manual_target("host:0", 28).is_err());
        assert!(parse_manual_target("host:70000", 28).is_err());
        assert!(parse_manual_target("host:notaport", 28).is_err());
        assert!(parse_manual_target("99", 28).is_err());
    }

    fn node_in(host: &str, country: &str) -> Node {
        let mut n = node(host);
        n.country = country.to_string();
        n
    }

    /// Plain-click a row by its cell label in a kittest harness.
    fn click_row_label(harness: &mut egui_kittest::Harness<'_, GuiApp>, label: &str) {
        use egui_kittest::kittest::Queryable;
        let cell = harness.get_by_label(label);
        let center = cell.rect().center();
        harness.drag_at(center);
        harness.run();
        harness.drop_at(center);
        harness.run();
    }

    /// Shift+click a row by its cell label in a kittest harness:
    /// ModifiersChanged(shift) before the press and again before the
    /// release (egui only tracks modifiers from ModifiersChanged
    /// events; the row code latches them at press time).
    fn shift_click_row_label(
        harness: &mut egui_kittest::Harness<'_, GuiApp>,
        label: &str,
    ) {
        use egui_kittest::kittest::Queryable;
        let shift = egui::Modifiers {
            shift: true,
            ..Default::default()
        };
        let cell = harness.get_by_label(label);
        let center = cell.rect().center();
        harness.event(egui::Event::ModifiersChanged(shift));
        harness.drag_at(center);
        harness.run();
        harness.event(egui::Event::ModifiersChanged(shift));
        harness.drop_at(center);
        harness.run();
        harness.event(egui::Event::ModifiersChanged(egui::Modifiers::default()));
        harness.run();
    }

    /// Shift+click a row by its cell label, releasing Shift before
    /// the mouse button — the exact gesture Leo reported as broken
    /// (the modifiers must be latched at press time, or the range
    /// is lost and delete has nothing to remove).
    fn shift_click_row_label_early_release(
        harness: &mut egui_kittest::Harness<'_, GuiApp>,
        label: &str,
    ) {
        use egui_kittest::kittest::Queryable;
        let shift = egui::Modifiers {
            shift: true,
            ..Default::default()
        };
        let cell = harness.get_by_label(label);
        let center = cell.rect().center();
        harness.event(egui::Event::ModifiersChanged(shift));
        harness.drag_at(center);
        harness.run();
        harness.event(egui::Event::ModifiersChanged(egui::Modifiers::default()));
        harness.drop_at(center);
        harness.run();
    }

    /// Click a button by its label in a kittest harness.
    fn click_button_label(harness: &mut egui_kittest::Harness<'_, GuiApp>, label: &str) {
        use egui_kittest::kittest::Queryable;
        let btn = harness.get_by_label(label);
        let center = btn.rect().center();
        harness.drag_at(center);
        harness.run();
        harness.drop_at(center);
        harness.run();
    }

    /// Node with explicit listed stats (sessions / speed / ping).
    /// All three zero = no data (the curated 28); any nonzero =
    /// has data.
    fn node_stats(host: &str, country: &str, sessions: u64, speed_mbps: u64, ping_ms: u64) -> Node {
        let mut n = node_in(host, country);
        n.sessions = sessions;
        n.speed_mbps = speed_mbps;
        n.ping_ms = ping_ms;
        n
    }

    #[test]
    fn country_sort_orders_by_country_then_host() {
        let mut app = GuiApp::for_tests();
        app.nodes = vec![
            node_in("b.opengw.net", "美国"),
            node_in("a.opengw.net", "日本"),
            node_in("c.opengw.net", "日本"),
            node_in("d.opengw.net", "德国"),
        ];
        // Default: fetch order, no sorting.
        assert_eq!(app.display_indices(), vec![0, 1, 2, 3]);
        app.sort = Some((SortCol::Country, SortDir::Asc));
        // Chinese names sort by string (Unicode code-point) order:
        // 德国 U+5FB7 < 日本 U+65E5 < 美国 U+7F8E; same-country rows
        // break ties by hostname (a before c).
        assert_eq!(app.display_indices(), vec![3, 1, 2, 0]);
        app.sort = Some((SortCol::Country, SortDir::Desc));
        assert_eq!(app.display_indices(), vec![0, 2, 1, 3]);
    }

    #[test]
    fn country_sort_survives_refresh() {
        let mut app = GuiApp::for_tests();
        app.nodes = vec![
            node_in("b.opengw.net", "美国"),
            node_in("a.opengw.net", "日本"),
        ];
        app.sort = Some((SortCol::Country, SortDir::Asc));
        assert_eq!(app.display_indices(), vec![1, 0]);
        app.apply_event(GuiEvent::Nodes(vpngate::NodeListUpdate {
            nodes: vec![
                node_in("z.opengw.net", "美国"),
                node_in("y.opengw.net", "德国"),
            ],
            source: vpngate::NodeListSource::Api,
            cache_age_secs: None,
            api_error: None,
            web_error: None,
        }));
        // New list, same chosen direction (德国 < 美国).
        assert_eq!(app.sort, Some((SortCol::Country, SortDir::Asc)));
        assert_eq!(app.display_indices(), vec![1, 0]);
    }

    #[test]
    fn sessions_sort_orders_few_first_then_many() {
        let mut app = GuiApp::for_tests();
        app.nodes = vec![
            node_stats("b.opengw.net", "日本", 47, 500, 10),
            node_stats("a.opengw.net", "日本", 0, 0, 0),
            node_stats("c.opengw.net", "日本", 301, 100, 5),
            node_stats("d.opengw.net", "日本", 5, 200, 20),
        ];
        assert_eq!(app.display_indices(), vec![0, 1, 2, 3]);
        app.sort = Some((SortCol::Sessions, SortDir::Asc));
        // Fewest first: 0, 5, 47, 301 (a no-data node has 0 sessions).
        assert_eq!(app.display_indices(), vec![1, 3, 0, 2]);
        app.sort = Some((SortCol::Sessions, SortDir::Desc));
        assert_eq!(app.display_indices(), vec![2, 0, 3, 1]);
    }

    #[test]
    fn quality_sort_orders_by_speed_then_ping_with_no_data_at_the_ends() {
        let mut app = GuiApp::for_tests();
        app.nodes = vec![
            node_stats("a.opengw.net", "日本", 10, 500, 30),
            node_stats("b.opengw.net", "日本", 10, 500, 10),
            node_stats("c.opengw.net", "日本", 10, 100, 5),
            node_stats("d.opengw.net", "日本", 0, 0, 0),
        ];
        app.sort = Some((SortCol::Quality, SortDir::Desc));
        // First click: fastest first; same speed -> smaller ping
        // first; the no-data node last.
        assert_eq!(app.display_indices(), vec![1, 0, 2, 3]);
        app.sort = Some((SortCol::Quality, SortDir::Asc));
        // Reversed: no-data first, slowest first; same speed ->
        // larger ping first.
        assert_eq!(app.display_indices(), vec![3, 2, 0, 1]);
    }

    #[test]
    fn api_speed_bps_converts_to_mbps_in_quality_cell() {
        // Leo's v3.6 report: 线路质量 showed "77570260M" — the API
        // Speed column is bits per second, not Mbps. Parse the
        // real-magnitude row and check the cell the GUI shows.
        let csv = "# comment\n* vpn_servers\nHostName,IP,Score,Ping,Speed,CountryLong,CountryShort,NumVpnSessions,Uptime,TotalUsers,TotalTraffic,LogType,Operator,Message,OpenVPN_ConfigData_Base64\nvpn267947139.opengw.net,1.2.3.4,1000000,12,77570260,Japan,JP,47,1000,2000,3000,0,SoftBank BB,,AAAA\n";
        let nodes = vpngate::parse_api_csv(csv);
        assert_eq!(nodes.len(), 1);
        assert_eq!(quality_cell(&nodes[0]), "78M@12ms");
        assert_eq!(sessions_cell(&nodes[0]), "47人");
    }

    #[test]
    fn quality_sort_compares_api_and_scrape_speeds_like_for_like() {
        // Before the v3.7 fix, API nodes stored raw bps in
        // speed_mbps, so an API node at 77.6 Mbps sorted as
        // 77,570,260 and always beat scrape nodes (whole Mbps) in
        // the quality sort. After the fix both are Mbps.
        let csv = "# comment\n* vpn_servers\nHostName,IP,Score,Ping,Speed,CountryLong,CountryShort,NumVpnSessions,Uptime,TotalUsers,TotalTraffic,LogType,Operator,Message,OpenVPN_ConfigData_Base64\nvpn267947139.opengw.net,1.2.3.4,1000000,12,77570260,Japan,JP,47,1000,2000,3000,0,SoftBank BB,,AAAA\n";
        let api = vpngate::parse_api_csv(csv).into_iter().next().unwrap();
        assert_eq!(api.speed_mbps, 78);
        // Scrape node: "261.83 Mbps" from the page → 262 Mbps.
        let scrape = node_stats("s.opengw.net", "日本", 70, 262, 8);
        let mut app = GuiApp::for_tests();
        app.nodes = vec![api, scrape];
        app.sort = Some((SortCol::Quality, SortDir::Desc));
        // Fastest first: 262 Mbps scrape node before 78 Mbps API node.
        assert_eq!(app.display_indices(), vec![1, 0]);
        app.sort = Some((SortCol::Quality, SortDir::Asc));
        assert_eq!(app.display_indices(), vec![0, 1]);
    }

    #[test]
    fn toggle_sort_resets_direction_when_switching_columns() {
        let mut app = GuiApp::for_tests();
        app.sort = Some((SortCol::Country, SortDir::Desc));
        app.toggle_sort(SortCol::Sessions);
        assert_eq!(
            app.sort,
            Some((SortCol::Sessions, SortDir::Asc)),
            "switching to sessions starts few-first"
        );
        app.toggle_sort(SortCol::Sessions);
        assert_eq!(
            app.sort,
            Some((SortCol::Sessions, SortDir::Desc)),
            "same column again reverses"
        );
        app.toggle_sort(SortCol::Quality);
        assert_eq!(
            app.sort,
            Some((SortCol::Quality, SortDir::Desc)),
            "switching to quality starts fastest-first"
        );
        app.toggle_sort(SortCol::Latency);
        assert_eq!(
            app.sort,
            Some((SortCol::Latency, SortDir::Asc)),
            "switching to latency starts available-first by ms"
        );
        app.toggle_sort(SortCol::Country);
        assert_eq!(
            app.sort,
            Some((SortCol::Country, SortDir::Asc)),
            "switching back to country starts A-Z"
        );
    }

    #[test]
    fn sessions_and_quality_cells_format() {
        // API node with data: "47人" / "572M@3ms".
        let n = node_stats("a.opengw.net", "日本", 47, 572, 3);
        assert_eq!(sessions_cell(&n), "47人");
        assert_eq!(quality_cell(&n), "572M@3ms");
        // Curated node: all three zero -> "—" in both cells.
        let n = node_stats("b.opengw.net", "日本", 0, 0, 0);
        assert_eq!(sessions_cell(&n), "—");
        assert_eq!(quality_cell(&n), "—");
        // Data but zero sessions: "0人", not "—".
        let n = node_stats("c.opengw.net", "日本", 0, 100, 5);
        assert_eq!(sessions_cell(&n), "0人");
        assert_eq!(quality_cell(&n), "100M@5ms");
    }

    #[test]
    fn latency_sort_orders_available_then_unprobed_then_failed() {
        let mut app = GuiApp::for_tests();
        app.nodes = vec![
            node_in("a.opengw.net", "日本"),
            node_in("b.opengw.net", "日本"),
            node_in("c.opengw.net", "日本"),
            node_in("d.opengw.net", "日本"),
            node_in("e.opengw.net", "日本"),
            node_in("f.opengw.net", "日本"),
        ];
        // a: 120 ms, b/f: 30 ms (tie), c: unprobed, d: failed,
        // e: probing (counts as unprobed).
        app.probe_ms.insert("a.opengw.net".to_string(), 120);
        app.probes
            .insert("a.opengw.net".to_string(), "120 ms".to_string());
        app.probe_ms.insert("b.opengw.net".to_string(), 30);
        app.probes
            .insert("b.opengw.net".to_string(), "30 ms".to_string());
        app.probe_ms.insert("f.opengw.net".to_string(), 30);
        app.probes
            .insert("f.opengw.net".to_string(), "30 ms".to_string());
        app.probes
            .insert("d.opengw.net".to_string(), "失败".to_string());
        app.probes
            .insert("e.opengw.net".to_string(), "探测中…".to_string());
        app.sort = Some((SortCol::Latency, SortDir::Asc));
        // Available by ms ascending (b/f tie -> host a-z), then
        // unprobed (c, e), then failed.
        assert_eq!(app.display_indices(), vec![1, 5, 0, 2, 4, 3]);
        app.sort = Some((SortCol::Latency, SortDir::Desc));
        // Whole order reversed: failed first, unprobed middle,
        // available by ms descending last.
        assert_eq!(app.display_indices(), vec![3, 4, 2, 0, 5, 1]);
    }

    #[test]
    fn probe_ms_tracks_probe_events() {
        let mut app = GuiApp::for_tests();
        app.nodes = vec![node_in("a.opengw.net", "日本")];
        // A successful probe stores the ms alongside the display
        // string — the latency sort reads this number, never the
        // string.
        app.apply_event(GuiEvent::Probe {
            host: "a.opengw.net".to_string(),
            result: Ok((
                std::net::Ipv4Addr::new(10, 0, 0, 1),
                std::time::Duration::from_millis(42),
            )),
        });
        assert_eq!(app.probe_ms.get("a.opengw.net"), Some(&42));
        assert_eq!(
            app.probes.get("a.opengw.net").map(|s| s.as_str()),
            Some("42 ms")
        );
        // A sweep progress event clears it (the row shows 探测中…).
        app.busy = Some(Busy::ProbingAll);
        app.apply_event(GuiEvent::ProbeAllProgress {
            current: "a.opengw.net".to_string(),
            done: 0,
            total: 1,
        });
        assert_eq!(app.probe_ms.get("a.opengw.net"), None);
        assert_eq!(
            app.probes.get("a.opengw.net").map(|s| s.as_str()),
            Some("探测中…")
        );
        // A sweep failure leaves no ms behind.
        app.apply_event(GuiEvent::ProbeAllResult {
            host: "a.opengw.net".to_string(),
            result: Err(ProbeFailure::NodeDown("no route".to_string())),
            done: 1,
            total: 1,
        });
        assert_eq!(app.probe_ms.get("a.opengw.net"), None);
        assert_eq!(
            app.probes.get("a.opengw.net").map(|s| s.as_str()),
            Some("失败")
        );
        // A sweep success stores it again.
        app.apply_event(GuiEvent::ProbeAllResult {
            host: "a.opengw.net".to_string(),
            result: Ok((
                std::net::Ipv4Addr::new(10, 0, 0, 2),
                std::time::Duration::from_millis(7),
            )),
            done: 1,
            total: 1,
        });
        assert_eq!(app.probe_ms.get("a.opengw.net"), Some(&7));
    }

    #[test]
    fn shortcut_action_guards() {
        let mut app = GuiApp::for_tests();
        // Nothing selected: neither shortcut fires.
        assert_eq!(app.shortcut_action(true, false, false), None);
        assert_eq!(app.shortcut_action(false, true, false), None);
        app.selected = Some(0);
        assert_eq!(
            app.shortcut_action(true, false, false),
            Some(ShortcutAction::Connect),
            "Enter connects the selected node"
        );
        assert_eq!(
            app.shortcut_action(false, true, false),
            Some(ShortcutAction::Probe),
            "Ctrl+R probes the selected node"
        );
        assert_eq!(
            app.shortcut_action(true, true, false),
            Some(ShortcutAction::Connect),
            "Enter wins when both are pressed"
        );
        // A text field has focus: typing must never trigger either.
        assert_eq!(app.shortcut_action(true, false, true), None);
        assert_eq!(app.shortcut_action(false, true, true), None);
        // Busy or About open: blocked.
        app.busy = Some(Busy::Fetching);
        assert_eq!(app.shortcut_action(true, false, false), None);
        app.busy = None;
        app.show_about = true;
        assert_eq!(app.shortcut_action(true, false, false), None);
        assert_eq!(app.shortcut_action(false, true, false), None);
    }

    #[test]
    fn move_selection_follows_display_order_and_stops_at_ends() {
        let mut app = GuiApp::for_tests();
        app.nodes = vec![
            node_in("b.opengw.net", "美国"),
            node_in("a.opengw.net", "日本"),
            node_in("c.opengw.net", "德国"),
        ];
        // No selection yet: ↓ picks the first row, ↑ picks the last.
        app.move_selection(1);
        assert_eq!(app.selected, Some(0));
        assert!(app.scroll_to_selected, "moving must request a follow-scroll");
        app.scroll_to_selected = false;
        app.selected = None;
        app.move_selection(-1);
        assert_eq!(app.selected, Some(2));
        app.scroll_to_selected = false;
        // Step down, up, and stop at the top (no change, no scroll).
        app.move_selection(-1);
        assert_eq!(app.selected, Some(1));
        app.scroll_to_selected = false;
        app.move_selection(-1);
        assert_eq!(app.selected, Some(0));
        app.scroll_to_selected = false;
        app.move_selection(-1);
        assert_eq!(app.selected, Some(0), "↑ at the top must not move");
        assert!(
            !app.scroll_to_selected,
            "no movement means no follow-scroll request"
        );
        // Walk to the bottom and stop there too.
        app.move_selection(1);
        app.move_selection(1);
        assert_eq!(app.selected, Some(2));
        app.scroll_to_selected = false;
        app.move_selection(1);
        assert_eq!(app.selected, Some(2), "↓ at the bottom must not move");
        assert!(!app.scroll_to_selected);
        // Sorted list: movement follows the shown order (德国 <
        // 日本 < 美国 in A-Z, i.e. display [2, 1, 0]).
        app.sort = Some((SortCol::Country, SortDir::Asc));
        app.selected = None;
        app.move_selection(1);
        assert_eq!(app.selected, Some(2), "first in A-Z order is 德国");
        app.move_selection(1);
        assert_eq!(app.selected, Some(1), "second is 日本");
        app.move_selection(1);
        assert_eq!(app.selected, Some(0), "third is 美国");
        // Empty list: nothing to move to.
        app.nodes = vec![];
        app.selected = None;
        app.move_selection(1);
        assert_eq!(app.selected, None);
        app.move_selection(-1);
        assert_eq!(app.selected, None);
    }

    /// Selection model: plain click selects one row, Ctrl+click
    /// toggles rows in/out of the set, Shift+click selects the
    /// range from the anchor to the clicked row in display order,
    /// and every click moves the anchor to the clicked row.
    #[test]
    fn click_row_selection_model() {
        let mut app = GuiApp::for_tests();
        app.nodes = vec![
            node_in("a.opengw.net", "日本"),
            node_in("b.opengw.net", "美国"),
            node_in("c.opengw.net", "德国"),
            node_in("d.opengw.net", "法国"),
            node_in("e.opengw.net", "英国"),
        ];
        // Shift+click with no anchor yet: just this row.
        app.click_row(2, false, true);
        assert_eq!(app.selected, Some(2));
        assert_eq!(app.selected_set, BTreeSet::from([2]));
        // Plain click: only that row, anchor moves.
        app.click_row(1, false, false);
        assert_eq!(app.selected, Some(1));
        assert_eq!(app.selected_set, BTreeSet::from([1]));
        // Ctrl+click adds a row; anchor moves to it.
        app.click_row(3, true, false);
        assert_eq!(app.selected, Some(3));
        assert_eq!(app.selected_set, BTreeSet::from([1, 3]));
        // Ctrl+click again toggles it back out; the anchor stays on
        // the clicked row even though it is no longer in the set.
        app.click_row(1, true, false);
        assert_eq!(app.selected, Some(1));
        assert_eq!(app.selected_set, BTreeSet::from([3]));
        // Shift+click from the anchor (1) to row 4: the whole range.
        app.click_row(4, false, true);
        assert_eq!(app.selected, Some(4));
        assert_eq!(app.selected_set, BTreeSet::from([1, 2, 3, 4]));
        // Shift+click backwards (anchor 4 -> row 0): the range the
        // other way, anchor moves to row 0.
        app.click_row(0, false, true);
        assert_eq!(app.selected, Some(0));
        assert_eq!(
            app.selected_set,
            BTreeSet::from([0, 1, 2, 3, 4])
        );
        // Plain click collapses back to one row.
        app.click_row(2, false, false);
        assert_eq!(app.selected, Some(2));
        assert_eq!(app.selected_set, BTreeSet::from([2]));
        // Shift+click follows display order, not list order: sorted
        // A-Z by country string (code points: 德国 U+5FB7 < 日本
        // U+65E5 < 法国 U+6CD5 < 美国 U+7F8E < 英国 U+82F1), the
        // display is [2 德国， 0 日本， 3 法国， 1 美国， 4 英国].
        // The anchor is row 2 (display position 0); Shift+clicking
        // row 1 (display position 3) selects display[0..=3].
        app.sort = Some((SortCol::Country, SortDir::Asc));
        assert_eq!(app.display_indices(), vec![2, 0, 3, 1, 4]);
        app.click_row(1, false, true);
        assert_eq!(app.selected, Some(1));
        assert_eq!(app.selected_set, BTreeSet::from([0, 1, 2, 3]));
    }

    /// The click modifiers are the press-time latch OR the release
    /// frame's: releasing Shift/Ctrl a beat before the mouse button
    /// must not lose the gesture (the v3.7 Shift+click failure),
    /// Shift pressed mid-click counts too, and Cmd counts as Ctrl.
    #[test]
    fn effective_click_mods_latches_press_time_modifiers() {
        let shift = egui::Modifiers {
            shift: true,
            ..Default::default()
        };
        let ctrl = egui::Modifiers {
            ctrl: true,
            ..Default::default()
        };
        let command = egui::Modifiers {
            command: true,
            ..Default::default()
        };
        let none = egui::Modifiers::default();
        // Shift held at press, released before the mouse button.
        assert_eq!(
            GuiApp::effective_click_mods(Some(shift), none),
            (false, true)
        );
        // Shift pressed mid-click (after the press, before release).
        assert_eq!(
            GuiApp::effective_click_mods(Some(none), shift),
            (false, true)
        );
        // Held through the whole click.
        assert_eq!(
            GuiApp::effective_click_mods(Some(shift), shift),
            (false, true)
        );
        // No modifiers at either end: a plain click.
        assert_eq!(
            GuiApp::effective_click_mods(Some(none), none),
            (false, false)
        );
        // No press latch at all (a press+release in one frame
        // before the latch was recorded): fall back to the current
        // frame's modifiers.
        assert_eq!(GuiApp::effective_click_mods(None, shift), (false, true));
        assert_eq!(GuiApp::effective_click_mods(None, none), (false, false));
        // Ctrl at press time only, and Cmd counts as Ctrl.
        assert_eq!(
            GuiApp::effective_click_mods(Some(ctrl), none),
            (true, false)
        );
        assert_eq!(
            GuiApp::effective_click_mods(Some(command), none),
            (true, false)
        );
        assert_eq!(
            GuiApp::effective_click_mods(Some(none), command),
            (true, false)
        );
        // Ctrl+Shift together survive.
        let both = egui::Modifiers {
            ctrl: true,
            shift: true,
            ..Default::default()
        };
        assert_eq!(
            GuiApp::effective_click_mods(Some(both), none),
            (true, true)
        );
    }

    /// Arrow keys keep the v3.4 behaviour and collapse the
    /// multi-select to just the row the anchor moved to.
    #[test]
    fn arrow_keys_collapse_multi_select_to_anchor() {
        let mut app = GuiApp::for_tests();
        app.nodes = vec![
            node_in("a.opengw.net", "日本"),
            node_in("b.opengw.net", "美国"),
            node_in("c.opengw.net", "德国"),
        ];
        app.click_row(0, false, false);
        app.click_row(2, true, false);
        assert_eq!(app.selected_set, BTreeSet::from([0, 2]));
        app.move_selection(-1);
        assert_eq!(app.selected, Some(1));
        assert_eq!(app.selected_set, BTreeSet::from([1]));
        app.move_selection(-1);
        assert_eq!(app.selected, Some(0));
        assert_eq!(app.selected_set, BTreeSet::from([0]));
        // ↓ from the anchor also collapses (and stops at the end).
        app.move_selection(1);
        assert_eq!(app.selected, Some(1));
        assert_eq!(app.selected_set, BTreeSet::from([1]));
    }

    fn four_nodes() -> Vec<Node> {
        vec![
            node_in("a.opengw.net", "日本"),
            node_in("b.opengw.net", "美国"),
            node_in("c.opengw.net", "德国"),
            node_in("d.opengw.net", "法国"),
        ]
    }

    /// Delete: removes the selected nodes from the list, records
    /// their hostnames on the persistent blocklist, keeps their full
    /// data for this session's restore, cleans their probe results,
    /// and clears the selection. The blocklist round-trips through
    /// the config.
    #[test]
    fn delete_selected_removes_and_blocks() {
        let mut app = GuiApp::for_tests();
        app.nodes = four_nodes();
        app.probes
            .insert("c.opengw.net".to_string(), "10 ms".to_string());
        app.probe_ms.insert("c.opengw.net".to_string(), 10);
        app.probes
            .insert("a.opengw.net".to_string(), "5 ms".to_string());
        app.probe_ms.insert("a.opengw.net".to_string(), 5);
        app.click_row(0, false, false);
        app.click_row(2, true, false);
        assert_eq!(app.selected_set, BTreeSet::from([0, 2]));
        app.delete_selected();
        let hosts: Vec<&str> = app.nodes.iter().map(|n| n.host.as_str()).collect();
        assert_eq!(hosts, vec!["b.opengw.net", "d.opengw.net"]);
        assert_eq!(
            app.blocked_hosts,
            BTreeSet::from(["a.opengw.net".to_string(), "c.opengw.net".to_string()])
        );
        let deleted: Vec<&str> = app
            .deleted_nodes
            .iter()
            .map(|n| n.host.as_str())
            .collect();
        assert_eq!(deleted, vec!["c.opengw.net", "a.opengw.net"]);
        // Probe results for the deleted hosts are gone; the kept
        // node's stay untouched (none recorded for b/d here).
        assert!(!app.probes.contains_key("c.opengw.net"));
        assert!(!app.probe_ms.contains_key("c.opengw.net"));
        assert!(!app.probes.contains_key("a.opengw.net"));
        // Selection is cleared.
        assert_eq!(app.selected, None);
        assert!(app.selected_set.is_empty());
        // The blocklist persists through the config round-trip.
        let cfg = app.current_config();
        assert_eq!(
            cfg.blocked_hosts,
            vec!["a.opengw.net".to_string(), "c.opengw.net".to_string()]
        );
        let mut app2 = GuiApp::for_tests();
        app2.apply_config(&cfg);
        assert_eq!(app2.blocked_hosts, app.blocked_hosts);
    }

    /// Delete skips the connected node: it stays in the list and the
    /// skip is logged. (`delete_selected_except` takes the connected
    /// host directly so the skip is testable without a live tunnel;
    /// `delete_selected` passes `connected_node_host()`.)
    #[test]
    fn delete_selected_skips_connected_node() {
        let mut app = GuiApp::for_tests();
        app.nodes = four_nodes();
        app.click_row(0, false, false);
        app.click_row(1, true, false);
        let (removed, skipped) = app.delete_selected_except(Some("b.opengw.net"));
        assert_eq!((removed, skipped), (1, 1));
        let hosts: Vec<&str> = app.nodes.iter().map(|n| n.host.as_str()).collect();
        assert_eq!(
            hosts,
            vec!["b.opengw.net", "c.opengw.net", "d.opengw.net"]
        );
        assert_eq!(
            app.blocked_hosts,
            BTreeSet::from(["a.opengw.net".to_string()])
        );
        assert_eq!(app.deleted_nodes.len(), 1);
        // The connected node was skipped even though it was selected.
        assert!(app.nodes.iter().any(|n| n.host == "b.opengw.net"));
    }

    /// The full delete path logs the connected-node skip in the
    /// visible log (same wording the user sees). Goes through the
    /// real reporting path with the connected host supplied
    /// directly (no live tunnel in unit tests).
    #[test]
    fn delete_selected_logs_connected_skip() {
        let mut app = GuiApp::for_tests();
        app.nodes = four_nodes();
        app.click_row(0, false, false);
        app.click_row(1, true, false);
        app.delete_selected_with_connected(Some("b.opengw.net"));
        assert!(
            app.log.iter().any(|l| l.contains("已连接的节点不能删除，已跳过 1")),
            "skip must be logged: {:?}",
            app.log
        );
        assert_eq!(app.nodes.len(), 3);
        assert!(app.nodes.iter().any(|n| n.host == "b.opengw.net"));
    }

    /// Busy guard: with a job running, deletion is disabled and a
    /// direct call is a no-op; with nothing running it is allowed.
    #[test]
    fn delete_selected_blocked_when_busy() {
        let mut app = GuiApp::for_tests();
        app.nodes = four_nodes();
        app.click_row(0, false, false);
        assert!(app.can_delete());
        app.busy = Some(Busy::Probing);
        assert!(!app.can_delete());
        app.delete_selected();
        assert_eq!(app.nodes.len(), 4, "busy delete must be a no-op");
        assert!(app.blocked_hosts.is_empty());
        app.busy = Some(Busy::ProbingAll);
        assert!(!app.can_delete());
        app.busy = None;
        assert!(app.can_delete());
        // Empty selection disables it too.
        app.clear_selection();
        assert!(!app.can_delete());
    }

    /// The blocklist filters every list load: an incoming list that
    /// still contains a blocked host loses it before it is shown,
    /// and the selection is cleared (indices from the old list are
    /// meaningless).
    #[test]
    fn list_load_filters_blocked_hosts() {
        let mut app = GuiApp::for_tests();
        app.nodes = four_nodes();
        app.blocked_hosts.insert("b.opengw.net".to_string());
        app.click_row(0, false, false);
        app.apply_event(GuiEvent::Nodes(vpngate::NodeListUpdate {
            nodes: four_nodes(),
            source: vpngate::NodeListSource::Api,
            cache_age_secs: None,
            api_error: None,
            web_error: None,
        }));
        let hosts: Vec<&str> = app.nodes.iter().map(|n| n.host.as_str()).collect();
        assert_eq!(
            hosts,
            vec!["a.opengw.net", "c.opengw.net", "d.opengw.net"]
        );
        assert_eq!(app.selected, None);
        assert!(app.selected_set.is_empty());
        // A bare API label matches its completed block entry too.
        let bare = vec![node_in("b", "美国"), node_in("z.opengw.net", "日本")];
        let kept = app.apply_blocklist(bare);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].host, "z.opengw.net");
    }

    /// Restore puts this session's deleted nodes back and clears the
    /// blocklist; the selection is cleared too.
    #[test]
    fn restore_deleted_puts_nodes_back() {
        let mut app = GuiApp::for_tests();
        app.nodes = four_nodes();
        app.click_row(0, false, false);
        app.click_row(2, true, false);
        app.delete_selected();
        assert_eq!(app.nodes.len(), 2);
        assert_eq!(app.blocked_hosts.len(), 2);
        app.restore_deleted();
        assert!(app.blocked_hosts.is_empty());
        assert!(app.deleted_nodes.is_empty());
        let hosts: Vec<&str> = app.nodes.iter().map(|n| n.host.as_str()).collect();
        assert_eq!(
            hosts,
            vec![
                "b.opengw.net",
                "d.opengw.net",
                "c.opengw.net",
                "a.opengw.net"
            ],
            "restored nodes come back (appended after the survivors)"
        );
        assert_eq!(app.selected, None);
        assert!(app.selected_set.is_empty());
        assert!(
            app.log.iter().any(|l| l.contains("已恢复 2 个节点")),
            "restore must be logged: {:?}",
            app.log
        );
        // And the config round-trip carries the cleared blocklist.
        assert!(app.current_config().blocked_hosts.is_empty());
    }

    /// After a restart there is nothing in memory to put back:
    /// restore only clears the block record and says the nodes will
    /// return on the next refresh.
    #[test]
    fn restore_without_session_copies_only_clears_blocklist() {
        let mut app = GuiApp::for_tests();
        app.nodes = vec![
            node_in("x.opengw.net", "日本"),
            node_in("y.opengw.net", "美国"),
        ];
        // A blocklist loaded from config.toml, with no deleted nodes
        // in this session's memory (as after a restart).
        app.blocked_hosts.insert("b.opengw.net".to_string());
        app.click_row(0, false, false);
        app.restore_deleted();
        assert!(app.blocked_hosts.is_empty());
        assert_eq!(app.nodes.len(), 2, "nothing in memory, nothing re-inserted");
        assert!(app.selected_set.is_empty());
        assert!(
            app.log
                .iter()
                .any(|l| l.contains("已清除删除记录，下次刷新后节点恢复")),
            "restart restore must log the next-refresh note: {:?}",
            app.log
        );
        // And a later load really does bring the host back.
        let kept = app.apply_blocklist(vec![node_in("b.opengw.net", "美国")]);
        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn probe_all_targets_follow_display_order() {
        let mut app = GuiApp::for_tests();
        app.nodes = vec![
            node_in("b.opengw.net", "美国"),
            node_in("a.opengw.net", "日本"),
            node_in("c.opengw.net", "日本"),
            node_in("d.opengw.net", "德国"),
        ];
        // Fetch order by default.
        let targets = app.probe_all_targets();
        let hosts: Vec<&str> = targets.iter().map(|(h, _)| h.as_str()).collect();
        assert_eq!(
            hosts,
            ["b.opengw.net", "a.opengw.net", "c.opengw.net", "d.opengw.net"]
        );
        // Sorted A-Z: 德国 < 日本 < 美国, hostname breaks ties.
        app.sort = Some((SortCol::Country, SortDir::Asc));
        let targets = app.probe_all_targets();
        let hosts: Vec<&str> = targets.iter().map(|(h, _)| h.as_str()).collect();
        assert_eq!(
            hosts,
            ["d.opengw.net", "a.opengw.net", "c.opengw.net", "b.opengw.net"]
        );
        // Targets are dialled with the completed host:port.
        assert_eq!(targets[0].1, "d.opengw.net:992");
        // Empty list: nothing to probe.
        app.nodes = vec![];
        assert!(app.probe_all_targets().is_empty());
    }

    #[test]
    fn probe_all_events_drive_progress_and_stop() {
        let mut app = GuiApp::for_tests();
        app.nodes = vec![
            node_in("a.opengw.net", "日本"),
            node_in("b.opengw.net", "美国"),
            node_in("c.opengw.net", "德国"),
        ];
        // Empty list: starting is a no-op (the button is disabled
        // then too).
        let mut empty = GuiApp::for_tests();
        empty.nodes = vec![];
        empty.start_probe_all(egui::Context::default());
        assert!(empty.busy.is_none());
        // Starting is blocked while another job runs.
        app.busy = Some(Busy::Fetching);
        app.start_probe_all(egui::Context::default());
        assert_eq!(app.busy, Some(Busy::Fetching));
        app.busy = None;
        app.start_probe_all(egui::Context::default());
        assert_eq!(app.busy, Some(Busy::ProbingAll));
        assert_eq!((app.probe_all_total, app.probe_all_done), (3, 0));
        assert_eq!(
            app.probes.get("a.opengw.net").map(|s| s.as_str()),
            Some("探测中…"),
            "the first row shows 探测中… right away"
        );
        assert!(app.status.contains("0/3"), "status: {}", app.status);
        // Starting again while sweeping is a no-op.
        app.start_probe_all(egui::Context::default());
        assert_eq!(app.probe_all_total, 3);
        // Progress on the first node, then its good result: the row
        // takes the latency and the progress advances.
        app.apply_event(GuiEvent::ProbeAllProgress {
            current: "a.opengw.net".to_string(),
            done: 0,
            total: 3,
        });
        assert!(app.status.contains("0/3"), "status: {}", app.status);
        assert!(app.status.contains("a.opengw.net"), "status: {}", app.status);
        app.apply_event(GuiEvent::ProbeAllResult {
            host: "a.opengw.net".to_string(),
            result: Ok((Ipv4Addr::new(10, 0, 0, 2), Duration::from_millis(95))),
            done: 1,
            total: 3,
        });
        assert_eq!(
            app.probes.get("a.opengw.net").map(|s| s.as_str()),
            Some("95 ms")
        );
        assert_eq!((app.probe_all_ok, app.probe_all_failed), (1, 0));
        // A failed node writes 失败 — and neither outcome sets the
        // error banner mid-sweep (the summary at the end is the
        // place for the outcome).
        app.apply_event(GuiEvent::ProbeAllProgress {
            current: "b.opengw.net".to_string(),
            done: 1,
            total: 3,
        });
        assert_eq!(
            app.probes.get("b.opengw.net").map(|s| s.as_str()),
            Some("探测中…")
        );
        app.apply_event(GuiEvent::ProbeAllResult {
            host: "b.opengw.net".to_string(),
            result: Err(ProbeFailure::NodeDown("timeout".to_string())),
            done: 2,
            total: 3,
        });
        assert_eq!(
            app.probes.get("b.opengw.net").map(|s| s.as_str()),
            Some("失败")
        );
        assert_eq!((app.probe_all_ok, app.probe_all_failed), (1, 1));
        assert!(
            app.error.is_empty(),
            "no per-node error banner mid-sweep: {}",
            app.error
        );
        assert_eq!(app.busy, Some(Busy::ProbingAll));
        // 停止: the flag is set (the task stops after the node in
        // flight) and the status says so; the Done event clears the
        // busy flag and writes the summary.
        app.stop_probe_all();
        assert!(app.probe_all_stop.load(Ordering::Relaxed));
        assert!(app.status.contains("正在停止"), "status: {}", app.status);
        app.apply_event(GuiEvent::ProbeAllDone {
            total: 3,
            ok: 1,
            failed: 1,
            stopped: true,
            proxy_down: false,
        });
        assert!(app.busy.is_none());
        assert!(app.status.contains("已停止"), "status: {}", app.status);
        assert!(app.status.contains("已测 2/3"), "status: {}", app.status);
        assert!(
            app.log
                .iter()
                .any(|l| l.contains("探测全部已停止：已测 2/3，1 个可用，1 个失败，1 个未测")),
            "log must carry the stopped summary: {:?}",
            app.log
        );
        // A completed sweep (not stopped) writes the finished summary.
        let mut done_app = GuiApp::for_tests();
        done_app.nodes = vec![node_in("a.opengw.net", "日本")];
        done_app.start_probe_all(egui::Context::default());
        done_app.apply_event(GuiEvent::ProbeAllResult {
            host: "a.opengw.net".to_string(),
            result: Ok((Ipv4Addr::new(10, 0, 0, 2), Duration::from_millis(64))),
            done: 1,
            total: 1,
        });
        done_app.apply_event(GuiEvent::ProbeAllDone {
            total: 1,
            ok: 1,
            failed: 0,
            stopped: false,
            proxy_down: false,
        });
        assert!(done_app.busy.is_none());
        assert_eq!(
            done_app.status,
            "探测全部完成：1 个节点，1 个可用，0 个失败。"
        );
        // A proxy-down result aborts the sweep with the proxy named.
        let mut proxy_app = GuiApp::for_tests();
        proxy_app.front_enabled = true;
        proxy_app.front_addr = "127.0.0.1:2080".to_string();
        proxy_app.nodes = vec![
            node_in("a.opengw.net", "日本"),
            node_in("b.opengw.net", "美国"),
        ];
        proxy_app.start_probe_all(egui::Context::default());
        proxy_app.apply_event(GuiEvent::ProbeAllResult {
            host: "a.opengw.net".to_string(),
            result: Err(ProbeFailure::ProxyDown("connection refused".to_string())),
            done: 1,
            total: 2,
        });
        assert_eq!(
            proxy_app.probes.get("a.opengw.net").map(|s| s.as_str()),
            Some("代理不通")
        );
        proxy_app.apply_event(GuiEvent::ProbeAllDone {
            total: 2,
            ok: 0,
            failed: 1,
            stopped: false,
            proxy_down: true,
        });
        assert!(proxy_app.busy.is_none());
        assert!(
            proxy_app.status.contains("前置代理不通"),
            "status: {}",
            proxy_app.status
        );
        assert!(
            proxy_app.error.contains("127.0.0.1:2080"),
            "error must name the proxy: {}",
            proxy_app.error
        );
        assert!(
            proxy_app
                .log
                .iter()
                .any(|l| l.contains("探测全部已停止：前置代理不通。已测 1/2，0 个可用，1 个失败，1 个未测")),
            "log must carry the proxy-down summary: {:?}",
            proxy_app.log
        );
    }

    #[test]
    fn branded_visuals_meet_text_contrast() {
        // WCAG contrast for the pairs a user actually reads: body
        // text on the panel, white on the indigo selection fill, the
        // cyan ∞ mark on the panel, error/warning text on the panel.
        // The palette comment above `branded_visuals` states the
        // expected ratios; this test pins them.
        fn luminance(c: egui::Color32) -> f32 {
            let f = |v: u8| {
                let s = v as f32 / 255.0;
                if s <= 0.04045 {
                    s / 12.92
                } else {
                    ((s + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * f(c.r()) + 0.7152 * f(c.g()) + 0.0722 * f(c.b())
        }
        fn contrast(a: egui::Color32, b: egui::Color32) -> f32 {
            let (l1, l2) = (luminance(a), luminance(b));
            (l1.max(l2) + 0.05) / (l1.min(l2) + 0.05)
        }
        for theme in [egui::Theme::Dark, egui::Theme::Light] {
            let v = branded_visuals(theme);
            let text = v.override_text_color.expect("text colour set");
            assert!(
                contrast(text, v.panel_fill) >= 4.5,
                "{theme:?} body text on panel: {:.2}",
                contrast(text, v.panel_fill)
            );
            assert!(
                contrast(egui::Color32::WHITE, v.selection.bg_fill) >= 4.5,
                "{theme:?} white on selection: {:.2}",
                contrast(egui::Color32::WHITE, v.selection.bg_fill)
            );
            let accent = infinity_accent(v.dark_mode);
            assert!(
                contrast(accent, v.panel_fill) >= 4.5,
                "{theme:?} infinity accent on panel: {:.2}",
                contrast(accent, v.panel_fill)
            );
            assert!(
                contrast(v.error_fg_color, v.panel_fill) >= 4.5,
                "{theme:?} error text on panel: {:.2}",
                contrast(v.error_fg_color, v.panel_fill)
            );
            assert!(
                contrast(v.warn_fg_color, v.panel_fill) >= 4.5,
                "{theme:?} warning text on panel: {:.2}",
                contrast(v.warn_fg_color, v.panel_fill)
            );
        }
    }

    #[test]
    fn nodes_event_updates_state() {
        let mut app = GuiApp::for_tests();
        app.apply_event(GuiEvent::Nodes(vpngate::NodeListUpdate {
            nodes: vec![node("a.opengw.net"), node("b.opengw.net")],
            source: vpngate::NodeListSource::Api,
            cache_age_secs: None,
            api_error: None,
            web_error: None,
        }));
        assert_eq!(app.nodes.len(), 2);
        assert!(app.busy.is_none());
        assert_eq!(app.selected, None);
        assert!(app.nodes_source.contains("官方 API"));
    }

    #[test]
    fn nodes_event_web_scrape_source_shows_that() {
        let mut app = GuiApp::for_tests();
        app.apply_event(GuiEvent::Nodes(vpngate::NodeListUpdate {
            nodes: vec![node("a.opengw.net")],
            source: vpngate::NodeListSource::WebScrape,
            cache_age_secs: None,
            api_error: Some("TLS reset".to_string()),
            web_error: None,
        }));
        assert_eq!(app.nodes.len(), 1);
        assert!(app.nodes_source.contains("网页抓取"));
        assert!(app.log.iter().any(|l| l.contains("TLS reset")));
    }

    #[test]
    fn nodes_event_falls_back_to_curated_list() {
        let mut app = GuiApp::for_tests();
        app.apply_event(GuiEvent::Nodes(vpngate::NodeListUpdate {
            nodes: vpngate::curated_nodes(),
            source: vpngate::NodeListSource::Curated,
            cache_age_secs: None,
            api_error: Some("network down".to_string()),
            web_error: Some("page changed".to_string()),
        }));
        assert_eq!(app.nodes.len(), vpngate::curated_nodes().len());
        assert!(app.nodes_source.contains("内嵌列表"));
        assert!(app.log.iter().any(|l| l.contains("network down")));
        assert!(app.log.iter().any(|l| l.contains("page changed")));
    }

    #[test]
    fn probe_event_records_latency_and_failure() {
        let mut app = GuiApp::for_tests();
        app.busy = Some(Busy::Probing);
        app.apply_event(GuiEvent::Probe {
            host: "a.opengw.net".to_string(),
            result: Ok((Ipv4Addr::new(10, 0, 0, 2), Duration::from_millis(128))),
        });
        assert_eq!(
            app.probes.get("a.opengw.net").map(|s| s.as_str()),
            Some("128 ms")
        );
        assert!(app.busy.is_none());
        app.busy = Some(Busy::Probing);
        app.apply_event(GuiEvent::Probe {
            host: "b.opengw.net".to_string(),
            result: Err(ProbeFailure::NodeDown("timeout".to_string())),
        });
        assert_eq!(
            app.probes.get("b.opengw.net").map(|s| s.as_str()),
            Some("失败")
        );
        assert!(
            !app.error.is_empty(),
            "probe failure must set an actionable error"
        );
        // Proxy-down failure names the proxy, not the node, and points
        // at the front-proxy test instead of suggesting another node.
        app.busy = Some(Busy::Probing);
        app.front_enabled = true;
        app.front_addr = "192.168.236.197:10808".to_string();
        app.apply_event(GuiEvent::Probe {
            host: "c.opengw.net".to_string(),
            result: Err(ProbeFailure::ProxyDown("connection refused".to_string())),
        });
        assert_eq!(
            app.probes.get("c.opengw.net").map(|s| s.as_str()),
            Some("代理不通")
        );
        assert!(
            app.error.contains("192.168.236.197:10808"),
            "error must name the proxy: {}",
            app.error
        );
        assert!(
            app.error.contains("测试前置代理"),
            "error must point at the proxy test: {}",
            app.error
        );
        assert!(
            app.log.iter().any(|l| l.contains("一起失败")),
            "log must note the shared cause"
        );
        // Node-down with the proxy on says the proxy is fine.
        app.busy = Some(Busy::Probing);
        app.apply_event(GuiEvent::Probe {
            host: "d.opengw.net".to_string(),
            result: Err(ProbeFailure::NodeDown("TLS handshake failed".to_string())),
        });
        assert!(
            app.error.contains("能连上，但节点不通"),
            "error must clear the proxy: {}",
            app.error
        );
    }

    #[test]
    fn classify_probe_failure_splits_proxy_from_node() {
        assert!(matches!(
            classify_probe_failure("x".to_string(), Some(false)),
            ProbeFailure::ProxyDown(_)
        ));
        assert!(matches!(
            classify_probe_failure("x".to_string(), Some(true)),
            ProbeFailure::NodeDown(_)
        ));
        assert!(matches!(
            classify_probe_failure("x".to_string(), None),
            ProbeFailure::NodeDown(_)
        ));
    }

    #[test]
    fn front_proxy_test_event_reports_both_sides() {
        use vpngate_client::net::FrontProxyCheck;
        let mut app = GuiApp::for_tests();
        app.front_addr = "192.168.236.197:10808".to_string();
        app.busy = Some(Busy::TestingFrontProxy);
        // Both reachable: status carries latencies, error clears.
        app.apply_event(GuiEvent::FrontProxyTest {
            addr: "192.168.236.197:10808".to_string(),
            google: FrontProxyCheck::Reachable {
                status: 200,
                ms: 312,
            },
            vpngate: FrontProxyCheck::Reachable {
                status: 200,
                ms: 405,
            },
        });
        assert!(
            app.status.contains("312 ms"),
            "status must show Google latency: {}",
            app.status
        );
        assert!(
            app.status.contains("405 ms"),
            "status must show vpngate.net latency: {}",
            app.status
        );
        assert!(app.error.is_empty());
        assert!(app.log.iter().any(|l| l.contains("前置代理测试")));
        // Proxy down: status + error blame the proxy and its address.
        app.busy = Some(Busy::TestingFrontProxy);
        app.apply_event(GuiEvent::FrontProxyTest {
            addr: "192.168.236.197:10808".to_string(),
            google: FrontProxyCheck::ProxyUnreachable("连前置代理端口失败".to_string()),
            vpngate: FrontProxyCheck::ProxyUnreachable("连前置代理端口失败".to_string()),
        });
        assert!(
            app.status.contains("前置代理连不上"),
            "status: {}",
            app.status
        );
        assert!(
            app.error.contains("192.168.236.197:10808"),
            "error must name the proxy: {}",
            app.error
        );
        assert!(
            app.error.contains("一起失败"),
            "error must note the shared cause: {}",
            app.error
        );
        // Proxy up, vpngate.net down through it: distinct wording.
        app.busy = Some(Busy::TestingFrontProxy);
        app.apply_event(GuiEvent::FrontProxyTest {
            addr: "192.168.236.197:10808".to_string(),
            google: FrontProxyCheck::Reachable {
                status: 200,
                ms: 300,
            },
            vpngate: FrontProxyCheck::TargetUnreachable("代理建好连接后目标站无响应".to_string()),
        });
        assert!(
            app.status.contains("vpngate.net不通"),
            "status: {}",
            app.status
        );
        assert!(app.error.contains("前置代理能连上"), "error: {}", app.error);
        assert!(
            app.error.contains("vpngate.net"),
            "error must name the failing target: {}",
            app.error
        );
        // TLS failure through a working proxy: the log must carry the
        // raw error (not just "失败") — a firewall blocking the
        // unsigned exe and a dead target look identical otherwise.
        app.busy = Some(Busy::TestingFrontProxy);
        app.apply_event(GuiEvent::FrontProxyTest {
            addr: "192.168.236.197:10808".to_string(),
            google: FrontProxyCheck::TargetUnreachable(
                "代理建好连接后目标站无响应：TLS handshake to www.google.com failed: invalid peer certificate: UnknownIssuer".to_string(),
            ),
            vpngate: FrontProxyCheck::TargetUnreachable(
                "代理建好连接后目标站无响应：TLS handshake to www.vpngate.net failed: invalid peer certificate: UnknownIssuer".to_string(),
            ),
        });
        assert!(
            app.log.iter().any(|l| l.contains("UnknownIssuer")),
            "log must carry the raw TLS error: {:?}",
            app.log
        );
        assert!(
            app.log.iter().any(|l| l.contains("TLS handshake")),
            "log must name the failing stage: {:?}",
            app.log
        );
    }

    #[test]
    fn front_proxy_test_event_uses_snapshot_address() {
        use vpngate_client::net::FrontProxyCheck;
        let mut app = GuiApp::for_tests();
        // The address field was edited mid-test (Leo typed 12.0.0.1,
        // fixed it to 127.0.0.1 while the test was running): the
        // result must name the address the test actually used
        // (12.0.0.1), not the field's current value.
        app.front_addr = "127.0.0.1:2080".to_string();
        app.busy = Some(Busy::TestingFrontProxy);
        app.apply_event(GuiEvent::FrontProxyTest {
            addr: "12.0.0.1:2080".to_string(),
            google: FrontProxyCheck::ProxyUnreachable("连接前置代理超时".to_string()),
            vpngate: FrontProxyCheck::ProxyUnreachable("连接前置代理超时".to_string()),
        });
        assert!(
            app.error.contains("12.0.0.1:2080"),
            "error must name the snapshotted address: {}",
            app.error
        );
        assert!(
            !app.error.contains("127.0.0.1:2080"),
            "error must not name the edited-in address: {}",
            app.error
        );
    }

    #[test]
    fn front_proxy_test_progress_shows_site() {
        let mut app = GuiApp::for_tests();
        app.busy = Some(Busy::TestingFrontProxy);
        app.apply_event(GuiEvent::FrontProxyTestProgress {
            site: "Google".to_string(),
        });
        assert_eq!(app.status, "正在测试前置代理… Google");
        assert!(
            app.busy.is_some(),
            "a progress event must not clear the busy flag"
        );
        app.apply_event(GuiEvent::FrontProxyTestProgress {
            site: "vpngate.net".to_string(),
        });
        assert_eq!(app.status, "正在测试前置代理… vpngate.net");
    }

    #[test]
    fn col_widths_roundtrip_through_config() {
        let mut app = GuiApp::for_tests();
        assert_eq!(app.col_widths, [90, 130, 64, 88, 190, 90]);
        app.col_widths = [120, 140, 70, 95, 200, 100];
        let cfg = app.current_config();
        assert_eq!(cfg.ui.node_col_widths, [120, 140, 70, 95, 200, 100]);
        let mut app2 = GuiApp::for_tests();
        app2.apply_config(&cfg);
        assert_eq!(app2.col_widths, [120, 140, 70, 95, 200, 100]);
    }

    #[test]
    fn connect_failure_and_test_result_paths() {
        let mut app = GuiApp::for_tests();
        app.busy = Some(Busy::Connecting);
        app.apply_event(GuiEvent::ConnectFailed("all candidates failed".to_string()));
        assert!(app.busy.is_none());
        assert!(app.conn.is_none());
        assert!(app.error.contains("换一个节点"));
        app.apply_event(GuiEvent::TestResult {
            gen204: true,
            google: false,
            detail: "generate_204：HTTP/1.1 204；Google TLS 握手失败。".to_string(),
        });
        assert!(app.status.contains("部分通过"));
        assert!(app.error.contains("换一个节点"));
    }

    #[test]
    fn current_config_roundtrips_settings() {
        let mut app = GuiApp::for_tests();
        app.front_enabled = true;
        app.front_addr = "127.0.0.1:1081".to_string();
        app.front_user = "u".to_string();
        app.front_pass = "p".to_string();
        app.local_port = 10809;
        app.allow_lan = true;
        app.system_proxy = true;
        let cfg = app.current_config();
        assert!(cfg.front_proxy.enabled);
        assert_eq!(cfg.front_proxy.addr, "127.0.0.1:1081");
        assert_eq!(cfg.front_proxy.username.as_deref(), Some("u"));
        assert_eq!(cfg.front_proxy.password.as_deref(), Some("p"));
        assert_eq!(cfg.listen.local_port, 10809);
        assert!(cfg.listen.allow_lan);
        assert!(cfg.system_proxy.enabled);
    }

    #[test]
    fn log_lines_go_to_file_log_with_levels() {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("vpngate-gui-test-{}.log", n));
        let old = vpngate_client::filelog::old_log_path(&path);
        let mut app = GuiApp::for_tests();
        // No logger yet: log calls only touch the on-screen log.
        app.log_line("info before logger".to_string());
        assert!(!path.exists(), "no file without a logger");
        app.file_logger = Some(FileLogger::new(&path));
        app.log_line("info line".to_string());
        app.log_warn("warn line".to_string());
        app.set_error("error line".to_string());
        // set_error also sets the banner.
        assert_eq!(app.error, "error line");
        drop(app);
        let text = std::fs::read_to_string(&path).expect("file log written");
        assert!(text.contains("[INFO] info line"), "info: {}", text);
        assert!(text.contains("[WARN] warn line"), "warn: {}", text);
        assert!(text.contains("[ERROR] error line"), "error: {}", text);
        assert!(
            !text.contains("info before logger"),
            "lines before the logger started stay out of the file: {}",
            text
        );
        for l in text.lines() {
            assert_eq!(
                l.as_bytes()[4],
                b'-',
                "every line starts with a YYYY-MM-DD timestamp: {}",
                l
            );
        }
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&old);
    }

    /// LAN sharing binds the same mixed port on all interfaces: one
    /// wildcard bind serves loopback too, so the list has a single
    /// entry — no second port, and no equal-port error to raise.
    #[test]
    fn listen_addrs_local_plus_lan_same_port() {
        let mut app = GuiApp::for_tests();
        assert_eq!(app.listen_addrs(), vec!["127.0.0.1:10818".to_string()]);
        assert_eq!(app.listen_display(), "127.0.0.1:10818");
        app.allow_lan = true;
        assert_eq!(app.listen_addrs(), vec!["0.0.0.0:10818".to_string()]);
        assert_eq!(
            app.listen_display(),
            "127.0.0.1:10818 ＋ 0.0.0.0:10818"
        );
    }

    #[test]
    fn auto_save_if_dirty_persists_front_proxy() {
        // Auto-save writes next to the test exe (portable dir); verify
        // the file round-trips the front-proxy fields, then clean up.
        let mut app = GuiApp::for_tests();
        app.front_enabled = true;
        app.front_addr = "127.0.0.1:1081".to_string();
        app.front_user = "u".to_string();
        app.front_pass = "p".to_string();
        app.auto_save_if_dirty();
        let path = Config::portable_config_path().expect("exe dir known");
        let back = Config::load(Some(&path)).expect("saved config loads");
        assert!(back.front_proxy.enabled);
        assert_eq!(back.front_proxy.addr, "127.0.0.1:1081");
        assert_eq!(back.front_proxy.username.as_deref(), Some("u"));
        assert_eq!(back.front_proxy.password.as_deref(), Some("p"));
        assert_eq!(app.config, back, "saved config becomes the new baseline");
        let _ = std::fs::remove_file(&path);
        // Second call with no changes must not rewrite the file.
        app.auto_save_if_dirty();
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn probe_flow_dead_front_proxy_is_proxy_down() {
        let connector = connector_for(true, &format!("127.0.0.1:{}", closed_port()), "", "");
        let res = probe_flow(
            &connector,
            "kanratown.opengw.net:992",
            Duration::from_secs(5),
        )
        .await;
        assert!(
            matches!(res, Err(ProbeFailure::ProxyDown(_))),
            "dead proxy must blame the proxy, got {:?}",
            res.map(|_| ())
        );
    }

    #[tokio::test]
    async fn probe_flow_node_down_when_proxy_tcp_ok() {
        // Fake proxy: accepts TCP, then closes. The proxy port answers,
        // so a probe failure must blame the node, not the proxy.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((s, _)) = listener.accept().await else {
                    break;
                };
                drop(s);
            }
        });
        let connector = connector_for(true, &format!("127.0.0.1:{}", port), "", "");
        let res = probe_flow(
            &connector,
            "kanratown.opengw.net:992",
            Duration::from_secs(5),
        )
        .await;
        assert!(
            matches!(res, Err(ProbeFailure::NodeDown(_))),
            "live proxy port must blame the node, got {:?}",
            res.map(|_| ())
        );
    }

    /// A TCP listener that accepts and holds connections forever,
    /// never sending a byte — a hanging target (a dead node whose
    /// TLS handshake never answers). Same idea as the integration
    /// test's `black_hole_port`, for the bin's own async tests.
    fn black_hole_port() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || loop {
            let Ok((s, _)) = listener.accept() else {
                break;
            };
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(3600));
                drop(s);
            });
        });
        port
    }

    #[tokio::test]
    async fn probe_all_flow_stops_after_stop_request() {
        // Four black-hole targets, 2 s timeout each: a full sweep
        // would take ~8 s. The stop flag is set as soon as the
        // first result arrives, so the sweep must halt after one
        // or two results — the flag may land just after the loop
        // already started the next probe, and that probe still
        // finishes first.
        let targets: Vec<(String, String)> = (0..4)
            .map(|_| {
                let p = black_hole_port();
                (format!("127.0.0.1:{}", p), format!("127.0.0.1:{}", p))
            })
            .collect();
        let (tx, rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flow = stop.clone();
        let stop_collector = stop.clone();
        let connector = Connector::new(None);
        let timeout = Duration::from_secs(2);
        let start = std::time::Instant::now();
        tokio::spawn(async move {
            probe_all_flow(
                &connector,
                targets,
                timeout,
                stop_flow,
                tx,
                egui::Context::default(),
            )
            .await;
        });
        // The std mpsc receiver blocks, so it runs on a blocking
        // thread; it sets the stop flag the moment the first
        // result arrives, then reports how many results came and
        // the Done event's verdict.
        let (results, done_ev) = tokio::task::spawn_blocking(move || {
            let mut results = 0usize;
            let mut done_ev = None;
            loop {
                match rx.recv_timeout(Duration::from_secs(20)) {
                    Ok(GuiEvent::ProbeAllResult { .. }) => {
                        results += 1;
                        stop_collector.store(true, Ordering::Relaxed);
                    }
                    Ok(GuiEvent::ProbeAllDone {
                        total,
                        ok,
                        failed,
                        stopped,
                        proxy_down,
                    }) => {
                        done_ev = Some((total, ok, failed, stopped, proxy_down));
                        break;
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            (results, done_ev)
        })
        .await
        .unwrap();
        let elapsed = start.elapsed();
        let (total, ok, failed, stopped, proxy_down) =
            done_ev.expect("a Done event must arrive");
        assert_eq!(total, 4);
        assert!(stopped, "the sweep must report stopped");
        assert!(!proxy_down, "no proxy is configured, so nothing is proxy-down");
        assert_eq!(ok, 0, "black holes never answer");
        assert_eq!(failed, results, "every probed node failed");
        assert!(
            (1..=2).contains(&results),
            "the stop must halt the sweep after one or two results, got {}",
            results
        );
        assert!(
            elapsed < Duration::from_secs(6),
            "a stopped sweep must not run the full queue (~8 s), took {:?}",
            elapsed
        );
    }

    /// A port with nothing listening on it.
    fn closed_port() -> u16 {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    }

    #[test]
    fn disconnect_is_a_no_op_when_not_connected() {
        let mut app = GuiApp::for_tests();
        app.disconnect();
        assert_eq!(app.status, "未连接");
    }

    /// Render the smallest window (560x420) in both themes for visual
    /// review, plus the 800x600 window in both themes, a 150% zoom
    /// render and scrolled-to-bottom renders (they must show the end
    /// of 高级设置 and the log — that is the outer-scroll fix being
    /// verified). Needs a wgpu adapter (software lavapipe is enough);
    /// on a machine without one this test fails and must be skipped,
    /// not faked.
    #[test]
    #[ignore = "renders PNGs for review; needs a graphics adapter"]
    fn render_for_review() {
        use egui_kittest::{kittest::Queryable, Harness};
        for (theme, name, zoom, scrolled, w, h) in [
            (egui::Theme::Dark, "dark", 1.0, false, 560.0, 420.0),
            (egui::Theme::Light, "light", 1.0, false, 560.0, 420.0),
            (egui::Theme::Dark, "dark-800x600", 1.0, false, 800.0, 600.0),
            (egui::Theme::Light, "light-800x600", 1.0, false, 800.0, 600.0),
            (egui::Theme::Dark, "dark-zoom150", 1.5, false, 560.0, 420.0),
            (egui::Theme::Dark, "dark-scrolled", 1.0, true, 560.0, 420.0),
            (egui::Theme::Light, "light-scrolled", 1.0, true, 560.0, 420.0),
        ] {
            let mut harness = Harness::builder()
                .with_size(egui::vec2(w, h))
                .with_theme(theme)
                .build_eframe(|cc| {
                    if scrolled {
                        GuiApp::for_render_tests_scrolled(cc)
                    } else {
                        GuiApp::for_render_tests(cc)
                    }
                });
            if zoom != 1.0 {
                harness.ctx.set_zoom_factor(zoom);
            }
            // ScrollAreas request repaints while settling, and the
            // always-visible scroll bars fade in over a few frames;
            // step a fixed number of frames instead of run() (see
            // egui-kittest skill).
            harness.run_steps(30);
            if scrolled {
                // The widgets the scroll must reveal exist in the UI.
                // (The PNG itself is the proof they are actually in
                // view — check it, don't trust this assertion alone.)
                harness.get_by_label("测试前置代理");
                harness.get_by_label("日志");
            }
            let path = std::env::temp_dir().join(format!("ui-review-{name}.png"));
            harness
                .render()
                .expect("needs a graphics adapter")
                .save(&path)
                .unwrap();
            eprintln!("{}", path.display());
        }
    }

    /// Isolation: does a bare Sense::drag handle (no ScrollArea) respond
    /// to the same kittest drag sequence? Temporary diagnostic for the
    /// column-separator test.
    #[test]
    #[ignore = "diagnostic; needs a graphics adapter"]
    fn kittest_drag_smoke() {
        use egui_kittest::Harness;
        struct DragApp {
            w: f32,
        }
        impl eframe::App for DragApp {
            fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
                egui::CentralPanel::default().show(ui, |ui| {
                    // Mirror the real layout: outer vertical ScrollArea,
                    // inner both-directions ScrollArea, header row in a
                    // horizontal layout with a 6 px drag separator.
                    egui::ScrollArea::vertical()
                        .id_salt("outer")
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            egui::ScrollArea::both()
                                .id_salt("inner")
                                .max_height(120.0)
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    ui.spacing_mut().item_spacing = egui::vec2(6.0, 4.0);
                                    // Fixed layout: cells and separator at
                                    // exact rects (see the real header — the
                                    // allocate_ui_with_layout version let the
                                    // cells shrink to their label width and
                                    // put the separator in the wrong place).
                                    let (header_rect, _) = ui.allocate_exact_size(
                                        egui::vec2(238.0, 22.0),
                                        egui::Sense::hover(),
                                    );
                                    ui.put(
                                        egui::Rect::from_min_size(
                                            egui::pos2(header_rect.left(), header_rect.top()),
                                            egui::vec2(90.0, 22.0),
                                        ),
                                        egui::Label::new("国家/地区"),
                                    );
                                    let handle_rect = egui::Rect::from_min_size(
                                        egui::pos2(header_rect.left() + 96.0, header_rect.top()),
                                        egui::vec2(6.0, 22.0),
                                    );
                                    let resp = ui.interact(
                                        handle_rect,
                                        egui::Id::new("smoke_col_sep"),
                                        egui::Sense::drag(),
                                    );
                                    if resp.dragged() || resp.drag_stopped() {
                                        self.w += resp.drag_delta().x;
                                    }
                                    ui.put(
                                        egui::Rect::from_min_size(
                                            egui::pos2(
                                                header_rect.left() + 108.0,
                                                header_rect.top(),
                                            ),
                                            egui::vec2(130.0, 22.0),
                                        ),
                                        egui::Label::new("运营商"),
                                    );
                                    for _ in 0..28 {
                                        ui.label("row");
                                    }
                                });
                        });
                });
            }
        }
        let mut harness = Harness::builder()
            .with_size(egui::vec2(560.0, 420.0))
            .build_eframe(|_cc| DragApp { w: 90.0 });
        harness.run_steps(10);
        // The handle sits after the 90 px first cell: left edge ~8 px,
        // centre ~(107, 19).
        let sep = egui::pos2(107.0, 19.0);
        harness.hover_at(sep);
        harness.run();
        harness.drag_at(sep);
        harness.run();
        let target = egui::pos2(sep.x + 40.0, sep.y);
        harness.hover_at(target);
        harness.run();
        harness.drop_at(target);
        harness.run();
        assert!(
            harness.state().w >= 120.0,
            "bare drag handle should widen: w={}",
            harness.state().w
        );
    }

    /// Drag the separator after the header cell with `label` right by
    /// `delta` px and return the column widths afterwards. The
    /// separator centre is one item gap (6 px) past the cell's right
    /// edge plus half the 6 px handle: left + width + 9.
    fn drag_header_separator(
        harness: &mut egui_kittest::Harness<GuiApp>,
        label: &str,
        wi: usize,
        delta: f32,
    ) -> [u16; 6] {
        use egui_kittest::kittest::Queryable;
        let w = harness.state().col_widths[wi];
        let header = harness.get_by_label(label);
        let r = header.rect();
        let sep = egui::pos2(r.left() + w as f32 + 9.0, r.center().y);
        harness.hover_at(sep);
        harness.run();
        harness.drag_at(sep);
        harness.run();
        // Move while the button is held (a real drag sends PointerMoved
        // events between press and release; drop_at alone only sends
        // the release event, whose delta the release frame would miss).
        let target = egui::pos2(sep.x + delta, sep.y);
        harness.hover_at(target);
        harness.run();
        harness.drop_at(target);
        harness.run();
        harness.state().col_widths
    }

    /// Dragging a header separator widens that column (needs a graphics
    /// adapter, like the render test). Every resizable column but the
    /// last has a separator: 国家/地区， 运营商， 当前人数， 线路质量，
    /// 主机:端口 — dragging one widens exactly that column and leaves
    /// the other five alone, and the widths persist through the
    /// config round-trip.
    #[test]
    #[ignore = "drives the UI with kittest; needs a graphics adapter"]
    fn column_separator_drag_widens_column() {
        use egui_kittest::Harness;
        // 800 px, not the 560 px minimum: at the default widths the
        // row is 788 px wide, so the 主机:端口 separator (x ≈ 689)
        // is off-screen at 560 px and cannot be grabbed. Dragging
        // right-to-left keeps the not-yet-dragged separators at
        // their initial positions (a drag only shifts columns to
        // its right).
        let mut harness = Harness::builder()
            .with_size(egui::vec2(800.0, 420.0))
            .build_eframe(|cc| GuiApp::for_render_tests(cc));
        harness.run_steps(10);
        assert_eq!(
            harness.state().col_widths,
            [90, 130, 64, 88, 190, 90],
            "defaults before any drag"
        );
        for (label, wi) in [
            ("主机:端口", 4usize),
            ("线路质量", 3usize),
            ("当前人数", 2usize),
            ("运营商", 1usize),
            ("国家/地区", 0usize),
        ] {
            let before = harness.state().col_widths;
            let after = drag_header_separator(&mut harness, label, wi, 40.0);
            assert!(
                after[wi] >= before[wi] + 30,
                "dragging the separator after {} should widen col_widths[{}]: {} -> {}",
                label,
                wi,
                before[wi],
                after[wi]
            );
            for (j, (&b, &a)) in before.iter().zip(after.iter()).enumerate() {
                if j != wi {
                    assert_eq!(
                        b, a,
                        "dragging {} must not move col_widths[{}]: {} -> {}",
                        label, j, b, a
                    );
                }
            }
        }
        // The dragged widths persist: current_config carries them,
        // and a fresh app built from that config shows the same
        // widths.
        let cfg = harness.state().current_config();
        assert_eq!(cfg.ui.node_col_widths, harness.state().col_widths);
        let mut app2 = GuiApp::for_tests();
        app2.apply_config(&cfg);
        assert_eq!(app2.col_widths, harness.state().col_widths);
    }

    /// A plain click selects the row, and a press that drifts 8 px
    /// (past egui's 6 px max_click_dist) and releases on the same row
    /// still selects it — the tolerant-click fix for "rows are hard
    /// to select". Needs a graphics adapter, like the render test.
    #[test]
    #[ignore = "drives the UI with kittest; needs a graphics adapter"]
    fn row_click_selects_and_drag_tolerance_selects() {
        use egui_kittest::{kittest::Queryable, Harness};
        // Taller than the 560x420 minimum so all three test rows (and
        // the selected-row screenshot at the end) fit the list.
        let mut harness = Harness::builder()
            .with_size(egui::vec2(560.0, 600.0))
            .build_eframe(|cc| {
                let mut app = GuiApp::for_render_tests(cc);
                app.nodes = vec![
                    node_in("a.opengw.net", "日本"),
                    node_in("b.opengw.net", "美国"),
                    node_in("c.opengw.net", "德国"),
                ];
                app
            });
        harness.run_steps(10);
        // Plain click on the 美国 row's country cell.
        let cell = harness.get_by_label("美国");
        let center = cell.rect().center();
        harness.drag_at(center);
        harness.run();
        harness.drop_at(center);
        harness.run();
        assert_eq!(
            harness.state().selected,
            Some(1),
            "clicking a row must select it"
        );
        // Drag tolerance: press on the 德国 row, drift 8 px down
        // (past egui's 6 px click limit), release on the same row.
        // Vertical drift keeps the row rect still (no vertical
        // overflow to scroll), so the release lands in the row.
        let cell = harness.get_by_label("德国");
        let start = cell.rect().center();
        harness.drag_at(start);
        harness.run();
        let moved = egui::pos2(start.x, start.y + 8.0);
        harness.hover_at(moved);
        harness.run();
        harness.drop_at(moved);
        harness.run();
        assert_eq!(
            harness.state().selected,
            Some(2),
            "an 8 px press-drag-release on a row must still select it"
        );
        // Save a screenshot of the selected row for visual review
        // (white text on the indigo selection fill).
        let path = std::env::temp_dir().join("ui-review-selected-row.png");
        harness
            .render()
            .expect("needs a graphics adapter")
            .save(&path)
            .unwrap();
        eprintln!("{}", path.display());
    }

    /// Ctrl+click multi-select, then the 删除选中 button: clicking
    /// one row selects it, Ctrl+clicking a second row adds it to the
    /// set, and 删除选中 removes both rows and records them on the
    /// blocklist; 恢复已删除 puts them back. Needs a graphics
    /// adapter, like the render test.
    #[test]
    #[ignore = "drives the UI with kittest; needs a graphics adapter"]
    fn ctrl_click_multi_select_and_delete_button() {
        use egui_kittest::{kittest::Queryable, Harness};
        let mut harness = Harness::builder()
            .with_size(egui::vec2(800.0, 600.0))
            .build_eframe(|cc| {
                let mut app = GuiApp::for_render_tests(cc);
                app.nodes = vec![
                    node_in("a.opengw.net", "日本"),
                    node_in("b.opengw.net", "美国"),
                    node_in("c.opengw.net", "德国"),
                ];
                app
            });
        harness.run_steps(10);
        // Plain click on the 日本 row.
        let cell = harness.get_by_label("日本");
        let center = cell.rect().center();
        harness.drag_at(center);
        harness.run();
        harness.drop_at(center);
        harness.run();
        assert_eq!(harness.state().selected, Some(0));
        assert_eq!(
            harness.state().selected_set,
            BTreeSet::from([0]),
            "plain click selects one row"
        );
        // Ctrl+click on the 美国 row: egui only tracks modifiers
        // from ModifiersChanged events, so queue one before the
        // press and again before the release (the row code reads
        // the modifiers on the release frame).
        let ctrl = egui::Modifiers {
            ctrl: true,
            ..Default::default()
        };
        let cell = harness.get_by_label("美国");
        let center = cell.rect().center();
        harness.event(egui::Event::ModifiersChanged(ctrl));
        harness.drag_at(center);
        harness.run();
        harness.event(egui::Event::ModifiersChanged(ctrl));
        harness.drop_at(center);
        harness.run();
        harness.event(egui::Event::ModifiersChanged(egui::Modifiers::default()));
        harness.run();
        assert_eq!(harness.state().selected, Some(1));
        assert_eq!(
            harness.state().selected_set,
            BTreeSet::from([0, 1]),
            "Ctrl+click must add the row to the set, not replace it"
        );
        // The delete button carries the count; clicking it removes
        // both rows and blocks their hostnames.
        let btn = harness.get_by_label("删除选中 (2)");
        let center = btn.rect().center();
        harness.drag_at(center);
        harness.run();
        harness.drop_at(center);
        harness.run();
        assert_eq!(harness.state().nodes.len(), 1);
        assert_eq!(harness.state().nodes[0].host, "c.opengw.net");
        assert_eq!(harness.state().blocked_hosts.len(), 2);
        assert!(harness.state().selected_set.is_empty());
        // The restore button appears with the blocked count; clicking
        // it puts this session's deleted nodes back.
        let btn = harness.get_by_label("恢复已删除 (2)");
        let center = btn.rect().center();
        harness.drag_at(center);
        harness.run();
        harness.drop_at(center);
        harness.run();
        assert_eq!(harness.state().nodes.len(), 3);
        assert!(harness.state().blocked_hosts.is_empty());
    }

    /// Shift+click selects the whole range from the anchor to the
    /// clicked row (display order), and arrow keys still collapse
    /// the multi-select to the row the anchor moved to. Needs a
    /// graphics adapter, like the render test.
    #[test]
    #[ignore = "drives the UI with kittest; needs a graphics adapter"]
    fn shift_click_selects_range() {
        use egui_kittest::{kittest::Queryable, Harness};
        let mut harness = Harness::builder()
            .with_size(egui::vec2(800.0, 600.0))
            .build_eframe(|cc| {
                let mut app = GuiApp::for_render_tests(cc);
                app.nodes = vec![
                    node_in("a.opengw.net", "日本"),
                    node_in("b.opengw.net", "美国"),
                    node_in("c.opengw.net", "德国"),
                    node_in("d.opengw.net", "法国"),
                    node_in("e.opengw.net", "英国"),
                ];
                app
            });
        harness.run_steps(10);
        let shift = egui::Modifiers {
            shift: true,
            ..Default::default()
        };
        // Shift+click helper: queue ModifiersChanged(shift) before
        // the press and again before the release, mirroring the
        // Ctrl+click test (the row code reads the modifiers on the
        // release frame).
        let shift_click = |harness: &mut egui_kittest::Harness<'_, GuiApp>, label: &str| {
            let cell = harness.get_by_label(label);
            let center = cell.rect().center();
            harness.event(egui::Event::ModifiersChanged(shift));
            harness.drag_at(center);
            harness.run();
            harness.event(egui::Event::ModifiersChanged(shift));
            harness.drop_at(center);
            harness.run();
            harness.event(egui::Event::ModifiersChanged(egui::Modifiers::default()));
            harness.run();
        };
        // Plain click on the 日本 row: anchor 0, set {0}.
        let cell = harness.get_by_label("日本");
        let center = cell.rect().center();
        harness.drag_at(center);
        harness.run();
        harness.drop_at(center);
        harness.run();
        assert_eq!(harness.state().selected, Some(0));
        assert_eq!(harness.state().selected_set, BTreeSet::from([0]));
        // Shift+click on the 德国 row: the range 0..=2.
        shift_click(&mut harness, "德国");
        assert_eq!(harness.state().selected, Some(2));
        assert_eq!(
            harness.state().selected_set,
            BTreeSet::from([0, 1, 2]),
            "Shift+click must select the range from the anchor"
        );
        // Shift+click on the 英国 row: the anchor moved to 德国 on
        // the previous click, so the range is 2..=4 — every click
        // moves the anchor (see `click_row`), a second Shift+click
        // re-anchors rather than extending from the first one.
        shift_click(&mut harness, "英国");
        assert_eq!(harness.state().selected, Some(4));
        assert_eq!(
            harness.state().selected_set,
            BTreeSet::from([2, 3, 4]),
            "Shift+click selects the range from the current anchor"
        );
        // Plain click on the 美国 row collapses to one row.
        let cell = harness.get_by_label("美国");
        let center = cell.rect().center();
        harness.drag_at(center);
        harness.run();
        harness.drop_at(center);
        harness.run();
        assert_eq!(harness.state().selected_set, BTreeSet::from([1]));
        // Shift+click on the 法国 row: the range 1..=3.
        shift_click(&mut harness, "法国");
        assert_eq!(harness.state().selected, Some(3));
        assert_eq!(
            harness.state().selected_set,
            BTreeSet::from([1, 2, 3]),
            "Shift+click after a plain click starts from the new anchor"
        );
        // Arrow key ↓ moves the anchor and collapses the set to it.
        harness.key_press(egui::Key::ArrowDown);
        harness.run();
        assert_eq!(harness.state().selected, Some(4));
        assert_eq!(
            harness.state().selected_set,
            BTreeSet::from([4]),
            "arrow keys must still collapse the multi-select to one row"
        );
    }

    /// Shift+click with no anchor yet (the selection was cleared by
    /// a delete) selects just that row and makes it the anchor.
    /// Needs a graphics adapter, like the render test.
    #[test]
    #[ignore = "drives the UI with kittest; needs a graphics adapter"]
    fn shift_click_without_anchor_selects_single_row() {
        use egui_kittest::{kittest::Queryable, Harness};
        let mut harness = Harness::builder()
            .with_size(egui::vec2(800.0, 600.0))
            .build_eframe(|cc| {
                let mut app = GuiApp::for_render_tests(cc);
                app.nodes = vec![
                    node_in("a.opengw.net", "日本"),
                    node_in("b.opengw.net", "美国"),
                    node_in("c.opengw.net", "德国"),
                ];
                app
            });
        harness.run_steps(10);
        // Plain click on 日本, then delete it: the selection (and
        // the anchor) is cleared, leaving [美国， 德国].
        let cell = harness.get_by_label("日本");
        let center = cell.rect().center();
        harness.drag_at(center);
        harness.run();
        harness.drop_at(center);
        harness.run();
        let btn = harness.get_by_label("删除选中 (1)");
        let center = btn.rect().center();
        harness.drag_at(center);
        harness.run();
        harness.drop_at(center);
        harness.run();
        assert_eq!(harness.state().nodes.len(), 2);
        assert_eq!(harness.state().selected, None);
        assert!(harness.state().selected_set.is_empty());
        // Shift+click on the first remaining row (美国） with no
        // anchor: just that row, and it becomes the anchor.
        let shift = egui::Modifiers {
            shift: true,
            ..Default::default()
        };
        let cell = harness.get_by_label("美国");
        let center = cell.rect().center();
        harness.event(egui::Event::ModifiersChanged(shift));
        harness.drag_at(center);
        harness.run();
        harness.event(egui::Event::ModifiersChanged(shift));
        harness.drop_at(center);
        harness.run();
        harness.event(egui::Event::ModifiersChanged(egui::Modifiers::default()));
        harness.run();
        assert_eq!(harness.state().selected, Some(0));
        assert_eq!(
            harness.state().selected_set,
            BTreeSet::from([0]),
            "Shift+click with no anchor must fall back to a plain click"
        );
    }

    /// Shift+click still selects the range even when the user lets
    /// go of Shift before releasing the mouse button — the
    /// modifiers are latched at press time. Needs a graphics
    /// adapter, like the render test.
    #[test]
    #[ignore = "drives the UI with kittest; needs a graphics adapter"]
    fn shift_click_survives_early_shift_release() {
        use egui_kittest::{kittest::Queryable, Harness};
        let mut harness = Harness::builder()
            .with_size(egui::vec2(800.0, 600.0))
            .build_eframe(|cc| {
                let mut app = GuiApp::for_render_tests(cc);
                app.nodes = vec![
                    node_in("a.opengw.net", "日本"),
                    node_in("b.opengw.net", "美国"),
                    node_in("c.opengw.net", "德国"),
                ];
                app
            });
        harness.run_steps(10);
        // Plain click on 日本: anchor 0, set {0}.
        let cell = harness.get_by_label("日本");
        let center = cell.rect().center();
        harness.drag_at(center);
        harness.run();
        harness.drop_at(center);
        harness.run();
        assert_eq!(harness.state().selected_set, BTreeSet::from([0]));
        // Shift+click on 德国， but release Shift before the mouse
        // button: ModifiersChanged(shift) only before the press,
        // ModifiersChanged(default) before the release.
        let shift = egui::Modifiers {
            shift: true,
            ..Default::default()
        };
        let cell = harness.get_by_label("德国");
        let center = cell.rect().center();
        harness.event(egui::Event::ModifiersChanged(shift));
        harness.drag_at(center);
        harness.run();
        harness.event(egui::Event::ModifiersChanged(egui::Modifiers::default()));
        harness.drop_at(center);
        harness.run();
        assert_eq!(harness.state().selected, Some(2));
        assert_eq!(
            harness.state().selected_set,
            BTreeSet::from([0, 1, 2]),
            "releasing Shift before the mouse button must not lose the range"
        );
    }

    /// End-to-end: Shift+click a range, then "删除选中" removes
    /// exactly those rows and records their hosts on the blocklist.
    /// This is the flow Leo reported as broken in v3.7 (Shift+click
    /// selected a range, but delete removed nothing): it covers both
    /// halves — the range must actually be selected (anchor +
    /// modifiers), and the delete button must read the same set the
    /// highlight shows. Needs a graphics adapter, like the render test.
    #[test]
    #[ignore = "drives the UI with kittest; needs a graphics adapter"]
    fn shift_click_then_delete_removes_range() {
        use egui_kittest::{kittest::Queryable, Harness};
        let mut harness = Harness::builder()
            .with_size(egui::vec2(800.0, 600.0))
            .build_eframe(|cc| {
                let mut app = GuiApp::for_render_tests(cc);
                app.nodes = vec![
                    node_in("a.opengw.net", "日本"),
                    node_in("b.opengw.net", "美国"),
                    node_in("c.opengw.net", "德国"),
                    node_in("d.opengw.net", "法国"),
                    node_in("e.opengw.net", "英国"),
                ];
                app
            });
        harness.run_steps(10);
        // Plain click on 日本 sets the anchor; Shift+click on 法国
        // selects the range 0..=3.
        click_row_label(&mut harness, "日本");
        assert_eq!(harness.state().selected, Some(0));
        shift_click_row_label(&mut harness, "法国");
        assert_eq!(harness.state().selected, Some(3));
        assert_eq!(
            harness.state().selected_set,
            BTreeSet::from([0, 1, 2, 3]),
            "Shift+click must select the range from the anchor"
        );
        // The delete button carries the count of the same set the
        // highlight shows — if it read a different state, the count
        // here would be wrong.
        let _btn = harness.get_by_label("删除选中 (4)");
        // Delete: the four rows go, their hosts land on the
        // blocklist, the selection is cleared, and only 英国 is left.
        click_button_label(&mut harness, "删除选中 (4)");
        assert_eq!(harness.state().nodes.len(), 1);
        assert_eq!(harness.state().nodes[0].host, "e.opengw.net");
        assert_eq!(
            harness.state().blocked_hosts.len(),
            4,
            "all four deleted hosts must be blocked, got: {:?}",
            harness.state().blocked_hosts
        );
        for host in ["a.opengw.net", "b.opengw.net", "c.opengw.net", "d.opengw.net"] {
            assert!(
                harness.state().blocked_hosts.contains(host),
                "blocked_hosts must contain {}, got: {:?}",
                host,
                harness.state().blocked_hosts
            );
        }
        assert!(harness.state().selected_set.is_empty());
        assert_eq!(harness.state().selected, None);
    }

    /// End-to-end, no-anchor variant: Shift+click as the very first
    /// click selects just that row, and "删除选中" removes it.
    /// Needs a graphics adapter, like the render test.
    #[test]
    #[ignore = "drives the UI with kittest; needs a graphics adapter"]
    fn shift_click_first_then_delete_removes_single_row() {
        use egui_kittest::{kittest::Queryable, Harness};
        let mut harness = Harness::builder()
            .with_size(egui::vec2(800.0, 600.0))
            .build_eframe(|cc| {
                let mut app = GuiApp::for_render_tests(cc);
                app.nodes = vec![
                    node_in("a.opengw.net", "日本"),
                    node_in("b.opengw.net", "美国"),
                    node_in("c.opengw.net", "德国"),
                ];
                app
            });
        harness.run_steps(10);
        // No anchor yet: Shift+click on 德国 falls back to that row.
        shift_click_row_label(&mut harness, "德国");
        assert_eq!(harness.state().selected, Some(2));
        assert_eq!(
            harness.state().selected_set,
            BTreeSet::from([2]),
            "Shift+click with no anchor must select just that row"
        );
        let _btn = harness.get_by_label("删除选中 (1)");
        click_button_label(&mut harness, "删除选中 (1)");
        assert_eq!(harness.state().nodes.len(), 2);
        assert_eq!(harness.state().nodes[0].host, "a.opengw.net");
        assert_eq!(harness.state().nodes[1].host, "b.opengw.net");
        assert!(harness.state().blocked_hosts.contains("c.opengw.net"));
        assert!(harness.state().selected_set.is_empty());
        assert_eq!(harness.state().selected, None);
    }

    /// End-to-end, early-release variant: the same range-select +
    /// delete flow, but Shift is released before the mouse button
    /// on the Shift+click — the gesture Leo actually makes. Without
    /// the press-time modifier latch the range would be lost here
    /// and delete would have nothing to remove. Needs a graphics
    /// adapter, like the render test.
    #[test]
    #[ignore = "drives the UI with kittest; needs a graphics adapter"]
    fn shift_click_early_release_then_delete_removes_range() {
        use egui_kittest::{kittest::Queryable, Harness};
        let mut harness = Harness::builder()
            .with_size(egui::vec2(800.0, 600.0))
            .build_eframe(|cc| {
                let mut app = GuiApp::for_render_tests(cc);
                app.nodes = vec![
                    node_in("a.opengw.net", "日本"),
                    node_in("b.opengw.net", "美国"),
                    node_in("c.opengw.net", "德国"),
                    node_in("d.opengw.net", "法国"),
                    node_in("e.opengw.net", "英国"),
                ];
                app
            });
        harness.run_steps(10);
        click_row_label(&mut harness, "日本");
        shift_click_row_label_early_release(&mut harness, "法国");
        assert_eq!(harness.state().selected, Some(3));
        assert_eq!(
            harness.state().selected_set,
            BTreeSet::from([0, 1, 2, 3]),
            "the range must survive Shift being released before the mouse button"
        );
        let _btn = harness.get_by_label("删除选中 (4)");
        click_button_label(&mut harness, "删除选中 (4)");
        assert_eq!(harness.state().nodes.len(), 1);
        assert_eq!(harness.state().nodes[0].host, "e.opengw.net");
        assert_eq!(harness.state().blocked_hosts.len(), 4);
        assert!(harness.state().selected_set.is_empty());
        assert_eq!(harness.state().selected, None);
    }

    /// Clicking the 国家/地区 header toggles the sort A-Z / Z-A.
    /// Needs a graphics adapter, like the render test.
    #[test]
    #[ignore = "drives the UI with kittest; needs a graphics adapter"]
    fn country_header_click_toggles_sort() {
        use egui_kittest::{kittest::Queryable, Harness};
        let mut harness = Harness::builder()
            .with_size(egui::vec2(560.0, 420.0))
            .build_eframe(|cc| {
                let mut app = GuiApp::for_render_tests(cc);
                app.nodes = vec![
                    node_in("b.opengw.net", "美国"),
                    node_in("a.opengw.net", "日本"),
                ];
                app
            });
        harness.run_steps(10);
        assert_eq!(harness.state().sort, None);
        let header = harness.get_by_label("国家/地区");
        let center = header.rect().center();
        harness.drag_at(center);
        harness.run();
        harness.drop_at(center);
        harness.run();
        assert_eq!(
            harness.state().sort,
            Some((SortCol::Country, SortDir::Asc)),
            "first header click sorts A-Z"
        );
        assert_eq!(
            harness.state().display_indices(),
            vec![1, 0],
            "日本 before 美国 in A-Z order"
        );
        harness.drag_at(center);
        harness.run();
        harness.drop_at(center);
        harness.run();
        assert_eq!(
            harness.state().sort,
            Some((SortCol::Country, SortDir::Desc)),
            "second header click sorts Z-A"
        );
        assert_eq!(harness.state().display_indices(), vec![0, 1]);
    }

    /// Clicking the 当前人数 / 线路质量 headers sorts by that column
    /// (first click = the column's most useful order, second click =
    /// reversed), and switching to the country header leaves the
    /// arrow only there. Needs a graphics adapter.
    #[test]
    #[ignore = "drives the UI with kittest; needs a graphics adapter"]
    fn sort_headers_toggle_and_switch_clears_arrow() {
        use egui_kittest::{kittest::Queryable, Harness};
        let mut harness = Harness::builder()
            .with_size(egui::vec2(560.0, 420.0))
            .build_eframe(|cc| {
                let mut app = GuiApp::for_render_tests(cc);
                app.nodes = vec![
                    node_stats("a.opengw.net", "日本", 47, 500, 10),
                    node_stats("b.opengw.net", "美国", 5, 100, 5),
                    node_stats("c.opengw.net", "德国", 301, 200, 20),
                ];
                app
            });
        harness.run_steps(10);
        assert_eq!(harness.state().sort, None);
        // 当前人数, first click: fewest first (b 5, a 47, c 301).
        let sessions_header = harness.get_by_label("当前人数");
        let sessions_center = sessions_header.rect().center();
        harness.drag_at(sessions_center);
        harness.run();
        harness.drop_at(sessions_center);
        harness.run();
        assert_eq!(
            harness.state().sort,
            Some((SortCol::Sessions, SortDir::Asc)),
            "first 当前人数 click sorts few-first"
        );
        assert_eq!(harness.state().display_indices(), vec![1, 0, 2]);
        // Same header again: most first.
        harness.drag_at(sessions_center);
        harness.run();
        harness.drop_at(sessions_center);
        harness.run();
        assert_eq!(
            harness.state().sort,
            Some((SortCol::Sessions, SortDir::Desc)),
            "second 当前人数 click sorts most-first"
        );
        assert_eq!(harness.state().display_indices(), vec![2, 0, 1]);
        // 线路质量, first click: fastest first (a 500, c 200, b 100).
        let quality_header = harness.get_by_label("线路质量");
        let quality_center = quality_header.rect().center();
        harness.drag_at(quality_center);
        harness.run();
        harness.drop_at(quality_center);
        harness.run();
        assert_eq!(
            harness.state().sort,
            Some((SortCol::Quality, SortDir::Desc)),
            "first 线路质量 click sorts fastest-first"
        );
        assert_eq!(harness.state().display_indices(), vec![0, 2, 1]);
        // 国家/地区: the arrow moves there alone (德国 < 日本 < 美国）.
        let country_header = harness.get_by_label("国家/地区");
        let country_center = country_header.rect().center();
        harness.drag_at(country_center);
        harness.run();
        harness.drop_at(country_center);
        harness.run();
        assert_eq!(
            harness.state().sort,
            Some((SortCol::Country, SortDir::Asc)),
            "switching to the country header starts A-Z"
        );
        assert_eq!(harness.state().display_indices(), vec![2, 0, 1]);
        harness.get_by_label("国家/地区 ▲");
        assert!(
            harness.query_by_label("当前人数 ▲").is_none(),
            "the sessions arrow must clear when another column sorts"
        );
        assert!(
            harness.query_by_label("当前人数 ▼").is_none(),
            "the sessions arrow must clear when another column sorts"
        );
        assert!(
            harness.query_by_label("线路质量 ▲").is_none(),
            "the quality arrow must clear when another column sorts"
        );
        assert!(
            harness.query_by_label("线路质量 ▼").is_none(),
            "the quality arrow must clear when another column sorts"
        );
    }

    /// Clicking the 延迟/状态 header sorts available nodes by ms
    /// ascending (unprobed middle, failed last) on the first click
    /// and reverses on the second. The header sits past the right
    /// edge at 560 px with the default column widths, so the test
    /// narrows the resizable columns to bring it into view. Needs
    /// a graphics adapter.
    #[test]
    #[ignore = "drives the UI with kittest; needs a graphics adapter"]
    fn latency_header_click_toggles_sort() {
        use egui_kittest::{kittest::Queryable, Harness};
        let mut harness = Harness::builder()
            .with_size(egui::vec2(560.0, 420.0))
            .build_eframe(|cc| {
                let mut app = GuiApp::for_render_tests(cc);
                app.nodes = vec![
                    node_stats("a.opengw.net", "日本", 10, 500, 10),
                    node_stats("b.opengw.net", "日本", 10, 100, 5),
                    node_stats("c.opengw.net", "日本", 10, 200, 20),
                ];
                app.col_widths = [50, 60, 40, 50, 80, 50];
                app
            });
        harness.run_steps(10);
        assert_eq!(harness.state().sort, None);
        // a: 120 ms, b: unprobed, c: failed.
        harness
            .state_mut()
            .probe_ms
            .insert("a.opengw.net".to_string(), 120);
        harness
            .state_mut()
            .probes
            .insert("a.opengw.net".to_string(), "120 ms".to_string());
        harness
            .state_mut()
            .probes
            .insert("c.opengw.net".to_string(), "失败".to_string());
        let header = harness.get_by_label("延迟/状态");
        let center = header.rect().center();
        harness.drag_at(center);
        harness.run();
        harness.drop_at(center);
        harness.run();
        assert_eq!(
            harness.state().sort,
            Some((SortCol::Latency, SortDir::Asc)),
            "first latency click: available by ms, unprobed middle, failed last"
        );
        assert_eq!(harness.state().display_indices(), vec![0, 1, 2]);
        harness.drag_at(center);
        harness.run();
        harness.drop_at(center);
        harness.run();
        assert_eq!(
            harness.state().sort,
            Some((SortCol::Latency, SortDir::Desc)),
            "second latency click reverses the whole order"
        );
        assert_eq!(harness.state().display_indices(), vec![2, 1, 0]);
    }

    /// Enter connects the selected node and Ctrl+R probes it (the
    /// same flows as the buttons). The harness has no tokio runtime,
    /// so the background task is skipped and the busy flag stays set
    /// — that is what is asserted. Needs a graphics adapter.
    #[test]
    #[ignore = "drives the UI with kittest; needs a graphics adapter"]
    fn keyboard_shortcuts_connect_and_probe() {
        use egui_kittest::{kittest::Queryable, Harness};
        let mut harness = Harness::builder()
            .with_size(egui::vec2(560.0, 420.0))
            .build_eframe(|cc| {
                let mut app = GuiApp::for_render_tests(cc);
                app.nodes = vec![node_in("a.opengw.net", "日本")];
                app
            });
        harness.run_steps(10);
        // No selection yet: Enter must do nothing.
        harness.key_press(egui::Key::Enter);
        harness.run_steps(3);
        assert!(harness.state().busy.is_none());
        // Select the row, then Enter: connect starts.
        let cell = harness.get_by_label("日本");
        let center = cell.rect().center();
        harness.drag_at(center);
        harness.run();
        harness.drop_at(center);
        harness.run();
        assert_eq!(harness.state().selected, Some(0));
        harness.key_press(egui::Key::Enter);
        // run_steps, not run(): the spinner shown while busy requests
        // a repaint every frame, so run() never settles.
        harness.run_steps(3);
        assert!(
            matches!(harness.state().busy, Some(Busy::Connecting)),
            "Enter must start a connect, busy={:?}",
            harness.state().busy
        );
        // Reset the busy flag (the harness never clears it — no
        // runtime, no background task), then Ctrl+R: probe starts.
        harness.state_mut().busy = None;
        harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::R);
        harness.run_steps(3);
        assert!(
            matches!(harness.state().busy, Some(Busy::Probing)),
            "Ctrl+R must start a probe, busy={:?}",
            harness.state().busy
        );
    }

    /// ↑/↓ move the selection in display order, stop at the ends,
    /// and scroll the newly selected row into view; Enter/Ctrl+R
    /// still act on the arrow-selected row, and moving works while
    /// busy. Needs a graphics adapter, like the render test.
    #[test]
    #[ignore = "drives the UI with kittest; needs a graphics adapter"]
    fn arrow_keys_move_selection_and_follow_scroll() {
        use egui_kittest::Harness;
        let mut harness = Harness::builder()
            .with_size(egui::vec2(560.0, 420.0))
            .build_eframe(|cc| {
                let mut app = GuiApp::for_render_tests(cc);
                // Ten rows overflow the ~120 px list at 560x420
                // (about three rows fit), so walking to the bottom
                // must scroll.
                app.nodes = (0..10)
                    .map(|i| node_in(&format!("n{i}.opengw.net"), "日本"))
                    .collect();
                app
            });
        harness.run_steps(10);
        assert_eq!(harness.state().selected, None);
        // ↓ with no selection picks the first row; ↑ at the top
        // stays put.
        harness.key_press(egui::Key::ArrowDown);
        harness.run();
        assert_eq!(harness.state().selected, Some(0));
        harness.key_press(egui::Key::ArrowUp);
        harness.run();
        assert_eq!(
            harness.state().selected,
            Some(0),
            "↑ at the top must not move"
        );
        // ↓ twice, ↑ once.
        harness.key_press(egui::Key::ArrowDown);
        harness.run();
        harness.key_press(egui::Key::ArrowDown);
        harness.run();
        assert_eq!(harness.state().selected, Some(2));
        harness.key_press(egui::Key::ArrowUp);
        harness.run();
        assert_eq!(harness.state().selected, Some(1));
        // Walk to the bottom: the list must scroll and the last row
        // must end up on screen.
        for _ in 0..8 {
            harness.key_press(egui::Key::ArrowDown);
            harness.run();
        }
        assert_eq!(harness.state().selected, Some(9));
        assert!(
            harness.state().node_list_scroll_y > 0.0,
            "the list must have scrolled, scroll_y={}",
            harness.state().node_list_scroll_y
        );
        assert!(
            harness.state().selected_row_in_view,
            "the selected last row must be in view after arrowing down"
        );
        // ↓ at the bottom stays put.
        harness.key_press(egui::Key::ArrowDown);
        harness.run();
        assert_eq!(harness.state().selected, Some(9));
        // Moving the selection still works while busy (it starts
        // nothing). run_steps, not run(): the busy spinner requests
        // a repaint every frame, so run() never settles.
        harness.state_mut().busy = Some(Busy::Fetching);
        harness.key_press(egui::Key::ArrowUp);
        harness.run_steps(5);
        assert_eq!(
            harness.state().selected,
            Some(8),
            "↑ must move the selection even while busy"
        );
        harness.state_mut().busy = None;
        // Enter still connects the arrow-selected row, Ctrl+R still
        // probes it (same busy-flag proof as the shortcut test).
        harness.key_press(egui::Key::Enter);
        harness.run_steps(3);
        assert!(
            matches!(harness.state().busy, Some(Busy::Connecting)),
            "Enter must start a connect on the arrow-selected row, busy={:?}",
            harness.state().busy
        );
        harness.state_mut().busy = None;
        harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::R);
        harness.run_steps(3);
        assert!(
            matches!(harness.state().busy, Some(Busy::Probing)),
            "Ctrl+R must start a probe on the arrow-selected row, busy={:?}",
            harness.state().busy
        );
    }

    /// Clicking 探测全部 starts a sweep (busy = ProbingAll, first
    /// row 探测中…), one driven result advances the progress, Enter
    /// is blocked while sweeping but ↑/↓ still move the selection,
    /// and 停止 sets the stop flag so the Done event ends it with
    /// the stopped summary. The harness has no tokio runtime, so
    /// the sweep's events are driven by hand through apply_event
    /// (same approach as the shortcut test's busy-flag proof).
    /// Needs a graphics adapter, like the render test.
    #[test]
    #[ignore = "drives the UI with kittest; needs a graphics adapter"]
    fn probe_all_button_starts_sweep_and_stop_halts_it() {
        use egui_kittest::{kittest::Queryable, Harness};
        let mut harness = Harness::builder()
            .with_size(egui::vec2(560.0, 420.0))
            .build_eframe(|cc| {
                let mut app = GuiApp::for_render_tests(cc);
                app.nodes = vec![
                    node_in("a.opengw.net", "日本"),
                    node_in("b.opengw.net", "美国"),
                    node_in("c.opengw.net", "德国"),
                ];
                app
            });
        harness.run_steps(10);
        assert!(harness.state().busy.is_none());
        let btn = harness.get_by_label("探测全部");
        let center = btn.rect().center();
        harness.drag_at(center);
        harness.run_steps(3);
        harness.drop_at(center);
        // run_steps, not run(): the spinner shown while busy
        // requests a repaint every frame, so run() never settles.
        harness.run_steps(3);
        assert!(
            matches!(harness.state().busy, Some(Busy::ProbingAll)),
            "clicking 探测全部 must start a sweep, busy={:?}",
            harness.state().busy
        );
        assert!(
            harness.state().status.contains("0/3"),
            "status must show the sweep progress: {}",
            harness.state().status
        );
        assert_eq!(
            harness.state().probes.get("a.opengw.net").map(|s| s.as_str()),
            Some("探测中…"),
            "the first row shows 探测中…"
        );
        // The same button is now 停止.
        let stop_btn = harness.get_by_label("停止");
        assert!(
            stop_btn.rect().width() > 0.0,
            "the button must read 停止 while sweeping"
        );
        // Drive one result through by hand: the row takes the
        // latency and the progress advances.
        harness.state_mut().apply_event(GuiEvent::ProbeAllResult {
            host: "a.opengw.net".to_string(),
            result: Ok((Ipv4Addr::new(10, 0, 0, 2), Duration::from_millis(95))),
            done: 1,
            total: 3,
        });
        harness.run_steps(3);
        assert_eq!(
            harness.state().probes.get("a.opengw.net").map(|s| s.as_str()),
            Some("95 ms")
        );
        assert!(
            harness.state().status.contains("1/3"),
            "status must advance: {}",
            harness.state().status
        );
        // Enter is blocked while sweeping (it would start a
        // connect); arrow keys still move the selection (they
        // start nothing).
        harness.key_press(egui::Key::Enter);
        harness.run_steps(3);
        assert!(
            matches!(harness.state().busy, Some(Busy::ProbingAll)),
            "Enter must not start a connect mid-sweep"
        );
        harness.key_press(egui::Key::ArrowDown);
        harness.run_steps(3);
        assert_eq!(
            harness.state().selected,
            Some(0),
            "↓ must still move the selection while sweeping"
        );
        // Click 停止: the flag is set (the sweep stops after the
        // node in flight); the Done event clears the busy flag
        // and writes the stopped summary.
        let stop = harness.get_by_label("停止");
        let center = stop.rect().center();
        harness.drag_at(center);
        harness.run_steps(3);
        harness.drop_at(center);
        harness.run_steps(3);
        assert!(
            harness.state().probe_all_stop.load(Ordering::Relaxed),
            "clicking 停止 must set the stop flag"
        );
        harness.state_mut().apply_event(GuiEvent::ProbeAllDone {
            total: 3,
            ok: 1,
            failed: 0,
            stopped: true,
            proxy_down: false,
        });
        harness.run_steps(3);
        assert!(harness.state().busy.is_none());
        assert!(
            harness.state().status.contains("已停止"),
            "status must carry the stopped summary: {}",
            harness.state().status
        );
        assert!(
            harness.state().status.contains("已测 1/3"),
            "status: {}",
            harness.state().status
        );
    }

    /// The ∞ mark (U+221E) must render as a glyph, not a missing-glyph
    /// box, in both themes, at the 17 pt size the status column uses.
    /// The PNGs are the proof — check them, don't trust this test
    /// alone (it only asserts the render pipeline ran).
    #[test]
    #[ignore = "renders PNGs for review; needs a graphics adapter"]
    fn render_infinity_glyph() {
        use egui_kittest::Harness;
        struct GlyphApp;
        impl eframe::App for GlyphApp {
            fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
                egui::CentralPanel::default().show(ui, |ui| {
                    ui.label(
                        egui::RichText::new("∞")
                            .size(17.0)
                            .color(infinity_accent(ui.visuals().dark_mode)),
                    );
                    ui.label(egui::RichText::new("∞ 已连接 public-vpn-192").size(15.0));
                    ui.label("∞ ∞ ∞ — 17 pt / 15 pt: must be the infinity glyph, not a box.");
                });
            }
        }
        for (theme, name) in [(egui::Theme::Dark, "dark"), (egui::Theme::Light, "light")] {
            let mut harness = Harness::builder()
                .with_size(egui::vec2(320.0, 160.0))
                .with_theme(theme)
                .build_eframe(|cc| {
                    GuiApp::setup_visuals(cc);
                    GlyphApp
                });
            harness.run_steps(10);
            let path = std::env::temp_dir().join(format!("ui-review-infinity-{name}.png"));
            harness
                .render()
                .expect("needs a graphics adapter")
                .save(&path)
                .unwrap();
            eprintln!("{}", path.display());
        }
    }
}
