mod config;
mod mcp;
mod onboarding;
mod provider;
mod storage;
mod tools;

use std::{collections::HashMap, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use config::Config;
use provider::{Message as ProviderMessage, Provider, Usage};
use storage::Database;
use teloxide::{
    dispatching::Dispatcher,
    payloads::SendMessageSetters,
    prelude::*,
    types::{CallbackQuery, ChatAction, InlineKeyboardButton, InlineKeyboardMarkup},
};
use tokio::sync::{Mutex, RwLock, oneshot};
use tools::ToolRegistry;
use uuid::Uuid;

struct AppState {
    config: Config,
    provider: Provider,
    tools: ToolRegistry,
    mcp_statuses: Vec<String>,
}

struct AgentTurn {
    answer: String,
    record: Vec<ProviderMessage>,
    usage: Usage,
    finish_reason: String,
    model: String,
}

const MAX_TOOL_ROUNDS: usize = 8;
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_APPROVAL_PREVIEW_CHARS: usize = 3500;
const SYSTEM_PROMPT: &str = "You are Kumo, a personal assistant running on the user's host. You may inspect the configured workspace with read-only tools. You may request shell commands when needed, but every command requires explicit user approval before Kumo executes it. Never claim a command ran unless its tool result confirms it.";
type PendingApprovals = Arc<Mutex<HashMap<String, oneshot::Sender<bool>>>>;

#[tokio::main]
async fn main() -> Result<()> {
    let command = parse_command()?;

    if matches!(command, Command::Help) {
        print_help();
        return Ok(());
    }

    let existing = Config::exists()?.then(Config::load).transpose()?;
    let needs_onboarding = matches!(command, Command::Onboard)
        || existing
            .as_ref()
            .is_none_or(|config| config.provider.is_none() || config.tools.is_none());
    let config = if needs_onboarding {
        let reconfigure_provider = matches!(command, Command::Onboard);
        let config = onboarding::run(existing, reconfigure_provider).await?;
        if matches!(command, Command::Onboard) {
            return Ok(());
        }
        println!();
        config
    } else {
        existing.expect("configuration exists when onboarding is not needed")
    };

    run_gateway(config).await?;
    Ok(())
}

async fn run_gateway(config: Config) -> Result<()> {
    let bot = Bot::new(config.telegram.bot_token.clone());
    let allowed_user_id = config.telegram.owner_user_id;
    let provider = Provider::new(config.provider()?.clone());
    let workspace = config
        .tools
        .as_ref()
        .context("tools are not configured; run `kumo onboard`")?
        .workspace
        .clone();
    let mcp = mcp::connect_all(&config.mcp).await;
    let mcp_statuses = mcp
        .statuses
        .iter()
        .map(|status| match &status.error {
            Some(error) => format!("{}: failed ({error})", status.name),
            None => format!(
                "{}: {} tool(s){}",
                status.name,
                status.tool_count,
                if status.trusted { " [trusted]" } else { "" }
            ),
        })
        .collect::<Vec<_>>();
    for status in &mcp.statuses {
        match &status.error {
            Some(error) => println!("MCP {}: failed ({error})", status.name),
            None => println!(
                "MCP {}: {} tool(s){}",
                status.name,
                status.tool_count,
                if status.trusted { " [trusted]" } else { "" }
            ),
        }
    }
    let tools = ToolRegistry::new(workspace, mcp.tools)?;
    let database = Arc::new(Mutex::new(Database::open()?));
    let turn_lock = Arc::new(Mutex::new(()));
    let approvals: PendingApprovals = Arc::new(Mutex::new(HashMap::new()));
    let state = Arc::new(RwLock::new(AppState {
        config,
        provider,
        tools,
        mcp_statuses,
    }));

    let current = state.read().await;
    println!(
        "Kumo is listening as @{}.",
        current.config.telegram.bot_username
    );
    println!("Model: {}", current.provider.active_model());
    println!(
        "Workspace: {}",
        current
            .config
            .tools
            .as_ref()
            .expect("tools are configured before gateway startup")
            .workspace
            .display()
    );
    drop(current);
    println!("Press Ctrl+C to stop.");

    let handler = teloxide::dptree::entry()
        .branch(Update::filter_message().endpoint(
            move |bot: Bot,
                  message: Message,
                  state: Arc<RwLock<AppState>>,
                  approvals: PendingApprovals,
                  database: Arc<Mutex<Database>>,
                  turn_lock: Arc<Mutex<()>>| async move {
                handle_message(
                    bot,
                    message,
                    allowed_user_id,
                    state,
                    approvals,
                    database,
                    turn_lock,
                )
                .await
            },
        ))
        .branch(Update::filter_callback_query().endpoint(
            move |bot: Bot, query: CallbackQuery, approvals: PendingApprovals| async move {
                handle_approval_callback(bot, query, allowed_user_id, approvals).await
            },
        ));
    let mut dispatcher = Dispatcher::builder(bot, handler)
        .dependencies(teloxide::dptree::deps![
            state,
            approvals.clone(),
            database,
            turn_lock
        ])
        .distribution_function(|_| None::<()>)
        .build();
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
            approvals.lock().await.clear();
            if let Ok(shutdown) = shutdown_token.shutdown() {
                shutdown.await;
            }
            dispatch_task.await.context("Telegram dispatcher task failed")?;
            println!("Kumo stopped.");
        }
    }

    Ok(())
}

