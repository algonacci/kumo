mod config;
mod onboarding;

use anyhow::{Context, Result, bail};
use config::Config;
use teloxide::{dispatching::Dispatcher, prelude::*};

const REPLY: &str = "Halo dari Kumo. Pesanmu sudah diterima.";

#[tokio::main]
async fn main() -> Result<()> {
    let command = parse_command()?;

    if matches!(command, Command::Help) {
        print_help();
        return Ok(());
    }

    if matches!(command, Command::Onboard) || !Config::exists()? {
        onboarding::run().await?;
        if matches!(command, Command::Onboard) {
            return Ok(());
        }
        println!();
    }

    run_gateway(Config::load()?).await?;
    Ok(())
}

async fn run_gateway(config: Config) -> Result<()> {
    let bot = Bot::new(config.telegram.bot_token);
    let allowed_user_id = config.telegram.owner_user_id;

    println!("Kumo is listening as @{}.", config.telegram.bot_username);
    println!("Press Ctrl+C to stop.");

    let handler = Update::filter_message().endpoint(move |bot: Bot, message: Message| async move {
        handle_message(bot, message, allowed_user_id).await
    });
    let mut dispatcher = Dispatcher::builder(bot, handler).build();
    let shutdown_token = dispatcher.shutdown_token();
    let mut dispatch_task = tokio::spawn(async move { dispatcher.dispatch().await });

    tokio::select! {
        result = &mut dispatch_task => {
            result.context("Telegram dispatcher task failed")?;
        }
        signal = tokio::signal::ctrl_c() => {
            signal.context("failed to listen for Ctrl+C")?;
            println!();
            println!("Shutting down Kumo...");
            if let Ok(shutdown) = shutdown_token.shutdown() {
                shutdown.await;
            }
            dispatch_task.await.context("Telegram dispatcher task failed")?;
            println!("Kumo stopped.");
        }
    }

    Ok(())
}

async fn handle_message(bot: Bot, message: Message, allowed_user_id: u64) -> ResponseResult<()> {
    let Some(user) = message.from.as_ref() else {
        return respond(());
    };

    if user.id.0 != allowed_user_id || message.text().is_none() {
        return respond(());
    }

    println!("Received a message from Telegram user {}", user.id.0);
    bot.send_message(message.chat.id, REPLY).await?;

    respond(())
}

#[derive(Clone, Copy)]
enum Command {
    Run,
    Onboard,
    Help,
}

fn parse_command() -> Result<Command> {
    let mut args = std::env::args().skip(1);
    let command = match args.next().as_deref() {
        None => Command::Run,
        Some("onboard") => Command::Onboard,
        Some("-h" | "--help") => Command::Help,
        Some(value) => bail!("unknown command '{value}'\n\nRun `kumo --help` for usage."),
    };

    if args.next().is_some() {
        bail!("too many arguments\n\nRun `kumo --help` for usage.");
    }
    Ok(command)
}

fn print_help() {
    println!("Kumo personal agent gateway");
    println!();
    println!("Usage:");
    println!("  kumo            Start the gateway (onboards on first run)");
    println!("  kumo onboard    Configure or replace the Telegram bot");
    println!("  kumo --help     Show this help");
}
