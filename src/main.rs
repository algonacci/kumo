mod compaction;
mod config;
mod markdown;
mod mcp;
mod onboarding;
mod provider;
mod scheduler;
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
    types::{CallbackQuery, ChatAction, InlineKeyboardButton, InlineKeyboardMarkup, ParseMode},
};
use tokio::sync::{Mutex, RwLock, oneshot};
use tools::ToolRegistry;
use uuid::Uuid;

pub(crate) struct AppState {
    config: Config,
    provider: Provider,
    tools: ToolRegistry,
    mcp_statuses: Vec<String>,
    /// A frozen-at-startup rendering of every remembered fact, appended to the system prompt for
    /// every request. Frozen (rather than re-read per turn) so it stays consistent across a turn
    /// and does not defeat provider-side prompt caching; a `remember`/`update_memory`/`forget`
    /// call updates storage immediately but only appears in the prompt after Kumo restarts.
    memory_snapshot: String,
}

pub(crate) struct AgentTurn {
    pub(crate) answer: String,
    pub(crate) record: Vec<ProviderMessage>,
    pub(crate) usage: Usage,
    pub(crate) finish_reason: String,
    pub(crate) model: String,
}

const MAX_TOOL_ROUNDS: usize = 8;
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_APPROVAL_PREVIEW_CHARS: usize = 3500;
const SYSTEM_PROMPT: &str = "You are Kumo, a personal assistant running on the user's host. You may inspect the configured workspace with read-only tools. You may request shell commands when needed, but every command requires explicit user approval before Kumo executes it. Never claim a command ran unless its tool result confirms it. If delegate_to_kamui is available and the task involves editing files or a multi-step coding change, prefer it over run_command: it runs a dedicated coding agent with a proper diff-reviewed file editor, rather than an ad hoc shell command.";
pub(crate) type PendingApprovals = Arc<Mutex<HashMap<String, oneshot::Sender<bool>>>>;

