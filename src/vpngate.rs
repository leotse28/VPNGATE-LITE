//! VPNGate node list.
//!
//! Sources:
//! * Official API `https://www.vpngate.net/api/iphone/` — CSV with columns
//!   HostName,IP,Score,Ping,Speed,CountryLong,CountryShort,NumVpnSessions,
//!   Uptime,TotalUsers,TotalTraffic,LogType,Operator,Message,
//!   OpenVPN_ConfigData_Base64. It does **not** carry the MS-SSTP port.
//! * Bundled `nodes.json` — 28 curated nodes decoded from the earlier
//!   Cloudflare Worker config, each with a **known SSTP host:port** and
//!   backup nodes. This is the only reliable SSTP-port source we have.
//!
//! `list` therefore shows API nodes merged with the curated table: a node
//! whose hostname appears in `nodes.json` gets its known SSTP port; every
//! other API node is shown with `sstp_port: ?` and can only be connected
//! by manual `host:port` entry or by `probe` trying candidate ports
//! (443, 992, 995).

use crate::net::{https_get, Connector};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

pub const API_URL: &str = "https://www.vpngate.net/api/iphone/";

/// Public relay list page, used as the fallback when the API is
/// unreachable (the page is plain HTML, no login, no JS needed).
pub const WEB_URL: &str = "https://www.vpngate.net/cn/";

/// Candidate SSTP ports tried by `probe` when no port is given.
pub const CANDIDATE_SSTP_PORTS: &[u16] = &[443, 992, 995];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub host: String,
    #[serde(default)]
    pub port: Option<u16>,
    pub country: String,
    #[serde(default)]
    pub country_code: String,
    #[serde(default)]
    pub isp: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub score: u64,
    #[serde(default)]
    pub ping_ms: u64,
    #[serde(default)]
    pub speed_mbps: u64,
    #[serde(default)]
    pub sessions: u64,
    #[serde(default)]
    pub ip: String,
    #[serde(default)]
    pub backups: Vec<String>,
    /// Where the SSTP port came from: "curated" | "manual" | "probed" | "unknown" | "web-scrape".
    #[serde(default)]
    pub port_source: String,
    /// Where this node entry came from: "api" | "web-scrape" | "curated" | "manual".
    #[serde(default)]
    pub source: String,
}

#[derive(Debug, Clone, Deserialize)]
struct CuratedNode {
    host: String,
    port: u16,
    country: String,
    #[serde(default)]
    country_code: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    isp: String,
    #[serde(default)]
    backups: Vec<String>,
}

/// The bundled curated list (28 nodes, known SSTP ports).
pub fn curated_nodes() -> Vec<Node> {
    let raw = include_str!("../nodes.json");
    let parsed: Vec<CuratedNode> = serde_json::from_str(raw).expect("nodes.json is valid JSON");
    parsed
        .into_iter()
        .map(|c| Node {
            host: c.host,
            port: Some(c.port),
            country: c.country,
            country_code: c.country_code,
            isp: c.isp,
            name: c.name,
            score: 0,
            ping_ms: 0,
            speed_mbps: 0,
            sessions: 0,
            ip: String::new(),
            backups: c.backups,
            port_source: "curated".to_string(),
            source: "curated".to_string(),
        })
        .collect()
}

pub fn parse_api_csv(text: &str) -> Vec<Node> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if i == 0 || line.starts_with('#') || line.starts_with('*') || line.trim().is_empty() {
            continue;
        }
        if line.starts_with("HostName,") {
            continue;
        }
        // The OpenVPN config column is base64 without commas, so a plain
        // split is safe for the first 15 columns.
        let cols: Vec<&str> = line.split(',').collect();
        if cols.len() < 14 {
            continue;
        }
        let num = |i: usize| cols.get(i).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
        out.push(Node {
            host: cols[0].to_string(),
            port: None,
            country: cols[5].to_string(),
            country_code: cols[6].to_string(),
            // cols[11] is LogType ("2weeks", "0", ...) — not the ISP.
            // Operator is cols[12] (e.g. "SoftBank BB"). The name stays
            // empty, like the web-scrape path: the Chinese display name
            // comes only from the curated table.
            isp: cols[12].to_string(),
            name: String::new(),
            score: num(2),
            ping_ms: num(3),
            speed_mbps: num(4),
            sessions: num(7),
            ip: cols[1].to_string(),
            backups: Vec::new(),
            port_source: "unknown".to_string(),
            source: "api".to_string(),
        });
    }
    out
}

