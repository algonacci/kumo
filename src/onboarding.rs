use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use dialoguer::{Confirm, FuzzySelect, Input, Password, theme::ColorfulTheme};
use teloxide::{payloads::GetUpdatesSetters, prelude::*, types::UpdateKind};
use uuid::Uuid;

use crate::{
    config::{Config, ProviderConfig, TelegramConfig, ToolsConfig},
    provider,
};

use chrono_tz::TZ_VARIANTS;

const BOTFATHER_URL: &str = "https://t.me/BotFather";
const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const PAIRING_TIMEOUT: Duration = Duration::from_secs(300);
/// How long each `getUpdates` call waits for Telegram to hand us an update.
const PAIRING_POLL_TIMEOUT: Duration = Duration::from_secs(20);
/// The HTTP client has to outlive the long poll it carries, otherwise reqwest
/// aborts the request before Telegram ever answers. Teloxide's default is 17s,
/// which is shorter than `PAIRING_POLL_TIMEOUT`.
const TELEGRAM_HTTP_TIMEOUT: Duration = Duration::from_secs(60);
/// Pause before retrying after a failed poll, so a flaky link does not spin.
const PAIRING_RETRY_DELAY: Duration = Duration::from_secs(2);

fn telegram_bot(token: String) -> Result<Bot> {
    let client = teloxide::net::default_reqwest_settings()
        .timeout(TELEGRAM_HTTP_TIMEOUT)
        .build()
        .context("could not build the Telegram HTTP client")?;
    Ok(Bot::with_client(token, client))
}

pub async fn run(existing: Option<Config>, reconfigure_provider: bool) -> Result<Config> {
    println!("Kumo onboarding");
    println!("===============");
    println!();

    let mut config = match existing {
        Some(config) => {
            println!(
                "Telegram is already connected as @{}.",
                config.telegram.bot_username
            );
            config
        }
        None => {
            let telegram = setup_telegram().await?;
            let config = Config {
                telegram,
                provider: None,
                providers: Default::default(),
                active_provider: None,
                tools: None,
                timezone: None,
                mcp: Default::default(),
            };
            config.save()?;
            config
        }
    };

    if config.provider().is_err() || reconfigure_provider {
        let provider = setup_provider().await?;
        install_provider(&mut config, provider)?;
    }
    config.tools = Some(setup_tools()?);
    if config.timezone.is_none() {
        config.timezone = Some(setup_timezone()?);
    }
    let path = config.save()?;

    println!();
    println!("Setup complete.");
    println!("Configuration saved to {}", path.display());
    Ok(config)
}

/// Files a freshly configured provider, keeping any provider already set up instead of replacing
/// it. Re-running `kumo onboard` used to cost you the previous provider entirely — base URL, key,
/// and model list — which is a lot to lose just to try a second one.
///
/// A first provider stays in the simple `[provider]` form. Only a second one migrates the config
/// to named `[providers.*]` entries, so an install that never wants two never has to think about
/// names at all.
fn install_provider(config: &mut Config, provider: ProviderConfig) -> Result<()> {
    if config.provider.is_none() && config.providers.is_empty() {
        config.provider = Some(provider);
        return Ok(());
    }

    if let Some(previous) = config.provider.take() {
        let name = unique_provider_name(&suggest_provider_name(&previous.base_url), config);
        println!("Keeping the provider you had as \"{name}\".");
        config.providers.insert(name, previous);
    }

    let suggested = unique_provider_name(&suggest_provider_name(&provider.base_url), config);
    let name = Input::<String>::with_theme(&ColorfulTheme::default())
        .with_prompt("Name for this provider (switch with /provider in Telegram)")
        .default(suggested)
        .interact_text()?
        .trim()
        .to_owned();
    config.providers.insert(name.clone(), provider);
    config.active_provider = Some(name);
    Ok(())
}

/// A short name derived from the provider's host: `https://api.groq.com/openai/v1` becomes
/// `groq`. Only a starting point — onboarding offers it as an editable default.
fn suggest_provider_name(base_url: &str) -> String {
    let host = base_url
        .rsplit("//")
        .next()
        .unwrap_or_default()
        .split('/')
        .next()
        .unwrap_or_default()
        .split(':')
        .next()
        .unwrap_or_default();
    let label = host
        .split('.')
        .find(|label| !label.is_empty() && *label != "api" && *label != "www")
        .unwrap_or_default();
    if label.is_empty() {
        "provider".to_owned()
    } else {
        label.to_owned()
    }
}

fn unique_provider_name(preferred: &str, config: &Config) -> String {
    if !config.providers.contains_key(preferred) {
        return preferred.to_owned();
    }
    (2..)
        .map(|suffix| format!("{preferred}-{suffix}"))
        .find(|name| !config.providers.contains_key(name))
        .expect("an unused suffix always exists")
}

fn setup_timezone() -> Result<String> {
    let theme = ColorfulTheme::default();
    println!();
    println!("Choose your timezone, used to interpret times for scheduled tasks.");

    let names: Vec<&str> = TZ_VARIANTS.iter().map(|tz| tz.name()).collect();
    let default = names
        .iter()
        .position(|name| *name == "UTC")
        .unwrap_or_default();
    let selected = FuzzySelect::with_theme(&theme)
        .with_prompt("Timezone (type to search, e.g. Jakarta)")
        .items(&names)
        .default(default)
        .interact()?;
    Ok(names[selected].to_owned())
}