#[tokio::main]
async fn main() -> Result<()> {
    let command = parse_command()?;

    if matches!(command, Command::Help) {
        print_help();
        return Ok(());
    }

    let existing = Config::exists()?.then(Config::load).transpose()?;
    let needs_onboarding = matches!(command, Command::Onboard)
        || existing.as_ref().is_none_or(|config| {
            config.provider.is_none() || config.tools.is_none() || config.timezone.is_none()
        });
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
    let database = Database::open()?;
    let reset_count = database.reset_stuck_running_tasks()?;
    if reset_count > 0 {
        println!("Recovered {reset_count} scheduled task(s) interrupted by a previous shutdown.");
    }
    let memory_snapshot = render_memory_snapshot(&database.list_memory()?);
    let database = Arc::new(Mutex::new(database));
    let tools = ToolRegistry::new(workspace, mcp.tools, database.clone(), config.timezone())?;
    let turn_lock = Arc::new(Mutex::new(()));
    let approvals: PendingApprovals = Arc::new(Mutex::new(HashMap::new()));
    let state = Arc::new(RwLock::new(AppState {
        config,
        provider,
        tools,
        mcp_statuses,
        memory_snapshot,
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

    let scheduler_task = tokio::spawn(scheduler::run(
        bot.clone(),
        state.clone(),
        approvals.clone(),
        database.clone(),
        turn_lock.clone(),
    ));

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
    scheduler_task.abort();

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
    if text == "/sessions" {
        let response = sessions_message(&database, message.chat.id.0).await?;
        bot.send_message(message.chat.id, response).await?;
        return Ok(());
    }
    if let Some(id_prefix) = text.strip_prefix("/resume ").map(str::trim) {
        let response = resume_session(&database, message.chat.id.0, id_prefix).await?;
        bot.send_message(message.chat.id, response).await?;
        return Ok(());
    }
    if let Some(id_prefix) = text.strip_prefix("/delete ").map(str::trim) {
        let response = delete_session(&database, message.chat.id.0, id_prefix).await?;
        bot.send_message(message.chat.id, response).await?;
        return Ok(());
    }
    if text == "/memory" {
        let response = memory_message(&database).await?;
        bot.send_message(message.chat.id, response).await?;
        return Ok(());
    }
    if let Some(argument) = text.strip_prefix("/forget ").map(str::trim) {
        let response = forget_command(&database, argument).await?;
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
    let history = prepare_history(&state, &database, message.chat.id.0).await?;
    match run_agent(&bot, message.chat.id, &state, &approvals, history, text).await {
        Ok(turn) => {
            for chunk in message_chunks(&turn.answer, 4000) {
                send_formatted(&bot, message.chat.id, &chunk).await?;
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

pub(crate) async fn run_agent(
    bot: &Bot,
    chat_id: ChatId,
    state: &RwLock<AppState>,
    approvals: &PendingApprovals,
    history: storage::History,
    prompt: &str,
) -> Result<AgentTurn> {
    let (provider, tool_definitions, model, timezone, memory_snapshot) = {
        let state = state.read().await;
        (
            state.provider.clone(),
            state.tools.definitions(),
            state.provider.active_model().to_owned(),
            state.config.timezone(),
            state.memory_snapshot.clone(),
        )
    };
    let user_message = ProviderMessage::user(prompt);
    let mut system = SYSTEM_PROMPT.to_owned();
    let now = chrono::Utc::now().with_timezone(&timezone);
    system.push_str(&format!(
        "\n\nCurrent date and time: {} ({timezone}).",
        now.format("%Y-%m-%d %H:%M:%S %:z")
    ));
    if !memory_snapshot.is_empty() {
        system.push_str("\n\n");
        system.push_str(&memory_snapshot);
    }
    if let Some(summary) = &history.summary {
        system.push_str("\n\nSummary of the earlier conversation:\n\n");
        system.push_str(summary);
    }
    let mut messages = vec![ProviderMessage::system(system)];
    messages.extend(history.messages);
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
                        tools.dispatch(chat_id.0, &call).await
                    }
                    Some(_) => "User denied this command. Do not run it.".to_owned(),
                    None => "Error: invalid command arguments".to_owned(),
                }
            } else {
                tools.dispatch(chat_id.0, &call).await
            };
            let result_message = ProviderMessage::tool_result(call.id, output);
            messages.push(result_message.clone());
            trail.push(result_message);
        }
    }

    bail!("model exceeded the {MAX_TOOL_ROUNDS}-round tool limit")
}

pub(crate) async fn prepare_history(
    state: &RwLock<AppState>,
    database: &Mutex<Database>,
    chat_id: i64,
) -> Result<storage::History> {
    let mut history = database.lock().await.load_active_history(chat_id)?;
    let (provider, context_window) = {
        let state = state.read().await;
        (
            state.provider.clone(),
            state.config.provider()?.context_window,
        )
    };
    if compaction::total_bytes(&history.messages) <= compaction::threshold(context_window) {
        return Ok(history);
    }
    let Some(cutoff) = compaction::cutoff(&history.messages) else {
        return Ok(history);
    };

    println!("Compacting {cutoff} older message(s)...");
    let rendered = compaction::render(&history.messages[..cutoff]);
    let summary = provider
        .summarize(&compaction::summary_messages(
            history.summary.as_deref(),
            &rendered,
        ))
        .await?;
    database
        .lock()
        .await
        .compact_active_session(chat_id, &summary, cutoff)?;
    history.messages.drain(..cutoff);
    history.summary = Some(summary);
    Ok(history)
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
    let context_window = state.config.provider()?.context_window;
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
            "{} ({})\nMessages: {}\nRequests: {}\nTokens: {}\nCompacted: {}",
            session.title,
            &session.id[..8],
            session.message_count,
            session.request_count,
            session.total_tokens,
            if session.summary.is_some() {
                format!("yes (through message {})", session.summarized_message_id)
            } else {
                "no".to_owned()
            }
        ),
        None => "none (created after the first successful reply)".to_owned(),
    };
    Ok(format!(
        "Model: {model}\nContext window: {}\nWorkspace: {workspace}\nSession: {session}\nMCP:\n{mcp}\nDatabase: {}",
        context_window.map_or_else(|| "default".to_owned(), |window| window.to_string()),
        database.path().display()
    ))
}

/// List every saved session for this chat, newest first, marking the active one. `/new` only
/// detaches the active pointer — it never deletes a session — so this is how a retired session
/// becomes visible and resumable again.
async fn sessions_message(database: &Mutex<Database>, chat_id: i64) -> Result<String> {
    let database = database.lock().await;
    let sessions = database.list_sessions(chat_id)?;
    if sessions.is_empty() {
        return Ok("No saved sessions yet.".to_owned());
    }
    let active_id = database.active_session(chat_id)?.map(|session| session.id);

    let mut lines = vec!["Saved sessions:".to_owned()];
    for session in &sessions {
        let marker = if active_id.as_deref() == Some(session.id.as_str()) {
            "*"
        } else {
            " "
        };
        lines.push(format!(
            "{marker} {}  {}  {:<40}  {} messages",
            &session.id[..8],
            format_timestamp(session.updated_at),
            truncate(&session.title, 40),
            session.message_count,
        ));
    }
    lines.push(String::new());
    lines.push("Use /resume <id> to switch, or /delete <id> to remove one.".to_owned());
    Ok(lines.join("\n"))
}

/// Switch this chat's active session to a previously saved one, identified by an unambiguous ID
/// prefix scoped to this chat (so one chat cannot resume another chat's session by guessing).
async fn resume_session(
    database: &Mutex<Database>,
    chat_id: i64,
    id_prefix: &str,
) -> Result<String> {
    if id_prefix.is_empty() {
        return Ok("Usage: /resume <id>. Use /sessions to list saved sessions.".to_owned());
    }
    let database = database.lock().await;
    let Some(session_id) = database.find_session_by_prefix(chat_id, id_prefix)? else {
        return Ok(format!(
            "No session matches '{id_prefix}', or the prefix is ambiguous. Use /sessions to list saved sessions."
        ));
    };
    database.set_active_session(chat_id, &session_id)?;
    Ok(format!("Resumed session {}.", &session_id[..8]))
}

/// Permanently delete a saved session (and, via cascade, its messages and usage records),
/// identified the same way as `/resume`.
async fn delete_session(
    database: &Mutex<Database>,
    chat_id: i64,
    id_prefix: &str,
) -> Result<String> {
    if id_prefix.is_empty() {
        return Ok("Usage: /delete <id>. Use /sessions to list saved sessions.".to_owned());
    }
    let database = database.lock().await;
    let Some(session_id) = database.find_session_by_prefix(chat_id, id_prefix)? else {
        return Ok(format!(
            "No session matches '{id_prefix}', or the prefix is ambiguous. Use /sessions to list saved sessions."
        ));
    };
    database.delete_session(&session_id)?;
    Ok(format!("Deleted session {}.", &session_id[..8]))
}

/// Show what is currently remembered on disk. Note this reads the database directly, so it can
/// briefly disagree with what a live conversation actually sees in its system prompt: the
/// snapshot injected into every turn is frozen at startup (see `AppState::memory_snapshot`), so a
/// `remember`/`update_memory`/`forget` call made just now will show up here immediately but only
/// take effect in conversation after Kumo restarts.
async fn memory_message(database: &Mutex<Database>) -> Result<String> {
    let entries = database.lock().await.list_memory()?;
    if entries.is_empty() {
        return Ok("Nothing remembered yet.".to_owned());
    }
    let mut lines = vec!["Remembered facts:".to_owned()];
    for entry in &entries {
        lines.push(format!("- {}", entry.content));
    }
    lines.push(String::new());
    lines.push(
        "Changes here take effect in conversation after Kumo restarts. Use /forget <text> or /forget all."
            .to_owned(),
    );
    Ok(lines.join("\n"))
}

/// `/forget all` clears every remembered fact; `/forget <text>` removes one matched by an
/// unambiguous substring, the same rule the `forget` tool uses.
async fn forget_command(database: &Mutex<Database>, argument: &str) -> Result<String> {
    if argument.is_empty() {
        return Ok(
            "Usage: /forget <text> or /forget all. Use /memory to list what is remembered."
                .to_owned(),
        );
    }
    let database = database.lock().await;
    if argument.eq_ignore_ascii_case("all") {
        let count = database.clear_memory()?;
        return Ok(format!("Forgot all {count} remembered fact(s)."));
    }
    if database.forget(argument)? {
        Ok(format!("Forgot the fact matching \"{argument}\"."))
    } else {
        Ok(format!(
            "No remembered fact matches \"{argument}\", or the text matches more than one. Use /memory to see exact wording."
        ))
    }
}

fn truncate(text: &str, max: usize) -> String {
    let mut result: String = text.chars().take(max).collect();
    if text.chars().count() > max {
        result.push('\u{2026}');
    }
    result
}

fn format_timestamp(timestamp: i64) -> String {
    chrono::DateTime::from_timestamp(timestamp, 0)
        .map(|value| value.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "unknown time".to_owned())
}

/// Render every remembered fact as a system-prompt block, or an empty string when there is
/// nothing remembered yet (so callers can skip adding an empty section).
fn render_memory_snapshot(entries: &[storage::MemoryEntry]) -> String {
    if entries.is_empty() {
        return String::new();
    }
    let mut text = "Remembered facts about the user (persist across conversations):".to_owned();
    for entry in entries {
        text.push_str("\n- ");
        text.push_str(&entry.content);
    }
    text
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

/// Send one message chunk rendered as MarkdownV2. Falls back to plain text if Telegram rejects the
/// formatted version (e.g. an entity split across a chunk boundary), so the reply is never lost.
pub(crate) async fn send_formatted(bot: &Bot, chat_id: ChatId, chunk: &str) -> Result<()> {
    let formatted = markdown::to_telegram_markdown_v2(chunk);
    let sent = bot
        .send_message(chat_id, formatted)
        .parse_mode(ParseMode::MarkdownV2)
        .await;
    match sent {
        Ok(_) => Ok(()),
        Err(error) => {
            eprintln!("Markdown formatting failed, sending plain text: {error:#}");
            bot.send_message(chat_id, chunk).await?;
            Ok(())
        }
    }
}

pub(crate) fn message_chunks(message: &str, max_chars: usize) -> Vec<String> {
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

    #[test]
    fn truncate_appends_ellipsis_only_when_needed() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 5), "hello\u{2026}");
    }

    #[test]
    fn format_timestamp_renders_a_readable_date() {
        // 2024-01-15T10:30:00Z
        assert_eq!(format_timestamp(1_705_314_600), "2024-01-15 10:30");
    }

    #[tokio::test]
    async fn sessions_message_reports_no_sessions_when_chat_is_empty() {
        let database = Mutex::new(Database::open_in_memory_for_tests());
        assert_eq!(
            sessions_message(&database, 42).await.unwrap(),
            "No saved sessions yet."
        );
    }

    #[tokio::test]
    async fn sessions_message_marks_the_active_session() {
        let mut db = Database::open_in_memory_for_tests();
        let id = db
            .save_turn(
                42,
                "model-a",
                &[
                    ProviderMessage::user("hi"),
                    ProviderMessage::assistant("hello"),
                ],
                &Usage::default(),
                "stop",
            )
            .unwrap();
        let database = Mutex::new(db);

        let response = sessions_message(&database, 42).await.unwrap();

        assert!(response.contains(&id[..8]));
        assert!(
            response.contains('*'),
            "the only session should be marked active"
        );
    }

    #[tokio::test]
    async fn resume_session_reports_usage_on_an_empty_argument() {
        let database = Mutex::new(Database::open_in_memory_for_tests());
        let response = resume_session(&database, 42, "").await.unwrap();
        assert!(response.starts_with("Usage:"));
    }

    #[tokio::test]
    async fn resume_session_reports_no_match_for_an_unknown_prefix() {
        let database = Mutex::new(Database::open_in_memory_for_tests());
        let response = resume_session(&database, 42, "nope").await.unwrap();
        assert!(response.contains("No session matches"));
    }

    #[tokio::test]
    async fn resume_session_switches_the_active_session() {
        let mut db = Database::open_in_memory_for_tests();
        let first = db
            .save_turn(
                42,
                "model-a",
                &[
                    ProviderMessage::user("first"),
                    ProviderMessage::assistant("a"),
                ],
                &Usage::default(),
                "stop",
            )
            .unwrap();
        db.clear_active_session(42).unwrap();
        db.save_turn(
            42,
            "model-a",
            &[
                ProviderMessage::user("second"),
                ProviderMessage::assistant("a"),
            ],
            &Usage::default(),
            "stop",
        )
        .unwrap();
        let database = Mutex::new(db);

        let response = resume_session(&database, 42, &first[..8]).await.unwrap();

        assert!(response.contains(&first[..8]));
        assert_eq!(
            database
                .lock()
                .await
                .active_session(42)
                .unwrap()
                .unwrap()
                .id,
            first
        );
    }

    #[tokio::test]
    async fn delete_session_reports_usage_on_an_empty_argument() {
        let database = Mutex::new(Database::open_in_memory_for_tests());
        let response = delete_session(&database, 42, "").await.unwrap();
        assert!(response.starts_with("Usage:"));
    }

    #[tokio::test]
    async fn delete_session_removes_a_resolved_session() {
        let mut db = Database::open_in_memory_for_tests();
        let id = db
            .save_turn(
                42,
                "model-a",
                &[
                    ProviderMessage::user("hi"),
                    ProviderMessage::assistant("hello"),
                ],
                &Usage::default(),
                "stop",
            )
            .unwrap();
        let database = Mutex::new(db);

        let response = delete_session(&database, 42, &id[..8]).await.unwrap();

        assert!(response.contains(&id[..8]));
        assert!(database.lock().await.list_sessions(42).unwrap().is_empty());
    }

    #[test]
    fn render_memory_snapshot_is_empty_with_no_entries() {
        assert_eq!(render_memory_snapshot(&[]), "");
    }

    #[test]
    fn render_memory_snapshot_lists_every_fact() {
        let entries = vec![
            storage::MemoryEntry {
                content: "The user is a researcher.".to_owned(),
            },
            storage::MemoryEntry {
                content: "Prefers concise answers.".to_owned(),
            },
        ];

        let rendered = render_memory_snapshot(&entries);

        assert!(rendered.contains("The user is a researcher."));
        assert!(rendered.contains("Prefers concise answers."));
    }

    #[tokio::test]
    async fn memory_message_reports_nothing_remembered_when_empty() {
        let database = Mutex::new(Database::open_in_memory_for_tests());
        assert_eq!(
            memory_message(&database).await.unwrap(),
            "Nothing remembered yet."
        );
    }

    #[tokio::test]
    async fn memory_message_lists_stored_facts() {
        let db = Database::open_in_memory_for_tests();
        db.remember("The user is a researcher.").unwrap();
        let database = Mutex::new(db);

        let response = memory_message(&database).await.unwrap();

        assert!(response.contains("The user is a researcher."));
    }

    #[tokio::test]
    async fn forget_command_reports_usage_on_an_empty_argument() {
        let database = Mutex::new(Database::open_in_memory_for_tests());
        let response = forget_command(&database, "").await.unwrap();
        assert!(response.starts_with("Usage:"));
    }

    #[tokio::test]
    async fn forget_command_all_clears_every_fact() {
        let db = Database::open_in_memory_for_tests();
        db.remember("one").unwrap();
        db.remember("two").unwrap();
        let database = Mutex::new(db);

        let response = forget_command(&database, "all").await.unwrap();

        assert!(response.contains('2'));
        assert!(database.lock().await.list_memory().unwrap().is_empty());
    }

    #[tokio::test]
    async fn forget_command_removes_a_matched_fact() {
        let db = Database::open_in_memory_for_tests();
        db.remember("The user is a researcher.").unwrap();
        let database = Mutex::new(db);

        let response = forget_command(&database, "researcher").await.unwrap();

        assert!(response.contains("Forgot"));
        assert!(database.lock().await.list_memory().unwrap().is_empty());
    }

    #[tokio::test]
    async fn forget_command_reports_no_match_without_erroring() {
        let database = Mutex::new(Database::open_in_memory_for_tests());
        let response = forget_command(&database, "nonexistent").await.unwrap();
        assert!(response.contains("No remembered fact matches"));
    }
}