/// Fetch the live API list and merge known SSTP ports from the curated table.
pub async fn fetch_api_nodes(connector: &Connector) -> Result<Vec<Node>, String> {
    let (status, body) = https_get(connector, API_URL, &[], true).await?;
    if status != 200 {
        return Err(format!("VPNGate API returned HTTP {}", status));
    }
    let text = String::from_utf8_lossy(&body).to_string();
    let mut nodes = parse_api_csv(&text);
    let curated = curated_nodes();
    for n in nodes.iter_mut() {
        if let Some(c) = curated.iter().find(|c| c.host == n.host) {
            n.port = c.port;
            n.backups = c.backups.clone();
            n.port_source = "curated".to_string();
            if n.isp.is_empty() {
                n.isp = c.isp.clone();
            }
        }
    }
    Ok(nodes)
}

// ---------------------------------------------------------------------------
// Web scrape fallback: https://www.vpngate.net/cn/
//
// When the official API is unreachable (common on Leo's network), the
// public relay list page is the next best source: it is plain HTML, no
// login, no JS. Each table row is one relay; the DDNS hostname is in
// the second cell and the MS-SSTP cell carries "SSTP 主机名 :
// host[:port]" — an explicit port there is the real SSTP port,
// otherwise 443 (VPNGate's standard SSTP port) is used.
//
// The parser is defensive on purpose: the page layout changes, so it
// scans table rows for `.opengw.net` hostnames instead of relying on
// exact markup, and never panics on unexpected input. Rows from the
// page's other tables (session log, country ranking) contain no
// `.opengw.net` hostname and are skipped automatically.
// ---------------------------------------------------------------------------

/// Strip HTML tags from `s` and collapse runs of whitespace to one space.
fn strip_tags_collapse(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    let mut last_ws = true; // trims leading whitespace
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if in_tag => {}
            _ if c.is_whitespace() => {
                if !last_ws {
                    out.push(' ');
                    last_ws = true;
                }
            }
            _ => {
                out.push(c);
                last_ws = false;
            }
        }
    }
    out.trim_end().to_string()
}

/// Text of the first `<td>...</td>` cell in a row (the country column),
/// tags stripped. Empty when the row has no readable first cell.
fn first_cell_text(row: &str) -> String {
    let Some(td) = row.find("<td") else {
        return String::new();
    };
    let Some(gt) = row[td..].find('>') else {
        return String::new();
    };
    let body = &row[td + gt + 1..];
    let Some(end) = body.find("</td>") else {
        return String::new();
    };
    strip_tags_collapse(&body[..end])
}

/// Is `host` a plausible VPNGate DDNS hostname? It must end in
/// `.opengw.net` with every label alphanumeric-or-hyphen and non-empty.
/// This accepts the `vpn<digits>` names, the `public-vpn-<n>` names
/// (most rows on the page) and the other DDNS names in the same table
/// (e.g. `kanratown`, `laud`); the ISP-hostname column
/// (`*.vpngate.v4.open.ad.jp`, `*.ppp-bb.dion.ne.jp`, ...) does not
/// end in `.opengw.net` and is excluded.
fn valid_ddns_host(host: &str) -> bool {
    let Some(base) = host.strip_suffix(".opengw.net") else {
        return false;
    };
    if base.is_empty() {
        return false;
    }
    base.split('.').all(|label| {
        !label.is_empty()
            && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            && !label.starts_with('-')
            && !label.ends_with('-')
    })
}

