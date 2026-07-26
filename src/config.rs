use std::{collections::BTreeMap, path::PathBuf};

use anyhow::{Context, Result};
use directories::BaseDirs;
use serde::{Deserialize, Serialize};

const CONFIG_FILE: &str = "kumo.toml";

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Config {
    pub telegram: TelegramConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<ProviderConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<ToolsConfig>,
    /// IANA timezone name (e.g. "Asia/Jakarta"), used to interpret relative times in scheduled
    /// tasks. Missing on installs that predate scheduling; `Config::timezone` falls back to UTC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub mcp: BTreeMap<String, McpServerConfig>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TelegramConfig {
    pub bot_token: String,
    pub bot_username: String,
    pub owner_user_id: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProviderConfig {
    pub base_url: String,
    pub api_key: String,
    pub active_model: String,
    pub models: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolsConfig {
    pub workspace: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct McpServerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Skip the Telegram approval prompt for every tool this server advertises.
    #[serde(default)]
    pub trusted: bool,
    /// Skip the approval prompt for these tools only, named as the server advertises them (no
    /// server prefix). Lets a server mix read-only tools with tools that send mail or drop rows
    /// without having to trust all of them at once.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trusted_tools: Vec<String>,
}

impl McpServerConfig {
    pub fn trusts(&self, tool: &str) -> bool {
        self.trusted || self.trusted_tools.iter().any(|name| name == tool)
    }
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

    pub fn provider(&self) -> Result<&ProviderConfig> {
        self.provider
            .as_ref()
            .context("model provider is not configured; run `kumo onboard`")
    }

    /// The configured IANA timezone, or UTC on installs from before scheduling existed. The
    /// timezone name is validated at onboarding time, so a stored value always parses here.
    pub fn timezone(&self) -> chrono_tz::Tz {
        self.timezone
            .as_deref()
            .and_then(|name| name.parse().ok())
            .unwrap_or(chrono_tz::UTC)
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
            provider: Some(ProviderConfig {
                base_url: "https://api.example.com/v1".into(),
                api_key: "secret".into(),
                active_model: "model-a".into(),
                models: vec!["model-a".into(), "model-b".into()],
                context_window: Some(128_000),
            }),
            tools: Some(ToolsConfig {
                workspace: PathBuf::from("/tmp/workspace"),
            }),
            timezone: Some("Asia/Jakarta".into()),
            mcp: BTreeMap::from([(
                "files".into(),
                McpServerConfig {
                    command: "server".into(),
                    args: vec!["--stdio".into()],
                    trusted: true,
                    trusted_tools: Vec::new(),
                },
            )]),
        };

        let encoded = toml::to_string(&config).unwrap();
        let decoded: Config = toml::from_str(&encoded).unwrap();

        assert_eq!(decoded.telegram.bot_token, "123:secret");
        assert_eq!(decoded.telegram.bot_username, "kumo_test_bot");
        assert_eq!(decoded.telegram.owner_user_id, 42);
        assert_eq!(decoded.provider.unwrap().active_model, "model-a");
        assert_eq!(
            decoded.tools.unwrap().workspace,
            PathBuf::from("/tmp/workspace")
        );
        assert!(decoded.mcp["files"].trusted);
        assert_eq!(decoded.timezone.as_deref(), Some("Asia/Jakarta"));
    }

    #[test]
    fn trusts_named_tools_without_trusting_the_whole_server() {
        let decoded: Config = toml::from_str(
            "[telegram]\nbot_token = \"123:secret\"\nbot_username = \"bot\"\nowner_user_id = 42\n\
             \n[mcp.tools]\ncommand = \"server\"\ntrusted_tools = [\"get_price\"]",
        )
        .unwrap();

        let server = &decoded.mcp["tools"];
        assert!(!server.trusted);
        assert!(server.trusts("get_price"));
        assert!(!server.trusts("send_email"));
    }

    #[test]
    fn a_trusted_server_trusts_every_tool() {
        let server = McpServerConfig {
            command: "server".into(),
            args: Vec::new(),
            trusted: true,
            trusted_tools: Vec::new(),
        };

        assert!(server.trusts("send_email"));
    }

    #[test]
    fn loads_legacy_telegram_only_config() {
        let decoded: Config = toml::from_str(
            "[telegram]\nbot_token = \"123:secret\"\nbot_username = \"bot\"\nowner_user_id = 42",
        )
        .unwrap();

        assert!(decoded.provider.is_none());
        assert!(decoded.tools.is_none());
        assert!(decoded.mcp.is_empty());
        assert!(decoded.timezone.is_none());
        assert_eq!(decoded.timezone(), chrono_tz::UTC);
    }

    #[test]
    fn timezone_resolves_a_configured_iana_name() {
        let decoded: Config = toml::from_str(
            "timezone = \"Asia/Jakarta\"\n[telegram]\nbot_token = \"t\"\nbot_username = \"b\"\nowner_user_id = 1",
        )
        .unwrap();

        assert_eq!(decoded.timezone(), chrono_tz::Asia::Jakarta);
    }
}