fn setup_tools() -> Result<ToolsConfig> {
    let theme = ColorfulTheme::default();
    let default = std::env::current_dir().context("could not determine the current directory")?;
    println!();
    println!("Choose the workspace Kumo may inspect with read-only tools.");

    loop {
        let value = Input::<String>::with_theme(&theme)
            .with_prompt("Workspace directory")
            .default(default.display().to_string())
            .interact_text()?;
        let path = std::path::PathBuf::from(value.trim());
        match path.canonicalize() {
            Ok(path) if path.is_dir() => return Ok(ToolsConfig { workspace: path }),
            _ => eprintln!("That workspace directory does not exist."),
        }
    }
}

async fn setup_telegram() -> Result<TelegramConfig> {
    let theme = ColorfulTheme::default();

    println!("Kumo needs a private Telegram bot. Setup takes about a minute.");
    println!();
    println!("1. Create a bot with @BotFather using /newbot.");
    println!("2. Copy the bot token BotFather gives you.");
    println!();
    println!("Opening BotFather: {BOTFATHER_URL}");
    let _ = webbrowser::open(BOTFATHER_URL);

    let (bot, bot_username, token) = loop {
        let token = Password::with_theme(&theme)
            .with_prompt("Create the bot, then paste its token here")
            .interact()?;
        let bot = telegram_bot(token.trim().to_owned())?;

        match bot.get_me().await {
            Ok(me) => break (bot, me.username().to_owned(), token.trim().to_owned()),
            Err(error) => {
                eprintln!("Could not verify that token: {error}");
                if !Confirm::with_theme(&theme)
                    .with_prompt("Try another token?")
                    .default(true)
                    .interact()?
                {
                    bail!("Telegram setup cancelled");
                }
            }
        }
    };

    println!();
    println!("Connected to @{bot_username}.");
    let nonce = Uuid::new_v4().simple().to_string();
    let payload = format!("kumo_{nonce}");
    let bot_link = format!("https://t.me/{bot_username}?start={payload}");
    println!("Opening your bot: {bot_link}");
    println!("Tap Start in Telegram. Kumo will detect your user ID automatically.");
    let _ = webbrowser::open(&bot_link);

    let owner_user_id = wait_for_owner(&bot, &payload).await?;
    bot.send_message(owner_user_id, "Kumo is connected to your account.")
        .await
        .context("paired successfully, but could not send confirmation")?;
    println!("Telegram connected successfully.");

    Ok(TelegramConfig {
        bot_token: token,
        bot_username,
        owner_user_id: owner_user_id.0,
    })
}

async fn setup_provider() -> Result<ProviderConfig> {
    let theme = ColorfulTheme::default();
    println!();
    println!("Connect an OpenAI-compatible model provider.");

    loop {
        let base_url = Input::<String>::with_theme(&theme)
            .with_prompt("Provider base URL")
            .default(DEFAULT_BASE_URL.to_owned())
            .interact_text()?
            .trim_end_matches('/')
            .to_owned();
        let api_key = Password::with_theme(&theme)
            .with_prompt("API key (leave empty for a local provider)")
            .allow_empty_password(true)
            .interact()?
            .trim()
            .to_owned();

        println!("Checking available models...");
        match provider::list_models(&base_url, &api_key).await {
            Ok(listing) => {
                let names: Vec<&str> = listing.iter().map(|model| model.id.as_str()).collect();
                let selected = FuzzySelect::with_theme(&theme)
                    .with_prompt("Choose the default model (type to search)")
                    .items(&names)
                    .default(0)
                    .interact()?;
                let active_model = listing[selected].id.clone();
                println!("Connected. Found {} models.", listing.len());

                let mut provider = ProviderConfig {
                    base_url,
                    api_key,
                    active_model,
                    models: Vec::new(),
                    context_window: None,
                    context_windows: Default::default(),
                };
                provider.apply_model_listing(listing);
                match provider.active_context_window() {
                    Some(window) => println!("Context window: {window} tokens."),
                    None => println!(
                        "This provider does not report a context window; Kumo will use its \
                         conservative default until one is set in kumo.toml."
                    ),
                }
                return Ok(provider);
            }
            Err(error) => {
                eprintln!("Could not load models: {error:#}");
                if !Confirm::with_theme(&theme)
                    .with_prompt("Try the provider setup again?")
                    .default(true)
                    .interact()?
                {
                    bail!("provider setup cancelled");
                }
            }
        }
    }
}

async fn wait_for_owner(bot: &Bot, payload: &str) -> Result<UserId> {
    let expected = format!("/start {payload}");
    let deadline = Instant::now() + PAIRING_TIMEOUT;
    let poll_timeout = PAIRING_POLL_TIMEOUT.as_secs() as u32;
    let mut offset = 0;
    let mut last_error = None;

    while Instant::now() < deadline {
        let updates = match bot.get_updates().offset(offset).timeout(poll_timeout).await {
            Ok(updates) => updates,
            // A slow or flaky connection should not end onboarding: keep polling
            // until the deadline, and only report the error if we never pair.
            Err(error) => {
                eprintln!("Still waiting for Telegram ({error}). Retrying...");
                last_error = Some(error);
                tokio::time::sleep(PAIRING_RETRY_DELAY).await;
                continue;
            }
        };

        for update in updates {
            offset = update.id.0.saturating_add(1) as i32;
            let UpdateKind::Message(message) = update.kind else {
                continue;
            };
            let Some(user) = message.from.as_ref() else {
                continue;
            };

            if message.chat.is_private() && message.text() == Some(expected.as_str()) {
                bot.get_updates().offset(offset).limit(1).await?;
                return Ok(user.id);
            }
        }
    }

    match last_error {
        Some(error) => {
            Err(anyhow::Error::new(error)
                .context("pairing timed out; run `kumo onboard` to try again"))
        }
        None => bail!("pairing timed out; run `kumo onboard` to try again"),
    }
}