async fn handle_message(
    bot: Bot,
    message: Message,
    allowed_user_id: u64,
    state: Arc<RwLock<AppState>>,
    approvals: PendingApprovals,
    database: Arc<Mutex<Database>>,
    turn_lock: Arc<Mutex<()>>,
) -> Result<()> {
    let Some(user) = message.from.as_ref() else {
        return Ok(());
    };

    let Some(text) = message.text() else {
        return Ok(());
    };
    if user.id.0 != allowed_user_id {
        return Ok(());
    }
    let _turn_guard = turn_lock.lock().await;

    println!("Received a message from Telegram user {}", user.id.0);
    if text == "/new" {
        let cleared = database
            .lock()
            .await
            .clear_active_session(message.chat.id.0)?;
        let response = if cleared {
            "Started a new conversation. Your previous history is still stored."
        } else {
            "There is no active conversation yet."
        };
        bot.send_message(message.chat.id, response).await?;
        return Ok(());
    }
    if text == "/status" {
        let response = status_message(&state, &database, message.chat.id.0).await?;
        bot.send_message(message.chat.id, response).await?;
        return Ok(());
    }
    if text == "/models" {
        let response = models_message(&state.read().await.config);
        bot.send_message(message.chat.id, response).await?;
        return Ok(());
    }
    if text == "/model" {
        let active_model = state.read().await.provider.active_model().to_owned();
        bot.send_message(
            message.chat.id,
            format!("Current model: {active_model}\n\nUse /models to list models or /model <id> to switch."),
        )
        .await?;
        return Ok(());
    }
    if let Some(model) = text.strip_prefix("/model ").map(str::trim) {
        let response = switch_model(&state, model).await;
        bot.send_message(message.chat.id, response).await?;
        return Ok(());
    }

    bot.send_chat_action(message.chat.id, ChatAction::Typing)
        .await?;
    let history = database
        .lock()
        .await
        .load_active_messages(message.chat.id.0)?;
    match run_agent(&bot, message.chat.id, &state, &approvals, history, text).await {
        Ok(turn) => {
            for chunk in message_chunks(&turn.answer, 4000) {
                bot.send_message(message.chat.id, chunk).await?;
            }
            database.lock().await.save_turn(
                message.chat.id.0,
                &turn.model,
                &turn.record,
                &turn.usage,
                &turn.finish_reason,
            )?;
        }
        Err(error) => {
            eprintln!("Model request failed: {error:#}");
            bot.send_message(
                message.chat.id,
                "The model provider could not answer. Check the Kumo terminal for details.",
            )
            .await?;
        }
    }

    Ok(())
}

