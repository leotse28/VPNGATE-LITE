//! DNS resolution for tunnel targets.
//!
//! Order (ported from the JS reference):
//! 1. Literal IPv4 → as-is.
//! 2. Small static map (google/gstatic fallback used by the JS code).
//! 3. DNS-over-HTTPS JSON to 1.1.1.1, then cloudflare-dns.com.
//! 4. System resolver (`tokio::net::lookup_host`) as a last resort.

use crate::net::{https_get, Connector};

const STATIC: &[(&str, &str)] = &[
    ("cp.cloudflare.com", "104.16.132.229"),
    ("one.one.one.one", "1.1.1.1"),
    ("api.ipify.org", "172.67.74.152"),
    ("icanhazip.com", "104.18.114.97"),
];

fn is_ipv4(s: &str) -> bool {
    s.split('.').count() == 4
        && s.split('.').all(|p| {
            !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) && p.parse::<u8>().is_ok()
        })
}

/// Resolve `host` to an IPv4 string.
pub async fn resolve_ipv4(connector: &Connector, host: &str) -> Result<String, String> {
    if is_ipv4(host) {
        return Ok(host.to_string());
    }
    if let Some((_, ip)) = STATIC.iter().find(|(h, _)| *h == host) {
        return Ok(ip.to_string());
    }
    for base in [
        "https://1.1.1.1/dns-query",
        "https://cloudflare-dns.com/dns-query",
    ] {
        let url = format!("{}?name={}&type=A", base, host);
        match https_get(connector, &url, &[("Accept", "application/dns-json")], true).await {
            Ok((200, body)) => {
                if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&body) {
                    if let Some(ans) = v.get("Answer").and_then(|a| a.as_array()) {
                        for a in ans {
                            if a.get("type").and_then(|t| t.as_u64()) == Some(1) {
                                if let Some(ip) = a.get("data").and_then(|d| d.as_str()) {
                                    return Ok(ip.to_string());
                                }
                            }
                        }
                    }
                }
            }
            Ok((code, _)) => {
                eprintln!("DoH {} returned HTTP {}", base, code);
            }
            Err(e) => {
                eprintln!("DoH {} failed: {}", base, e);
            }
        }
    }
    // Fallback: system resolver (may be poisoned behind a firewall; best effort).
    if host.ends_with("gstatic.com") || host.ends_with("google.com") {
        return Ok("142.251.150.120".to_string());
    }
    match tokio::net::lookup_host((host, 80)).await {
        Ok(mut it) => match it.next() {
            Some(sa) => Ok(sa.ip().to_string()),
            None => Err(format!("system DNS: no address for {}", host)),
        },
        Err(e) => Err(format!("DNS failed for {}: {}", host, e)),
    }
}