/// Scan one table row for `.opengw.net` hostnames, in row order.
/// Returns `(host, explicit_port)` pairs; the port is `Some` only when
/// the hostname is immediately followed by `:digits` (the MS-SSTP cell
/// writes `host:port`; the DDNS cell writes the bare hostname).
fn extract_hosts(row: &str) -> Vec<(String, Option<u16>)> {
    let mut out = Vec::new();
    let bytes = row.as_bytes();
    let mut search_from = 0usize;
    while let Some(rel) = row[search_from..].find(".opengw.net") {
        let end = search_from + rel; // index just past the hostname
                                     // Walk back over hostname characters.
        let mut start = end;
        while start > 0 {
            let c = bytes[start - 1] as char;
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                start -= 1;
            } else {
                break;
            }
        }
        let host = &row[start..end + ".opengw.net".len()];
        // Optional ":port" right after the hostname.
        let mut port = None;
        let after = end + ".opengw.net".len();
        if bytes.get(after) == Some(&b':') {
            let mut digits = String::new();
            let mut i = after + 1;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                digits.push(bytes[i] as char);
                i += 1;
            }
            if let Ok(p) = digits.parse::<u16>() {
                if p > 0 {
                    port = Some(p);
                }
            }
        }
        if valid_ddns_host(host) {
            out.push((host.to_string(), port));
        }
        search_from = end + ".opengw.net".len();
    }
    out
}

/// Parse the VPNGate relay list page into nodes, in table order,
/// deduplicated by hostname. A hostname seen first in the DDNS cell
/// (no port) and again in the MS-SSTP cell with an explicit port keeps
/// its first-seen position but takes the explicit port. Nodes whose
/// hostname is in the curated table get their known SSTP port instead
/// of the scraped 443 default.
pub fn parse_web_page(html: &str) -> Vec<Node> {
    let mut out: Vec<Node> = Vec::new();
    for row in html.split("<tr") {
        if !row.contains(".opengw.net") {
            continue;
        }
        let country = first_cell_text(row);
        for (host, port) in extract_hosts(row) {
            // "opengw.net" on its own is not a relay (it is the domain
            // itself, not a DDNS hostname under it). `valid_ddns_host`
            // already rejects it, but filter it explicitly so a bare
            // domain can never become a node entry.
            if host == "opengw.net" {
                continue;
            }
            if let Some(existing) = out.iter_mut().find(|n| n.host == host) {
                // Seen again (DDNS cell, then MS-SSTP cell): an explicit
                // port beats the 443 default from the first sighting.
                if let Some(p) = port {
                    if existing.port == Some(443) {
                        existing.port = Some(p);
                        existing.port_source = "web-scrape".to_string();
                    }
                }
                continue;
            }
            out.push(Node {
                host,
                port: Some(port.unwrap_or(443)),
                country: if country.is_empty() {
                    "未知".to_string()
                } else {
                    country.clone()
                },
                country_code: String::new(),
                isp: String::new(),
                name: String::new(),
                score: 0,
                ping_ms: 0,
                speed_mbps: 0,
                sessions: 0,
                ip: String::new(),
                backups: Vec::new(),
                port_source: "web-scrape".to_string(),
                source: "web-scrape".to_string(),
            });
        }
    }
    let curated = curated_nodes();
    for n in out.iter_mut() {
        if let Some(c) = curated.iter().find(|c| c.host == n.host) {
            n.port = c.port;
            n.backups = c.backups.clone();
            n.port_source = "curated".to_string();
            if n.isp.is_empty() {
                n.isp = c.isp.clone();
            }
        }
    }
    out
}