async fn run_agent(
    bot: &Bot,
    chat_id: ChatId,
    state: &RwLock<AppState>,
    approvals: &PendingApprovals,
    history: Vec<ProviderMessage>,
    prompt: &str,
) -> Result<AgentTurn> {
    let (provider, tool_definitions, model) = {
        let state = state.read().await;
        (
            state.provider.clone(),
            state.tools.definitions(),
            state.provider.active_model().to_owned(),
        )
    };
    let user_message = ProviderMessage::user(prompt);
    let mut messages = vec![ProviderMessage::system(SYSTEM_PROMPT)];
    messages.extend(history);
    messages.push(user_message.clone());
    let mut trail = Vec::new();
    let mut usage = Usage::default();

    for _ in 0..MAX_TOOL_ROUNDS {
        let response = provider.chat(&messages, &tool_definitions).await?;
        accumulate_usage(&mut usage, &response.usage);
        if response.tool_calls.is_empty() {
            if response.content.trim().is_empty() {
                bail!("provider returned an empty response");
            }
            let assistant = ProviderMessage::assistant(response.content.clone());
            let mut record = Vec::with_capacity(trail.len() + 2);
            record.push(user_message);
            record.append(&mut trail);
            record.push(assistant);
            return Ok(AgentTurn {
                answer: response.content,
                record,
                usage,
                finish_reason: response.finish_reason,
                model,
            });
        }

        println!(
            "Model requested {} tool call(s).",
            response.tool_calls.len()
        );
        let request_message =
            ProviderMessage::tool_request(response.content, response.tool_calls.clone());
        messages.push(request_message.clone());
        trail.push(request_message);
        for call in response.tool_calls {
            println!("Tool: {}", call.name);
            let tools = state.read().await.tools.clone();
            let output = if tools.requires_confirmation(&call.name) {
                match tools.preview(&call) {
                    Some(preview)
                        if request_approval(bot, chat_id, approvals, &preview).await? =>
                    {
                        tools.dispatch(&call).await
                    }
                    Some(_) => "User denied this command. Do not run it.".to_owned(),
                    None => "Error: invalid command arguments".to_owned(),
                }
            } else {
                tools.dispatch(&call).await
            };
            let result_message = ProviderMessage::tool_result(call.id, output);
            messages.push(result_message.clone());
            trail.push(result_message);
        }
    }

    bail!("model exceeded the {MAX_TOOL_ROUNDS}-round tool limit")
}

fn accumulate_usage(total: &mut Usage, usage: &Usage) {
    total.prompt_tokens = total.prompt_tokens.saturating_add(usage.prompt_tokens);
    total.completion_tokens = total
        .completion_tokens
        .saturating_add(usage.completion_tokens);
    total.total_tokens = total.total_tokens.saturating_add(usage.total_tokens);
}

async fn status_message(
    state: &RwLock<AppState>,
    database: &Mutex<Database>,
    chat_id: i64,
) -> Result<String> {
    let state = state.read().await;
    let workspace = state
        .config
        .tools
        .as_ref()
        .expect("tools are configured before gateway startup")
        .workspace
        .display()
        .to_string();
    let model = state.provider.active_model().to_owned();
    let mcp = if state.mcp_statuses.is_empty() {
        "none".to_owned()
    } else {
        state.mcp_statuses.join("\n")
    };
    drop(state);

    let database = database.lock().await;
    let session = database.active_session(chat_id)?;
    let session = match session {
        Some(session) => format!(
            "{} ({})\nMessages: {}\nRequests: {}\nTokens: {}",
            session.title,
            &session.id[..8],
            session.message_count,
            session.request_count,
            session.total_tokens
        ),
        None => "none (created after the first successful reply)".to_owned(),
    };
    Ok(format!(
        "Model: {model}\nWorkspace: {workspace}\nSession: {session}\nMCP:\n{mcp}\nDatabase: {}",
        database.path().display()
    ))
}

