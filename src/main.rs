mod commands;
mod compaction;
mod config;
mod daemon;
mod logging;
mod markdown;
mod mcp;
mod onboarding;
mod provider;
mod scheduler;
mod service;
mod storage;
mod tools;

use std::{
    collections::HashMap,
    future::Future,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use config::Config;
use provider::{ImageAttachment, Message as ProviderMessage, ModelProvider, Provider, Usage};
use storage::Database;
use teloxide::{
    dispatching::Dispatcher,
    net::Download,
    payloads::SendMessageSetters,
    prelude::*,
    types::{
        CallbackQuery, ChatAction, InlineKeyboardButton, InlineKeyboardMarkup, InputFile,
        MessageId, ParseMode,
    },
};
use tokio::sync::{Mutex, RwLock, oneshot};
use tools::ToolRegistry;
use uuid::Uuid;

pub(crate) struct AppState {
    config: Config,
    provider: Arc<dyn ModelProvider>,
    tools: ToolRegistry,
    mcp_statuses: Vec<String>,
    /// A frozen-at-startup rendering of every remembered fact, appended to the system prompt for
    /// every request. Frozen (rather than re-read per turn) so it stays consistent across a turn
    /// and does not defeat provider-side prompt caching; a `remember`/`update_memory`/`forget`
    /// call updates storage immediately but only appears in the prompt after Kumo restarts.
    memory_snapshot: String,
    /// Serialized size of every tool definition offered to the model. Fixed for the life of the
    /// process (MCP servers are connected once, at startup), so it is measured once here rather
    /// than re-serialized per turn, and counted as request overhead by compaction.
    tool_schema_bytes: usize,
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
const SYSTEM_PROMPT: &str = "You are Kumo, a personal assistant running on the user's host. Prefer a specialized MCP tool whenever one directly supports the requested capability; do not recreate that capability with delegate_to_kamui, run_command, or an ad hoc script. When an MCP tool returns image content, the gateway delivers it to the chat automatically, so describe the result briefly and never fabricate a Markdown download link to a local path. You may inspect the configured workspace with read-only tools. For multi-step workspace investigation that would consume several tool rounds, use delegate_readonly so only its final answer enters this conversation. You may request shell commands when needed, but every command requires explicit user approval before Kumo executes it. Set run_command background=true for builds, tests, or other commands likely to exceed 30 seconds; use command_status or stop_command to manage those jobs. Never claim a command ran unless its tool result confirms it. Use delegate_to_kamui only for genuine coding tasks that require reading or editing a codebase, not for generating an artifact already supported by an MCP tool. For coding tasks, prefer delegate_to_kamui over run_command because it runs a dedicated coding agent with a proper diff-reviewed file editor. When the user asks to stop, change, or remove a reminder, call list_scheduled_tasks and then cancel_scheduled_task: a scheduled task keeps running until it is cancelled, so acknowledging the request without cancelling leaves it firing. Never schedule a task in order to remember not to do something.";
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

/// Which part of the system a failed turn actually failed in. `run_agent` returns one
/// `anyhow::Error` for every one of these, and without the distinction `deliver_agent_turn` has to
/// guess — it used to guess "provider" every time, which sends the owner to check a service that
/// is fine while the real fault (a tool, the database, Telegram itself) goes unnamed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum FailureKind {
    /// The model or the service in front of it: a transport error, a rejected request, or an
    /// empty answer.
    Provider,
    /// A tool Kumo dispatched, including the read-only sub-agent.
    Tool,
    /// Kumo's own SQLite database.
    Storage,
    /// Sending or editing a Telegram message Kumo needed for the turn (an approval prompt, a
    /// question).
    Telegram,
    /// Anything else — a bug in Kumo, or a failure that carries no class.
    Internal,
}

impl FailureKind {
    /// The value logged next to the reference id, so the log line says which class was reported.
    fn as_str(self) -> &'static str {
        match self {
            Self::Provider => "provider",
            Self::Tool => "tool",
            Self::Storage => "storage",
            Self::Telegram => "telegram",
            Self::Internal => "internal",
        }
    }
}

/// Wraps a turn error with the class of thing that failed. Constructed at the point of failure
/// inside `run_agent` (where what failed is still known) and read back by `deliver_agent_turn`;
/// `run_agent` keeps returning a plain `anyhow::Error` so `scheduler.rs` is unaffected.
#[derive(Debug)]
pub(crate) struct TurnFailure {
    kind: FailureKind,
    source: anyhow::Error,
}

impl TurnFailure {
    pub(crate) fn new(kind: FailureKind, source: anyhow::Error) -> Self {
        Self { kind, source }
    }
}

impl std::fmt::Display for TurnFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{} failure: {}", self.kind.as_str(), self.source)
    }
}

impl std::error::Error for TurnFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}

/// Tags a fallible step inside the agent loop with the class of failure it can produce.
trait Classify<T> {
    fn classify(self, kind: FailureKind) -> Result<T>;
}

impl<T, E> Classify<T> for std::result::Result<T, E>
where
    E: Into<anyhow::Error>,
{
    fn classify(self, kind: FailureKind) -> Result<T> {
        self.map_err(|error| anyhow::Error::new(TurnFailure::new(kind, error.into())))
    }
}

/// The class an error was tagged with, or `Internal` for one that was never tagged. An untagged
/// error is deliberately *not* reported as a provider fault: an unclassified failure is a Kumo
/// problem until someone says otherwise.
fn failure_kind(error: &anyhow::Error) -> FailureKind {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<TurnFailure>())
        .map_or(FailureKind::Internal, |failure| failure.kind)
}

/// The sentence the owner gets for a failed turn. Deliberately built from the *class* alone: the
/// underlying error text carries file paths, base URLs, SQL and occasionally an API key, none of
/// which mean anything in a Telegram chat and all of which would be quoted back into it. The
/// reference id is the only handle into the log, exactly as before.
fn turn_failure_report(error: &anyhow::Error, reference: &str) -> String {
    let detail = match failure_kind(error) {
        FailureKind::Provider => "The model provider could not answer",
        FailureKind::Tool => "A tool failed while answering, so the turn was dropped",
        FailureKind::Storage => "Kumo could not reach its own database, so the turn was dropped",
        FailureKind::Telegram => "Kumo could not finish sending a message needed for this turn",
        FailureKind::Internal => "Kumo hit an internal error and could not finish this turn",
    };
    format!("{detail}. Check the Kumo log for reference {reference}.")
}

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
            // `provider()` rather than the `provider` field: a config that keeps its providers as
            // named entries leaves that field empty and is still fully configured.
            config.provider().is_err() || config.tools.is_none() || config.timezone.is_none()
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
    logging::info(
        "gateway",
        format!("status=starting version={}", env!("CARGO_PKG_VERSION")),
    );
    let bot = Bot::new(config.telegram.bot_token.clone());
    let allowed_user_id = config.telegram.owner_user_id;
    let provider: Arc<dyn ModelProvider> = Arc::new(Provider::new(config.provider()?.clone()));
    let workspace = config
        .tools
        .as_ref()
        .context("tools are not configured; run `kumo onboard`")?
        .workspace
        .clone();
    // A template named after a built-in can never be reached, and the owner has no reason to
    // suspect it: name the collision at startup, where the rest of the "what is loaded" reporting
    // already lives. Per-chat workspaces can hold their own templates, so this covers the default
    // workspace and the global directory; `/commands` reports whichever workspace is in use.
    match commands::list(&workspace) {
        Ok(set) => {
            for shadowed in set.shadowed {
                logging::warn(
                    "commands",
                    format!(
                        "template={} status=not_loaded reason=builtin_name command=/{}",
                        shadowed.path.display(),
                        shadowed.name
                    ),
                );
            }
        }
        Err(error) => logging::warn("commands", format!("status=unreadable error={error:#}")),
    }
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
            Some(error) => logging::warn(
                "mcp",
                format!("server={} status=failed error={error}", status.name),
            ),
            None => {
                let trust = match status.trusted_count {
                    0 => "approval_required".to_owned(),
                    count if count == status.tool_count => "trusted".to_owned(),
                    count => format!("partial({count}/{})", status.tool_count),
                };
                logging::info(
                    "mcp",
                    format!(
                        "server={} status=connected tools={} trust={}",
                        status.name, status.tool_count, trust
                    ),
                )
            }
        }
    }
    let database = Database::open()?;
    let reset_count = database.reset_stuck_running_tasks()?;
    if reset_count > 0 {
        logging::warn(
            "scheduler",
            format!("recovered_tasks={reset_count} reason=previous_shutdown"),
        );
    }
    let interrupted_jobs = database.fail_interrupted_command_jobs()?;
    if interrupted_jobs > 0 {
        logging::warn(
            "jobs",
            format!("failed_jobs={interrupted_jobs} reason=previous_shutdown"),
        );
    }
    let memory_snapshot = render_memory_snapshot(&database.list_memory()?);
    let database = Arc::new(Mutex::new(database));
    let rtk_enabled = config.tools.as_ref().is_some_and(|tools| tools.rtk);
    let background_max = config
        .tools
        .as_ref()
        .and_then(|tools| tools.background_max_secs)
        .map(Duration::from_secs);
    let mut tools = ToolRegistry::new(workspace, mcp.tools, database.clone(), config.timezone())?
        .with_rtk(rtk_enabled);
    if let Some(limit) = background_max {
        tools = tools.with_background_max(limit);
    }
    let turn_lock = Arc::new(Mutex::new(()));
    let approvals: PendingApprovals = Arc::new(Mutex::new(HashMap::new()));
    let questions: PendingQuestions = Arc::new(Mutex::new(HashMap::new()));
    let tool_schema_bytes = tools
        .definitions()
        .iter()
        .map(provider::ToolDefinition::payload_bytes)
        .sum();
    let state = Arc::new(RwLock::new(AppState {
        config,
        provider,
        tools,
        mcp_statuses,
        memory_snapshot,
        tool_schema_bytes,
    }));

    let current = state.read().await;
    logging::info(
        "gateway",
        format!(
            "status=listening bot=@{} model={} workspace={}",
            current.config.telegram.bot_username,
            current.provider.active_model(),
            current
                .config
                .tools
                .as_ref()
                .expect("tools are configured before gateway startup")
                .workspace
                .display()
        ),
    );
    drop(current);
    logging::info("gateway", "press Ctrl+C to stop");

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
    // Kept out of the dependency map, which takes ownership of the originals: shutdown needs both
    // to stop the scheduler without cutting a scheduled task in half.
    let shutdown_database = database.clone();
    let shutdown_turn_lock = turn_lock.clone();
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
            logging::info("gateway", "shutdown requested by Ctrl+C");
            approvals.lock().await.clear();
            questions.lock().await.clear();
            if let Ok(shutdown) = shutdown_token.shutdown() {
                shutdown.await;
            }
            dispatch_task.await.context("Telegram dispatcher task failed")?;
            logging::info("gateway", "status=stopped");
        }
    }
    stop_scheduler(
        scheduler_task,
        &shutdown_turn_lock,
        &shutdown_database,
        SCHEDULER_SHUTDOWN_GRACE,
    )
    .await;

    Ok(())
}

