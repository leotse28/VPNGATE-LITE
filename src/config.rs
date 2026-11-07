//! TOML configuration. CLI flags override the file; the file is optional.
//!
//! Portable layout: the program keeps everything next to its own
//! executable, nothing in %APPDATA% or the user profile. Config is read
//! from (in order) `--config <path>`, `config.toml` next to the exe,
//! `./config.toml` in the working directory; if none exists, defaults
//! are used. The GUI's "保存设置" writes to `config.toml` next to the
//! exe, so the whole folder can be copied elsewhere and keeps working.
//!
//! Example (`config.example.toml`):
//! ```toml
//! [front_proxy]
//! enabled = false
//! addr = "127.0.0.1:1080"
//! # username = ""
//! # password = ""
//!
//! [listen]
//! addr = "127.0.0.1"
//! local_port = 10818
//! lan_port = 10809
//! allow_lan = false
//!
//! [system_proxy]
//! enabled = false
//! # service = "Wi-Fi"   # macOS network service name
//!
//! [tunnel]
//! timeout_secs = 10
//!
//! [ui]
//! node_col_widths = [90, 130, 190, 90]
//! ```

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq, Eq)]
pub struct Config {
    #[serde(default)]
    pub front_proxy: FrontProxy,
    #[serde(default)]
    pub listen: Listen,
    #[serde(default)]
    pub system_proxy: SystemProxy,
    #[serde(default)]
    pub tunnel: Tunnel,
    #[serde(default)]
    pub ui: UiSettings,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct FrontProxy {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_front_addr")]
    pub addr: String,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
}

impl Default for FrontProxy {
    fn default() -> Self {
        Self {
            enabled: false,
            addr: default_front_addr(),
            username: None,
            password: None,
        }
    }
}

fn default_front_addr() -> String {
    "127.0.0.1:1080".to_string()
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Listen {
    #[serde(default = "default_listen_addr")]
    pub addr: String,
    /// Local SOCKS5 port, always listened on. `port` stays accepted as
    /// an alias so config files written by older versions keep working.
    #[serde(default = "default_local_port", alias = "port")]
    pub local_port: u16,
    /// Extra LAN SOCKS5 port, only listened on when LAN sharing is on.
    /// Must differ from `local_port` when LAN sharing is on.
    #[serde(default = "default_lan_port")]
    pub lan_port: u16,
    #[serde(default)]
    pub allow_lan: bool,
}

impl Default for Listen {
    fn default() -> Self {
        Self {
            addr: default_listen_addr(),
            local_port: default_local_port(),
            lan_port: default_lan_port(),
            allow_lan: false,
        }
    }
}

fn default_listen_addr() -> String {
    "127.0.0.1".to_string()
}

fn default_local_port() -> u16 {
    10818
}

fn default_lan_port() -> u16 {
    10809
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct SystemProxy {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub service: Option<String>,
}

impl Default for SystemProxy {
    fn default() -> Self {
        Self {
            enabled: false,
            service: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Tunnel {
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

impl Default for Tunnel {
    fn default() -> Self {
        Self {
            timeout_secs: default_timeout(),
        }
    }
}

fn default_timeout() -> u64 {
    10
}

/// GUI-only settings: no effect on the CLI.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct UiSettings {
    /// Node-list column widths in pixels (country, ISP, host:port,
    /// latency/status). Draggable at the header separators in the GUI;
    /// persisted with the rest of the settings via auto-save.
    #[serde(default = "default_col_widths")]
    pub node_col_widths: [u16; 4],
}

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            node_col_widths: default_col_widths(),
        }
    }
}

fn default_col_widths() -> [u16; 4] {
    [90, 130, 190, 90]
}

impl Config {
    /// Directory holding this executable — the portable home for
    /// `config.toml` and `nodes_cache.json`. `None` when the exe path
    /// cannot be determined (rare; then only the working directory
    /// is used).
    pub fn portable_dir() -> Option<PathBuf> {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
    }

    /// `config.toml` next to the executable (portable location), whether
    /// or not the file exists yet. This is where the GUI saves settings.
    pub fn portable_config_path() -> Option<PathBuf> {
        Self::portable_dir().map(|d| d.join("config.toml"))
    }

    /// Where to load the config from: an explicit `--config` path first,
    /// then `config.toml` next to the exe if it exists, then
    /// `./config.toml` in the working directory if it exists.
    /// `None` means "no file — use defaults".
    pub fn resolve_config_path(explicit: Option<&Path>) -> Option<PathBuf> {
        if let Some(p) = explicit {
            return Some(p.to_path_buf());
        }
        if let Some(p) = Self::portable_config_path() {
            if p.exists() {
                return Some(p);
            }
        }
        let cwd = PathBuf::from("config.toml");
        if cwd.exists() {
            return Some(cwd);
        }
        None
    }

    /// Load from `path` if it exists; otherwise return defaults.
    /// A missing file is not an error.
    pub fn load(path: Option<&Path>) -> Result<Config, String> {
        let Some(path) = path else {
            return Ok(Config::default());
        };
        if !path.exists() {
            return Ok(Config::default());
        }
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read config {}: {}", path.display(), e))?;
        toml::from_str(&text).map_err(|e| format!("cannot parse config {}: {}", path.display(), e))
    }

    /// Write this config as TOML to `path` (creates parent dirs if
    /// needed). Used by the GUI's "保存设置".
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let text = toml::to_string(self).map_err(|e| format!("cannot encode config: {}", e))?;
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("cannot create {}: {}", parent.display(), e))?;
            }
        }
        std::fs::write(path, text)
            .map_err(|e| format!("cannot write config {}: {}", path.display(), e))
    }
}

