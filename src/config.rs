use std::{collections::BTreeMap, path::PathBuf};

use anyhow::{Context, Result};
use directories::BaseDirs;
use serde::{Deserialize, Serialize};

const CONFIG_FILE: &str = "kumo.toml";

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Config {
    pub telegram: TelegramConfig,
    /// The single-provider form, kept for installs that never defined a named one. When
    /// `providers` is non-empty this is ignored, the same way Kamui's `[profiles.*]` win over its
    /// flat provider settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<ProviderConfig>,
    /// Named provider configurations, each a complete `[provider]` block of its own, so a second
    /// provider can be set up without discarding the first.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub providers: BTreeMap<String, ProviderConfig>,
    /// Which entry of `providers` is in use. Only meaningful when more than one exists — a single
    /// named provider is selected by being the only one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_provider: Option<String>,
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
    /// A hand-set window, and the fallback for any model the provider does not report one for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// Context windows as the provider reported them, keyed by model id. Kept per model rather
    /// than as one number so switching models with `/model` does not silently keep the previous
    /// model's compaction budget.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub context_windows: BTreeMap<String, u64>,
}

impl ProviderConfig {
    /// The window compaction should budget against: what the provider reported for the model in
    /// use, or the hand-set `context_window` when it reported nothing.
    pub fn active_context_window(&self) -> Option<u64> {
        self.context_windows
            .get(&self.active_model)
            .copied()
            .or(self.context_window)
    }

