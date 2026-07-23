use std::{env, error::Error};

use teloxide::prelude::*;

const REPLY: &str = "Halo dari Kumo. Pesanmu sudah diterima.";

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    dotenvy::dotenv().ok();

    let token = env::var("KUMO_TELEGRAM_TOKEN")?;
    let allowed_user_id = env::var("KUMO_TELEGRAM_USER_ID")?.parse::<u64>()?;
    let bot = Bot::new(token);

    println!("Kumo is listening for Telegram messages. Press Ctrl+C to stop.");

    teloxide::repl(bot, move |bot: Bot, message: Message| async move {
        let Some(user) = message.from.as_ref() else {
            return respond(());
        };

        if user.id.0 != allowed_user_id || message.text().is_none() {
            return respond(());
        }

        println!("Received a message from Telegram user {}", user.id.0);
        bot.send_message(message.chat.id, REPLY).await?;

        respond(())
    })
    .await;

    Ok(())
}