/// Fetch the relay list page and parse it. Errors when the page cannot
/// be fetched or no node parses out of it (the layout may have changed).
pub async fn fetch_web_nodes(connector: &Connector) -> Result<Vec<Node>, String> {
    let (status, body) = https_get(
        connector,
        WEB_URL,
        &[("Accept-Language", "zh-CN,zh;q=0.9")],
        true,
    )
    .await?;
    if status != 200 {
        return Err(format!("VPNGate 网页返回 HTTP {}", status));
    }
    let text = String::from_utf8_lossy(&body).to_string();
    let nodes = parse_web_page(&text);
    if nodes.is_empty() {
        return Err("VPNGate 网页里没有解析到节点（页面结构可能变了）".to_string());
    }
    Ok(nodes)
}

/// Where a node list came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeListSource {
    /// Official API `api/iphone`.
    Api,
    /// Scraped from the public relay list page.
    WebScrape,
    /// `nodes_cache.json` from the last successful fetch.
    Cache,
    /// Bundled 28-node curated list.
    Curated,
}

impl NodeListSource {
    /// One-line label for the UI, given the node count and (for cache)
    /// its age in seconds.
    pub fn label(&self, n: usize, cache_age_secs: Option<u64>) -> String {
        match self {
            NodeListSource::Api => format!("官方 API（{} 个节点）", n),
            NodeListSource::WebScrape => {
                format!("网页抓取（{} 个节点，官方 API 不可用）", n)
            }
            NodeListSource::Cache => {
                let age = cache_age_secs.unwrap_or(0);
                let age_text = if age < 3600 {
                    format!("约 {} 分钟前保存", age / 60 + 1)
                } else {
                    format!("约 {} 小时前保存", age / 3600)
                };
                format!("缓存列表（{} 个，{}）", n, age_text)
            }
            NodeListSource::Curated => "内嵌列表（官方 API 与网页都不可用）".to_string(),
        }
    }
}

/// The result of one node-list refresh: the nodes, where they came from,
/// and why the earlier sources failed (for the log line).
#[derive(Debug, Clone)]
pub struct NodeListUpdate {
    pub nodes: Vec<Node>,
    pub source: NodeListSource,
    pub cache_age_secs: Option<u64>,
    pub api_error: Option<String>,
    pub web_error: Option<String>,
}

/// Pure picker for the fallback chain (no network): the first
/// non-empty source wins — API, then web scrape, then cache, then the
/// bundled curated list.
pub fn pick_node_list(
    api: Option<Vec<Node>>,
    web: Option<Vec<Node>>,
    cache: Option<(Vec<Node>, u64)>,
) -> (Vec<Node>, NodeListSource, Option<u64>) {
    if let Some(nodes) = api {
        if !nodes.is_empty() {
            return (nodes, NodeListSource::Api, None);
        }
    }
    if let Some(nodes) = web {
        if !nodes.is_empty() {
            return (nodes, NodeListSource::WebScrape, None);
        }
    }
    if let Some((nodes, age)) = cache {
        return (nodes, NodeListSource::Cache, Some(age));
    }
    (curated_nodes(), NodeListSource::Curated, None)
}

/// Full fallback chain: official API first, then the relay list page,
/// then the local cache, then the bundled curated list. The last two
/// never fail, so this always returns a list.
pub async fn fetch_nodes_with_fallback(connector: &Connector) -> NodeListUpdate {
    let mut api_error = None;
    let api = match fetch_api_nodes(connector).await {
        Ok(nodes) if !nodes.is_empty() => Some(nodes),
        Ok(_) => {
            api_error = Some("官方 API 返回了空列表".to_string());
            None
        }
        Err(e) => {
            api_error = Some(e);
            None
        }
    };
    let mut web_error = None;
    let web = if api.is_none() {
        match fetch_web_nodes(connector).await {
            Ok(nodes) if !nodes.is_empty() => Some(nodes),
            Ok(_) => {
                web_error = Some("网页抓取解析到 0 个节点".to_string());
                None
            }
            Err(e) => {
                web_error = Some(e);
                None
            }
        }
    } else {
        None
    };
    let cache = load_cached_nodes();
    let (nodes, source, cache_age_secs) = pick_node_list(api, web, cache);
    NodeListUpdate {
        nodes,
        source,
        cache_age_secs,
        api_error,
        web_error,
    }
}

