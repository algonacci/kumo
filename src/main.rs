mod compaction;
mod config;
mod daemon;
mod markdown;
mod mcp;
mod onboarding;
mod provider;
mod scheduler;
mod service;
mod storage;
mod tools;

use std::{collections::HashMap, future::Future, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use config::Config;
use provider::{ImageAttachment, Message as ProviderMessage, Provider, Usage};
use storage::Database;
use teloxide::{
    dispatching::Dispatcher,
    net::Download,
    payloads::SendMessageSetters,
    prelude::*,
    types::{
        CallbackQuery, ChatAction, InlineKeyboardButton, InlineKeyboardMarkup, InputFile, ParseMode,
    },
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
    pub(crate) images: Vec<mcp::McpImage>,
}

const MAX_TOOL_ROUNDS: usize = 8;
/// Stored in place of the provider's own finish reason when Kumo's round cap, not the model, ended
/// the turn.
const TOOL_ROUND_LIMIT_FINISH_REASON: &str = "tool_round_limit";
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(120);
/// Telegram clears a typing indicator on its own after roughly five seconds, so anything longer
/// has to keep saying so.
const TYPING_REFRESH: Duration = Duration::from_secs(4);
const MAX_APPROVAL_PREVIEW_CHARS: usize = 3500;
const SYSTEM_PROMPT: &str = "You are Kumo, a personal assistant running on the user's host. You may inspect the configured workspace with read-only tools. You may request shell commands when needed, but every command requires explicit user approval before Kumo executes it. Never claim a command ran unless its tool result confirms it. If delegate_to_kamui is available and the task involves editing files or a multi-step coding change, prefer it over run_command: it runs a dedicated coding agent with a proper diff-reviewed file editor, rather than an ad hoc shell command.";
/// A user's answer to an approval prompt. `AlwaysAllow` is distinct from `AllowOnce`: it also
/// grants the calling tool blanket approval for the rest of this conversation (see
/// `Database::always_allow_tool`), not just this one call.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ApprovalOutcome {
    AllowOnce,
    AlwaysAllow,
    Deny,
}

pub(crate) type PendingApprovals = Arc<Mutex<HashMap<String, oneshot::Sender<ApprovalOutcome>>>>;

/// State for one in-flight `ask_user` question: which chat it was asked in (so `handle_message`
/// can tell whether incoming text should answer it instead of starting a new turn), the offered
/// button labels (so a callback's option *index* — kept short to fit Telegram's 64-byte callback
/// data limit — can be turned back into the actual answer text), and the sender that delivers the
/// final answer back to `ask_user`.
pub(crate) struct PendingQuestion {
    chat_id: ChatId,
    options: Vec<String>,
    sender: oneshot::Sender<String>,
}

/// Nonce -> pending question. A question's answer is free text: either the label of a button the
/// user tapped, or a plain text message they sent instead of tapping anything.
pub(crate) type PendingQuestions = Arc<Mutex<HashMap<String, PendingQuestion>>>;

