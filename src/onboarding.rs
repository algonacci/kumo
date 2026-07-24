use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use dialoguer::{Confirm, FuzzySelect, Input, Password, theme::ColorfulTheme};
use teloxide::{payloads::GetUpdatesSetters, prelude::*, types::UpdateKind};
use uuid::Uuid;

use crate::{
    config::{Config, ProviderConfig, TelegramConfig, ToolsConfig},
    provider,
};

const BOTFATHER_URL: &str = "https://t.me/BotFather";
const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const PAIRING_TIMEOUT: Duration = Duration::from_secs(180);

pub async fn run(existing: Option<Config>, reconfigure_provider: bool) -> Result<Config> {
    println!("Kumo onboarding");
    println!("===============");
    println!();

    let (telegram, existing_provider) = match existing {
        Some(config) => {
            println!(
                "Telegram is already connected as @{}.",
                config.telegram.bot_username
            );
            (config.telegram, config.provider)
        }
        None => {
            let telegram = setup_telegram().await?;
            Config {
                telegram: telegram.clone(),
                provider: None,
                tools: None,
            }
            .save()?;
            (telegram, None)
        }
    };
    let provider = match existing_provider {
        Some(provider) if !reconfigure_provider => provider,
        _ => setup_provider().await?,
    };
    let tools = setup_tools()?;
    let config = Config {
        telegram,
        provider: Some(provider),
        tools: Some(tools),
    };
    let path = config.save()?;

    println!();
    println!("Setup complete.");
    println!("Configuration saved to {}", path.display());
    Ok(config)
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
        let bot = Bot::new(token.trim().to_owned());

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
            Ok(models) => {
                let selected = FuzzySelect::with_theme(&theme)
                    .with_prompt("Choose the default model (type to search)")
                    .items(&models)
                    .default(0)
                    .interact()?;
                let active_model = models[selected].clone();
                println!("Connected. Found {} models.", models.len());
                return Ok(ProviderConfig {
                    base_url,
                    api_key,
                    active_model,
                    models,
                });
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
    let mut offset = 0;

    while Instant::now() < deadline {
        let updates = bot
            .get_updates()
            .offset(offset)
            .timeout(20)
            .await
            .context("failed while waiting for Telegram pairing")?;

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

    bail!("pairing timed out; run `kumo onboard` to try again")
}