impl Listen {
    /// Addresses to listen on: always `addr:local_port`, plus
    /// `0.0.0.0:lan_port` when LAN sharing is on. Both SOCKS5 servers
    /// share one tunnel. Errors when LAN sharing is on and both ports
    /// are the same (two binds cannot share one port).
    pub fn listen_addrs(&self) -> Result<Vec<String>, String> {
        let mut addrs = vec![format!("{}:{}", self.addr, self.local_port)];
        if self.allow_lan {
            if self.lan_port == self.local_port {
                return Err(format!(
                    "本地端口和局域网端口不能设成一样（都是 {}），改一个再连。",
                    self.local_port
                ));
            }
            addrs.push(format!("0.0.0.0:{}", self.lan_port));
        }
        Ok(addrs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tmp_path(name: &str) -> PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("vpngate-cfg-test-{}-{}.toml", n, name))
    }

    #[test]
    fn save_load_roundtrip() {
        let mut cfg = Config::default();
        cfg.front_proxy.enabled = true;
        cfg.front_proxy.addr = "127.0.0.1:1080".to_string();
        cfg.front_proxy.username = Some("u".to_string());
        cfg.front_proxy.password = Some("p".to_string());
        cfg.listen.local_port = 10809;
        cfg.listen.lan_port = 10810;
        cfg.listen.allow_lan = true;
        cfg.system_proxy.enabled = true;
        cfg.tunnel.timeout_secs = 15;
        let path = tmp_path("roundtrip");
        cfg.save(&path).expect("save works");
        let back = Config::load(Some(&path)).expect("load works");
        assert!(back.front_proxy.enabled);
        assert_eq!(back.front_proxy.username.as_deref(), Some("u"));
        assert_eq!(back.front_proxy.password.as_deref(), Some("p"));
        assert_eq!(back.listen.local_port, 10809);
        assert_eq!(back.listen.lan_port, 10810);
        assert!(back.listen.allow_lan);
        assert!(back.system_proxy.enabled);
        assert_eq!(back.tunnel.timeout_secs, 15);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn listen_addrs_local_always_lan_extra() {
        let mut l = Listen::default();
        assert_eq!(
            l.listen_addrs().unwrap(),
            vec!["127.0.0.1:10818".to_string()]
        );
        l.allow_lan = true;
        assert_eq!(
            l.listen_addrs().unwrap(),
            vec!["127.0.0.1:10818".to_string(), "0.0.0.0:10809".to_string()]
        );
        l.lan_port = 10818;
        let err = l.listen_addrs().unwrap_err();
        assert!(
            err.contains("不能设成一样"),
            "equal ports must fail loudly: {}",
            err
        );
    }

    #[test]
    fn old_port_key_still_parses_as_local_port() {
        let cfg: Config = toml::from_str("[listen]\nport = 10811\n").unwrap();
        assert_eq!(cfg.listen.local_port, 10811);
        assert_eq!(cfg.listen.lan_port, 10809, "lan_port keeps its default");
    }

    #[test]
    fn ui_col_widths_default_and_roundtrip() {
        let cfg = Config::default();
        assert_eq!(cfg.ui.node_col_widths, [90, 130, 190, 90]);
        let parsed: Config =
            toml::from_str("[ui]\nnode_col_widths = [120, 140, 200, 100]\n").unwrap();
        assert_eq!(parsed.ui.node_col_widths, [120, 140, 200, 100]);
        // A config file without [ui] keeps the defaults.
        let parsed2: Config = toml::from_str("[listen]\nlocal_port = 10818\n").unwrap();
        assert_eq!(parsed2.ui.node_col_widths, [90, 130, 190, 90]);
        assert_eq!(parsed2.listen.local_port, 10818);
        let path = tmp_path("ui-widths");
        parsed.save(&path).expect("save works");
        let back = Config::load(Some(&path)).expect("load works");
        assert_eq!(back.ui.node_col_widths, [120, 140, 200, 100]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn resolve_prefers_explicit_path() {
        let p = PathBuf::from("does-not-exist.toml");
        assert_eq!(Config::resolve_config_path(Some(&p)), Some(p));
    }

    #[test]
    fn portable_path_is_next_to_exe() {
        let p = Config::portable_config_path().expect("exe path known in tests");
        assert_eq!(p.file_name().unwrap(), "config.toml");
        assert!(p.parent().is_some());
    }
}
