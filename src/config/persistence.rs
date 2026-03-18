use std::path::Path;

use anyhow::{Context, Result};

use super::models::AppConfig;

/// Load configuration from a JSON file.
/// Returns default config if the file doesn't exist.
pub fn load_config(path: &Path) -> Result<AppConfig> {
    if !path.exists() {
        tracing::info!("Config file not found at {}, starting with empty config", path.display());
        return Ok(AppConfig::default());
    }

    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read config file: {}", path.display()))?;

    let config: AppConfig = serde_json::from_str(&contents)
        .with_context(|| format!("Failed to parse config file: {}", path.display()))?;

    tracing::info!(
        "Loaded config with {} flow(s) from {}",
        config.flows.len(),
        path.display()
    );

    Ok(config)
}

/// Save configuration to a JSON file.
pub fn save_config(path: &Path, config: &AppConfig) -> Result<()> {
    let contents = serde_json::to_string_pretty(config)
        .context("Failed to serialize config")?;

    // Write to a temp file first, then rename for atomicity
    let tmp_path = path.with_extension("json.tmp");
    std::fs::write(&tmp_path, &contents)
        .with_context(|| format!("Failed to write temp config file: {}", tmp_path.display()))?;

    std::fs::rename(&tmp_path, path)
        .with_context(|| format!("Failed to rename temp config to: {}", path.display()))?;

    tracing::debug!("Config saved to {}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::models::*;
    use tempfile::NamedTempFile;

    #[test]
    fn test_load_nonexistent_returns_default() {
        let path = Path::new("/tmp/nonexistent_bilbycast_edge_config_test.json");
        let config = load_config(path).unwrap();
        assert!(config.flows.is_empty());
        assert_eq!(config.version, 1);
    }

    #[test]
    fn test_save_and_load_roundtrip() {
        let tmp = NamedTempFile::new().unwrap();
        let config = AppConfig {
            version: 1,
            server: ServerConfig::default(),
            monitor: None,
            flows: vec![FlowConfig {
                id: "test".to_string(),
                name: "Test".to_string(),
                enabled: true,
                input: InputConfig::Rtp(RtpInputConfig {
                    bind_addr: "0.0.0.0:5000".to_string(),
                    interface_addr: None,
                    fec_decode: None,
                    allowed_sources: None,
                    allowed_payload_types: None,
                    max_bitrate_mbps: None,
                    tr07_mode: None,
                }),
                outputs: vec![],
            }],
        };

        save_config(tmp.path(), &config).unwrap();
        let loaded = load_config(tmp.path()).unwrap();
        assert_eq!(loaded.flows.len(), 1);
        assert_eq!(loaded.flows[0].id, "test");
    }
}
