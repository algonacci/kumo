use std::path::PathBuf;

use anyhow::{Context, Result};
use directories::BaseDirs;
use serde::{Deserialize, Serialize};

const CONFIG_FILE: &str = "kumo.toml";

#[derive(Debug, Deserialize, Serialize)]
pub struct Config {
    pub telegram: TelegramConfig,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct TelegramConfig {
    pub bot_token: String,
    pub bot_username: String,
    pub owner_user_id: u64,
}

impl Config {
    pub fn exists() -> Result<bool> {
        Ok(path()?.is_file())
    }

    pub fn load() -> Result<Self> {
        let path = path()?;
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        toml::from_str(&content).with_context(|| format!("failed to parse {}", path.display()))
    }

    pub fn save(&self) -> Result<PathBuf> {
        let path = path()?;
        let parent = path
            .parent()
            .context("config path has no parent directory")?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;

        let content = toml::to_string_pretty(self).context("failed to serialize configuration")?;
        std::fs::write(&path, content)
            .with_context(|| format!("failed to write {}", path.display()))?;
        restrict_permissions(&path)?;
        Ok(path)
    }
}

pub fn path() -> Result<PathBuf> {
    BaseDirs::new()
        .map(|dirs| dirs.config_dir().join("kumo").join(CONFIG_FILE))
        .context("could not determine the operating system config directory")
}

#[cfg(unix)]
fn restrict_permissions(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to secure {}", path.display()))
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &std::path::Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_round_trips_through_toml() {
        let config = Config {
            telegram: TelegramConfig {
                bot_token: "123:secret".into(),
                bot_username: "kumo_test_bot".into(),
                owner_user_id: 42,
            },
        };

        let encoded = toml::to_string(&config).unwrap();
        let decoded: Config = toml::from_str(&encoded).unwrap();

        assert_eq!(decoded.telegram.bot_token, "123:secret");
        assert_eq!(decoded.telegram.bot_username, "kumo_test_bot");
        assert_eq!(decoded.telegram.owner_user_id, 42);
    }
}
