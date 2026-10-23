// Copyright (c) 2026 Softside Tech Pty Ltd. All rights reserved.
// SPDX-License-Identifier: AGPL-3.0-or-later

pub mod handlers;
pub mod wizard;

use std::path::Path;

use crate::config::models::AppConfig;
use crate::config::persistence::save_config_split;

/// Mint the one-shot setup token at start, when the wizard is open and holds
/// none: 256 bits from the OS RNG, hex-encoded, kept in `config` and saved
/// (encrypted) to `secrets.json`. Returns the token minted, for the start-up
/// banner, or `None` when there was nothing to mint.
///
/// A save that fails is logged, not fatal. The token gates `/setup` from
/// memory until the process exits either way; what the failure costs is
/// `--print-setup-token`, which reads the file, and a fresh token at the next
/// start. It used to stop the edge before it reached its manager or started a
/// flow, so on the full disk that makes such a save fail, a node that would
/// otherwise have run did not.
pub fn mint_setup_token(config_path: &Path, secrets_path: &Path, config: &mut AppConfig) -> Option<String> {
    if !config.setup_enabled || config.setup_token.is_some() {
        return None;
    }
    use rand::RngExt;
    let mut bytes = [0u8; 32];
    rand::rng().fill(&mut bytes);
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    config.setup_token = Some(token.clone());
    if let Err(e) = save_config_split(config_path, secrets_path, config) {
        tracing::warn!(
            "Could not save the setup token this start minted: {e}. It gates /setup until the \
             process exits; if it did not reach {}, --print-setup-token cannot show it and the \
             next start mints another",
            secrets_path.display()
        );
    }
    Some(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A token that cannot be saved still gates the wizard, and the start
    /// goes on: it used to end it.
    #[test]
    fn a_token_that_cannot_be_saved_is_minted_all_the_same() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.json");
        let secrets_path = dir.path().join("secrets.json");
        // Each file's temporary sibling is a directory, so neither write
        // can start: the disk is full.
        for path in [&config_path, &secrets_path] {
            std::fs::create_dir(path.with_extension("json.tmp")).unwrap();
        }
        let mut config = AppConfig::default();
        assert!(config.setup_enabled);

        let minted = mint_setup_token(&config_path, &secrets_path, &mut config);
        let token = minted.expect("a token is minted whether or not it can be saved");
        assert_eq!(token.len(), 64);
        assert_eq!(config.setup_token.as_deref(), Some(token.as_str()));
        assert!(!secrets_path.exists());

        // The wizard holding one, or closed, has nothing to mint.
        assert_eq!(mint_setup_token(&config_path, &secrets_path, &mut config), None);
        let mut closed = AppConfig { setup_enabled: false, ..Default::default() };
        assert_eq!(mint_setup_token(&config_path, &secrets_path, &mut closed), None);
        assert!(closed.setup_token.is_none());
    }
}
