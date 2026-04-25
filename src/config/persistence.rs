// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

use std::path::Path;

use anyhow::{Context, Result};

use super::crypto;
use super::models::AppConfig;
use super::secrets::{SecretsConfig, has_secrets};

/// Load configuration from a JSON file.
/// Returns default config if the file doesn't exist.
pub fn load_config(path: &Path) -> Result<AppConfig> {
    if !path.exists() {
        tracing::info!("Config file not found at {}, starting with empty config", path.display());
        return Ok(AppConfig::default());
    }

    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read config file: {}", path.display()))?;

    let mut config: AppConfig = serde_json::from_str(&contents)
        .with_context(|| format!("Failed to parse config file: {}", path.display()))?;

    // Migrate legacy `tunnel.relay_addr` (string) → `tunnel.relay_addrs` (vec).
    for tunnel in &mut config.tunnels {
        tunnel.normalize_relay_addrs();
    }

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

/// Load configuration from split files (`config.json` + `secrets.json`).
///
/// - If `secrets.json` doesn't exist but `config.json` contains secrets,
///   performs automatic migration: extracts secrets to `secrets.json` and
///   rewrites `config.json` without secrets.
/// - If neither file exists, returns defaults (first-time setup).
/// - If both exist, loads and merges normally.
///
/// Secrets are encrypted at rest using a machine-specific key derived from
/// `/etc/machine-id` (Linux) or a generated `.secrets_key` file (fallback).
/// Existing unencrypted secrets.json files are auto-migrated to encrypted form.
pub fn load_config_split(config_path: &Path, secrets_path: &Path) -> Result<AppConfig> {
    let mut config = load_config(config_path)?;

    // Resolve the machine seed for secrets encryption/decryption
    let secrets_dir = secrets_path.parent().unwrap_or(Path::new("."));
    let machine_seed = crypto::get_machine_seed(secrets_dir)
        .context("Failed to obtain machine seed for secrets encryption")?;

    if secrets_path.exists() {
        // Normal path: load secrets and merge into config
        let secrets = load_secrets(secrets_path, &machine_seed)?;

        // Check if secrets.json has legacy flow secrets that need migration
        let has_legacy_flows = !secrets.flows.is_empty();

        secrets.merge_into(&mut config);

        if has_legacy_flows {
            // Re-save both files: flow params now go into config.json,
            // and secrets.json will no longer contain flow entries.
            tracing::info!(
                "Migrating flow secrets from secrets.json back to config.json"
            );
            save_config_split(config_path, secrets_path, &config)?;
            tracing::info!("Flow secrets migration complete");
        }
    } else if config_path.exists() && has_secrets(&config) {
        // Migration: config.json has secrets but no secrets.json yet
        tracing::info!(
            "Migrating secrets from {} to {}",
            config_path.display(),
            secrets_path.display()
        );
        let secrets = SecretsConfig::extract_from(&config);
        save_secrets(secrets_path, &secrets, &machine_seed)?;

        // Rewrite config.json without secrets
        let mut stripped = config.clone();
        stripped.strip_secrets();
        save_config(config_path, &stripped)?;

        tracing::info!("Migration complete: secrets moved to {} (encrypted)", secrets_path.display());
    }
    // else: first-time setup with no files — config is defaults, no secrets to merge

    Ok(config)
}

/// Save configuration to split files (`config.json` + `secrets.json`).
///
/// Extracts secrets from the in-memory `AppConfig`, writes the stripped
/// operational config to `config.json` and encrypted secrets to `secrets.json`.
pub fn save_config_split(
    config_path: &Path,
    secrets_path: &Path,
    config: &AppConfig,
) -> Result<()> {
    let secrets = SecretsConfig::extract_from(config);

    // Write stripped config to config.json
    let mut stripped = config.clone();
    stripped.strip_secrets();
    save_config(config_path, &stripped)?;

    // Write encrypted secrets to secrets.json (only if there are any)
    if !secrets.is_empty() {
        let secrets_dir = secrets_path.parent().unwrap_or(Path::new("."));
        let machine_seed = crypto::get_machine_seed(secrets_dir)
            .context("Failed to obtain machine seed for secrets encryption")?;
        save_secrets(secrets_path, &secrets, &machine_seed)?;
    }

    Ok(())
}

/// Async wrapper around [`save_config_split`] that offloads the blocking
/// file I/O to a Tokio blocking thread pool via `spawn_blocking`.
///
/// Use this from async contexts (API handlers, manager client) to avoid
/// stalling the Tokio runtime during disk writes.
pub async fn save_config_split_async(
    config_path: std::path::PathBuf,
    secrets_path: std::path::PathBuf,
    config: AppConfig,
) -> Result<()> {
    tokio::task::spawn_blocking(move || {
        save_config_split(&config_path, &secrets_path, &config)
    })
    .await
    .context("Config save task panicked")?
}

/// Load secrets from a JSON file, decrypting if encrypted.
///
/// Supports both encrypted (`v1:...`) and legacy unencrypted formats.
/// Legacy files are automatically re-encrypted on the next save.
fn load_secrets(path: &Path, machine_seed: &str) -> Result<SecretsConfig> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read secrets file: {}", path.display()))?;

    let json_str = if crypto::is_encrypted(&raw) {
        crypto::decrypt_secrets(&raw, machine_seed)
            .with_context(|| format!("Failed to decrypt secrets file: {}", path.display()))?
    } else {
        tracing::info!(
            "Found unencrypted secrets.json — will be encrypted on next save"
        );
        raw
    };

    let secrets: SecretsConfig = serde_json::from_str(&json_str)
        .with_context(|| format!("Failed to parse secrets file: {}", path.display()))?;

    tracing::debug!("Loaded secrets from {} (encrypted at rest)", path.display());
    Ok(secrets)
}

