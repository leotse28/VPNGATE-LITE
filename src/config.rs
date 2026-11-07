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
//! port = 10808
//! allow_lan = false
//!
//! [system_proxy]
//! enabled = false
//! # service = "Wi-Fi"   # macOS network service name
//!
//! [tunnel]
//! timeout_secs = 10
//! ```

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct Config {
    #[serde(default)]
    pub front_proxy: FrontProxy,
    #[serde(default)]
    pub listen: Listen,
    #[serde(default)]
    pub system_proxy: SystemProxy,
    #[serde(default)]
    pub tunnel: Tunnel,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
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

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Listen {
    #[serde(default = "default_listen_addr")]
    pub addr: String,
    #[serde(default = "default_listen_port")]
    pub port: u16,
    #[serde(default)]
    pub allow_lan: bool,
}

impl Default for Listen {
    fn default() -> Self {
        Self {
            addr: default_listen_addr(),
            port: default_listen_port(),
            allow_lan: false,
        }
    }
}

fn default_listen_addr() -> String {
    "127.0.0.1".to_string()
}

fn default_listen_port() -> u16 {
    10808
}

#[derive(Debug, Clone, Deserialize, Serialize)]
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

#[derive(Debug, Clone, Deserialize, Serialize)]
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

impl Config {
    /// Directory holding this executable — the portable home for
    /// `config.toml` and `nodes_cache.json`. `None` when the exe path
    /// cannot be determined (rare; then only the working directory
    /// is used).
    pub fn portable_dir() -> Option<PathBuf> {
        std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf()))
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
        let text = toml::to_string(self)
            .map_err(|e| format!("cannot encode config: {}", e))?;
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("cannot create {}: {}", parent.display(), e))?;
            }
        }
        std::fs::write(path, text)
            .map_err(|e| format!("cannot write config {}: {}", path.display(), e))
    }

    /// Effective listen address, honouring --allow-lan.
    pub fn listen_addr(&self, allow_lan: bool) -> String {
        if allow_lan || self.listen.allow_lan {
            format!("0.0.0.0:{}", self.listen.port)
        } else {
            format!("{}:{}", self.listen.addr, self.listen.port)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tmp_path(name: &str) -> PathBuf {
        let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        std::env::temp_dir().join(format!("vpngate-cfg-test-{}-{}.toml", n, name))
    }

    #[test]
    fn save_load_roundtrip() {
        let mut cfg = Config::default();
        cfg.front_proxy.enabled = true;
        cfg.front_proxy.addr = "127.0.0.1:1080".to_string();
        cfg.front_proxy.username = Some("u".to_string());
        cfg.listen.port = 10809;
        cfg.listen.allow_lan = true;
        cfg.system_proxy.enabled = true;
        cfg.tunnel.timeout_secs = 15;
        let path = tmp_path("roundtrip");
        cfg.save(&path).expect("save works");
        let back = Config::load(Some(&path)).expect("load works");
        assert!(back.front_proxy.enabled);
        assert_eq!(back.front_proxy.username.as_deref(), Some("u"));
        assert_eq!(back.listen.port, 10809);
        assert!(back.listen.allow_lan);
        assert!(back.system_proxy.enabled);
        assert_eq!(back.tunnel.timeout_secs, 15);
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
