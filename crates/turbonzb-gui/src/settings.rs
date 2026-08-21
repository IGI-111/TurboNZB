//! Persisted application configuration.
//!
//! Stored as JSON in the OS config directory (`~/.config/turbonzb/config.json`
//! on Linux, `%APPDATA%\turbonzb` on Windows, `~/Library/Application Support/turbonzb`
//! on macOS). On first run — when the file is absent — the wizard runs to
//! collect the NNTP server and at least one indexer.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use turbonzb_core::nntp::ServerConfig;
use turbonzb_index::types::IndexerConfig;

/// File name for the config JSON.
const CONFIG_FILENAME: &str = "config.json";

/// Top-level application configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// Configured NNTP servers (priority order).
    pub servers: Vec<ServerEntry>,
    /// Configured Newznab indexers.
    pub indexers: Vec<IndexerConfig>,
    /// Directory for the SQLite queue database.
    pub db_path: PathBuf,
    /// Directory for in-progress downloads (temp).
    pub download_dir: PathBuf,
    /// Directory for completed/unpacked files.
    pub completed_dir: PathBuf,
    /// Default category → subfolder mapping (category name → subfolder).
    pub categories: Vec<CategoryMapping>,
    /// Default max simultaneous NNTP connections across all servers.
    pub max_connections: usize,
    /// Default post-processing behavior.
    pub post_process: PostProcessDefaults,
}

/// An NNTP server entry in settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServerEntry {
    pub host: String,
    pub port: u16,
    pub tls: bool,
    pub user: Option<String>,
    pub password: Option<String>,
    pub max_connections: u32,
    pub priority: u32,
}

impl From<&ServerEntry> for ServerConfig {
    fn from(s: &ServerEntry) -> Self {
        Self {
            host: s.host.clone(),
            port: s.port,
            tls: s.tls,
            user: s.user.clone(),
            password: s.password.clone(),
            max_connections: s.max_connections,
            priority: s.priority,
        }
    }
}

impl From<ServerEntry> for ServerConfig {
    fn from(s: ServerEntry) -> Self {
        Self::from(&s)
    }
}

/// A category → subfolder mapping.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CategoryMapping {
    pub name: String,
    pub subfolder: String,
}

/// Default post-processing behavior.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostProcessDefaults {
    /// Auto-run post-processing after a download completes.
    pub auto_post_process: bool,
    /// Skip PAR2 verification.
    pub skip_verify: bool,
    /// Delete archive files and temp dirs after successful unpack.
    pub cleanup_archives: bool,
}

impl Default for PostProcessDefaults {
    fn default() -> Self {
        Self {
            auto_post_process: true,
            skip_verify: false,
            cleanup_archives: true,
        }
    }
}

impl AppConfig {
    /// Resolve the config file path in the OS config directory.
    pub fn config_path(dirs: &directories::ProjectDirs) -> PathBuf {
        dirs.config_dir().join(CONFIG_FILENAME)
    }

    /// Load config from the given path, or return `None` if the file does
    /// not exist (first run → wizard).
    pub fn load(path: &Path) -> Option<Self> {
        let bytes = std::fs::read(path).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Save config to the given path, creating parent dirs as needed.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json)?;
        Ok(())
    }

    /// Build a default config from the OS directories, using sensible
    /// defaults for paths.
    pub fn defaults(dirs: &directories::ProjectDirs) -> Self {
        let data_dir = dirs.data_dir().to_path_buf();
        // Default download/completed dirs to the OS Downloads directory
        // (~/Downloads on Linux, etc.). Fall back to data_dir if unavailable.
        let downloads_base = directories::UserDirs::new()
            .and_then(|d| d.download_dir().map(|p| p.to_path_buf()))
            .unwrap_or_else(|| data_dir.join("downloads"));
        let download_dir = downloads_base.join("turbonzb");
        let completed_dir = download_dir.join("completed");
        Self {
            servers: Vec::new(),
            indexers: Vec::new(),
            db_path: data_dir.join("turbonzb-queue.db"),
            download_dir,
            completed_dir,
            categories: vec![
                CategoryMapping {
                    name: "tv".into(),
                    subfolder: "tv".into(),
                },
                CategoryMapping {
                    name: "movies".into(),
                    subfolder: "movies".into(),
                },
                CategoryMapping {
                    name: "music".into(),
                    subfolder: "music".into(),
                },
                CategoryMapping {
                    name: "books".into(),
                    subfolder: "books".into(),
                },
            ],
            max_connections: 0,
            post_process: PostProcessDefaults::default(),
        }
    }

    /// Check if the config is complete enough to skip the wizard.
    pub fn is_configured(&self) -> bool {
        !self.servers.is_empty() && !self.indexers.is_empty()
    }

    /// Convert server entries to `ServerConfig`s.
    pub fn server_configs(&self) -> Vec<ServerConfig> {
        let mut servers: Vec<ServerConfig> = self.servers.iter().map(Into::into).collect();
        servers.sort_by_key(|s| s.priority);
        servers
    }

    /// Compute the effective total connection count.
    ///
    /// If `max_connections` is 0 (infinite), the total is the sum of all
    /// servers' `max_connections`. Otherwise, `max_connections` acts as a
    /// ceiling: `min(sum_of_server_connections, max_connections)`.
    pub fn effective_max_connections(&self) -> usize {
        let server_total: usize = self
            .servers
            .iter()
            .map(|s| s.max_connections as usize)
            .sum();
        if self.max_connections == 0 {
            server_total.max(1)
        } else {
            server_total.min(self.max_connections).max(1)
        }
    }
}