async fn request_approval(
    bot: &Bot,
    chat_id: ChatId,
    approvals: &PendingApprovals,
    action: &str,
) -> Result<bool> {
    let nonce = Uuid::new_v4().simple().to_string();
    let keyboard = InlineKeyboardMarkup::new([[
        InlineKeyboardButton::callback("Allow once", format!("approval:{nonce}:allow")),
        InlineKeyboardButton::callback("Deny", format!("approval:{nonce}:deny")),
    ]]);
    let (sender, receiver) = oneshot::channel();
    approvals.lock().await.insert(nonce.clone(), sender);

    let preview = action
        .chars()
        .take(MAX_APPROVAL_PREVIEW_CHARS)
        .collect::<String>();
    let preview = if action.chars().count() > MAX_APPROVAL_PREVIEW_CHARS {
        format!("{preview}\n... approval preview truncated")
    } else {
        preview
    };
    let prompt = match bot
        .send_message(
            chat_id,
            format!("Kumo wants to run this host action:\n\n{preview}"),
        )
        .reply_markup(keyboard)
        .await
    {
        Ok(prompt) => prompt,
        Err(error) => {
            approvals.lock().await.remove(&nonce);
            return Err(error).context("could not send command approval prompt");
        }
    };

    let approved = match tokio::time::timeout(APPROVAL_TIMEOUT, receiver).await {
        Ok(Ok(approved)) => approved,
        Ok(Err(_)) => false,
        Err(_) => {
            approvals.lock().await.remove(&nonce);
            false
        }
    };
    let _ = bot.edit_message_reply_markup(chat_id, prompt.id).await;
    Ok(approved)
}

async fn handle_approval_callback(
    bot: Bot,
    query: CallbackQuery,
    allowed_user_id: u64,
    approvals: PendingApprovals,
) -> Result<()> {
    bot.answer_callback_query(query.id.clone()).await?;
    if query.from.id.0 != allowed_user_id {
        return Ok(());
    }
    let Some(data) = query.data.as_deref() else {
        return Ok(());
    };
    let Some(rest) = data.strip_prefix("approval:") else {
        return Ok(());
    };
    let Some((nonce, decision)) = rest.rsplit_once(':') else {
        return Ok(());
    };
    let approved = match decision {
        "allow" => true,
        "deny" => false,
        _ => return Ok(()),
    };

    if let Some(sender) = approvals.lock().await.remove(nonce) {
        let _ = sender.send(approved);
    }
    Ok(())
}

fn message_chunks(message: &str, max_chars: usize) -> Vec<String> {
    if message.is_empty() {
        return Vec::new();
    }

    let mut chunks = Vec::new();
    let mut current = String::new();
    for character in message.chars() {
        if current.chars().count() == max_chars {
            chunks.push(std::mem::take(&mut current));
        }
        current.push(character);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

fn models_message(config: &Config) -> String {
    let provider = config
        .provider
        .as_ref()
        .expect("provider is configured before gateway startup");
    let mut message = format!("Available models (current: {}):\n", provider.active_model);
    for model in &provider.models {
        let line = format!("\n{model}");
        if message.len() + line.len() > 3800 {
            message.push_str("\n\nList truncated.");
            break;
        }
        message.push_str(&line);
    }
    message.push_str("\n\nSwitch with /model <id>.");
    message
}

async fn switch_model(state: &RwLock<AppState>, model: &str) -> String {
    let mut state = state.write().await;
    let Some(provider_config) = state.config.provider.as_mut() else {
        return "Model provider is not configured.".to_owned();
    };
    if !provider_config
        .models
        .iter()
        .any(|available| available == model)
    {
        return format!("Unknown model: {model}\n\nUse /models to see available models.");
    }

    provider_config.active_model = model.to_owned();
    state.provider = Provider::new(provider_config.clone());
    match state.config.save() {
        Ok(_) => format!("Switched to {model}."),
        Err(error) => {
            eprintln!("Could not save model selection: {error:#}");
            "Model changed for this run, but Kumo could not save the selection.".to_owned()
        }
    }
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
    println!("  kumo onboard    Configure the model provider and workspace");
    println!("  kumo --help     Show this help");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_long_unicode_messages_without_corruption() {
        assert_eq!(message_chunks("abé日", 2), vec!["ab", "é日"]);
    }
}