    /// Replaces the cached listing with what the provider currently advertises. The active model
    /// is left selected either way — a model that has disappeared upstream is reported rather than
    /// silently swapped, since picking a replacement is the owner's call.
    pub fn apply_model_listing(&mut self, listing: Vec<crate::provider::ModelInfo>) -> bool {
        self.context_windows = listing
            .iter()
            .filter_map(|model| Some((model.id.clone(), model.context_window?)))
            .collect();
        self.models = listing.into_iter().map(|model| model.id).collect();
        self.models.contains(&self.active_model)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolsConfig {
    pub workspace: PathBuf,
    /// Rewrite supported shell commands through RTK before execution to reduce model-facing
    /// output. Disabled by default for existing configurations.
    #[serde(default)]
    pub rtk: bool,
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

    /// The name of the provider in use, when there are named ones to choose between.
    pub fn active_provider_name(&self) -> Option<String> {
        if self.providers.is_empty() {
            return None;
        }
        match &self.active_provider {
            Some(name) if self.providers.contains_key(name) => Some(name.clone()),
            // A config with exactly one named provider does not need to say which is active, and a
            // stale `active_provider` (renamed or removed by hand) resolves the same way rather
            // than leaving the gateway with no provider at all.
            _ => self.providers.keys().next().cloned(),
        }
    }

    pub fn provider(&self) -> Result<&ProviderConfig> {
        if let Some(name) = self.active_provider_name() {
            return self
                .providers
                .get(&name)
                .context("the active provider disappeared from the configuration");
        }
        self.provider
            .as_ref()
            .context("model provider is not configured; run `kumo onboard`")
    }

    /// The same resolution as `provider`, for the commands that edit it (`/model`, `/context`,
    /// `/models refresh`) — they must write to whichever entry is actually in use.
    pub fn provider_mut(&mut self) -> Option<&mut ProviderConfig> {
        match self.active_provider_name() {
            Some(name) => self.providers.get_mut(&name),
            None => self.provider.as_mut(),
        }
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
                context_windows: BTreeMap::from([("model-a".into(), 200_000)]),
            }),
            providers: Default::default(),
            active_provider: None,
            tools: Some(ToolsConfig {
                workspace: PathBuf::from("/tmp/workspace"),
                rtk: true,
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
        let tools = decoded.tools.unwrap();
        assert_eq!(tools.workspace, PathBuf::from("/tmp/workspace"));
        assert!(tools.rtk);
        assert!(decoded.mcp["files"].trusted);
        assert_eq!(decoded.timezone.as_deref(), Some("Asia/Jakarta"));
    }

    fn provider(active: &str, windows: BTreeMap<String, u64>) -> ProviderConfig {
        ProviderConfig {
            base_url: "https://api.example.com/v1".into(),
            api_key: "secret".into(),
            active_model: active.into(),
            models: windows.keys().cloned().collect(),
            context_window: None,
            context_windows: windows,
        }
    }

    #[test]
    fn the_active_window_follows_the_active_model() {
        let config = provider(
            "small",
            BTreeMap::from([("small".into(), 8_000), ("large".into(), 200_000)]),
        );

        assert_eq!(config.active_context_window(), Some(8_000));

        let switched = provider(
            "large",
            BTreeMap::from([("small".into(), 8_000), ("large".into(), 200_000)]),
        );

        assert_eq!(switched.active_context_window(), Some(200_000));
    }

    #[test]
    fn a_hand_set_window_covers_a_model_the_provider_says_nothing_about() {
        let mut config = provider("unreported", BTreeMap::new());
        config.context_window = Some(32_000);

        assert_eq!(config.active_context_window(), Some(32_000));
    }

    #[test]
    fn a_refreshed_listing_replaces_the_cached_models_and_windows() {
        let mut config = provider("model-a", BTreeMap::from([("model-a".into(), 8_000)]));

        let still_available = config.apply_model_listing(vec![
            crate::provider::ModelInfo {
                id: "model-a".into(),
                context_window: Some(128_000),
            },
            crate::provider::ModelInfo {
                id: "model-c".into(),
                context_window: None,
            },
        ]);

        assert!(still_available);
        assert_eq!(config.models, vec!["model-a", "model-c"]);
        assert_eq!(config.active_context_window(), Some(128_000));
        // A model the provider reports no window for must not inherit a stale one.
        assert!(!config.context_windows.contains_key("model-c"));
    }

    #[test]
    fn a_refresh_reports_an_active_model_the_provider_dropped() {
        let mut config = provider("retired", BTreeMap::from([("retired".into(), 8_000)]));

        let still_available = config.apply_model_listing(vec![crate::provider::ModelInfo {
            id: "model-a".into(),
            context_window: Some(128_000),
        }]);

        assert!(!still_available);
        // The selection is left alone: replacing it is the owner's call, not a silent swap.
        assert_eq!(config.active_model, "retired");
        assert_eq!(config.active_context_window(), None);
    }

    /// Top-level keys have to precede the first table header, or TOML reads them as belonging to
    /// that table instead.
    fn config_with_providers(toml: &str) -> Config {
        toml::from_str(&format!(
            "{toml}\n[telegram]\nbot_token = \"123:secret\"\nbot_username = \"bot\"\n\
             owner_user_id = 42\n"
        ))
        .unwrap()
    }

    #[test]
    fn a_named_provider_wins_over_the_flat_one() {
        let config = config_with_providers(
            "\n[provider]\nbase_url = \"https://old.example.com/v1\"\napi_key = \"k\"\n\
             active_model = \"old\"\nmodels = [\"old\"]\n\
             \n[providers.new]\nbase_url = \"https://new.example.com/v1\"\napi_key = \"k\"\n\
             active_model = \"new\"\nmodels = [\"new\"]",
        );

        assert_eq!(config.provider().unwrap().active_model, "new");
        assert_eq!(config.active_provider_name().as_deref(), Some("new"));
    }

    #[test]
    fn active_provider_selects_between_named_ones() {
        let config = config_with_providers(
            "active_provider = \"b\"\n\
             \n[providers.a]\nbase_url = \"https://a.example.com/v1\"\napi_key = \"k\"\n\
             active_model = \"model-a\"\nmodels = [\"model-a\"]\n\
             \n[providers.b]\nbase_url = \"https://b.example.com/v1\"\napi_key = \"k\"\n\
             active_model = \"model-b\"\nmodels = [\"model-b\"]",
        );

        assert_eq!(config.provider().unwrap().active_model, "model-b");
    }

    #[test]
    fn a_stale_active_provider_still_resolves_to_something() {
        let mut config = config_with_providers(
            "active_provider = \"renamed-away\"\n\
             \n[providers.a]\nbase_url = \"https://a.example.com/v1\"\napi_key = \"k\"\n\
             active_model = \"model-a\"\nmodels = [\"model-a\"]",
        );

        // A hand-edited name that no longer exists must not leave the gateway provider-less.
        assert_eq!(config.provider().unwrap().active_model, "model-a");
        assert!(config.provider_mut().is_some());
    }

    #[test]
    fn a_config_with_no_providers_at_all_reports_it() {
        let config = config_with_providers("");

        assert!(config.provider().is_err());
        assert!(config.active_provider_name().is_none());
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
    fn legacy_tools_config_defaults_rtk_off() {
        let decoded: Config = toml::from_str(
            "[telegram]\nbot_token = \"t\"\nbot_username = \"b\"\nowner_user_id = 1\n\
             [tools]\nworkspace = \"/tmp\"",
        )
        .unwrap();
        assert!(!decoded.tools.unwrap().rtk);
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