// ---------------------------------------------------------------------------
// Node-list cache (portable): the last good API list is saved next to the
// executable as `nodes_cache.json`, so a refresh that fails — the API is
// often unreachable (TLS reset / blocked network) — still shows a recent
// list instead of only the 28 bundled nodes.
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
struct CachedList {
    fetched_at: u64,
    nodes: Vec<Node>,
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `nodes_cache.json` next to the executable (portable location).
pub fn cache_path() -> Option<PathBuf> {
    crate::config::Config::portable_dir().map(|d| d.join("nodes_cache.json"))
}

/// Save a fresh API list to the cache. Errors are swallowed by callers
/// (a missing cache only means the next failed refresh falls back to
/// the bundled list).
pub fn save_cached_nodes(nodes: &[Node]) -> Result<(), String> {
    let Some(path) = cache_path() else {
        return Err("cannot locate executable directory".to_string());
    };
    save_cached_nodes_to(&path, nodes)
}

/// Path-taking core of `save_cached_nodes` (tests use a temp path).
pub fn save_cached_nodes_to(path: &std::path::Path, nodes: &[Node]) -> Result<(), String> {
    let payload = CachedList {
        fetched_at: now_unix(),
        nodes: nodes.to_vec(),
    };
    let text = serde_json::to_string(&payload).map_err(|e| e.to_string())?;
    std::fs::write(path, text).map_err(|e| format!("cannot write {}: {}", path.display(), e))
}

/// Load the cached list, if it exists and parses. Returns the nodes and
/// their age in seconds.
pub fn load_cached_nodes() -> Option<(Vec<Node>, u64)> {
    let path = cache_path()?;
    load_cached_nodes_from(&path)
}

/// Path-taking core of `load_cached_nodes` (tests use a temp path).
pub fn load_cached_nodes_from(path: &std::path::Path) -> Option<(Vec<Node>, u64)> {
    let text = std::fs::read_to_string(path).ok()?;
    let payload: CachedList = serde_json::from_str(&text).ok()?;
    if payload.nodes.is_empty() {
        return None;
    }
    Some((payload.nodes, now_unix().saturating_sub(payload.fetched_at)))
}

/// Nodes to show when the official API is unreachable: the cached list
/// saved after the last successful fetch, if any, else the bundled
/// curated list. The second value is the cache age in seconds
/// (`None` when the list is the bundled curated one).
pub fn fallback_nodes() -> (Vec<Node>, Option<u64>) {
    if let Some((nodes, age)) = load_cached_nodes() {
        return (nodes, Some(age));
    }
    (curated_nodes(), None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_api_csv_sample() {
        let csv = "# comment\n* vpn_servers\nHostName,IP,Score,Ping,Speed,CountryLong,CountryShort,NumVpnSessions,Uptime,TotalUsers,TotalTraffic,LogType,Operator,Message,OpenVPN_ConfigData_Base64\nvpn267947139.opengw.net,1.2.3.4,1000000,10,100,Japan,JP,5,1000,2000,3000,0,SoftBank BB,,AAAA\n";
        let nodes = parse_api_csv(csv);
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].host, "vpn267947139.opengw.net");
        assert_eq!(nodes[0].country, "Japan");
        assert_eq!(nodes[0].score, 1000000);
        assert_eq!(nodes[0].port, None);
        // cols[11] is LogType ("0" here, "2weeks" in the live list) —
        // the ISP is Operator, cols[12]. Before the fix both isp and
        // name read cols[11], so the ISP column showed "2weeks".
        assert_eq!(nodes[0].isp, "SoftBank BB");
        assert!(nodes[0].name.is_empty(), "API nodes carry no display name");
    }

