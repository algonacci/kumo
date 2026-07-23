use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use dialoguer::{Confirm, Password, theme::ColorfulTheme};
use teloxide::{payloads::GetUpdatesSetters, prelude::*, types::UpdateKind};
use uuid::Uuid;

use crate::config::{Config, TelegramConfig};

const BOTFATHER_URL: &str = "https://t.me/BotFather";
const PAIRING_TIMEOUT: Duration = Duration::from_secs(180);

pub async fn run() -> Result<()> {
    let theme = ColorfulTheme::default();

    println!("Kumo onboarding");
    println!("===============");
    println!();
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
    println!("Now Kumo will securely pair your Telegram account.");

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

    let config = Config {
        telegram: TelegramConfig {
            bot_token: token,
            bot_username,
            owner_user_id: owner_user_id.0,
        },
    };
    let path = config.save()?;

    println!();
    println!("Telegram connected successfully.");
    println!("Configuration saved to {}", path.display());
    println!("Setup complete.");
    Ok(())
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
