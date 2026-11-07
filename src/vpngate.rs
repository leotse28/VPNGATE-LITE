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
    /// Where the SSTP port came from: "curated" | "manual" | "probed" | "unknown".
    #[serde(default)]
    pub port_source: String,
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
            isp: cols[11].to_string(),
            name: cols[11].to_string(),
            score: num(2),
            ping_ms: num(3),
            speed_mbps: num(4),
            sessions: num(7),
            ip: cols[1].to_string(),
            backups: Vec::new(),
            port_source: "unknown".to_string(),
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
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
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
    let payload = CachedList { fetched_at: now_unix(), nodes: nodes.to_vec() };
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
    }

    #[test]
    fn curated_has_28_known_ports() {
        let nodes = curated_nodes();
        assert_eq!(nodes.len(), 28);
        assert!(nodes.iter().all(|n| n.port.is_some()));
        assert!(nodes.iter().any(|n| n.host == "kanratown.opengw.net" && n.port == Some(992)));
    }

    #[test]
    fn node_cache_roundtrip() {
        let dir = std::env::temp_dir();
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
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
}

/// Resolve a user-supplied target into an ordered list of (host, port)
/// candidates to try. Backups from the curated list are appended.
pub fn resolve_target(target: &str) -> Vec<(String, u16)> {
    // host:port
    if let Some((h, p)) = target.rsplit_once(':') {
        if let Ok(port) = p.parse::<u16>() {
            if !h.is_empty() {
                return vec![(h.to_string(), port)];
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
        .map(|p| (target.to_string(), *p))
        .collect()
}

