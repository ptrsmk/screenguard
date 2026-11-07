use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

const DEFAULT_CONFIG_PATH: &str = "/etc/screenguard/agent.toml";
const CONFIG_PATH_ENV: &str = "SCREENGUARD_AGENT_CONFIG";

/// Default endpoint for **experimental** cloud mode (`--cloud-account`).
/// Overridable via `cloud_url` in agent.toml or `--server-url` on the CLI.
pub const DEFAULT_CLOUD_URL: &str = "wss://api.screenguard.cc/ws";

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct AgentConfig {
    /// Direct server WSS URL; if set, mDNS discovery is skipped.
    pub server_url: Option<String>,

    /// Web UI URL shown in the tray "Open Admin Page" menu item.
    /// If unset, derived from server_url (same host, same port).
    pub webui_url: Option<String>,

    /// **Experimental.** Cloud account (email) to bind this agent to. When set
    /// (here or via `--cloud-account`), mDNS/local discovery is skipped and the
    /// agent connects to the cloud endpoint. Keep it here so it survives
    /// restarts — the agent must present it on every reconnect.
    pub cloud_account: Option<String>,

    /// **Experimental.** Override for the cloud endpoint used in cloud mode.
    /// Defaults to [`DEFAULT_CLOUD_URL`]. `--server-url` overrides this too.
    pub cloud_url: Option<String>,

    #[serde(default = "default_heartbeat_interval")]
    pub heartbeat_interval: u64,

    #[serde(default = "default_user_scan_interval")]
    pub user_scan_interval: u64,

    #[serde(default = "default_cache_ttl_hours")]
    pub cache_ttl_hours: u64,

    #[serde(default = "default_min_uid")]
    pub min_uid: u32,

    /// Per-app usage reports (#27). `app_tracking = false` turns off scanning,
    /// storing and sending per-app usage on this machine.
    #[serde(default = "default_app_tracking")]
    pub app_tracking: bool,

    /// GitHub repo (`owner/repo`) whose latest release a remote update
    /// (Agents → Update) installs. Set on the agent, never by the server, so a
    /// server can only ask for an update, not choose what code runs as root.
    #[serde(default = "default_update_repo")]
    pub update_repo: String,
}

fn default_heartbeat_interval() -> u64 { 10 }
fn default_user_scan_interval() -> u64 { 300 }
fn default_cache_ttl_hours() -> u64 { 48 }
fn default_min_uid() -> u32 { 1000 }
fn default_app_tracking() -> bool { true }
fn default_update_repo() -> String { DEFAULT_UPDATE_REPO.to_string() }

pub const DEFAULT_UPDATE_REPO: &str = "adambie/screenguard";

/// `owner/repo` as GitHub allows it: one slash, ASCII letters, digits, `-`,
/// `_` and `.`, and neither part `.` or `..`.
pub fn is_valid_repo(repo: &str) -> bool {
    let mut parts = repo.split('/');
    let (Some(owner), Some(name), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    [owner, name].iter().all(|p| {
        !p.is_empty()
            && *p != "."
            && *p != ".."
            && p.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    })
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            server_url: None,
            webui_url: None,
            cloud_account: None,
            cloud_url: None,
            heartbeat_interval: default_heartbeat_interval(),
            user_scan_interval: default_user_scan_interval(),
            cache_ttl_hours: default_cache_ttl_hours(),
            min_uid: default_min_uid(),
            app_tracking: default_app_tracking(),
            update_repo: default_update_repo(),
        }
    }
}

pub fn load(path: Option<&str>) -> Result<AgentConfig> {
    let env_path = std::env::var(CONFIG_PATH_ENV).ok();
    let path = path.or(env_path.as_deref()).unwrap_or(DEFAULT_CONFIG_PATH);
    if !Path::new(path).exists() {
        tracing::warn!("Config file not found at {path}, using defaults");
        return Ok(AgentConfig::default());
    }
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read config file: {path}"))?;
    let config: AgentConfig = toml::from_str(&raw)
        .with_context(|| format!("Failed to parse config file: {path}"))?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_tracking_defaults_on_and_can_be_disabled() {
        let cfg: AgentConfig = toml::from_str("").unwrap();
        assert!(cfg.app_tracking);
        assert!(AgentConfig::default().app_tracking);
        let cfg: AgentConfig = toml::from_str("app_tracking = false").unwrap();
        assert!(!cfg.app_tracking);
    }

    #[test]
    fn update_repo_defaults_to_upstream_and_can_be_set() {
        let cfg: AgentConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.update_repo, "adambie/screenguard");
        assert_eq!(AgentConfig::default().update_repo, "adambie/screenguard");
        let cfg: AgentConfig = toml::from_str(r#"update_repo = "someone/screenguard""#).unwrap();
        assert_eq!(cfg.update_repo, "someone/screenguard");
    }

    #[test]
    fn repo_validation() {
        for ok in ["adambie/screenguard", "some-one/screen_guard.fork", "a/b"] {
            assert!(is_valid_repo(ok), "{ok}");
        }
        for bad in [
            "", "screenguard", "a/b/c", "/b", "a/", "../b", "a/..", "a b/c",
            "a/b;rm -rf /", "a/$(id)", "a/b\nc", "ä/b",
        ] {
            assert!(!is_valid_repo(bad), "{bad}");
        }
    }
}
