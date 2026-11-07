//! vpngate-gui — desktop GUI for the VPNGate Rust client (egui/eframe 0.36).
//!
//! Same tunnel code as the CLI (`vpngate_client` lib); this binary only
//! adds a window around it. Long work (fetch / probe / connect / test)
//! runs on tokio tasks; the UI polls them through a channel and never
//! blocks.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui;
use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use vpngate_client::config::Config;
use vpngate_client::net::{self, Connector};
use vpngate_client::sstp::SstpTunnel;
use vpngate_client::sysproxy::SystemProxyGuard;
use vpngate_client::vpngate::{self, Node};

// ---------------------------------------------------------------------------
// Background work
// ---------------------------------------------------------------------------

/// What the UI is waiting for right now (at most one job at a time).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Busy {
    Fetching,
    Probing,
    Connecting,
    Testing,
    TestingFrontProxy,
}

impl Busy {
    fn label(self) -> &'static str {
        match self {
            Busy::Fetching => "正在拉取节点列表…",
            Busy::Probing => "正在探测节点…",
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

/// A live connection: tunnel + the SOCKS5 server task(s) + the optional
/// system-proxy guard. Dropping the tunnel Arc closes the TLS stream
/// (the lib's writer task sends close_notify when its channel closes).
/// With LAN sharing on there are two serve tasks (local + LAN) sharing
/// the one tunnel; both are aborted together on disconnect.
struct ActiveConnection {
    tunnel: Arc<SstpTunnel>,
    serve: Vec<tokio::task::JoinHandle<()>>,
    proxy_guard: Option<SystemProxyGuard>,
    /// Display label, e.g. "127.0.0.1:10818" or
    /// "127.0.0.1:10818 ＋ 0.0.0.0:10809".
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
    Connected(Box<ActiveConnection>),
    ConnectFailed(String),
    TestResult {
        gen204: bool,
        google: bool,
        detail: String,
    },
    FrontProxyTest {
        google: net::FrontProxyCheck,
        vpngate: net::FrontProxyCheck,
    },
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
    let mut last_err = String::new();
    for (host, port) in &candidates {
        match SstpTunnel::probe(connector, host, *port, timeout).await {
            Ok(v) => return Ok(v),
            Err(e) => last_err = e,
        }
    }
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

/// Test the front proxy itself: HTTPS GET to Google and to vpngate.net
/// through it (SOCKS5 connectivity, not ICMP ping). Runs on a
/// background task; the result distinguishes "the proxy is down" from
/// "the proxy is up but the target does not answer through it".
async fn front_proxy_test_flow(
    connector: &Connector,
    timeout: Duration,
) -> (net::FrontProxyCheck, net::FrontProxyCheck) {
    let google = net::test_url_through_proxy(connector, "https://www.google.com", timeout).await;
    let vpngate = net::test_url_through_proxy(connector, "https://www.vpngate.net", timeout).await;
    (google, vpngate)
}

/// Connect the tunnel, start the local SOCKS5 server(s), optionally set
/// the system proxy. Runs on a background task; returns the live
/// connection. `listen_addrs` is always the local address first, plus
/// the LAN address when LAN sharing is on; both servers share the one
/// tunnel, and the system proxy (if on) points at the local address.
async fn connect_flow(
    connector: Connector,
    target: String,
    listen_addrs: Vec<String>,
    timeout: Duration,
    system_proxy: bool,
    service: Option<String>,
) -> Result<ActiveConnection, String> {
    let candidates = vpngate::resolve_target(&target);
    let mut last_err = String::new();
    let mut tunnel = None;
    for (host, port) in &candidates {
        match SstpTunnel::connect(&connector, host, *port, timeout).await {
            Ok(t) => {
                tunnel = Some(t);
                break;
            }
            Err(e) => last_err = e,
        }
    }
    let tunnel = tunnel
        .ok_or_else(|| format!("{} 个候选均失败；最后一次：{}", candidates.len(), last_err))?;
    let vip = tunnel.vip;
    let mut listeners = Vec::new();
    for addr in &listen_addrs {
        let listener = tokio::net::TcpListener::bind(addr).await.map_err(|e| {
            format!(
                "本地 SOCKS5 无法监听 {}：{}（端口可能被占用，换一个端口再试）",
                addr, e
            )
        })?;
        listeners.push(listener);
    }
    let mut serve = Vec::new();
    for listener in listeners {
        serve.push(tokio::spawn(vpngate_client::socks5::serve_socks5(
            listener,
            tunnel.clone(),
            timeout,
        )));
    }
    let listen_addr = listen_addrs.join(" ＋ ");
    let mut guard = None;
    if system_proxy {
        // System proxy always points at the local port (first address).
        match SystemProxyGuard::set(&listen_addrs[0], service.as_deref()) {
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
            let name =
                tokio_rustls::rustls::pki_types::ServerName::try_from("www.google.com".to_string())
                    .expect("valid server name");
            let tls = tokio_rustls::TlsConnector::from(net::verified_tls_config());
            match tokio::time::timeout(timeout, tls.connect(name, stream)).await {
                Ok(Ok(mut tls)) => {
                    let req = b"GET / HTTP/1.1\r\nHost: www.google.com\r\nConnection: close\r\nUser-Agent: vpngate-client/0.1\r\n\r\n";
                    if tls.write_all(req).await.is_ok() {
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
    selected: Option<usize>,
    probes: HashMap<String, String>,
    busy: Option<Busy>,
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
    lan_port: u16,
    allow_lan: bool,
    system_proxy: bool,
    /// Node-list column widths in pixels (country, ISP, host:port,
    /// latency/status). Draggable at the header separators; persisted
    /// to config.toml with the rest of the settings via auto-save.
    col_widths: [u16; 4],
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
            probes: HashMap::new(),
            busy: None,
            status: "未连接".to_string(),
            error: String::new(),
            log: vec!["就绪。正在拉取最新节点列表…".to_string()],
            conn: None,
            front_enabled: false,
            front_addr: "127.0.0.1:1080".to_string(),
            front_user: String::new(),
            front_pass: String::new(),
            local_port: 10818,
            lan_port: 10809,
            allow_lan: false,
            system_proxy: false,
            col_widths: vpngate_client::config::UiSettings::default().node_col_widths,
            advanced_open: false,
            render_scroll_bottom: false,
            manual_host: String::new(),
            manual_error: String::new(),
            show_about: false,
            font_ok: false,
            last_save_error: String::new(),
            event_tx: tx,
            event_rx: Some(rx),
        }
    }
}

impl GuiApp {
    /// Fonts + text sizes + theme, set up once. Returns whether a CJK
    /// font was found. Shared by `new` and the render test constructor.
    fn setup_visuals(cc: &eframe::CreationContext<'_>) -> bool {
        cc.egui_ctx.set_theme(egui::ThemePreference::System);
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
        if !app.font_ok {
            app.log
                .push("未找到系统里的中文字体，中文可能显示为方框。".to_string());
        }
        // Portable config: config.toml next to the exe first, then
        // ./config.toml in the working directory, else defaults.
        // Nothing is read from or written to the user profile.
        let cfg_path = Config::resolve_config_path(None);
        if let Some(p) = &cfg_path {
            if let Ok(cfg) = Config::load(Some(p)) {
                app.apply_config(&cfg);
            }
        }
        app.config_path = cfg_path;
        // Fetch the live node list in the background.
        app.start_fetch(cc.egui_ctx.clone());
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
        self.lan_port = cfg.listen.lan_port;
        self.allow_lan = cfg.listen.allow_lan;
        self.system_proxy = cfg.system_proxy.enabled;
        self.col_widths = cfg.ui.node_col_widths;
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
        cfg.listen.lan_port = self.lan_port;
        cfg.listen.allow_lan = self.allow_lan;
        cfg.system_proxy.enabled = self.system_proxy;
        cfg.ui.node_col_widths = self.col_widths;
        cfg
    }

    /// Save the current settings to `config.toml` next to the exe
    /// (portable mode). Reports the result in the log and status line.
    /// With auto-save on, this is normally already done — the button is
    /// an explicit "save again now" that confirms it.
    fn save_settings(&mut self) {
        let Some(path) = Config::portable_config_path() else {
            self.error = "保存失败：找不到程序所在目录。".to_string();
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
                self.error = format!("保存设置失败：{}。程序目录可能不可写。", e);
                self.log_line(format!("保存设置失败：{}。", e));
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
                    self.error = msg;
                    self.log_line(format!("自动保存设置失败：{}。", e));
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

    fn log_line(&mut self, line: String) {
        self.log.push(line);
        if self.log.len() > 100 {
            let excess = self.log.len() - 100;
            self.log.drain(..excess);
        }
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
        let target_str = match port {
            Some(p) => format!("{}:{}", target, p),
            None => target.clone(),
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
        let tx = self.event_tx.clone();
        let connector = self.connector();
        let timeout = self.timeout();
        tokio::spawn(async move {
            let res = probe_flow(&connector, &target_str, timeout).await;
            let _ = tx.send(GuiEvent::Probe {
                host: target,
                result: res,
            });
            ctx.request_repaint();
        });
    }

    /// Listen addresses for the next connection, from the live UI
    /// fields: always the local port, plus the LAN port when LAN
    /// sharing is on. Fails (with a plain message) when LAN sharing
    /// is on and both ports are the same.
    fn listen_addrs(&self) -> Result<Vec<String>, String> {
        self.current_config().listen.listen_addrs()
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
            Some(p) => format!("{}:{}", node.host, p),
            None => node.host.clone(),
        };
        let listen_addrs = match self.listen_addrs() {
            Ok(a) => a,
            Err(e) => {
                self.status = "连接失败。".to_string();
                self.error = format!("连接失败：{}。", e);
                self.log_line(format!("连接失败：{}。", e));
                return;
            }
        };
        self.busy = Some(Busy::Connecting);
        self.status = format!("正在连接 {}…", target);
        self.error.clear();
        let tx = self.event_tx.clone();
        let connector = self.connector();
        let timeout = self.timeout();
        let system_proxy = self.system_proxy;
        let service = self.config.system_proxy.service.clone();
        tokio::spawn(async move {
            match connect_flow(
                connector,
                target.clone(),
                listen_addrs,
                timeout,
                system_proxy,
                service,
            )
            .await
            {
                Ok(c) => {
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
    fn start_test_front_proxy(&mut self, ctx: egui::Context) {
        if !self.front_enabled || self.busy.is_some() {
            return;
        }
        self.busy = Some(Busy::TestingFrontProxy);
        self.status = format!("正在测试前置代理 {}…", self.front_addr);
        self.log_line(format!(
            "开始测试前置代理 {}（到 Google 与 vpngate.net）。",
            self.front_addr
        ));
        let tx = self.event_tx.clone();
        let connector = self.connector();
        let timeout = self.timeout();
        tokio::spawn(async move {
            let (google, vpngate) = front_proxy_test_flow(&connector, timeout).await;
            let _ = tx.send(GuiEvent::FrontProxyTest { google, vpngate });
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
                let n = upd.nodes.len();
                // A fresh list from the API or the web scrape is worth
                // caching, so the next failed refresh still has a recent
                // list to show.
                if matches!(
                    upd.source,
                    vpngate::NodeListSource::Api | vpngate::NodeListSource::WebScrape
                ) {
                    let _ = vpngate::save_cached_nodes(&upd.nodes);
                }
                self.nodes = upd.nodes;
                self.nodes_source = upd.source.label(n, upd.cache_age_secs);
                self.selected = None;
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
                        self.status = "探测失败：前置代理连不上。".to_string();
                        self.error = format!(
                            "探测 {} 失败：前置代理 {} 连不上（{}）。节点还没连上——问题在代理不在节点：刷新、探测、连接都走这条代理，代理不通时会一起失败，换节点没用。先点高级设置里的“测试前置代理”确认。",
                            host, self.front_addr, e
                        );
                        self.log_line(format!(
                            "探测 {}：失败：前置代理 {} 连不上（{}）。提示：刷新节点、探测、连接都走前置代理，代理不通时它们会一起失败。",
                            host, self.front_addr, e
                        ));
                    }
                    Err(ProbeFailure::NodeDown(e)) => {
                        self.probes.insert(host.clone(), "失败".to_string());
                        self.status = format!("{} 探测失败。", host);
                        if self.front_enabled {
                            self.error = format!(
                                "探测 {} 失败：前置代理 {} 能连上，但节点不通（{}）。换一个节点再试。",
                                host, self.front_addr, e
                            );
                            self.log_line(format!(
                                "探测 {}：失败：前置代理 {} 能连上，节点不通（{}）。",
                                host, self.front_addr, e
                            ));
                        } else {
                            self.error = format!("探测 {} 失败：{}。换一个节点再试。", host, e);
                            self.log_line(format!("探测 {}：失败：{}。", host, e));
                        }
                    }
                }
            }
            GuiEvent::Connected(c) => {
                self.busy = None;
                self.status = format!("已连接 {}，虚拟 IP {}。", c.node_label, c.vip);
                self.error.clear();
                self.log_line(format!(
                    "已连接 {}，虚拟 IP {}，本地 SOCKS5 {}。浏览器/应用连这个端口即可上网。",
                    c.node_label, c.vip, c.listen_addr
                ));
                self.conn = Some(*c);
            }
            GuiEvent::ConnectFailed(e) => {
                self.busy = None;
                self.status = "连接失败。".to_string();
                self.error = format!("连接失败：{}。换一个节点，或检查前置 SOCKS5 设置。", e);
                self.log_line(format!("连接失败：{}。", e));
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
                    self.error = format!("测试未通过：{} 换一个节点再试。", detail);
                } else if !google {
                    self.error =
                        format!("Google 不通：{} 可能是出口被挡，换一个节点再试。", detail);
                } else {
                    self.error.clear();
                }
            }
            GuiEvent::FrontProxyTest { google, vpngate } => {
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
                    self.error = format!(
                        "前置代理 {} 连不上（{}）。刷新、探测、连接都走这条代理，代理不通时会一起失败：检查前置代理（NekoBox）开没开、地址对不对。",
                        self.front_addr, d
                    );
                } else if target_down.is_empty() {
                    self.status = format!("前置代理测试：{}；{}。", g, v);
                    self.error.clear();
                } else {
                    let labels: Vec<&str> = target_down.iter().map(|(l, _)| *l).collect();
                    self.status = format!("前置代理测试：前置代理通，{}不通。", labels.join("、"));
                    self.error = format!(
                        "前置代理能连上，但 {} 通过它打不开（{}）。刷新节点要走 vpngate.net，这个不通就拉不到新节点；Google 不通则是出口的问题。",
                        labels.join("、"),
                        target_down[0].1
                    );
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
                        ui.label(b.label());
                    }
                });
            });
            ui.separator();

            // Node list (4 columns; host:port merged so the grid fits a
            // 560 px window — hostnames alone are ~200 px wide).
            // Each row: (country, isp, host:port shown, status, host:port full).
            let rows: Vec<(String, String, String, String, String)> = self
                .nodes
                .iter()
                .map(|n| {
                    let status = self
                        .probes
                        .get(&n.host)
                        .cloned()
                        .unwrap_or_else(|| "未探测".to_string());
                    let full = match n.port {
                        Some(p) => format!("{}:{}", n.host, p),
                        None => n.host.clone(),
                    };
                    (
                        // Full text here: the cells truncate with "…" to
                        // the current column width (see below), so
                        // widening a column shows more instead of a
                        // fixed 12/14-character cut.
                        n.country.clone(),
                        if n.isp.is_empty() { n.name.clone() } else { n.isp.clone() },
                        // All curated hosts end in .opengw.net; drop the
                        // suffix for display (full host is in the tooltip).
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
            if rows.is_empty() {
                ui.label("还没有节点。点右上角“刷新节点”，或在下面“高级设置”里手动添加。");
            } else {
                egui::ScrollArea::both()
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
                        // Tighter gaps so the four columns fit a 560 px
                        // window at the default widths.
                        ui.spacing_mut().item_spacing = egui::vec2(6.0, 4.0);
                        let widths: [f32; 4] = self.col_widths.map(|w| w as f32);
                        let row_h = 22.0;
                        // One row is 4 cells + 3 six-pixel separators +
                        // the 6 gaps between the 7 items.
                        let handle_w = 6.0f32;
                        let gap = ui.spacing().item_spacing.x;
                        let total_w =
                            widths.iter().sum::<f32>() + 3.0 * handle_w + 6.0 * gap;
                        // Header row: labels plus a draggable separator
                        // after each column but the last. Dragging resizes
                        // that column (40–400 px); the width is remembered
                        // in the settings and auto-saved to config.toml.
                        // NOTE: the cells are placed at exact rects (like
                        // the data rows below), not via
                        // allocate_ui_with_layout: that advances the parent
                        // by the child's *content* size, so a short label
                        // would pull the separator and the next column left
                        // of its intended position, and the header would
                        // drift out of line with the data columns.
                        // Handles get explicit stable ids (not auto ids).
                        let (header_rect, _) = ui.allocate_exact_size(
                            egui::vec2(total_w, row_h),
                            egui::Sense::hover(),
                        );
                        let mut x = header_rect.left();
                        for (i, label) in ["国家/地区", "运营商", "主机:端口", "延迟/状态"]
                            .iter()
                            .enumerate()
                        {
                            let cell_rect = egui::Rect::from_min_size(
                                egui::pos2(x, header_rect.top()),
                                egui::vec2(widths[i], row_h),
                            );
                            put_cell_label(
                                ui,
                                cell_rect,
                                egui::RichText::new(*label).strong(),
                            );
                            x += widths[i];
                            if i < 3 {
                                x += gap;
                                let handle_rect = egui::Rect::from_min_size(
                                    egui::pos2(x, header_rect.top()),
                                    egui::vec2(handle_w, row_h),
                                );
                                let resp = ui.interact(
                                    handle_rect,
                                    egui::Id::new(("node_col_sep", i)),
                                    egui::Sense::drag(),
                                );
                                let active = resp.hovered() || resp.dragged();
                                // Apply the per-frame delta while
                                // dragging, and also on release: a
                                // drag whose pointer only moved in the
                                // release event still carries its delta
                                // then (kittest teleports this way).
                                if resp.dragged() || resp.drag_stopped() {
                                    self.col_widths[i] = (self.col_widths[i] as f32
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
                                x += handle_w + gap;
                            }
                        }
                        // Data rows: whole-row click selects; text
                        // truncates with "…" to the current column width,
                        // and widening a column shows more of it (the
                        // horizontal scroll bar reveals the rest).
                        for (i, (country, isp, hostport, status, full)) in
                            rows.iter().enumerate()
                        {
                            let (row_rect, row_resp) = ui.allocate_exact_size(
                                egui::vec2(total_w, row_h),
                                egui::Sense::click(),
                            );
                            let fill = if self.selected == Some(i) {
                                ui.visuals().selection.bg_fill
                            } else if i % 2 == 0 {
                                ui.visuals().faint_bg_color
                            } else {
                                egui::Color32::TRANSPARENT
                            };
                            ui.painter().rect_filled(row_rect, 0.0, fill);
                            // Place each cell label at its exact rect
                            // (egui 0.36 has no allocate_ui_at_rect).
                            let cells = [country, isp, hostport, status];
                            let mut x = row_rect.left();
                            for (j, text) in cells.iter().enumerate() {
                                let cell_rect = egui::Rect::from_min_size(
                                    egui::pos2(x, row_rect.top()),
                                    egui::vec2(widths[j], row_h),
                                );
                                put_cell_label(ui, cell_rect, *text);
                                x += widths[j];
                                if j < 3 {
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
                            if row_resp.clicked() {
                                self.selected = Some(i);
                            }
                        }
                    });
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
                    .on_disabled_hover_text(if self.selected.is_none() {
                        "请先在上面选择一个节点"
                    } else {
                        "正在忙，请稍候"
                    })
                    .clicked()
                {
                    self.start_probe(ui.ctx().clone());
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
            // 常用开关和端口放主面板上，不用展开高级设置也能看到；改动会自动保存。
            // 端口只在连接时生效：连接中两个端口输入都锁住；局域网端口只在
            // 勾选局域网分享时可改。
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.allow_lan, "允许局域网分享（绑定 0.0.0.0）")
                    .on_hover_text("开启后，局域网里的其他设备也能连这个本地代理端口");
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
                ui.label("局域网端口：");
                ui.add_enabled(
                    !connected && self.allow_lan,
                    egui::DragValue::new(&mut self.lan_port).range(1..=65535),
                )
                .on_disabled_hover_text(if connected {
                    "连接中，断开后才能改端口"
                } else {
                    "先勾选“允许局域网分享”"
                });
                if connected {
                    ui.label("连接中，断开后才能改。");
                } else if self.allow_lan && self.local_port == self.lan_port {
                    ui.label(
                        egui::RichText::new("两个端口不能相同。")
                            .color(ui.visuals().error_fg_color),
                    );
                } else if !self.allow_lan {
                    ui.label("（需先勾选局域网分享）");
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
                                    self.selected = Some(0);
                                    self.log_line(format!("手动添加节点：{}。", target));
                                    self.manual_host.clear();
                                }
                                Err(e) => self.manual_error = e,
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
                    ui.label("VPNGate 客户端 0.1.0（Rust / egui）");
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
    fn col_widths_roundtrip_through_config() {
        let mut app = GuiApp::for_tests();
        assert_eq!(app.col_widths, [90, 130, 190, 90]);
        app.col_widths = [120, 140, 200, 100];
        let cfg = app.current_config();
        assert_eq!(cfg.ui.node_col_widths, [120, 140, 200, 100]);
        let mut app2 = GuiApp::for_tests();
        app2.apply_config(&cfg);
        assert_eq!(app2.col_widths, [120, 140, 200, 100]);
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
        app.lan_port = 10810;
        app.allow_lan = true;
        app.system_proxy = true;
        let cfg = app.current_config();
        assert!(cfg.front_proxy.enabled);
        assert_eq!(cfg.front_proxy.addr, "127.0.0.1:1081");
        assert_eq!(cfg.front_proxy.username.as_deref(), Some("u"));
        assert_eq!(cfg.front_proxy.password.as_deref(), Some("p"));
        assert_eq!(cfg.listen.local_port, 10809);
        assert_eq!(cfg.listen.lan_port, 10810);
        assert!(cfg.listen.allow_lan);
        assert!(cfg.system_proxy.enabled);
    }

    #[test]
    fn listen_addrs_local_plus_lan_and_equal_port_error() {
        let mut app = GuiApp::for_tests();
        assert_eq!(
            app.listen_addrs().unwrap(),
            vec!["127.0.0.1:10818".to_string()]
        );
        app.allow_lan = true;
        assert_eq!(
            app.listen_addrs().unwrap(),
            vec!["127.0.0.1:10818".to_string(), "0.0.0.0:10809".to_string()]
        );
        app.lan_port = 10818;
        let err = app.listen_addrs().unwrap_err();
        assert!(
            err.contains("不能设成一样"),
            "equal ports must fail loudly: {}",
            err
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
    /// review, plus a 150% zoom render and scrolled-to-bottom renders
    /// (they must show the end of 高级设置 and the log — that is the
    /// outer-scroll fix being verified). Needs a wgpu adapter (software
    /// lavapipe is enough); on a machine without one this test fails and
    /// must be skipped, not faked.
    #[test]
    #[ignore = "renders PNGs for review; needs a graphics adapter"]
    fn render_for_review() {
        use egui_kittest::{kittest::Queryable, Harness};
        for (theme, name, zoom, scrolled) in [
            (egui::Theme::Dark, "dark", 1.0, false),
            (egui::Theme::Light, "light", 1.0, false),
            (egui::Theme::Dark, "dark-zoom150", 1.5, false),
            (egui::Theme::Dark, "dark-scrolled", 1.0, true),
            (egui::Theme::Light, "light-scrolled", 1.0, true),
        ] {
            let mut harness = Harness::builder()
                .with_size(egui::vec2(560.0, 420.0))
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

    /// Dragging a header separator widens that column (needs a graphics
    /// adapter, like the render test).
    #[test]
    #[ignore = "drives the UI with kittest; needs a graphics adapter"]
    fn column_separator_drag_widens_column() {
        use egui_kittest::{kittest::Queryable, Harness};
        let mut harness = Harness::builder()
            .with_size(egui::vec2(560.0, 420.0))
            .build_eframe(|cc| GuiApp::for_render_tests(cc));
        harness.run_steps(10);
        let before = harness.state().col_widths[0];
        // The first header cell starts at its label's left edge and is
        // `before` px wide; the 6 px separator sits one item gap (6 px)
        // past its right edge, so its centre is at left + width + 9.
        let header = harness.get_by_label("国家/地区");
        let r = header.rect();
        let sep = egui::pos2(r.left() + before as f32 + 9.0, r.center().y);
        harness.hover_at(sep);
        harness.run();
        harness.drag_at(sep);
        harness.run();
        // Move while the button is held (a real drag sends PointerMoved
        // events between press and release; drop_at alone only sends
        // the release event, whose delta the release frame would miss).
        let target = egui::pos2(sep.x + 40.0, sep.y);
        harness.hover_at(target);
        harness.run();
        harness.drop_at(target);
        harness.run();
        let after = harness.state().col_widths[0];
        assert!(
            after >= before + 30,
            "dragging the separator should widen column 0: {} -> {}",
            before,
            after
        );
    }
}