#[tokio::main]
async fn main() -> Result<()> {
    let command = parse_command()?;

    if matches!(command, Command::Help) {
        print_help();
        return Ok(());
    }
    if matches!(command, Command::Version) {
        println!("kumo {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if matches!(command, Command::Status) {
        return print_status().await;
    }
    if matches!(command, Command::Doctor) {
        return run_doctor().await;
    }
    if matches!(command, Command::Start) {
        return daemon::start();
    }
    if matches!(command, Command::Stop) {
        return daemon::stop();
    }
    if matches!(command, Command::Restart) {
        daemon::stop()?;
        return daemon::start();
    }
    if matches!(command, Command::Enable) {
        return service::enable();
    }
    if matches!(command, Command::Disable) {
        return service::disable();
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
                status.trust_label()
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
                status.trust_label()
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
    let questions: PendingQuestions = Arc::new(Mutex::new(HashMap::new()));
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
        questions.clone(),
        database.clone(),
        turn_lock.clone(),
    ));

    let handler = teloxide::dptree::entry()
        .branch(Update::filter_message().endpoint(
            move |bot: Bot,
                  message: Message,
                  state: Arc<RwLock<AppState>>,
                  approvals: PendingApprovals,
                  questions: PendingQuestions,
                  database: Arc<Mutex<Database>>,
                  turn_lock: Arc<Mutex<()>>| async move {
                handle_message(
                    bot,
                    message,
                    allowed_user_id,
                    state,
                    approvals,
                    questions,
                    database,
                    turn_lock,
                )
                .await
            },
        ))
        .branch(Update::filter_callback_query().endpoint(
            move |bot: Bot,
                  query: CallbackQuery,
                  approvals: PendingApprovals,
                  questions: PendingQuestions| async move {
                handle_callback(bot, query, allowed_user_id, approvals, questions).await
            },
        ));
    let mut dispatcher = Dispatcher::builder(bot, handler)
        .dependencies(teloxide::dptree::deps![
            state,
            approvals.clone(),
            questions.clone(),
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
            questions.lock().await.clear();
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

#[allow(clippy::too_many_arguments)]
async fn handle_message(
    bot: Bot,
    message: Message,
    allowed_user_id: u64,
    state: Arc<RwLock<AppState>>,
    approvals: PendingApprovals,
    questions: PendingQuestions,
    database: Arc<Mutex<Database>>,
    turn_lock: Arc<Mutex<()>>,
) -> Result<()> {
    let Some(user) = message.from.as_ref() else {
        return Ok(());
    };
    if user.id.0 != allowed_user_id {
        return Ok(());
    }

    // If an ask_user question is waiting on this chat, any text the user sends next answers it
    // (a free-text reply instead of tapping one of the offered buttons) rather than starting a
    // new command or agent turn. This check happens before the turn lock, since answering a
    // question does not itself start a new turn — the turn that asked it is already holding the
    // lock and waiting on this very answer.
    if let Some(text) = message.text() {
        let nonce = questions
            .lock()
            .await
            .iter()
            .find(|(_, pending)| pending.chat_id == message.chat.id)
            .map(|(nonce, _)| nonce.clone());
        if let Some(nonce) = nonce
            && let Some(pending) = questions.lock().await.remove(&nonce)
        {
            let _ = pending.sender.send(text.to_owned());
            return Ok(());
        }
    }

    // A photo has no `.text()` (only an optional `.caption()`) and never carries a slash command,
    // so it takes a separate, simpler path straight to the agent loop instead of the text command
    // routing below.
    if message.photo().is_some() {
        let _turn_guard = turn_lock.lock().await;
        return handle_photo_message(bot, message, state, approvals, questions, database).await;
    }
    if message.document().is_some() {
        let _turn_guard = turn_lock.lock().await;
        return handle_document_message(bot, message, state, approvals, questions, database).await;
    }

    let Some(text) = message.text() else {
        return Ok(());
    };
    let _turn_guard = turn_lock.lock().await;

    println!("Received a message from Telegram user {}", user.id.0);
    if text == "/new" {
        let database = database.lock().await;
        let cleared = database.clear_active_session(message.chat.id.0)?;
        // A fresh conversation starts with the normal per-call approval prompts again, not
        // whatever tools were "always allow"-ed in the conversation being left behind.
        database.clear_always_allowed(message.chat.id.0)?;
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
    if text == "/reminders" {
        let response = reminders_message(&database, &state, message.chat.id.0).await?;
        bot.send_message(message.chat.id, response).await?;
        return Ok(());
    }
    if let Some(id_prefix) = text.strip_prefix("/reminders cancel ").map(str::trim) {
        let response = cancel_reminder(&database, message.chat.id.0, id_prefix).await?;
        bot.send_message(message.chat.id, response).await?;
        return Ok(());
    }
    if text == "/models" {
        let response = models_message(&state.read().await.config);
        bot.send_message(message.chat.id, response).await?;
        return Ok(());
    }
    if text == "/models refresh" {
        bot.send_chat_action(message.chat.id, ChatAction::Typing)
            .await?;
        let response = refresh_models(&state).await;
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
    if text == "/context" {
        let response = context_window_message(&state.read().await.config);
        bot.send_message(message.chat.id, response).await?;
        return Ok(());
    }
    if let Some(tokens) = text.strip_prefix("/context ").map(str::trim) {
        let response = set_context_window(&state, tokens).await;
        bot.send_message(message.chat.id, response).await?;
        return Ok(());
    }

    bot.send_chat_action(message.chat.id, ChatAction::Typing)
        .await?;
    let history = prepare_history(&state, &database, message.chat.id.0).await?;
    let user_message = ProviderMessage::user(text);
    deliver_agent_turn(
        &bot,
        message.chat.id,
        &state,
        &approvals,
        &questions,
        &database,
        history,
        user_message,
    )
    .await
}

/// Maximum size of a downloaded Telegram photo, matching Kamui's `@file` image cap: generous
/// enough for a phone photo, small enough to keep one request's payload reasonable.
const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;
const MAX_DOCUMENT_BYTES: u64 = 20 * 1024 * 1024;

/// Handle a message containing a photo: download the highest-resolution size Telegram sent,
/// attach it to a user message (with the caption as text, if any), and run it through the same
/// agent loop as a text message. Whether the active model can actually see the image is left to
/// the provider — if it rejects or ignores the attachment, that surfaces as a normal request
/// error rather than something Kumo tries to predict up front.
async fn handle_photo_message(
    bot: Bot,
    message: Message,
    state: Arc<RwLock<AppState>>,
    approvals: PendingApprovals,
    questions: PendingQuestions,
    database: Arc<Mutex<Database>>,
) -> Result<()> {
    // Telegram sends several resolutions of the same photo; the last is the largest.
    let photo = message
        .photo()
        .and_then(|sizes| sizes.last())
        .context("photo message unexpectedly had no sizes")?;
    let file = bot.get_file(photo.file.id.clone()).await?;
    if file.size > 0 && u64::from(file.size) > MAX_IMAGE_BYTES {
        bot.send_message(
            message.chat.id,
            format!(
                "That photo is too large ({} bytes); the limit is {MAX_IMAGE_BYTES} bytes.",
                file.size
            ),
        )
        .await?;
        return Ok(());
    }

    let mut bytes = Vec::new();
    bot.download_file(&file.path, &mut bytes).await?;
    let image = ImageAttachment {
        media_type: "image/jpeg".to_owned(), // Telegram always transcodes photos to JPEG.
        data: base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &bytes),
    };
    let caption = message.caption().unwrap_or_default();
    let user_message = ProviderMessage::user_with_images(caption, vec![image]);

    bot.send_chat_action(message.chat.id, ChatAction::Typing)
        .await?;
    let history = prepare_history(&state, &database, message.chat.id.0).await?;
    deliver_agent_turn(
        &bot,
        message.chat.id,
        &state,
        &approvals,
        &questions,
        &database,
        history,
        user_message,
    )
    .await
}

async fn handle_document_message(
    bot: Bot,
    message: Message,
    state: Arc<RwLock<AppState>>,
    approvals: PendingApprovals,
    questions: PendingQuestions,
    database: Arc<Mutex<Database>>,
) -> Result<()> {
    let document = message
        .document()
        .context("document message unexpectedly had no document")?;
    if document.file.size > 0 && u64::from(document.file.size) > MAX_DOCUMENT_BYTES {
        bot.send_message(
            message.chat.id,
            format!(
                "That document is too large ({} bytes); the limit is {MAX_DOCUMENT_BYTES} bytes.",
                document.file.size
            ),
        )
        .await?;
        return Ok(());
    }

    let original_name = document.file_name.as_deref().unwrap_or("upload.bin");
    let filename = std::path::Path::new(original_name)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("upload.bin");
    let extension = std::path::Path::new(filename)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !matches!(extension.as_str(), "csv" | "xlsx" | "xlsm") {
        bot.send_message(
            message.chat.id,
            "Only CSV, XLSX, and XLSM documents are currently supported.",
        )
        .await?;
        return Ok(());
    }

    let workspace = state
        .read()
        .await
        .config
        .tools
        .as_ref()
        .context("tools workspace is not configured")?
        .workspace
        .clone();
    let upload_dir = workspace.join("uploads");
    std::fs::create_dir_all(&upload_dir)?;
    let path = upload_dir.join(format!("{}-{filename}", Uuid::new_v4()));

    let file = bot.get_file(document.file.id.clone()).await?;
    let mut bytes = Vec::new();
    bot.download_file(&file.path, &mut bytes).await?;
    if bytes.len() as u64 > MAX_DOCUMENT_BYTES {
        bail!("downloaded document exceeds the {MAX_DOCUMENT_BYTES}-byte limit");
    }
    std::fs::write(&path, bytes)?;

    let caption = message.caption().unwrap_or_default();
    let instruction = if caption.is_empty() {
        "Inspect its columns and ask what analysis or chart they want if the request is unclear."
    } else {
        caption
    };
    let prompt = format!(
        "The user uploaded a data file at `{}`. {instruction}",
        path.display()
    );
    bot.send_chat_action(message.chat.id, ChatAction::Typing)
        .await?;
    let history = prepare_history(&state, &database, message.chat.id.0).await?;
    deliver_agent_turn(
        &bot,
        message.chat.id,
        &state,
        &approvals,
        &questions,
        &database,
        history,
        ProviderMessage::user(prompt),
    )
    .await
}

/// Shared tail of both the text and photo message paths: run the agent loop, deliver the answer
/// (or a generic failure notice) to the chat, and persist a successful turn.
#[allow(clippy::too_many_arguments)]
async fn deliver_agent_turn(
    bot: &Bot,
    chat_id: ChatId,
    state: &RwLock<AppState>,
    approvals: &PendingApprovals,
    questions: &PendingQuestions,
    database: &Mutex<Database>,
    history: storage::History,
    user_message: ProviderMessage,
) -> Result<()> {
    match run_agent(
        bot,
        chat_id,
        state,
        approvals,
        questions,
        database,
        history,
        user_message,
    )
    .await
    {
        Ok(turn) => {
            for chunk in message_chunks(&turn.answer, 4000) {
                send_formatted(bot, chat_id, &chunk).await?;
            }
            send_mcp_images(bot, chat_id, &turn.images).await?;
            database.lock().await.save_turn(
                chat_id.0,
                &turn.model,
                &turn.record,
                &turn.usage,
                &turn.finish_reason,
            )?;
        }
        Err(error) => {
            eprintln!("Model request failed: {error:#}");
            bot.send_message(
                chat_id,
                "The model provider could not answer. Check the Kumo terminal for details.",
            )
            .await?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
/// Runs `work` while holding Telegram's typing indicator up, so a turn that spends a minute in a
/// provider call or a shell command does not look like it stalled. Deliberately wrapped around
/// work only: an approval prompt or an `ask_user` question is Kumo waiting on the owner, and
/// showing "typing" while the owner is the one being waited on would be a lie.
async fn with_typing<T>(bot: &Bot, chat_id: ChatId, work: impl Future<Output = T>) -> T {
    tokio::pin!(work);
    loop {
        let _ = bot.send_chat_action(chat_id, ChatAction::Typing).await;
        tokio::select! {
            output = &mut work => return output,
            _ = tokio::time::sleep(TYPING_REFRESH) => {}
        }
    }
}

pub(crate) async fn run_agent(
    bot: &Bot,
    chat_id: ChatId,
    state: &RwLock<AppState>,
    approvals: &PendingApprovals,
    questions: &PendingQuestions,
    database: &Mutex<Database>,
    history: storage::History,
    user_message: ProviderMessage,
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
    let mut images = Vec::new();

    for _ in 0..MAX_TOOL_ROUNDS {
        let response =
            with_typing(bot, chat_id, provider.chat(&messages, &tool_definitions)).await?;
        accumulate_usage(&mut usage, &response.usage);
        if response.tool_calls.is_empty() {
            if response.content.trim().is_empty() {
                bail!("provider returned an empty response");
            }
            return Ok(finish_turn(
                user_message,
                trail,
                response.content,
                usage,
                response.finish_reason,
                model,
                images,
            ));
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
            let output = if call.name == "ask_user" {
                ask_user(bot, chat_id, questions, &call.arguments).await?
            } else {
                let tools = state.read().await.tools.clone();
                let always_allowed = tools.requires_confirmation(&call.name)
                    && database
                        .lock()
                        .await
                        .is_tool_always_allowed(chat_id.0, &call.name)?;
                if tools.requires_confirmation(&call.name) && !always_allowed {
                    match tools.preview(&call) {
                        Some(preview) => {
                            match request_approval(bot, chat_id, approvals, &preview).await? {
                                ApprovalOutcome::AllowOnce => {
                                    with_typing(bot, chat_id, tools.dispatch(chat_id.0, &call))
                                        .await
                                }
                                ApprovalOutcome::AlwaysAllow => {
                                    database
                                        .lock()
                                        .await
                                        .always_allow_tool(chat_id.0, &call.name)?;
                                    with_typing(bot, chat_id, tools.dispatch(chat_id.0, &call))
                                        .await
                                }
                                ApprovalOutcome::Deny => {
                                    "User denied this command. Do not run it.".to_owned()
                                }
                            }
                        }
                        None => "Error: invalid command arguments".to_owned(),
                    }
                } else {
                    with_typing(bot, chat_id, tools.dispatch(chat_id.0, &call)).await
                }
            };
            let (output, mut tool_images) = mcp::extract_media(output);
            images.append(&mut tool_images);
            let result_message = ProviderMessage::tool_result(call.id, output);
            messages.push(result_message.clone());
            trail.push(result_message);
        }
    }

    // The round cap is spent and the model is still asking for tools. Failing here would throw the
    // entire turn away — including commands the owner already approved and the results they
    // produced — and report it as a provider failure, which it is not. Ask once more with no tools
    // offered instead, so the model has to answer from what it already gathered.
    println!("Reached the {MAX_TOOL_ROUNDS}-round tool limit; asking for a final answer.");
    let response = with_typing(bot, chat_id, provider.chat(&messages, &[])).await?;
    accumulate_usage(&mut usage, &response.usage);
    if response.content.trim().is_empty() {
        bail!("model exceeded the {MAX_TOOL_ROUNDS}-round tool limit without answering");
    }

    Ok(finish_turn(
        user_message,
        trail,
        response.content,
        usage,
        // Recorded rather than the provider's own reason, so a turn cut short by Kumo's cap is
        // distinguishable in storage from one the model chose to end.
        TOOL_ROUND_LIMIT_FINISH_REASON.to_owned(),
        model,
        images,
    ))
}

/// Assembles the turn as it will be persisted: the user's message, everything the tool rounds
/// produced, then the answer.
fn finish_turn(
    user_message: ProviderMessage,
    mut trail: Vec<ProviderMessage>,
    answer: String,
    usage: Usage,
    finish_reason: String,
    model: String,
    images: Vec<mcp::McpImage>,
) -> AgentTurn {
    let mut record = Vec::with_capacity(trail.len() + 2);
    record.push(user_message);
    record.append(&mut trail);
    record.push(ProviderMessage::assistant(answer.clone()));
    AgentTurn {
        answer,
        record,
        usage,
        finish_reason,
        model,
        images,
    }
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
            state.config.provider()?.active_context_window(),
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
    let context_window = state.config.provider()?.active_context_window();
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
    match database.forget(argument)? {
        storage::MemoryMatch::One(_) => Ok(format!("Forgot the fact matching \"{argument}\".")),
        storage::MemoryMatch::None => Ok(format!(
            "No remembered fact contains \"{argument}\". Use /memory to see exact wording."
        )),
        storage::MemoryMatch::Ambiguous(entries) => {
            let mut message = format!(
                "\"{argument}\" matches {} remembered facts. Use text unique to one of them:",
                entries.len()
            );
            for entry in &entries {
                message.push_str(&format!("\n- {entry}"));
            }
            Ok(message)
        }
    }
}

/// `/reminders`: list this chat's pending scheduled tasks (one-shot and recurring), with the
/// unambiguous ID prefix `/reminders cancel <id>` accepts.
async fn reminders_message(
    database: &Mutex<Database>,
    state: &RwLock<AppState>,
    chat_id: i64,
) -> Result<String> {
    let tasks = database.lock().await.list_scheduled_tasks(chat_id)?;
    if tasks.is_empty() {
        return Ok("No scheduled reminders.".to_owned());
    }
    let timezone = state.read().await.config.timezone();

    let mut lines = vec!["Scheduled reminders:".to_owned()];
    for task in &tasks {
        let local_time = chrono::DateTime::from_timestamp(task.run_at, 0)
            .map(|value| {
                value
                    .with_timezone(&timezone)
                    .format("%Y-%m-%d %H:%M")
                    .to_string()
            })
            .unwrap_or_else(|| "unknown time".to_owned());
        let recurrence = match task.repeat_interval_seconds {
            Some(interval) => format!(" (repeats every {interval}s)"),
            None => String::new(),
        };
        lines.push(format!(
            "- {}  {local_time}{recurrence}  \"{}\"",
            &task.id[..8],
            truncate(&task.prompt, 60)
        ));
    }
    lines.push(String::new());
    lines.push("Use /reminders cancel <id> to cancel one.".to_owned());
    Ok(lines.join("\n"))
}

/// `/reminders cancel <id>`: cancel a pending reminder by an unambiguous ID prefix scoped to this
/// chat, the same resolution rule `/resume`/`/delete` use for sessions.
async fn cancel_reminder(
    database: &Mutex<Database>,
    chat_id: i64,
    id_prefix: &str,
) -> Result<String> {
    if id_prefix.is_empty() {
        return Ok("Usage: /reminders cancel <id>. Use /reminders to list them.".to_owned());
    }
    if database
        .lock()
        .await
        .cancel_scheduled_task(chat_id, id_prefix)?
    {
        Ok(format!("Cancelled reminder {id_prefix}."))
    } else {
        Ok(format!(
            "No reminder matches '{id_prefix}', or the prefix is ambiguous. Use /reminders to list them."
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
) -> Result<ApprovalOutcome> {
    let nonce = Uuid::new_v4().simple().to_string();
    let keyboard = InlineKeyboardMarkup::new([[
        InlineKeyboardButton::callback("Allow once", format!("approval:{nonce}:allow")),
        InlineKeyboardButton::callback("Always allow", format!("approval:{nonce}:always")),
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
            format!(
                "Kumo wants to run this host action:\n\n{preview}\n\n\"Always allow\" skips this \
                 prompt for this tool for the rest of the conversation, until /new."
            ),
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

    let outcome = match tokio::time::timeout(APPROVAL_TIMEOUT, receiver).await {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(_)) => ApprovalOutcome::Deny,
        Err(_) => {
            approvals.lock().await.remove(&nonce);
            ApprovalOutcome::Deny
        }
    };
    let _ = bot.edit_message_reply_markup(chat_id, prompt.id).await;
    Ok(outcome)
}

/// Dispatches a Telegram inline-button tap to whichever pending prompt it answers: an
/// `approval:<nonce>:<allow|deny>` callback from `request_approval`, or a
/// `question:<nonce>:<index>` callback from `ask_user`.
async fn handle_callback(
    bot: Bot,
    query: CallbackQuery,
    allowed_user_id: u64,
    approvals: PendingApprovals,
    questions: PendingQuestions,
) -> Result<()> {
    bot.answer_callback_query(query.id.clone()).await?;
    if query.from.id.0 != allowed_user_id {
        return Ok(());
    }
    let Some(data) = query.data.as_deref() else {
        return Ok(());
    };

    if let Some(decision) = parse_approval_callback(data) {
        if let Some(sender) = approvals.lock().await.remove(decision.nonce) {
            let _ = sender.send(decision.outcome);
        }
        return Ok(());
    }

    if let Some(answer) = parse_question_callback(data) {
        if let Some(pending) = questions.lock().await.remove(answer.nonce)
            && let Some(text) = pending.options.get(answer.option_index)
        {
            let _ = pending.sender.send(text.clone());
        }
    }
    Ok(())
}

struct ApprovalDecision<'a> {
    nonce: &'a str,
    outcome: ApprovalOutcome,
}

/// Parse an `approval:<nonce>:<allow|always|deny>` callback payload. Pure and side-effect-free so
/// it can be unit-tested without a real `Bot`/`CallbackQuery`.
fn parse_approval_callback(data: &str) -> Option<ApprovalDecision<'_>> {
    let rest = data.strip_prefix("approval:")?;
    let (nonce, decision) = rest.rsplit_once(':')?;
    let outcome = match decision {
        "allow" => ApprovalOutcome::AllowOnce,
        "always" => ApprovalOutcome::AlwaysAllow,
        "deny" => ApprovalOutcome::Deny,
        _ => return None,
    };
    Some(ApprovalDecision { nonce, outcome })
}

struct QuestionAnswer<'a> {
    nonce: &'a str,
    option_index: usize,
}

/// Parse a `question:<nonce>:<option index>` callback payload. Pure for the same reason as
/// `parse_approval_callback`.
fn parse_question_callback(data: &str) -> Option<QuestionAnswer<'_>> {
    let rest = data.strip_prefix("question:")?;
    let (nonce, index) = rest.split_once(':')?;
    let option_index = index.parse().ok()?;
    Some(QuestionAnswer {
        nonce,
        option_index,
    })
}

/// Maximum number of buttons `ask_user` offers before falling back to a plain text prompt (a
/// Telegram inline keyboard row is unwieldy past a handful of options, and the model is asked for
/// at most 4 anyway per its tool description).
const MAX_ASK_USER_OPTIONS: usize = 4;

/// Ask the user a clarifying question mid-turn and wait (bounded by `APPROVAL_TIMEOUT`, the same
/// window `request_approval` uses) for either a tapped option or a free-text reply, whichever
/// comes first. `handle_message` is what resolves a free-text reply — it checks `questions` for a
/// pending entry on this chat before treating incoming text as a new command or prompt.
async fn ask_user(
    bot: &Bot,
    chat_id: ChatId,
    questions: &PendingQuestions,
    arguments: &str,
) -> Result<String> {
    #[derive(serde::Deserialize)]
    struct Arguments {
        question: String,
        #[serde(default)]
        options: Vec<String>,
    }
    let arguments: Arguments = match serde_json::from_str(arguments) {
        Ok(arguments) => arguments,
        Err(error) => return Ok(format!("Error: invalid ask_user arguments: {error}")),
    };
    if arguments.question.trim().is_empty() {
        return Ok("Error: ask_user requires a non-empty 'question' argument".to_owned());
    }

    let nonce = Uuid::new_v4().simple().to_string();
    let options: Vec<String> = arguments
        .options
        .into_iter()
        .take(MAX_ASK_USER_OPTIONS)
        .collect();
    // The callback carries only the option's index, not its text: Telegram's callback data is
    // capped at 64 bytes with no escaping convention, which a long option label could easily
    // exceed. The actual text is looked back up from PendingQuestion::options when the tap or a
    // free-text reply resolves the question.
    let keyboard = (!options.is_empty()).then(|| {
        InlineKeyboardMarkup::new(options.iter().enumerate().map(|(index, option)| {
            [InlineKeyboardButton::callback(
                option,
                format!("question:{nonce}:{index}"),
            )]
        }))
    });

    let (sender, receiver) = oneshot::channel();
    questions.lock().await.insert(
        nonce.clone(),
        PendingQuestion {
            chat_id,
            options,
            sender,
        },
    );

    let mut send = bot.send_message(chat_id, arguments.question.clone());
    if let Some(keyboard) = keyboard {
        send = send.reply_markup(keyboard);
    }
    let prompt = match send.await {
        Ok(prompt) => prompt,
        Err(error) => {
            questions.lock().await.remove(&nonce);
            return Err(error).context("could not send ask_user prompt");
        }
    };

    let answer = match tokio::time::timeout(APPROVAL_TIMEOUT, receiver).await {
        Ok(Ok(answer)) => answer,
        Ok(Err(_)) => "The user did not answer.".to_owned(),
        Err(_) => {
            questions.lock().await.remove(&nonce);
            "The user did not answer in time.".to_owned()
        }
    };
    let _ = bot.edit_message_reply_markup(chat_id, prompt.id).await;
    Ok(answer)
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

pub(crate) async fn send_mcp_images(
    bot: &Bot,
    chat_id: ChatId,
    images: &[mcp::McpImage],
) -> Result<()> {
    for image in images {
        if !image.mime_type.starts_with("image/") {
            continue;
        }
        bot.send_photo(chat_id, InputFile::memory(image.data.clone()))
            .await?;
    }
    Ok(())
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
        let line = match provider.context_windows.get(model) {
            Some(window) => format!("\n{model} ({window} tokens)"),
            None => format!("\n{model}"),
        };
        if message.len() + line.len() > 3800 {
            message.push_str("\n\nList truncated.");
            break;
        }
        message.push_str(&line);
    }
    message.push_str("\n\nSwitch with /model <id>, or re-read the list with /models refresh.");
    message
}

/// Not every OpenAI-compatible provider reports a context window in its model listing, and Kumo's
/// fallback (48 KiB of history) is deliberately conservative — small enough to be safe against an
/// unknown model, small enough to compact away history a large model could still hold. `/context`
/// is the way to tell Kumo the real number when the provider will not.
fn context_window_message(config: &Config) -> String {
    let provider = config
        .provider
        .as_ref()
        .expect("provider is configured before gateway startup");
    match provider.active_context_window() {
        Some(window) => {
            let source = if provider
                .context_windows
                .contains_key(&provider.active_model)
            {
                "reported by the provider"
            } else {
                "set with /context"
            };
            format!(
                "Context window for {}: {window} tokens ({source}).\n\nHistory is compacted once \
                 it passes about half of that. Change it with /context <tokens>.",
                provider.active_model
            )
        }
        None => format!(
            "This provider reports no context window for {}, so Kumo compacts history using its \
             conservative default.\n\nSet the real one with /context <tokens>.",
            provider.active_model
        ),
    }
}

async fn set_context_window(state: &RwLock<AppState>, tokens: &str) -> String {
    let Ok(window) = tokens.replace([',', '_'], "").parse::<u64>() else {
        return format!(
            "Not a number of tokens: {tokens}\n\nUse /context <tokens>, e.g. /context 128000."
        );
    };
    if window < 4_000 {
        return "A context window under 4000 tokens would leave no room for a conversation."
            .to_owned();
    }

    let mut state = state.write().await;
    let Some(provider_config) = state.config.provider.as_mut() else {
        return "Model provider is not configured.".to_owned();
    };
    provider_config.context_window = Some(window);
    // A provider-reported window for this model would otherwise keep winning over the number the
    // owner just typed.
    let model = provider_config.active_model.clone();
    provider_config.context_windows.remove(&model);

    match state.config.save() {
        Ok(_) => format!("Context window set to {window} tokens."),
        Err(error) => {
            eprintln!("Could not save the context window: {error:#}");
            "Context window changed for this run, but Kumo could not save it.".to_owned()
        }
    }
}

/// Re-reads the provider's model listing into the config. Kumo otherwise caches whatever
/// onboarding saw, so a model added or withdrawn since then is invisible to `/models` and rejected
/// by `/model <id>` — including, if the listing has drifted, the model currently in use.
async fn refresh_models(state: &RwLock<AppState>) -> String {
    let (base_url, api_key) = {
        let state = state.read().await;
        let Some(provider) = state.config.provider.as_ref() else {
            return "Model provider is not configured.".to_owned();
        };
        (provider.base_url.clone(), provider.api_key.clone())
    };

    // Deliberately outside the lock: this is a network round trip, and holding the write lock
    // across it would stall every message and scheduled task until the provider answers.
    let listing = match provider::list_models(&base_url, &api_key).await {
        Ok(listing) => listing,
        Err(error) => return format!("Could not reach the provider: {error:#}"),
    };
    let count = listing.len();

    let mut state = state.write().await;
    let Some(provider_config) = state.config.provider.as_mut() else {
        return "Model provider is not configured.".to_owned();
    };
    let active_available = provider_config.apply_model_listing(listing);
    let active_model = provider_config.active_model.clone();
    let context_window = provider_config.active_context_window();

    let mut message = format!("Refreshed: {count} model(s) available.");
    if !active_available {
        message.push_str(&format!(
            "\n\nThe active model ({active_model}) is no longer offered by this provider. \
             Switch with /model <id>."
        ));
    }
    match context_window {
        Some(window) => {
            message.push_str(&format!("\n\nContext window: {window} tokens ({active_model})."))
        }
        None => message.push_str(
            "\n\nThis provider reports no context window, so compaction keeps using Kumo's default.",
        ),
    }
    if let Err(error) = state.config.save() {
        eprintln!("Could not save the refreshed model list: {error:#}");
        message.push_str("\n\nThe list is updated for this run, but Kumo could not save it.");
    }
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
        return format!(
            "Unknown model: {model}\n\nUse /models to see available models, or /models refresh \
             if the list is out of date."
        );
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
    Start,
    Stop,
    Restart,
    Onboard,
    Status,
    Doctor,
    Enable,
    Disable,
    Help,
    Version,
}

fn parse_command() -> Result<Command> {
    let mut args = std::env::args().skip(1);
    let command = match args.next().as_deref() {
        None => Command::Run,
        Some("run") => Command::Run,
        Some("start") => Command::Start,
        Some("stop") => Command::Stop,
        Some("restart") => Command::Restart,
        Some("onboard") => Command::Onboard,
        Some("status") => Command::Status,
        Some("doctor") => Command::Doctor,
        Some("enable") => Command::Enable,
        Some("disable") => Command::Disable,
        Some("-h" | "--help") => Command::Help,
        Some("-V" | "--version") => Command::Version,
        Some(value) => bail!("unknown command '{value}'\n\nRun `kumo --help` for usage."),
    };

    if args.next().is_some() {
        bail!("too many arguments\n\nRun `kumo --help` for usage.");
    }
    Ok(command)
}

/// `kumo status`: read configuration and the local database directly and print a summary,
/// without connecting to Telegram, the model provider, or any MCP server. Meant for checking on
/// Kumo from a terminal (e.g. over SSH) without needing to go through the bot itself.
async fn print_status() -> Result<()> {
    println!("Kumo v{}", env!("CARGO_PKG_VERSION"));
    println!();

    match daemon::running_pid()? {
        Some(pid) => println!("Process:   running in the background (pid {pid})"),
        None => println!("Process:   not running (use `kumo start` or `kumo run`)"),
    }

    match Config::exists()? {
        false => {
            println!("Config:    not set up yet (run `kumo` or `kumo onboard`)");
            return Ok(());
        }
        true => {
            let path = config::path()?;
            println!("Config:    {}", path.display());
            let config = Config::load()?;
            println!("Telegram:  connected as @{}", config.telegram.bot_username);
            match &config.provider {
                Some(provider) => println!(
                    "Provider:  {} ({})",
                    provider.active_model, provider.base_url
                ),
                None => println!("Provider:  not configured (run `kumo onboard`)"),
            }
            match &config.tools {
                Some(tools) => println!("Workspace: {}", tools.workspace.display()),
                None => println!("Workspace: not configured (run `kumo onboard`)"),
            }
            println!("Timezone:  {}", config.timezone());
            if config.mcp.is_empty() {
                println!("MCP:       none configured");
            } else {
                println!("MCP:       {} server(s) configured", config.mcp.len());
                for name in config.mcp.keys() {
                    println!("             - {name}");
                }
            }
        }
    }

    let database = Database::open()?;
    println!("Database:  {}", database.path().display());
    let summary = database.storage_summary()?;
    println!("Sessions:  {}", summary.session_count);
    println!(
        "Pending scheduled tasks: {}",
        summary.pending_scheduled_tasks
    );
    println!("Remembered facts:        {}", summary.memory_entries);

    Ok(())
}

/// `kumo doctor`: check configuration, provider connectivity, MCP servers, and optional
/// dependencies one at a time, printing a pass/fail line for each with actionable guidance on
/// failure, rather than kumo status's plain summary. Exits with an error if anything failed, so
/// it is usable as a pre-flight check in a script.
async fn run_doctor() -> Result<()> {
    println!("Kumo doctor");
    println!();
    let mut failures = 0usize;

    let config = match Config::exists() {
        Ok(true) => match Config::load() {
            Ok(config) => {
                check_ok("Config file parses");
                Some(config)
            }
            Err(error) => {
                check_fail(&format!("Config file is invalid: {error:#}"));
                failures += 1;
                None
            }
        },
        Ok(false) => {
            check_fail("No config file yet — run `kumo onboard` first");
            failures += 1;
            None
        }
        Err(error) => {
            check_fail(&format!("Could not locate the config directory: {error:#}"));
            failures += 1;
            None
        }
    };

    if let Some(config) = &config {
        match &config.provider {
            Some(provider_config) => {
                check_ok("Provider is configured");
                let provider = Provider::new(provider_config.clone());
                match provider.chat(&[ProviderMessage::user("ping")], &[]).await {
                    Ok(_) => check_ok("Provider responds to a test request"),
                    Err(error) => {
                        check_fail(&format!("Provider request failed: {error:#}"));
                        failures += 1;
                    }
                }
            }
            None => {
                check_fail("Provider is not configured — run `kumo onboard`");
                failures += 1;
            }
        }

        match &config.tools {
            Some(tools) => {
                if tools.workspace.is_dir() {
                    check_ok(&format!("Workspace exists: {}", tools.workspace.display()));
                } else {
                    check_fail(&format!(
                        "Workspace directory does not exist: {}",
                        tools.workspace.display()
                    ));
                    failures += 1;
                }
            }
            None => {
                check_fail("Workspace is not configured — run `kumo onboard`");
                failures += 1;
            }
        }

        if config.mcp.is_empty() {
            check_ok("No MCP servers configured (nothing to check)");
        } else {
            let mcp = mcp::connect_all(&config.mcp).await;
            for status in &mcp.statuses {
                match &status.error {
                    Some(error) => {
                        check_fail(&format!("MCP server '{}' failed: {error}", status.name));
                        failures += 1;
                    }
                    None => check_ok(&format!(
                        "MCP server '{}' connected ({} tool(s){})",
                        status.name,
                        status.tool_count,
                        status.trust_label()
                    )),
                }
            }
        }
    }

    if tools::kamui_available() {
        check_ok("kamui binary found on PATH (delegate_to_kamui is available)");
    } else {
        println!(
            "  i  kamui binary not found on PATH — delegate_to_kamui will not be offered to the model \
             (this is optional, not an error)"
        );
    }

    match Database::open() {
        Ok(_) => check_ok("Database opens successfully"),
        Err(error) => {
            check_fail(&format!("Database could not be opened: {error:#}"));
            failures += 1;
        }
    }

    println!();
    if failures == 0 {
        println!("All checks passed.");
        Ok(())
    } else {
        bail!("{failures} check(s) failed");
    }
}

fn check_ok(message: &str) {
    println!("  \u{2713} {message}");
}

fn check_fail(message: &str) {
    println!("  \u{2717} {message}");
}

fn print_help() {
    println!("Kumo personal agent gateway");
    println!();
    println!("Usage:");
    println!("  kumo            Run the gateway in the foreground (onboards on first run)");
    println!("  kumo run        Same as plain `kumo`, explicit for use as a service ExecStart");
    println!("  kumo start      Run the gateway detached in the background");
    println!("  kumo stop       Stop a background instance started with `kumo start`");
    println!("  kumo restart    Stop then start the background instance");
    println!("  kumo onboard    Configure the model provider and workspace");
    println!("  kumo status     Show configuration and storage status, no Telegram connection");
    println!("  kumo doctor     Check configuration, provider, MCP servers, and dependencies");
    println!("  kumo enable     Install as a user service that starts on login (Linux/macOS only)");
    println!("  kumo disable    Remove the user service installed by `kumo enable`");
    println!("  kumo --help     Show this help");
    println!("  kumo --version  Print the version");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_long_unicode_messages_without_corruption() {
        assert_eq!(message_chunks("abé日", 2), vec!["ab", "é日"]);
    }

    #[test]
    fn a_finished_turn_records_the_prompt_then_the_tool_trail_then_the_answer() {
        let turn = finish_turn(
            ProviderMessage::user("what is in the log?".to_owned()),
            vec![ProviderMessage::tool_result(
                "call-1".to_owned(),
                "42 errors".to_owned(),
            )],
            "There are 42 errors.".to_owned(),
            Usage::default(),
            TOOL_ROUND_LIMIT_FINISH_REASON.to_owned(),
            "model-a".to_owned(),
            Vec::new(),
        );

        assert_eq!(turn.record.len(), 3);
        assert_eq!(turn.record[0].content, "what is in the log?");
        assert_eq!(turn.record[1].content, "42 errors");
        assert_eq!(turn.record[2].content, "There are 42 errors.");
        assert_eq!(turn.answer, "There are 42 errors.");
        // A turn cut short by the round cap stays distinguishable once stored.
        assert_eq!(turn.finish_reason, "tool_round_limit");
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
        assert!(
            response.contains("No remembered fact contains"),
            "{response}"
        );
    }

    #[tokio::test]
    async fn forget_command_names_the_facts_an_ambiguous_text_matched() {
        let db = Database::open_in_memory_for_tests();
        db.remember("The user likes tea.").unwrap();
        db.remember("The user likes coffee.").unwrap();
        let database = Mutex::new(db);

        let response = forget_command(&database, "The user likes").await.unwrap();

        assert!(
            response.contains("matches 2 remembered facts"),
            "{response}"
        );
        assert!(response.contains("The user likes tea."), "{response}");
        assert!(response.contains("The user likes coffee."), "{response}");
        // Nothing is removed while the text is ambiguous.
        assert_eq!(database.lock().await.list_memory().unwrap().len(), 2);
    }

    #[test]
    fn parse_approval_callback_reads_allow_always_and_deny() {
        let allow = parse_approval_callback("approval:abc123:allow").unwrap();
        assert_eq!(allow.nonce, "abc123");
        assert_eq!(allow.outcome, ApprovalOutcome::AllowOnce);

        let always = parse_approval_callback("approval:abc123:always").unwrap();
        assert_eq!(always.outcome, ApprovalOutcome::AlwaysAllow);

        let deny = parse_approval_callback("approval:abc123:deny").unwrap();
        assert_eq!(deny.nonce, "abc123");
        assert_eq!(deny.outcome, ApprovalOutcome::Deny);
    }

    #[test]
    fn parse_approval_callback_rejects_malformed_or_unrelated_data() {
        assert!(parse_approval_callback("question:abc123:0").is_none());
        assert!(parse_approval_callback("approval:abc123:maybe").is_none());
        assert!(parse_approval_callback("not a callback at all").is_none());
    }

    #[test]
    fn parse_question_callback_reads_the_nonce_and_option_index() {
        let answer = parse_question_callback("question:abc123:2").unwrap();
        assert_eq!(answer.nonce, "abc123");
        assert_eq!(answer.option_index, 2);
    }

    #[test]
    fn parse_question_callback_rejects_malformed_or_unrelated_data() {
        assert!(parse_question_callback("approval:abc123:allow").is_none());
        assert!(parse_question_callback("question:abc123:not-a-number").is_none());
        assert!(parse_question_callback("question:abc123").is_none());
    }

    #[tokio::test]
    async fn a_tapped_option_resolves_to_its_text_via_the_pending_question() {
        let questions: PendingQuestions = Arc::new(Mutex::new(HashMap::new()));
        let (sender, receiver) = oneshot::channel();
        questions.lock().await.insert(
            "abc123".to_owned(),
            PendingQuestion {
                chat_id: ChatId(1),
                options: vec!["yes".to_owned(), "no".to_owned()],
                sender,
            },
        );

        let answer = parse_question_callback("question:abc123:1").unwrap();
        let pending = questions.lock().await.remove(answer.nonce).unwrap();
        let text = pending.options.get(answer.option_index).unwrap().clone();
        pending.sender.send(text).unwrap();

        assert_eq!(receiver.await.unwrap(), "no");
        assert!(questions.lock().await.is_empty());
    }

    #[test]
    fn an_out_of_range_option_index_parses_but_does_not_match_any_option() {
        // handle_callback only removes/answers a PendingQuestion once `options.get(index)`
        // succeeds, so a stale or malformed index (e.g. a button from an earlier, differently
        // sized question) leaves the entry untouched rather than panicking or answering wrongly.
        let answer = parse_question_callback("question:abc123:5").unwrap();
        let options = ["yes".to_owned()];
        assert!(options.get(answer.option_index).is_none());
    }
}