    #[test]
    fn resolve_target_completes_bare_ddns_labels() {
        // Leo's v3.1 log: API nodes arrived as bare labels
        // ("public-vpn-192", "vpn321359264") and the probe dialled
        // them as-is. The suffix is completed here, at the single
        // choke point probe/connect (GUI + CLI) both use.
        let c = resolve_target("public-vpn-192");
        assert_eq!(c.len(), 3, "candidate SSTP ports");
        assert!(c.iter().all(|(h, _)| h == "public-vpn-192.opengw.net"));
        assert_eq!(
            c.iter().map(|(_, p)| *p).collect::<Vec<_>>(),
            vec![443, 992, 995]
        );
        assert_eq!(
            resolve_target("public-vpn-192:995"),
            vec![("public-vpn-192.opengw.net".to_string(), 995)]
        );
        // A bare "opengw" completes to a name that cleanly fails DNS
        // instead of dialling whatever the proxy makes of the label.
        assert_eq!(
            resolve_target("opengw"),
            vec![
                ("opengw.opengw.net".to_string(), 443),
                ("opengw.opengw.net".to_string(), 992),
                ("opengw.opengw.net".to_string(), 995),
            ]
        );
        // FQDNs and IPs are untouched.
        assert_eq!(
            resolve_target("kanratown.opengw.net:992"),
            vec![("kanratown.opengw.net".to_string(), 992)]
        );
        assert_eq!(
            resolve_target("127.0.0.1:8443"),
            vec![("127.0.0.1".to_string(), 8443)]
        );
        // Curated hosts keep their known port and backups.
        let c = resolve_target("kanratown.opengw.net");
        assert_eq!(c[0], ("kanratown.opengw.net".to_string(), 992));
        assert_eq!(c.len(), 3, "known port + 2 curated backups");
    }

    #[test]
    fn parse_web_page_never_yields_bare_domain() {
        // A page that merely mentions the domain must not produce a
        // node whose host is "opengw.net" itself.
        let html = r#"<table>
<tr><td>Japan</td><td>DDNS hostnames end with opengw.net — see <b>vpn1.opengw.net</b></td><td>SSTP 主机名 : <b>opengw.net</b></td></tr>
<tr><td>Japan</td><td><b>vpn2.opengw.net</b> 1.1.1.1</td><td>SSTP 主机名 : <b>vpn2.opengw.net</b></td></tr>
</table>"#;
        let nodes = parse_web_page(html);
        // vpn1 (mentioned in the text) and vpn2 (the real row) both
        // parse; the bare "opengw.net" in the SSTP cell must not.
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].host, "vpn1.opengw.net");
        assert_eq!(nodes[1].host, "vpn2.opengw.net");
        assert!(nodes.iter().all(|n| n.host != "opengw.net"));
    }

    #[test]
    fn curated_has_28_known_ports() {
        let nodes = curated_nodes();
        assert_eq!(nodes.len(), 28);
        assert!(nodes.iter().all(|n| n.port.is_some()));
        assert!(nodes
            .iter()
            .any(|n| n.host == "kanratown.opengw.net" && n.port == Some(992)));
    }

    #[test]
    fn node_cache_roundtrip() {
        let dir = std::env::temp_dir();
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = dir.join(format!("vpngate-cache-test-{}.json", stamp));
        let mut nodes = curated_nodes();
        nodes.truncate(3);
        save_cached_nodes_to(&path, &nodes).expect("save cache");
        let (back, age) = load_cached_nodes_from(&path).expect("load cache");
        assert_eq!(back.len(), 3);
        assert_eq!(back[0].host, nodes[0].host);
        assert!(age < 60, "fresh cache must be seconds old");
        let _ = std::fs::remove_file(&path);
        assert!(load_cached_nodes_from(&path).is_none());
    }

    /// Synthetic HTML shaped like the real relay table: a header row,
    /// rows with/without an explicit SSTP port, a duplicate host, a row
    /// with no SSTP support, and non-DDNS hostnames that must be ignored.
    fn sample_relay_html() -> &'static str {
        r#"<html><body><table>
