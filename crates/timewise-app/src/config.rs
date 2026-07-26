//! Per-OS-account configuration (application-design §4).
//!
//! `TIMEWISE_HOME` overrides the platform config dir — this is what lets a
//! master and a worker run side by side on one machine for local testing.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const DEFAULT_PORT: u16 = 47820;
pub const DEFAULT_SYNC_INTERVAL_S: u64 = 60;
pub const DEFAULT_HEARTBEAT_INTERVAL_S: u64 = 30;
pub const DEFAULT_BREAK_PROMPT_AFTER_MIN: u64 = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Master,
    Worker,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MasterRegistration {
    pub base_url: String,
    pub token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// None until the first-run role screen completes (US-01 AC1).
    pub role: Option<Role>,
    /// Generated on first run; identifies this account's instance forever.
    pub worker_id: String,
    pub masters: Vec<MasterRegistration>,
    pub sync_interval_s: u64,
    pub heartbeat_interval_s: u64,
    pub break_prompt_after_min: u64,
    /// Master only: LAN port for the embedded API.
    pub port: u16,
    /// Pause tracking after this many seconds without keyboard/mouse input.
    #[serde(default = "default_idle_threshold_s")]
    pub idle_threshold_s: u64,
    /// Master only: also track this account's own usage (iteration 2).
    #[serde(default)]
    pub track_self: bool,
    /// Master only: token for the self-tracking worker (generated on enable).
    #[serde(default)]
    pub self_token: Option<String>,
}

fn default_idle_threshold_s() -> u64 {
    300
}

impl Config {
    pub fn new_first_run() -> Self {
        Config {
            role: None,
            worker_id: uuid::Uuid::new_v4().to_string(),
            masters: Vec::new(),
            sync_interval_s: DEFAULT_SYNC_INTERVAL_S,
            heartbeat_interval_s: DEFAULT_HEARTBEAT_INTERVAL_S,
            break_prompt_after_min: DEFAULT_BREAK_PROMPT_AFTER_MIN,
            port: DEFAULT_PORT,
            idle_threshold_s: default_idle_threshold_s(),
            track_self: false,
            self_token: None,
        }
    }

    /// Per-account config directory. Honors TIMEWISE_HOME for testing.
    pub fn dir() -> PathBuf {
        if let Ok(home) = std::env::var("TIMEWISE_HOME") {
            return PathBuf::from(home);
        }
        directories::ProjectDirs::from("com", "timewise", "timewise")
            .map(|d| d.config_dir().to_path_buf())
            .unwrap_or_else(|| PathBuf::from(".timewise"))
    }

    fn path_in(dir: &Path) -> PathBuf {
        dir.join("config.json")
    }

    pub fn load_from(dir: &Path) -> std::io::Result<Self> {
        match std::fs::read(Self::path_in(dir)) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::new_first_run()),
            Err(e) => Err(e),
        }
    }

    pub fn save_to(&self, dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let json = serde_json::to_vec_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(Self::path_in(dir), json)
    }

    pub fn buffer_db_path(dir: &Path) -> PathBuf {
        dir.join("buffer.db")
    }

    pub fn master_db_path(dir: &Path) -> PathBuf {
        dir.join("timewise.db")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_preserves_all_fields() {
        let dir = std::env::temp_dir().join(format!("timewise-test-{}", uuid::Uuid::new_v4()));
        let mut cfg = Config::new_first_run();
        cfg.role = Some(Role::Worker);
        cfg.masters.push(MasterRegistration {
            base_url: "http://192.168.1.10:47820".into(),
            token: "tok".into(),
        });
        cfg.save_to(&dir).unwrap();
        let loaded = Config::load_from(&dir).unwrap();
        assert_eq!(loaded.role, Some(Role::Worker));
        assert_eq!(loaded.worker_id, cfg.worker_id);
        assert_eq!(loaded.masters.len(), 1);
        assert_eq!(loaded.masters[0].base_url, "http://192.168.1.10:47820");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_file_yields_first_run_defaults() {
        let dir = std::env::temp_dir().join(format!("timewise-test-{}", uuid::Uuid::new_v4()));
        let cfg = Config::load_from(&dir).unwrap();
        assert_eq!(cfg.role, None);
        assert_eq!(cfg.sync_interval_s, DEFAULT_SYNC_INTERVAL_S);
        assert_eq!(cfg.port, DEFAULT_PORT);
        assert!(!cfg.worker_id.is_empty());
    }
}