/// Save secrets to an encrypted JSON file with restrictive permissions.
///
/// The file is encrypted using AES-256-GCM with a machine-specific key.
pub fn save_secrets(path: &Path, secrets: &SecretsConfig, machine_seed: &str) -> Result<()> {
    let json =
        serde_json::to_string_pretty(secrets).context("Failed to serialize secrets")?;

    let encrypted =
        crypto::encrypt_secrets(&json, machine_seed).context("Failed to encrypt secrets")?;

    let tmp_path = path.with_extension("json.tmp");
    std::fs::write(&tmp_path, &encrypted)
        .with_context(|| format!("Failed to write temp secrets file: {}", tmp_path.display()))?;

    std::fs::rename(&tmp_path, path)
        .with_context(|| format!("Failed to rename temp secrets to: {}", path.display()))?;

    // Set restrictive file permissions on Unix (owner read/write only)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }

    tracing::debug!("Secrets saved to {} (encrypted)", path.display());
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
        assert_eq!(config.version, 2);
    }

    #[test]
    fn test_save_and_load_roundtrip() {
        let tmp = NamedTempFile::new().unwrap();
        let config = AppConfig {
            version: 1,
            node_id: None,
            device_name: None,
            setup_enabled: true,
            server: ServerConfig::default(),
            monitor: None,
            manager: None,
            resource_limits: None,
            inputs: vec![InputDefinition {
                active: true,
                group: None,
                id: "in-1".to_string(),
                name: "Input 1".to_string(),
                config: InputConfig::Rtp(RtpInputConfig {
                    bind_addr: "0.0.0.0:5000".to_string(),
                    interface_addr: None,
                    fec_decode: None,
                    allowed_sources: None,
                    allowed_payload_types: None,
                    max_bitrate_mbps: None,
                    tr07_mode: None,
                    redundancy: None,
                    audio_encode: None,
                    transcode: None,
                    video_encode: None,
                }),
            }],
            outputs: Vec::new(),
            tunnels: Vec::new(),
            flow_groups: Vec::new(),
            flows: vec![FlowConfig {
                id: "test".to_string(),
                name: "Test".to_string(),
                enabled: true,
                media_analysis: true,
                thumbnail: true,
                thumbnail_program_number: None,
                bandwidth_limit: None,
                flow_group_id: None,
                clock_domain: None,
                input_ids: vec!["in-1".to_string()],
                output_ids: vec![],
                assembly: None,
                content_analysis: None,
            }],
        };

        save_config(tmp.path(), &config).unwrap();
        let loaded = load_config(tmp.path()).unwrap();
        assert_eq!(loaded.flows.len(), 1);
        assert_eq!(loaded.flows[0].id, "test");
    }
}