<tr><th>国家 / 地区</th><th>DDNS 主机名 IP 地址</th><th>MS-SSTP</th></tr>
<tr><td><img src="/images/flags/JP.png"> Japan</td>
<td><b>public-vpn-52.opengw.net</b> 219.100.37.16 (public-vpn-01-16.vpngate.v4.open.ad.jp)</td>
<td>SSTP 主机名 : <b>public-vpn-52.opengw.net</b></td></tr>
<tr><td><img src="/images/flags/US.png"> United States</td>
<td><b>vpn678091301.opengw.net</b> 184.97.202.118</td>
<td>SSTP 主机名 : <b>vpn678091301.opengw.net:1427</b></td></tr>
<tr><td><img src="/images/flags/JP.png"> Japan</td>
<td><b>kanratown.opengw.net</b> 1.2.3.4</td>
<td>SSTP 主机名 : <b>kanratown.opengw.net</b></td></tr>
<tr><td><img src="/images/flags/JP.png"> Japan</td>
<td><b>vpn395062778.opengw.net</b> 50.110.65.218</td>
<td></td></tr>
</table>
<table><tr><td>VPN-23751640530</td><td>United Kingdom</td><td>163.131.1.1</td></tr></table>
</body></html>"#
    }

    #[test]
    fn parse_web_page_sample() {
        let nodes = parse_web_page(sample_relay_html());
        // Table order, deduplicated; the session-log table contributes nothing.
        assert_eq!(nodes.len(), 4);
        assert_eq!(nodes[0].host, "public-vpn-52.opengw.net");
        assert_eq!(nodes[0].country, "Japan");
        assert_eq!(nodes[0].port, Some(443), "no explicit port -> 443 default");
        assert_eq!(nodes[0].source, "web-scrape");
        assert_eq!(nodes[1].host, "vpn678091301.opengw.net");
        assert_eq!(nodes[1].country, "United States");
        assert_eq!(
            nodes[1].port,
            Some(1427),
            "explicit SSTP port from the MS-SSTP cell"
        );
        // kanratown is in the curated table with known port 992.
        assert_eq!(nodes[2].host, "kanratown.opengw.net");
        assert_eq!(
            nodes[2].port,
            Some(992),
            "curated port beats the scraped default"
        );
        assert_eq!(nodes[2].port_source, "curated");
        // Row with an empty MS-SSTP cell still yields a node on port 443.
        assert_eq!(nodes[3].host, "vpn395062778.opengw.net");
        assert_eq!(nodes[3].port, Some(443));
        // ISP hostnames (vpngate.v4.open.ad.jp) must not leak in.
        assert!(nodes.iter().all(|n| n.host.ends_with(".opengw.net")));
    }

    #[test]
    fn parse_web_page_dedupes_and_ignores_garbage() {
        // Same host in two rows: first occurrence wins, explicit port upgrades.
        let html = r#"<table>
<tr><td>Japan</td><td><b>vpn111.opengw.net</b> 1.1.1.1</td><td>SSTP 主机名 : <b>vpn111.opengw.net</b></td></tr>
<tr><td>Japan</td><td><b>vpn111.opengw.net</b> 1.1.1.1</td><td>SSTP 主机名 : <b>vpn111.opengw.net:1999</b></td></tr>
<tr><td>Japan</td><td><b>not-a-host.example.com</b></td><td></td></tr>
</table>"#;
        let nodes = parse_web_page(html);
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].host, "vpn111.opengw.net");
        assert_eq!(nodes[0].port, Some(1999));

        assert!(parse_web_page("").is_empty());
        assert!(parse_web_page("<html><body>no table here</body></html>").is_empty());
        assert!(parse_web_page("<tr><td>no hostname at all</td></tr>").is_empty());
    }

    #[test]
    fn fallback_chain_prefers_api_then_web_then_cache() {
        let one = |h: &str| Node {
            host: h.to_string(),
            port: Some(443),
            country: "Japan".to_string(),
            country_code: "JP".to_string(),
            isp: String::new(),
            name: String::new(),
            score: 0,
            ping_ms: 0,
            speed_mbps: 0,
            sessions: 0,
            ip: String::new(),
            backups: Vec::new(),
            port_source: "api".to_string(),
            source: "api".to_string(),
        };
        // API wins when present, even if web and cache also exist.
        let (nodes, src, age) = pick_node_list(
            Some(vec![one("api.opengw.net")]),
            Some(vec![one("web.opengw.net")]),
            Some((vec![one("cache.opengw.net")], 3600)),
        );
        assert_eq!(src, NodeListSource::Api);
        assert_eq!(nodes[0].host, "api.opengw.net");
        assert_eq!(age, None);
        // API empty -> web scrape wins over cache.
        let (nodes, src, _) = pick_node_list(
            Some(vec![]),
            Some(vec![one("web.opengw.net")]),
            Some((vec![one("cache.opengw.net")], 3600)),
        );
        assert_eq!(src, NodeListSource::WebScrape);
        assert_eq!(nodes[0].host, "web.opengw.net");
        // Both empty -> cache, with its age.
        let (nodes, src, age) =
            pick_node_list(None, None, Some((vec![one("cache.opengw.net")], 7200)));
        assert_eq!(src, NodeListSource::Cache);
        assert_eq!(nodes[0].host, "cache.opengw.net");
        assert_eq!(age, Some(7200));
        // Nothing at all -> bundled curated list.
        let (nodes, src, _) = pick_node_list(None, None, None);
        assert_eq!(src, NodeListSource::Curated);
        assert_eq!(nodes.len(), 28);
    }
}