/// How long shutdown waits for a scheduled task that is already running to reach a terminal state
/// before it stops waiting. Bounded on purpose: a `Ctrl+C` that hangs is worse than a row this
/// process resets on its way out. Thirty seconds covers a provider call and a foreground command
/// (`tools::COMMAND_TIMEOUT` is 30 s too) but deliberately not a five-minute Kamui delegation; the
/// pending approvals and questions are cleared just before this, so nothing here is waiting on the
/// owner.
const SCHEDULER_SHUTDOWN_GRACE: Duration = Duration::from_secs(30);

/// What shutdown found when it went to stop the scheduler. Returned rather than only logged so the
/// behaviour is testable without a Telegram bot.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct SchedulerShutdown {
    /// False when the grace period ran out with a task still in flight.
    idle: bool,
    /// Rows left `running` that shutdown put back to `pending` itself.
    recovered: usize,
}

/// Stop the scheduler without leaving a scheduled task stranded.
///
/// `claim_due_scheduled_tasks` moves a due row to `running` and `complete_scheduled_task` moves it
/// out again; aborting the scheduler task between the two leaves the row `running` for the next
/// startup to recover, which made both `storage.rs`'s doc comment and the ROADMAP's claim — that
/// only a hard crash can do that — false. So: wait for the scheduler to be between tasks (it holds
/// `turn_lock` for the whole of each one), then abort, then put back anything still claimed. The
/// wait is bounded by `grace`; if it expires the abort happens anyway and the reset is what keeps
/// the promise, since the very narrow window between the claim and the first `turn_lock` acquire
/// is not covered by the lock either.
async fn stop_scheduler(
    scheduler_task: tokio::task::JoinHandle<()>,
    turn_lock: &Mutex<()>,
    database: &Mutex<Database>,
    grace: Duration,
) -> SchedulerShutdown {
    let guard = tokio::time::timeout(grace, turn_lock.lock()).await;
    let idle = guard.is_ok();
    if !idle {
        logging::warn(
            "scheduler",
            format!(
                "shutdown=timed_out after_secs={} action=abort_and_recover",
                grace.as_secs()
            ),
        );
    }
    scheduler_task.abort();
    // Held until after the abort: releasing it earlier would let the scheduler start the next due
    // task in the gap.
    drop(guard);

    let recovered = match database.lock().await.reset_stuck_running_tasks() {
        Ok(recovered) => recovered,
        Err(error) => {
            logging::error("scheduler", "shutdown=reset_failed", &error);
            0
        }
    };
    if recovered > 0 {
        logging::warn(
            "scheduler",
            format!("shutdown=recovered_tasks={recovered} status=pending"),
        );
    }
    let shutdown = SchedulerShutdown { idle, recovered };
    logging::info(
        "scheduler",
        format!(
            "status=stopped idle={} recovered={}",
            shutdown.idle, shutdown.recovered
        ),
    );
    shutdown
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
        logging::warn(
            "telegram",
            format!("event=unauthorized_message user_id={}", user.id.0),
        );
        return Ok(());
    }

    let message_kind = if message.photo().is_some() {
        "photo"
    } else if message.document().is_some() {
        "document"
    } else if message.text().is_some() {
        "text"
    } else {
        "unsupported"
    };
    logging::info(
        "telegram",
        format!(
            "event=message_received type={} chat_id={} user_id={} message_id={}",
            message_kind, message.chat.id, user.id.0, message.id
        ),
    );

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
    if let Some(argument) = text.strip_prefix("/memory edit ").map(str::trim) {
        let response = edit_memory_command(&database, argument).await?;
        bot.send_message(message.chat.id, response).await?;
        return Ok(());
    }
    if let Some(argument) = text.strip_prefix("/forget ").map(str::trim) {
        let response = forget_command(&database, argument).await?;
        bot.send_message(message.chat.id, response).await?;
        return Ok(());
    }
    if text == "/workspace" {
        let response = workspace_message(&state, &database, message.chat.id.0).await?;
        bot.send_message(message.chat.id, response).await?;
        return Ok(());
    }
    if let Some(argument) = text.strip_prefix("/workspace ").map(str::trim) {
        let response =
            set_workspace_command(&state, &database, message.chat.id.0, argument).await?;
        bot.send_message(message.chat.id, response).await?;
        return Ok(());
    }
    if text == "/audit" {
        let response = audit_message(&database, message.chat.id.0).await?;
        bot.send_message(message.chat.id, response).await?;
        return Ok(());
    }
    if text == "/jobs" {
        let response = jobs_message(&database, message.chat.id.0).await?;
        bot.send_message(message.chat.id, response).await?;
        return Ok(());
    }
    if let Some(id) = text.strip_prefix("/jobs stop ").map(str::trim) {
        let tools = state.read().await.tools.clone();
        let call = provider::ToolCall {
            id: "telegram-jobs".to_owned(),
            name: "stop_command".to_owned(),
            arguments: serde_json::json!({ "id": id }).to_string(),
        };
        let response = tools.dispatch(message.chat.id.0, &call).await;
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
    if text == "/providers" || text == "/provider" {
        let response = providers_message(&state.read().await.config);
        bot.send_message(message.chat.id, response).await?;
        return Ok(());
    }
    if let Some(name) = text.strip_prefix("/provider ").map(str::trim) {
        let response = switch_provider(&state, name).await;
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
    if text == "/rtk" {
        let enabled = state.read().await.tools.rtk_enabled();
        bot.send_message(
            message.chat.id,
            format!(
                "RTK command compression: {}. Use /rtk on or /rtk off.",
                if enabled { "on" } else { "off" }
            ),
        )
        .await?;
        return Ok(());
    }
    if let Some(value) = text.strip_prefix("/rtk ").map(str::trim) {
        let response = set_rtk(&state, value).await;
        bot.send_message(message.chat.id, response).await?;
        return Ok(());
    }

    let expanded = if text.starts_with('/') {
        let tools = state.read().await.tools.clone();
        let workspace = tools.workspace(message.chat.id.0).await?;
        if text == "/commands" {
            let response = commands_message(commands::list(&workspace)?);
            bot.send_message(message.chat.id, response).await?;
            return Ok(());
        }
        commands::expand(text, &workspace)?
    } else {
        None
    };
    let text = expanded.as_deref().unwrap_or(text);

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

/// The prompt an upload turns into, naming the file both ways.
///
/// The two kinds of reader need different spellings and the model cannot tell them apart from one
/// path. Kumo's own `read_file` requires a workspace-relative path and rejects an absolute one
/// outright (`ToolRegistry::resolve`), while an MCP server is a separate process with its own
/// working directory and can only use the absolute form. Handing over the absolute path alone is
/// what made the built-in reader unusable on an upload: the feature's most obvious next step
/// failed with "path must be relative to the workspace".
fn upload_prompt(workspace: &std::path::Path, path: &std::path::Path, instruction: &str) -> String {
    let relative = path
        .strip_prefix(workspace)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/");
    format!(
        "The user uploaded a data file at `{relative}` (relative to the workspace — use this path with read_file). Its absolute path, for tools that run outside the workspace, is `{}`. {instruction}",
        path.display()
    )
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

    let tools = state.read().await.tools.clone();
    let workspace = tools.workspace(message.chat.id.0).await?;
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
    let prompt = upload_prompt(&workspace, &path, instruction);
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
            let saved = database.lock().await.save_turn(
                chat_id.0,
                &turn.model,
                &turn.record,
                &turn.usage,
                &turn.finish_reason,
            );
            // The answer is already delivered, so a storage failure here is not "the turn failed";
            // it is "the turn happened and Kumo will not remember it", which is what the owner
            // needs to be told rather than a silent log line.
            if let Err(error) = saved {
                let reference = &Uuid::new_v4().to_string()[..8];
                logging::error(
                    "agent",
                    format!("request_id={reference} status=not_saved kind=storage"),
                    &error,
                );
                bot.send_message(
                    chat_id,
                    format!(
                        "That answer was delivered but could not be saved to Kumo's database, so \
                         it is not part of the conversation history. Check the Kumo log for \
                         reference {reference}."
                    ),
                )
                .await?;
            }
        }
        Err(error) => {
            let reference = &Uuid::new_v4().to_string()[..8];
            logging::error(
                "agent",
                format!(
                    "request_id={reference} status=failed kind={}",
                    failure_kind(&error).as_str()
                ),
                &error,
            );
            bot.send_message(chat_id, turn_failure_report(&error, reference))
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

#[allow(clippy::too_many_arguments)]
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
        let model_started = Instant::now();
        let response = with_typing(bot, chat_id, provider.chat(&messages, &tool_definitions))
            .await
            .classify(FailureKind::Provider)?;
        logging::info(
            "model",
            format!(
                "status=completed model={} duration_ms={} tool_calls={} finish_reason={}",
                model,
                model_started.elapsed().as_millis(),
                response.tool_calls.len(),
                response.finish_reason
            ),
        );
        accumulate_usage(&mut usage, &response.usage);
        if response.tool_calls.is_empty() {
            if response.content.trim().is_empty() {
                return Err(TurnFailure::new(
                    FailureKind::Provider,
                    anyhow::anyhow!("provider returned an empty response"),
                )
                .into());
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

        logging::info(
            "agent",
            format!("event=tool_round calls={}", response.tool_calls.len()),
        );
        let request_message =
            ProviderMessage::tool_request(response.content, response.tool_calls.clone());
        messages.push(request_message.clone());
        trail.push(request_message);
        for call in response.tool_calls {
            let tool_started = Instant::now();
            database
                .lock()
                .await
                .record_audit_event(
                    chat_id.0,
                    "tool_request",
                    Some(&call.name),
                    Some(&call.arguments),
                    "requested",
                )
                .classify(FailureKind::Storage)?;
            logging::info("tool", format!("name={} status=started", call.name));
            let progress = send_tool_progress(bot, chat_id, &call.name).await;
            let output = if call.name == "ask_user" {
                ask_user(bot, chat_id, questions, &call.arguments)
                    .await
                    .classify(FailureKind::Telegram)?
            } else if call.name == "delegate_readonly" {
                let tools = state.read().await.tools.clone();
                let subagent = with_typing(
                    bot,
                    chat_id,
                    run_readonly_subagent(provider.clone(), tools, chat_id.0, &call.arguments),
                )
                .await
                .classify(FailureKind::Tool)?;
                accumulate_usage(&mut usage, &subagent.usage);
                subagent.answer
            } else {
                let tools = state.read().await.tools.clone();
                let always_allowed = tools.requires_confirmation(&call.name)
                    && database
                        .lock()
                        .await
                        .is_tool_always_allowed(chat_id.0, &call.name)
                        .classify(FailureKind::Storage)?;
                if tools.requires_confirmation(&call.name) && !always_allowed {
                    match tools.preview(&call) {
                        Some(preview) => {
                            match request_approval(bot, chat_id, approvals, &preview)
                                .await
                                .classify(FailureKind::Telegram)?
                            {
                                ApprovalOutcome::AllowOnce => {
                                    database
                                        .lock()
                                        .await
                                        .record_audit_event(
                                            chat_id.0,
                                            "approval",
                                            Some(&call.name),
                                            None,
                                            "allow_once",
                                        )
                                        .classify(FailureKind::Storage)?;
                                    with_typing(bot, chat_id, tools.dispatch(chat_id.0, &call))
                                        .await
                                }
                                ApprovalOutcome::AlwaysAllow => {
                                    let database = database.lock().await;
                                    database
                                        .always_allow_tool(chat_id.0, &call.name)
                                        .classify(FailureKind::Storage)?;
                                    database
                                        .record_audit_event(
                                            chat_id.0,
                                            "approval",
                                            Some(&call.name),
                                            None,
                                            "always_allow",
                                        )
                                        .classify(FailureKind::Storage)?;
                                    drop(database);
                                    with_typing(bot, chat_id, tools.dispatch(chat_id.0, &call))
                                        .await
                                }
                                ApprovalOutcome::Deny => {
                                    database
                                        .lock()
                                        .await
                                        .record_audit_event(
                                            chat_id.0,
                                            "approval",
                                            Some(&call.name),
                                            None,
                                            "denied",
                                        )
                                        .classify(FailureKind::Storage)?;
                                    "User denied this command. Do not run it.".to_owned()
                                }
                            }
                        }
                        None => "Error: invalid command arguments".to_owned(),
                    }
                } else {
                    if always_allowed {
                        database
                            .lock()
                            .await
                            .record_audit_event(
                                chat_id.0,
                                "approval",
                                Some(&call.name),
                                None,
                                "standing_allow",
                            )
                            .classify(FailureKind::Storage)?;
                    }
                    with_typing(bot, chat_id, tools.dispatch(chat_id.0, &call)).await
                }
            };
            let (output, mut tool_images) = mcp::extract_media(output);
            let failed = output.starts_with("Error:") || output.starts_with("User denied");
            let result_details = serde_json::json!({
                "output_chars": output.chars().count(),
                "images": tool_images.len(),
            })
            .to_string();
            database
                .lock()
                .await
                .record_audit_event(
                    chat_id.0,
                    "tool_result",
                    Some(&call.name),
                    Some(&result_details),
                    if failed { "failed" } else { "completed" },
                )
                .classify(FailureKind::Storage)?;
            logging::info(
                "tool",
                format!(
                    "name={} status={} duration_ms={} images={}",
                    call.name,
                    if failed { "failed" } else { "completed" },
                    tool_started.elapsed().as_millis(),
                    tool_images.len()
                ),
            );
            finish_tool_progress(
                bot,
                chat_id,
                progress,
                &call.name,
                failed,
                tool_started.elapsed(),
            )
            .await;
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
    logging::warn(
        "agent",
        format!("tool_round_limit={MAX_TOOL_ROUNDS} action=request_final_answer"),
    );
    let response = with_typing(bot, chat_id, provider.chat(&messages, &[]))
        .await
        .classify(FailureKind::Provider)?;
    accumulate_usage(&mut usage, &response.usage);
    if response.content.trim().is_empty() {
        return Err(TurnFailure::new(
            FailureKind::Provider,
            anyhow::anyhow!(
                "model exceeded the {MAX_TOOL_ROUNDS}-round tool limit without answering"
            ),
        )
        .into());
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

async fn run_readonly_subagent(
    provider: Arc<dyn ModelProvider>,
    tools: ToolRegistry,
    chat_id: i64,
    arguments: &str,
) -> Result<ReadonlySubagentResult> {
    #[derive(serde::Deserialize)]
    struct Arguments {
        task: String,
    }

    let arguments: Arguments =
        serde_json::from_str(arguments).context("sub-agent arguments were not valid JSON")?;
    let task = arguments.task.trim();
    if task.is_empty() {
        bail!("delegate_readonly requires a non-empty task");
    }
    let definitions = tools.read_only_definitions();
    let mut messages = vec![
        ProviderMessage::system(
            "You are a read-only research sub-agent. Investigate the requested task using only \
             read_file and list_directory. Never claim to run commands or modify files. Return a \
             concise, self-contained answer with relevant workspace-relative paths.",
        ),
        ProviderMessage::user(task),
    ];
    let mut usage = Usage::default();
    for _ in 0..6 {
        let response = provider.chat(&messages, &definitions).await?;
        accumulate_usage(&mut usage, &response.usage);
        if response.tool_calls.is_empty() {
            if response.content.trim().is_empty() {
                bail!("read-only sub-agent returned an empty response");
            }
            return Ok(ReadonlySubagentResult {
                answer: response.content,
                usage,
            });
        }
        let request = ProviderMessage::tool_request(response.content, response.tool_calls.clone());
        messages.push(request);
        for call in response.tool_calls {
            let output = tools.dispatch_read_only(chat_id, &call).await;
            messages.push(ProviderMessage::tool_result(call.id, output));
        }
    }
    let response = provider.chat(&messages, &[]).await?;
    accumulate_usage(&mut usage, &response.usage);
    if response.content.trim().is_empty() {
        bail!("read-only sub-agent exhausted its tool limit without an answer");
    }
    Ok(ReadonlySubagentResult {
        answer: response.content,
        usage,
    })
}

struct ReadonlySubagentResult {
    answer: String,
    usage: Usage,
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
    let (provider, context_window, overhead) = {
        let state = state.read().await;
        // Everything a request carries besides the history itself. The system prompt, the memory
        // snapshot and the tool schemas are constant per process; the summary is not.
        let overhead = SYSTEM_PROMPT.len()
            + state.memory_snapshot.len()
            + state.tool_schema_bytes
            + history.summary.as_deref().map_or(0, str::len);
        (
            state.provider.clone(),
            state.config.provider()?.active_context_window(),
            overhead,
        )
    };
    if compaction::total_bytes(&history.messages)
        <= compaction::message_budget(context_window, overhead)
    {
        return Ok(history);
    }
    let Some(cutoff) = compaction::cutoff(&history.messages) else {
        return Ok(history);
    };

    logging::info("storage", format!("event=compaction messages={cutoff}"));
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
    let tools = state.tools.clone();
    let model = match state.config.active_provider_name() {
        Some(name) => format!("{} ({name})", state.provider.active_model()),
        None => state.provider.active_model().to_owned(),
    };
    let context_window = state.config.provider()?.active_context_window();
    let mcp = if state.mcp_statuses.is_empty() {
        "none".to_owned()
    } else {
        state.mcp_statuses.join("\n")
    };
    drop(state);
    let workspace = tools.workspace(chat_id).await?.display().to_string();

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
        "Model: {model}\nContext window: {}\nWorkspace: {workspace}\nRTK: {}\nSession: {session}\nMCP:\n{mcp}\nDatabase: {}",
        context_window.map_or_else(|| "default".to_owned(), |window| window.to_string()),
        if tools.rtk_enabled() { "on" } else { "off" },
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
        lines.push(format!("- #{} {}", entry.id, entry.content));
    }
    lines.push(String::new());
    lines.push(
        "Changes here take effect in conversation after Kumo restarts. Use /memory edit <id> <fact>, /forget <id>, or /forget all."
            .to_owned(),
    );
    Ok(lines.join("\n"))
}

/// `/forget all` clears every remembered fact; `/forget <text>` removes one matched by an
/// unambiguous substring, the same rule the `forget` tool uses.
async fn forget_command(database: &Mutex<Database>, argument: &str) -> Result<String> {
    if argument.is_empty() {
        return Ok(
            "Usage: /forget <id>, /forget <text>, or /forget all. Use /memory to list IDs."
                .to_owned(),
        );
    }
    let database = database.lock().await;
    if argument.eq_ignore_ascii_case("all") {
        let count = database.clear_memory()?;
        return Ok(format!("Forgot all {count} remembered fact(s)."));
    }
    if let Ok(id) = argument.trim_start_matches('#').parse::<i64>() {
        return Ok(if database.forget_memory_by_id(id)? {
            format!("Forgot memory #{id}.")
        } else {
            format!("Memory #{id} does not exist. Use /memory to list IDs.")
        });
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

async fn edit_memory_command(database: &Mutex<Database>, argument: &str) -> Result<String> {
    let Some((id, fact)) = argument.split_once(char::is_whitespace) else {
        return Ok("Usage: /memory edit <id> <fact>. Use /memory to list IDs.".to_owned());
    };
    let Ok(id) = id.trim_start_matches('#').parse::<i64>() else {
        return Ok("Memory ID must be a number shown by /memory.".to_owned());
    };
    let fact = fact.trim();
    if fact.is_empty() {
        return Ok("The replacement fact cannot be empty.".to_owned());
    }
    Ok(if database.lock().await.update_memory_by_id(id, fact)? {
        format!("Updated memory #{id}. Restart Kumo for it to affect conversations.")
    } else {
        format!("Memory #{id} does not exist. Use /memory to list IDs.")
    })
}

async fn workspace_message(
    state: &RwLock<AppState>,
    database: &Mutex<Database>,
    chat_id: i64,
) -> Result<String> {
    let override_path = database.lock().await.workspace_for_chat(chat_id)?;
    let tools = state.read().await.tools.clone();
    let active = tools.workspace(chat_id).await?;
    let source = if override_path.is_some() {
        "chat override"
    } else {
        "default"
    };
    Ok(format!(
        "Workspace ({source}): {}\n\nUse /workspace <path> to change it or /workspace reset to use the default.",
        active.display()
    ))
}

async fn set_workspace_command(
    state: &RwLock<AppState>,
    database: &Mutex<Database>,
    chat_id: i64,
    argument: &str,
) -> Result<String> {
    if argument.eq_ignore_ascii_case("reset") {
        database.lock().await.clear_workspace_for_chat(chat_id)?;
        let tools = state.read().await.tools.clone();
        let active = tools.workspace(chat_id).await?;
        return Ok(format!("Workspace reset to default: {}", active.display()));
    }
    let path = match std::path::PathBuf::from(argument).canonicalize() {
        Ok(path) => path,
        Err(_) => return Ok(format!("Workspace does not exist: {argument}")),
    };
    if !path.is_dir() {
        return Ok(format!("Workspace is not a directory: {}", path.display()));
    }
    database
        .lock()
        .await
        .set_workspace_for_chat(chat_id, &path)?;
    Ok(format!("Workspace for this chat: {}", path.display()))
}

/// The `/commands` reply. A template whose name collides with a built-in is named here rather than
/// omitted: the file exists on disk and the owner expects it to work, so "it is not loaded, and
/// this is why" is the only answer that tells them what to do about it.
fn commands_message(set: commands::CommandSet) -> String {
    let mut lines = Vec::new();
    if set.templates.is_empty() {
        lines.push("No custom commands found.".to_owned());
    } else {
        lines.push("Custom commands:".to_owned());
        for template in set.templates {
            let description = template
                .description
                .map(|value| format!(" — {value}"))
                .unwrap_or_default();
            lines.push(format!("- /{}{}", template.name, description));
        }
    }
    if !set.shadowed.is_empty() {
        lines.push(String::new());
        lines.push("Not loaded — these names are built-in Kumo commands:".to_owned());
        for shadowed in set.shadowed {
            lines.push(format!(
                "- {} would answer to /{}, which is built in. Rename the file to use it.",
                shadowed.path.file_name().map_or_else(
                    || shadowed.name.clone(),
                    |name| name.to_string_lossy().into_owned()
                ),
                shadowed.name
            ));
        }
    }
    lines.join("\n")
}

async fn audit_message(database: &Mutex<Database>, chat_id: i64) -> Result<String> {
    let events = database.lock().await.list_audit_events(chat_id, 20)?;
    if events.is_empty() {
        return Ok("No audit events for this chat yet.".to_owned());
    }
    let mut lines = vec!["Recent audit events:".to_owned()];
    for event in events {
        let at = chrono::DateTime::from_timestamp(event.created_at, 0)
            .map(|time| time.format("%Y-%m-%d %H:%M:%S UTC").to_string())
            .unwrap_or_else(|| event.created_at.to_string());
        let tool = event
            .tool_name
            .map(|name| format!(" {name}"))
            .unwrap_or_default();
        lines.push(format!(
            "- #{} {at} {}{tool}: {}",
            event.id, event.event_type, event.outcome
        ));
    }
    Ok(lines.join("\n"))
}

async fn jobs_message(database: &Mutex<Database>, chat_id: i64) -> Result<String> {
    let jobs = database.lock().await.list_command_jobs(chat_id, 20)?;
    if jobs.is_empty() {
        return Ok("No background jobs for this chat yet.".to_owned());
    }
    let mut lines = vec!["Background jobs:".to_owned()];
    for job in jobs {
        let command = truncate(&job.command.replace('\n', " "), 80);
        let created = format_timestamp(job.created_at);
        lines.push(format!(
            "- {} [{}] {created} — {command}",
            &job.id[..8],
            job.status
        ));
    }
    lines.push(String::new());
    lines.push("Use /jobs stop <id> to stop a running job.".to_owned());
    Ok(lines.join("\n"))
}

async fn set_rtk(state: &RwLock<AppState>, value: &str) -> String {
    let enabled = match value.to_ascii_lowercase().as_str() {
        "on" => true,
        "off" => false,
        _ => return "Usage: /rtk on or /rtk off.".to_owned(),
    };
    let mut state = state.write().await;
    let Some(tools_config) = state.config.tools.as_mut() else {
        return "Tools are not configured.".to_owned();
    };
    tools_config.rtk = enabled;
    state.tools = state.tools.clone().with_rtk(enabled);
    match state.config.save() {
        Ok(_) => format!(
            "RTK command compression {}.",
            if enabled { "enabled" } else { "disabled" }
        ),
        Err(error) => {
            logging::error("config", "could not save RTK setting", &error);
            "RTK setting changed for this run, but Kumo could not save it.".to_owned()
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
            logging::warn(
                "telegram",
                format!("markdown_rejected=true fallback=plain_text error={error:#}"),
            );
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
            logging::warn(
                "telegram",
                format!("image_skipped=true mime_type={}", image.mime_type),
            );
            continue;
        }
        let started = Instant::now();
        bot.send_photo(chat_id, InputFile::memory(image.data.clone()))
            .await?;
        logging::info(
            "telegram",
            format!(
                "event=image_sent chat_id={} mime_type={} size_bytes={} duration_ms={}",
                chat_id,
                image.mime_type,
                image.data.len(),
                started.elapsed().as_millis()
            ),
        );
    }
    Ok(())
}

fn user_facing_tool_name(name: &str) -> String {
    name.rsplit("__").next().unwrap_or(name).replace('_', " ")
}

fn should_show_tool_progress(name: &str) -> bool {
    name.contains("__") || matches!(name, "delegate_to_kamui" | "run_command")
}

async fn send_tool_progress(bot: &Bot, chat_id: ChatId, name: &str) -> Option<MessageId> {
    if !should_show_tool_progress(name) {
        return None;
    }
    let label = user_facing_tool_name(name);
    match bot
        .send_message(chat_id, format!("🛠️ Sedang menjalankan {label}..."))
        .await
    {
        Ok(message) => Some(message.id),
        Err(error) => {
            logging::warn(
                "telegram",
                format!("tool_progress=send_failed tool={name} error={error:#}"),
            );
            None
        }
    }
}

async fn finish_tool_progress(
    bot: &Bot,
    chat_id: ChatId,
    message_id: Option<MessageId>,
    name: &str,
    failed: bool,
    elapsed: Duration,
) {
    let Some(message_id) = message_id else {
        return;
    };
    let label = user_facing_tool_name(name);
    let icon = if failed { "❌" } else { "✅" };
    let status = if failed { "gagal" } else { "selesai" };
    let seconds = elapsed.as_secs_f64();
    if let Err(error) = bot
        .edit_message_text(
            chat_id,
            message_id,
            format!("{icon} {label} {status} dalam {seconds:.1} detik."),
        )
        .await
    {
        logging::warn(
            "telegram",
            format!("tool_progress=edit_failed tool={name} error={error:#}"),
        );
    }
}

/// The fenced-code-block marker, in the raw Markdown a chunk is cut out of and in the MarkdownV2
/// `markdown::to_telegram_markdown_v2` produces from it.
const CODE_FENCE: &str = "```";

/// Split `message` into chunks of at most `max_chars` characters, preferring boundaries that leave
/// every chunk valid Markdown on its own.
///
/// Chunking happens on the model's *raw* Markdown: `deliver_agent_turn` chunks first, and
/// `send_formatted` then converts each chunk with `markdown::to_telegram_markdown_v2` and sends it
/// with `ParseMode::MarkdownV2`. So a boundary in the wrong place does not merely split a rendered
/// message, it changes what the converter is handed — the tail of a fenced block arrives with no
/// opening fence and is escaped as prose, and half a `[label](url)` becomes literal text. Order of
/// preference:
///
/// 1. a line boundary outside any code fence,
/// 2. a line boundary inside a code fence, closing the fence at the end of the chunk and
///    re-opening it (with its original info string) at the start of the next,
/// 3. a space within a line at a point where no inline entity is open,
/// 4. a character boundary — reached only by a single unbroken token longer than the limit.
///
/// Only the last of those can still cut an entity, and `send_formatted`'s plain-text fallback is
/// still the backstop for it. Fence tracking is line-based, so the rare fence written mid-line is
/// not seen; the converter pairs those differently and the fallback covers the disagreement.
pub(crate) fn message_chunks(message: &str, max_chars: usize) -> Vec<String> {
    if message.is_empty() {
        return Vec::new();
    }
    let mut writer = ChunkWriter::new(max_chars);
    // `split_inclusive` keeps each line's own newline, so chunks rejoin into the original text
    // apart from the fence markers re-opening deliberately adds.
    for line in message.split_inclusive('\n') {
        writer.write_line(line);
    }
    writer.finish()
}

/// Accumulates lines into chunks for `message_chunks`, keeping track of the code fence a chunk
/// boundary would otherwise leave hanging open.
struct ChunkWriter {
    max_chars: usize,
    chunks: Vec<String>,
    current: String,
    /// The opening fence line (with its info string, without its newline) of the block being
    /// written, if any. `Some` means a chunk that ends here has to close the fence and the next
    /// one has to re-open it.
    fence: Option<String>,
    /// Characters of `current` that are a re-opened fence rather than message content, so an
    /// only-a-fence chunk is never emitted and never counts as progress.
    seed: usize,
}

impl ChunkWriter {
    fn new(max_chars: usize) -> Self {
        Self {
            max_chars: max_chars.max(1),
            chunks: Vec::new(),
            current: String::new(),
            fence: None,
            seed: 0,
        }
    }

    /// Characters still available in this chunk, holding back room for the closing fence a split
    /// inside a code block has to append.
    fn room(&self) -> usize {
        let reserved = if self.fence.is_some() {
            CODE_FENCE.chars().count() + 1
        } else {
            0
        };
        self.max_chars
            .saturating_sub(reserved)
            .saturating_sub(self.current.chars().count())
    }

    fn has_content(&self) -> bool {
        self.current.chars().count() > self.seed
    }

    /// Emit the current chunk, closing an open fence, and seed the next chunk by re-opening it.
    fn flush(&mut self) {
        if !self.has_content() {
            return;
        }
        let mut chunk = std::mem::take(&mut self.current);
        self.seed = 0;
        if let Some(fence) = self.fence.clone() {
            if !chunk.ends_with('\n') {
                chunk.push('\n');
            }
            chunk.push_str(CODE_FENCE);
            self.current.push_str(&fence);
            self.current.push('\n');
            self.seed = self.current.chars().count();
        }
        self.chunks.push(chunk);
    }

    fn write_line(&mut self, line: &str) {
        let is_fence = line.trim_start().starts_with(CODE_FENCE);
        if is_fence && self.fence.is_some() {
            // Leave the block *before* writing the closing fence: `room` has been holding exactly
            // this many characters back for it, so it always fits and never triggers a split that
            // would strand the block open.
            self.fence = None;
            let newline = if line.ends_with('\n') { "\n" } else { "" };
            self.write_text(&format!("{CODE_FENCE}{newline}"));
            return;
        }
        self.write_text(line);
        if is_fence {
            self.fence = Some(line.trim_end_matches('\n').to_owned());
        }
    }

    /// Append `text` to the current chunk, flushing (and, inside a fence, re-opening) as often as
    /// the limit requires.
    fn write_text(&mut self, text: &str) {
        let mut rest = text;
        while !rest.is_empty() {
            let room = self.room();
            if rest.chars().count() <= room {
                self.current.push_str(rest);
                return;
            }
            if self.has_content() {
                self.flush();
                continue;
            }
            // A single line longer than a whole chunk: split inside it, at the least damaging
            // point available. `max(1)` keeps the loop making progress even for a limit so small
            // that the re-opened fence alone fills it.
            let take = safe_break(rest, room).max(1);
            let split = rest
                .char_indices()
                .nth(take)
                .map_or(rest.len(), |(index, _)| index);
            let (head, tail) = rest.split_at(split);
            self.current.push_str(head);
            self.flush();
            rest = tail;
        }
    }

    fn finish(mut self) -> Vec<String> {
        if self.has_content() {
            self.chunks.push(std::mem::take(&mut self.current));
        }
        self.chunks
    }
}

/// How many characters of `text` to keep when the whole of it does not fit in `limit`. Prefers the
/// last space at which no inline code span, `**bold**` run or `[label](url)` link is open, so the
/// piece carried into the next chunk is not the tail of a half-written entity; falls back to the
/// last space of any kind, and then to `limit` itself for an unbroken token.
fn safe_break(text: &str, limit: usize) -> usize {
    let characters: Vec<char> = text.chars().collect();
    let limit = limit.min(characters.len());
    let mut code = false;
    let mut bold = false;
    // 0: outside a link, 1: inside the `[label]`, 2: inside the `(url)`.
    let mut link = 0u8;
    let mut last_space = None;
    let mut last_open_space = None;
    let mut index = 0;
    while index < limit {
        let character = characters[index];
        if character == '`' {
            code = !code;
            index += 1;
            continue;
        }
        if !code {
            if character == '*' && characters.get(index + 1) == Some(&'*') {
                bold = !bold;
                index += 2;
                continue;
            }
            match link {
                0 if character == '[' => link = 1,
                1 if character == ']' && characters.get(index + 1) == Some(&'(') => {
                    link = 2;
                    index += 2;
                    continue;
                }
                // A `[` that never becomes a link (a citation marker, a list of keys) must not
                // make the whole rest of the message unsplittable.
                1 if character == '\n' => link = 0,
                2 if character == ')' => link = 0,
                _ => {}
            }
        }
        if character == ' ' {
            last_space = Some(index + 1);
            if !code && !bold && link == 0 {
                last_open_space = Some(index + 1);
            }
        }
        index += 1;
    }
    last_open_space.or(last_space).unwrap_or(limit)
}

fn models_message(config: &Config) -> String {
    // `config.provider()` resolves the *active* entry; the `provider` field alone is `None` as
    // soon as onboarding migrates a second provider into `[providers.*]`, which is why reading
    // the field here used to panic on exactly the installs that had more than one to list.
    let provider = match config.provider() {
        Ok(provider) => provider,
        Err(error) => return error.to_string(),
    };
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

fn providers_message(config: &Config) -> String {
    if config.providers.is_empty() {
        return "One provider is configured, without a name.\n\nRun `kumo onboard` on the host to \
                add a second one: Kumo keeps this one under a name of its own, and /provider \
                switches between them from here."
            .to_owned();
    }

    let active = config.active_provider_name();
    let mut message = "Configured providers:\n".to_owned();
    for (name, provider) in &config.providers {
        let marker = if Some(name) == active.as_ref() {
            " (active)"
        } else {
            ""
        };
        message.push_str(&format!("\n{name}{marker} — {}", provider.active_model));
    }
    message.push_str("\n\nSwitch with /provider <name>.");
    message
}

/// Switching provider swaps the model, the credentials, and the context budget together, so the
/// live `Provider` is rebuilt from whichever entry is now active.
async fn switch_provider(state: &RwLock<AppState>, name: &str) -> String {
    let mut state = state.write().await;
    if state.config.providers.is_empty() {
        return "Only one provider is configured, so there is nothing to switch to.".to_owned();
    }
    let Some(provider_config) = state.config.providers.get(name).cloned() else {
        return format!("Unknown provider: {name}\n\nUse /providers to see the configured ones.");
    };

    state.config.active_provider = Some(name.to_owned());
    state.provider = Arc::new(Provider::new(provider_config.clone()));
    let model = provider_config.active_model;
    match state.config.save() {
        Ok(_) => format!("Switched to {name} ({model})."),
        Err(error) => {
            logging::error("config", "could not save provider selection", &error);
            format!("Switched to {name} for this run, but Kumo could not save the selection.")
        }
    }
}

/// Not every OpenAI-compatible provider reports a context window in its model listing, and Kumo's
/// fallback (48 KiB of history) is deliberately conservative — small enough to be safe against an
/// unknown model, small enough to compact away history a large model could still hold. `/context`
/// is the way to tell Kumo the real number when the provider will not.
fn context_window_message(config: &Config) -> String {
    // Same resolution as `models_message` — and the same reason. See the note there.
    let provider = match config.provider() {
        Ok(provider) => provider,
        Err(error) => return error.to_string(),
    };
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
    let Some(provider_config) = state.config.provider_mut() else {
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
            logging::error("config", "could not save context window", &error);
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
        // The active entry, not the flat block — the same distinction that used to make
        // `/models` panic. Reading the field here reported "not configured" on an install that
        // has several providers, which is the opposite of the truth.
        let provider = match state.config.provider() {
            Ok(provider) => provider,
            Err(error) => return error.to_string(),
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
    let Some(provider_config) = state.config.provider_mut() else {
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
        logging::error("config", "could not save refreshed model list", &error);
        message.push_str("\n\nThe list is updated for this run, but Kumo could not save it.");
    }
    message
}

async fn switch_model(state: &RwLock<AppState>, model: &str) -> String {
    let mut state = state.write().await;
    let Some(provider_config) = state.config.provider_mut() else {
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
    state.provider = Arc::new(Provider::new(provider_config.clone()));
    match state.config.save() {
        Ok(_) => format!("Switched to {model}."),
        Err(error) => {
            logging::error("config", "could not save model selection", &error);
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
            match config.provider() {
                Ok(provider) => {
                    let name = config
                        .active_provider_name()
                        .map(|name| format!("{name}: "))
                        .unwrap_or_default();
                    println!(
                        "Provider:  {name}{} ({})",
                        provider.active_model, provider.base_url
                    );
                    if config.providers.len() > 1 {
                        println!(
                            "           {} configured, switch with /provider",
                            config.providers.len()
                        );
                    }
                }
                Err(_) => println!("Provider:  not configured (run `kumo onboard`)"),
            }
            match &config.tools {
                Some(tools) => {
                    println!("Workspace: {}", tools.workspace.display());
                    println!("RTK:       {}", if tools.rtk { "on" } else { "off" });
                }
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
    println!("Audit events:            {}", summary.audit_events);
    println!("Running background jobs: {}", summary.running_jobs);

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
                if tools.rtk {
                    let available = std::process::Command::new("rtk")
                        .arg("--version")
                        .stdin(std::process::Stdio::null())
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .status()
                        .is_ok_and(|status| status.success());
                    if available {
                        check_ok("RTK command backend is available");
                    } else {
                        check_fail("RTK is enabled but the `rtk` binary is not on PATH");
                        failures += 1;
                    }
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
    println!("  kumo enable     Install as a user service that starts on login");
    println!("  kumo disable    Remove the user service installed by `kumo enable`");
    println!("  kumo --help     Show this help");
    println!("  kumo --version  Print the version");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An upload used to be announced by its absolute path alone, which `read_file` refuses —
    /// so the tool most likely to be reached for next could not open the file just delivered.
    #[test]
    fn an_upload_is_announced_by_a_path_read_file_will_accept() {
        let workspace = std::path::Path::new("/srv/kumo/workspace");
        let path = workspace.join("uploads").join("abc-sales.csv");
        let prompt = upload_prompt(workspace, &path, "Summarise it.");

        assert!(
            prompt.contains("`uploads/abc-sales.csv`"),
            "the workspace-relative path must be offered: {prompt}"
        );
        assert!(
            !prompt.contains("`/srv/kumo/workspace/uploads/abc-sales.csv` (relative"),
            "the relative slot must not be filled with an absolute path: {prompt}"
        );
        assert!(
            prompt.contains(&path.display().to_string()),
            "the absolute path still has to reach tools outside the workspace: {prompt}"
        );
        assert!(
            prompt.ends_with("Summarise it."),
            "the caption survives: {prompt}"
        );
    }

    #[test]
    fn splits_long_unicode_messages_without_corruption() {
        assert_eq!(message_chunks("abé日", 2), vec!["ab", "é日"]);
    }

    /// Every chunk is converted and sent on its own, so a chunk that ends inside a fenced block
    /// hands `markdown::to_telegram_markdown_v2` an opening fence with no close (and the next
    /// chunk a body with no opening fence). Splitting has to close and re-open the block instead.
    #[test]
    fn a_split_inside_a_code_fence_closes_and_reopens_it() {
        let code: String = (0..40)
            .map(|line| format!("let value_{line} = compute({line});\n"))
            .collect();
        let message = format!("Here is the code:\n\n```rust\n{code}```\n\nThat is all.");

        let chunks = message_chunks(&message, 400);

        assert!(chunks.len() > 1, "the fixture has to actually split");
        for chunk in &chunks {
            assert!(
                chunk.matches("```").count() % 2 == 0,
                "a chunk must not leave a fence open: {chunk:?}"
            );
            assert!(
                chunk.chars().count() <= 400,
                "chunk over the limit: {chunk:?}"
            );
        }
        // Every line of code still arrives, and the re-opened fences carry the info string.
        for line in code.lines() {
            assert!(
                chunks.iter().any(|chunk| chunk.contains(line)),
                "lost code line {line:?}"
            );
        }
        assert!(
            chunks
                .iter()
                .skip(1)
                .any(|chunk| chunk.starts_with("```rust")),
            "a continued block has to re-open with its own info string: {chunks:?}"
        );
    }

    /// The plain-prose case: a boundary belongs at a line break, not in the middle of a sentence.
    #[test]
    fn prose_is_split_at_line_boundaries() {
        let message: String = (0..60)
            .map(|line| format!("Line number {line}.\n"))
            .collect();

        let chunks = message_chunks(&message, 200);

        assert!(chunks.len() > 1);
        for chunk in chunks.iter().take(chunks.len() - 1) {
            assert!(chunk.ends_with('\n'), "chunk cut mid-line: {chunk:?}");
        }
        assert_eq!(chunks.concat(), message, "no content added or lost");
    }

    /// A link cut in half stops being a link: `[label](htt` renders as literal text and the rest
    /// of the URL lands in the next message.
    #[test]
    fn a_link_is_never_cut_in_half() {
        // The padding puts the link across the 240-character mark, so a plain count-to-the-limit
        // split lands inside its URL.
        let message = format!(
            "{}[the docs](https://example.com/a/very/long/path/to/documentation) and more text",
            "padding ".repeat(24)
        );

        let chunks = message_chunks(&message, 120);

        assert!(chunks.len() > 1);
        for chunk in &chunks {
            assert_eq!(
                chunk.matches('[').count(),
                chunk.matches(']').count(),
                "half a link in {chunk:?}"
            );
            assert!(
                !chunk.contains("](") || chunk.contains(')'),
                "a link target was cut: {chunk:?}"
            );
        }
        assert!(
            chunks.iter().any(|chunk| chunk
                .contains("[the docs](https://example.com/a/very/long/path/to/documentation)")),
            "the link has to survive whole: {chunks:?}"
        );
    }

    /// An inline code span is an entity too, and the same rule applies to it.
    #[test]
    fn an_inline_code_span_is_not_split() {
        // The span straddles the 100-character mark, which is where a count-only split would cut.
        let message = format!(
            "{}`cargo test --all-features` finishes the job",
            "word ".repeat(19)
        );

        let chunks = message_chunks(&message, 100);

        for chunk in &chunks {
            assert!(
                chunk.matches('`').count() % 2 == 0,
                "an unclosed code span in {chunk:?}"
            );
        }
    }

    /// The fallback: one token longer than a whole chunk still has to be delivered, not dropped or
    /// looped over.
    #[test]
    fn an_unbroken_token_still_gets_chunked() {
        let message = "x".repeat(250);

        let chunks = message_chunks(&message, 100);

        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks.concat(), message);
        assert!(chunks.iter().all(|chunk| chunk.chars().count() <= 100));
    }

    #[test]
    fn an_empty_message_produces_no_chunks() {
        assert!(message_chunks("", 4000).is_empty());
        assert_eq!(message_chunks("short", 4000), vec!["short"]);
    }

    /// Onboarding a second provider moves the flat `[provider]` block into `[providers.*]` and
    /// leaves the field itself `None`. Both message builders used to read that field directly and
    /// panicked here — on precisely the installs with more than one provider to report.
    #[test]
    fn listing_models_and_context_survives_a_second_provider() {
        let config: Config = toml::from_str(
            "active_provider = \"b\"\n\
             \n[providers.b]\nbase_url = \"https://b.example.com/v1\"\napi_key = \"k\"\n\
             active_model = \"model-b\"\nmodels = [\"model-b\"]\n\
             \n[telegram]\nbot_token = \"123:secret\"\nbot_username = \"bot\"\n\
             owner_user_id = 42\n",
        )
        .unwrap();
        assert!(
            config.provider.is_none(),
            "the flat block is what onboarding empties"
        );

        assert!(models_message(&config).contains("model-b"));
        assert!(!context_window_message(&config).is_empty());
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
    fn progress_is_shown_only_for_visible_external_work() {
        assert!(should_show_tool_progress("mcptools__render_plantuml"));
        assert!(should_show_tool_progress("delegate_to_kamui"));
        assert!(should_show_tool_progress("run_command"));
        assert!(!should_show_tool_progress("read_file"));
        assert!(!should_show_tool_progress("remember"));
        assert_eq!(
            user_facing_tool_name("mcptools__render_plantuml"),
            "render plantuml"
        );
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
                id: 1,
                content: "The user is a researcher.".to_owned(),
            },
            storage::MemoryEntry {
                id: 2,
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

    struct FakeProvider {
        responses: std::sync::Mutex<std::collections::VecDeque<provider::ChatResponse>>,
    }

    #[async_trait::async_trait]
    impl ModelProvider for FakeProvider {
        fn active_model(&self) -> &str {
            "fake-model"
        }

        async fn chat(
            &self,
            _messages: &[ProviderMessage],
            _tools: &[provider::ToolDefinition],
        ) -> Result<provider::ChatResponse> {
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .context("fake provider ran out of responses")
        }
    }

    #[tokio::test]
    async fn readonly_subagent_uses_isolated_provider_and_read_tools() {
        let root = std::env::temp_dir().join(format!("kumo-subagent-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("note.txt"), "important detail").unwrap();
        let database = Arc::new(Mutex::new(Database::open_in_memory_for_tests()));
        let tools = ToolRegistry::new(root.clone(), Vec::new(), database, chrono_tz::UTC).unwrap();
        let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider {
            responses: std::sync::Mutex::new(std::collections::VecDeque::from([
                provider::ChatResponse {
                    content: String::new(),
                    tool_calls: vec![provider::ToolCall {
                        id: "read-1".to_owned(),
                        name: "read_file".to_owned(),
                        arguments: r#"{"path":"note.txt"}"#.to_owned(),
                    }],
                    usage: Usage {
                        prompt_tokens: 2,
                        completion_tokens: 1,
                        total_tokens: 3,
                    },
                    finish_reason: "tool_calls".to_owned(),
                },
                provider::ChatResponse {
                    content: "The note contains an important detail.".to_owned(),
                    tool_calls: Vec::new(),
                    usage: Usage {
                        prompt_tokens: 3,
                        completion_tokens: 1,
                        total_tokens: 4,
                    },
                    finish_reason: "stop".to_owned(),
                },
            ])),
        });

        let result = run_readonly_subagent(provider, tools, 42, r#"{"task":"Inspect note.txt"}"#)
            .await
            .unwrap();
        assert!(result.answer.contains("important detail"));
        assert_eq!(result.usage.total_tokens, 7);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Every class of turn failure used to reach the owner as "the model provider could not
    /// answer", which sends them to check a service that is fine while the real fault goes
    /// unnamed. Each class has to describe itself.
    #[test]
    fn a_failed_turn_is_reported_as_the_thing_that_actually_failed() {
        let report = |kind| {
            turn_failure_report(
                &TurnFailure::new(kind, anyhow::anyhow!("underlying detail")).into(),
                "ref12345",
            )
        };

        let provider = report(FailureKind::Provider);
        let tool = report(FailureKind::Tool);
        let storage = report(FailureKind::Storage);
        let telegram = report(FailureKind::Telegram);

        assert!(provider.contains("model provider"), "{provider}");
        assert!(tool.contains("tool"), "{tool}");
        assert!(storage.contains("database"), "{storage}");
        assert!(telegram.contains("message"), "{telegram}");
        for other in [&tool, &storage, &telegram] {
            assert!(
                !other.contains("model provider"),
                "a non-provider failure must not blame the provider: {other}"
            );
        }
        for report in [&provider, &tool, &storage, &telegram] {
            assert!(report.contains("ref12345"), "{report}");
        }
    }

    /// An error that nothing classified is Kumo's problem until proven otherwise — it must not
    /// fall back to blaming the provider, which is precisely the old behaviour.
    #[test]
    fn an_unclassified_failure_is_reported_as_internal_not_as_the_provider() {
        let error = anyhow::anyhow!("a bug in Kumo");

        assert_eq!(failure_kind(&error), FailureKind::Internal);
        let report = turn_failure_report(&error, "ref00001");
        assert!(report.contains("internal error"), "{report}");
        assert!(!report.contains("model provider"), "{report}");
    }

    /// The class survives the layers `anyhow` adds on the way out of the agent loop.
    #[test]
    fn a_classified_failure_keeps_its_class_under_added_context() {
        let error: anyhow::Error =
            TurnFailure::new(FailureKind::Storage, anyhow::anyhow!("database is locked")).into();

        let wrapped = error.context("while recording an audit event");

        assert_eq!(failure_kind(&wrapped), FailureKind::Storage);
    }

    /// The report is the only thing the owner sees, and a Telegram chat is not a log: it must
    /// carry the reference id and nothing from the error itself.
    #[test]
    fn a_failure_report_never_leaks_paths_or_secrets() {
        let error: anyhow::Error = TurnFailure::new(
            FailureKind::Provider,
            anyhow::anyhow!(
                "POST https://api.example.com/v1/chat failed: 401 for key sk-live-abc123 \
                 (config C:\\Users\\owner\\AppData\\Roaming\\kumo\\kumo.toml)"
            ),
        )
        .into();

        let report = turn_failure_report(&error, "ref99999");

        for secret in ["sk-live-abc123", "kumo.toml", "api.example.com", "401"] {
            assert!(!report.contains(secret), "leaked {secret}: {report}");
        }
        assert!(report.contains("ref99999"), "{report}");
    }

    /// A template named after a built-in never runs, so `/commands` has to say so by name rather
    /// than listing it as if it worked or omitting it as if it were not there.
    #[test]
    fn commands_message_names_a_template_shadowed_by_a_builtin() {
        let set = commands::CommandSet {
            templates: Vec::new(),
            shadowed: vec![commands::ShadowedCommand {
                name: "status".to_owned(),
                path: std::path::PathBuf::from("/home/owner/.kumo/commands/status.md"),
            }],
        };

        let message = commands_message(set);

        assert!(message.contains("status.md"), "{message}");
        assert!(message.contains("/status"), "{message}");
        assert!(message.contains("Rename"), "{message}");
    }

    /// The routing block in this file is the definition of "built-in"; `commands::RESERVED` is a
    /// copy of it. A new built-in added without a matching reserved entry re-opens the gap
    /// silently, so the copy is checked against the source it copies.
    #[test]
    fn builtin_commands_are_all_reserved() {
        let source = include_str!("main.rs");
        let mut routed: Vec<String> = Vec::new();
        for (marker, terminator) in [("text == \"/", '"'), ("text.strip_prefix(\"/", ' ')] {
            for occurrence in source.split(marker).skip(1) {
                let name: String = occurrence
                    .chars()
                    .take_while(|character| {
                        *character != terminator && character.is_ascii_alphanumeric()
                    })
                    .collect();
                if !name.is_empty() {
                    routed.push(name);
                }
            }
        }
        assert!(
            routed.len() > 10,
            "the scan found no built-ins, so it stopped testing anything: {routed:?}"
        );
        for name in routed {
            assert!(
                commands::is_reserved(&name),
                "/{name} is routed as a built-in but missing from commands::RESERVED, so a \
                 {name}.md template would be silently unreachable"
            );
        }
    }

    /// `Ctrl+C` used to abort the scheduler outright, which could cut a task between
    /// `claim_due_scheduled_tasks` (which sets `running`) and `complete_scheduled_task`, leaving
    /// the row `running` — the state both `storage.rs` and the ROADMAP said only a hard crash
    /// could produce. Shutdown waits for the in-flight task instead.
    #[tokio::test]
    async fn shutdown_waits_for_an_in_flight_scheduled_task() {
        let database = Arc::new(Mutex::new(Database::open_in_memory_for_tests()));
        let turn_lock = Arc::new(Mutex::new(()));
        let id = database
            .lock()
            .await
            .create_scheduled_task(42, "ping", 0, None)
            .unwrap();
        let claimed = database.lock().await.claim_due_scheduled_tasks(1).unwrap();
        assert_eq!(claimed.len(), 1, "the fixture has to leave a running row");

        // Stands in for the scheduler mid-task: it holds the turn lock across the run, exactly as
        // scheduler.rs does, and completes the row before letting go.
        let scheduler = tokio::spawn({
            let database = database.clone();
            let turn_lock = turn_lock.clone();
            async move {
                let guard = turn_lock.lock().await;
                tokio::time::sleep(Duration::from_millis(150)).await;
                database
                    .lock()
                    .await
                    .complete_scheduled_task(&id, "completed", chrono::Utc::now().timestamp())
                    .unwrap();
                drop(guard);
                std::future::pending::<()>().await;
            }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;

        let shutdown =
            stop_scheduler(scheduler, &turn_lock, &database, Duration::from_secs(5)).await;

        assert!(
            shutdown.idle,
            "shutdown must wait for the task, not abort it"
        );
        assert_eq!(
            shutdown.recovered, 0,
            "a task allowed to finish needs no recovery"
        );
        assert_eq!(
            database.lock().await.reset_stuck_running_tasks().unwrap(),
            0,
            "no row may be left running by a graceful shutdown"
        );
    }

    /// The wait is bounded: a task that will not finish must not hold shutdown open. What it
    /// leaves behind is put back to `pending` here rather than being discovered as a stale
    /// `running` row on the next start.
    #[tokio::test]
    async fn shutdown_gives_up_on_a_stuck_task_and_marks_its_row() {
        let database = Arc::new(Mutex::new(Database::open_in_memory_for_tests()));
        let turn_lock = Arc::new(Mutex::new(()));
        database
            .lock()
            .await
            .create_scheduled_task(42, "ping", 0, None)
            .unwrap();
        database.lock().await.claim_due_scheduled_tasks(1).unwrap();

        let scheduler = tokio::spawn({
            let turn_lock = turn_lock.clone();
            async move {
                let _guard = turn_lock.lock().await;
                std::future::pending::<()>().await;
            }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;

        let started = Instant::now();
        let shutdown =
            stop_scheduler(scheduler, &turn_lock, &database, Duration::from_millis(100)).await;

        assert!(
            !shutdown.idle,
            "the stuck task should have timed out the wait"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "shutdown has to stay bounded, took {:?}",
            started.elapsed()
        );
        assert_eq!(shutdown.recovered, 1, "the abandoned row has to be marked");
        assert_eq!(
            database.lock().await.reset_stuck_running_tasks().unwrap(),
            0,
            "nothing may be left running"
        );
        // Put back to pending, so the task is not lost: it is due again immediately.
        assert_eq!(
            database
                .lock()
                .await
                .list_scheduled_tasks(42)
                .unwrap()
                .len(),
            1
        );
    }
}
