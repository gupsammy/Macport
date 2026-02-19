use std::fs::{self, Permissions};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Config {
    #[serde(default)]
    pub monitoring: MonitoringConfig,
    #[serde(default)]
    pub integrations: IntegrationsConfig,
    #[serde(default)]
    pub notifications: NotificationsConfig,
    #[serde(default)]
    pub system: SystemConfig,
    #[serde(default)]
    pub updates: UpdateConfig,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct MonitoringConfig {
    pub poll_interval_secs: u64,
    pub port_ranges: Vec<(u16, u16)>,
    pub show_project_names: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct IntegrationsConfig {
    pub brew_enabled: bool,
    pub docker_enabled: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct NotificationsConfig {
    pub enabled: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct SystemConfig {
    pub launch_at_login: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct UpdateConfig {
    pub check_enabled: bool,
    pub check_interval_hours: u64,
    pub last_check_timestamp: Option<i64>,
    pub dismissed_version: Option<String>,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self {
            check_enabled: true,
            check_interval_hours: 24,
            last_check_timestamp: None,
            dismissed_version: None,
        }
    }
}

impl Default for MonitoringConfig {
    fn default() -> Self {
        Self {
            poll_interval_secs: 2,
            port_ranges: vec![
                (3000, 3010),   // Node.js, React, Next.js, Vite
                (3306, 3306),   // MySQL
                (4000, 4010),   // Alternative Node servers
                (5001, 5010),   // Flask, general dev servers (excluding 5000)
                (5173, 5173),   // Vite default
                (5432, 5432),   // PostgreSQL
                (6379, 6380),   // Redis (6379 default, 6380 for testing)
                (8000, 8100),   // Django, Python HTTP servers
                (8080, 8090),   // Tomcat, alternative HTTP
                (9000, 9010),   // Various dev tools
                (27017, 27017), // MongoDB
            ],
            show_project_names: true,
        }
    }
}

impl Default for IntegrationsConfig {
    fn default() -> Self {
        Self {
            brew_enabled: true,
            docker_enabled: true,
        }
    }
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

pub fn get_config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".portkiller.json")
}

pub fn load_or_create_config() -> Result<Config> {
    let path = get_config_path();

    if path.exists() {
        // Validate file permissions (should be 0600 for security)
        ensure_secure_permissions(&path)?;
        let content = fs::read_to_string(&path).context("failed to read config file")?;
        let config: Config =
            serde_json::from_str(&content).context("failed to parse config file")?;
        validate_config(&config)?;
        Ok(config)
    } else {
        let config = Config::default();
        save_config(&config)?;
        Ok(config)
    }
}

pub fn save_config(config: &Config) -> Result<()> {
    let path = get_config_path();
    let content = serde_json::to_string_pretty(config).context("failed to serialize config")?;
    fs::write(&path, &content).context("failed to write config file")?;
    // Set secure permissions (owner read/write only)
    fs::set_permissions(&path, Permissions::from_mode(0o600))
        .context("failed to set config file permissions")?;
    Ok(())
}

fn ensure_secure_permissions(path: &PathBuf) -> Result<()> {
    let metadata = fs::metadata(path).context("failed to read config file metadata")?;
    let mode = metadata.permissions().mode();
    // Check if group or others have any permissions (should be 0600)
    if mode & 0o077 != 0 {
        log::warn!(
            "Config file has insecure permissions ({:o}), fixing to 0600",
            mode & 0o777
        );
        fs::set_permissions(path, Permissions::from_mode(0o600))
            .context("failed to fix config file permissions")?;
    }
    Ok(())
}

/// Load and validate config from disk. Returns error if file doesn't exist or is invalid.
/// Used by hot-reload (file watcher and manual reload).
pub fn load_and_validate_config() -> Result<Config> {
    let path = get_config_path();
    if !path.exists() {
        anyhow::bail!("config file does not exist");
    }
    ensure_secure_permissions(&path)?;
    let content = fs::read_to_string(&path).context("failed to read config file")?;
    let config: Config = serde_json::from_str(&content).context("failed to parse config file")?;
    validate_config(&config)?;
    Ok(config)
}

fn validate_config(config: &Config) -> Result<()> {
    // Validate poll interval (1-300 seconds)
    if config.monitoring.poll_interval_secs == 0 || config.monitoring.poll_interval_secs > 300 {
        anyhow::bail!(
            "poll_interval_secs must be between 1 and 300, got {}",
            config.monitoring.poll_interval_secs
        );
    }

    if config.monitoring.port_ranges.is_empty() {
        anyhow::bail!("port_ranges must contain at least one range");
    }

    // Validate port ranges (u16 already enforces 0-65535)
    for (start, end) in &config.monitoring.port_ranges {
        if start > end {
            anyhow::bail!("invalid port range: start ({}) > end ({})", start, end);
        }
    }

    let mut sorted_ranges = config.monitoring.port_ranges.clone();
    sorted_ranges.sort_unstable_by_key(|(start, end)| (*start, *end));

    for ranges in sorted_ranges.windows(2) {
        let (prev_start, prev_end) = ranges[0];
        let (next_start, next_end) = ranges[1];
        if next_start <= prev_end {
            anyhow::bail!(
                "overlapping port ranges: ({}, {}) overlaps with ({}, {})",
                prev_start,
                prev_end,
                next_start,
                next_end
            );
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with_ranges(ranges: Vec<(u16, u16)>) -> Config {
        let mut config = Config::default();
        config.monitoring.port_ranges = ranges;
        config
    }

    #[test]
    fn validate_config_rejects_empty_port_ranges() {
        let config = config_with_ranges(vec![]);
        let err = validate_config(&config).unwrap_err().to_string();
        assert!(err.contains("port_ranges must contain at least one range"));
    }

    #[test]
    fn validate_config_rejects_overlapping_ranges() {
        let config = config_with_ranges(vec![(3000, 3010), (3005, 3015)]);
        let err = validate_config(&config).unwrap_err().to_string();
        assert!(err.contains("overlapping port ranges"));
    }

    #[test]
    fn validate_config_rejects_overlapping_ranges_when_unsorted() {
        let config = config_with_ranges(vec![(4000, 4010), (3005, 3015), (3000, 3010)]);
        let err = validate_config(&config).unwrap_err().to_string();
        assert!(err.contains("overlapping port ranges"));
    }

    #[test]
    fn validate_config_allows_non_overlapping_ranges() {
        let config = config_with_ranges(vec![(3000, 3010), (3011, 3020), (8080, 8080)]);
        assert!(validate_config(&config).is_ok());
    }
}