/// Resolve a user-supplied target into an ordered list of (host, port)
/// candidates to try. Backups from the curated list are appended.
pub fn resolve_target(target: &str) -> Vec<(String, u16)> {
    // host:port
    if let Some((h, p)) = target.rsplit_once(':') {
        if let Ok(port) = p.parse::<u16>() {
            if !h.is_empty() {
                return vec![(complete_ddns_host(h), port)];
            }
        }
    }
    // Index into the curated list.
    if let Ok(idx) = target.parse::<usize>() {
        let nodes = curated_nodes();
        if let Some(n) = nodes.get(idx) {
            let mut out = vec![(n.host.clone(), n.port.unwrap_or(443))];
            for b in &n.backups {
                if let Some((h, p)) = b.rsplit_once(':') {
                    if let Ok(port) = p.parse::<u16>() {
                        out.push((h.to_string(), port));
                    }
                }
            }
            return out;
        }
        eprintln!("curated index {} out of range (0..{})", idx, nodes.len());
        std::process::exit(2);
    }
    // Hostname known in the curated list?
    let nodes = curated_nodes();
    if let Some(n) = nodes.iter().find(|n| n.host == target) {
        let mut out = vec![(n.host.clone(), n.port.unwrap_or(443))];
        for b in &n.backups {
            if let Some((h, p)) = b.rsplit_once(':') {
                if let Ok(port) = p.parse::<u16>() {
                    out.push((h.to_string(), port));
                }
            }
        }
        return out;
    }
    // Unknown hostname: try the common SSTP ports.
    CANDIDATE_SSTP_PORTS
        .iter()
        .map(|p| (complete_ddns_host(target), *p))
        .collect()
}

/// A bare VPNGate label with no dot is a DDNS hostname whose
/// ".opengw.net" suffix is missing. Leo's v3.1 log showed API nodes
/// arriving as "public-vpn-192" / "vpn321359264" (and even "opengw"),
/// and the probe then dialled the bare label — whatever the front
/// proxy's DNS made of it — instead of the real relay. Complete it
/// defensively; FQDNs (they contain a dot) and IP addresses are
/// untouched. Public so the GUI can log the completed host:port in
/// its probe/connect start lines.
pub fn complete_ddns_host(host: &str) -> String {
    if host.contains('.') {
        host.to_string()
    } else {
        format!("{}.opengw.net", host)
    }
}
