use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use chrono::DateTime;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Mutex;

use crate::{
    provider::{ToolCall, ToolDefinition},
    storage::{Database, MemoryMatch},
};

const MAX_FILE_SIZE: u64 = 64 * 1024;
const MAX_DIRECTORY_ENTRIES: usize = 200;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_COMMAND_OUTPUT: usize = 16 * 1024;
/// Coding tasks delegated to Kamui can run a whole agent loop (read, edit, test), so they get more
/// headroom than a single shell command.
const KAMUI_TIMEOUT: Duration = Duration::from_secs(300);
/// How far into the future a scheduled task may be set, as a sanity bound against the model
/// misparsing a relative date (e.g. the wrong year).
const MAX_SCHEDULE_HORIZON: chrono::Duration = chrono::Duration::days(366);
/// Shortest allowed gap between runs of a recurring task. This was 60 seconds, which turned out to
/// be no protection at all: a model that reads "every minute" literally produces a task that
/// delivers a message every single minute, forever, and every one of those runs costs a full agent
/// turn. Five minutes is still far shorter than any real reminder ("hourly", "daily") and no longer
/// reads as a plausible interval to reach for by accident.
const MIN_REPEAT_INTERVAL: Duration = Duration::from_secs(300);
/// How many pending tasks one chat may hold. A scheduled run goes through the same agent loop as a
/// live message, so a task can schedule further tasks — without a ceiling, one confused turn can
/// leave a chat with an ever-growing pile of them.
const MAX_PENDING_TASKS: usize = 20;
/// Total bytes of stored memory content allowed before `remember` refuses to add more, keeping the
/// system prompt (which carries every entry on every request) from growing unbounded.
const MAX_MEMORY_BYTES: i64 = 4 * 1024;
/// Bounds on how much of an ambiguous match is echoed back: enough for the model to tell the
/// candidates apart, not so much that a wide substring returns most of memory as a tool result.
const MAX_AMBIGUOUS_MEMORY_ENTRIES: usize = 5;
const MAX_AMBIGUOUS_FACT_CHARS: usize = 120;

#[derive(Clone)]
pub struct ToolRegistry {
    root: PathBuf,
    extra: Vec<Arc<dyn ExternalTool>>,
    database: Arc<Mutex<Database>>,
    timezone: chrono_tz::Tz,
}

#[async_trait]
pub trait ExternalTool: Send + Sync {
    fn name(&self) -> &str;
    fn definition(&self) -> ToolDefinition;
    fn requires_confirmation(&self) -> bool;
    fn preview(&self, arguments: &str) -> Option<String>;
    async fn run(&self, arguments: &str) -> Result<String>;
}

impl ToolRegistry {
    pub fn new(
        root: PathBuf,
        extra: Vec<Arc<dyn ExternalTool>>,
        database: Arc<Mutex<Database>>,
        timezone: chrono_tz::Tz,
    ) -> Result<Self> {
        let root = root
            .canonicalize()
            .with_context(|| format!("could not resolve workspace {}", root.display()))?;
        if !root.is_dir() {
            bail!("workspace is not a directory: {}", root.display());
        }
        Ok(Self {
            root,
            extra,
            database,
            timezone,
        })
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        let mut definitions = vec![
            ToolDefinition {
                name: "read_file".to_owned(),
                description: "Read a UTF-8 text file inside the configured workspace.".to_owned(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Workspace-relative file path, for example src/main.rs."
                        }
                    },
                    "required": ["path"]
                }),
            },
            ToolDefinition {
                name: "list_directory".to_owned(),
                description: "List entries in a directory inside the configured workspace."
                    .to_owned(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Workspace-relative directory path; use . for the root."
                        }
                    },
                    "required": ["path"]
                }),
            },
            ToolDefinition {
                name: "run_command".to_owned(),
                description: "Run a shell command in the configured workspace after the user explicitly approves it. Use this for checks, builds, tests, and other host tasks."
                    .to_owned(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "command": {
                            "type": "string",
                            "description": "Shell command to run in the workspace, for example cargo test."
                        }
                    },
                    "required": ["command"]
                }),
            },
        ];
        if kamui_available() {
            definitions.push(ToolDefinition {
                name: "delegate_to_kamui".to_owned(),
                description: "Delegate a coding task (reading, editing, or running commands \
                              against files in the workspace) to Kamui, an independent coding \
                              agent. Use this instead of run_command for anything that involves \
                              editing files. The user must approve the task before it runs."
                    .to_owned(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "task": {
                            "type": "string",
                            "description": "A clear, self-contained description of the coding task for Kamui to perform, e.g. \"add input validation to the login handler and run the tests\"."
                        }
                    },
                    "required": ["task"]
                }),
            });
        }
        definitions.push(ToolDefinition {
            name: "schedule_task".to_owned(),
            description: "Schedule a message to yourself for a future time, once or repeating. \
                          At the scheduled time (and again after each repeat_interval_seconds, if \
                          given), the given prompt is sent through your own agent loop (with all \
                          the same tools) as if the user had just sent it, and the result is \
                          delivered to the user in this chat. Use the user's configured timezone \
                          (given in the system prompt) to compute run_at, which is always the \
                          *first* time it should run — for a recurring task like \"every day at \
                          9am\", set run_at to the next upcoming 9am, not today's if it has \
                          already passed."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "The instruction to run at the scheduled time, e.g. \"check BTC price and summarize the last 24h\"."
                    },
                    "run_at": {
                        "type": "string",
                        "description": "When to run it the first time, as RFC 3339 with a UTC offset, e.g. \"2026-07-27T09:00:00+07:00\"."
                    },
                    "repeat_interval_seconds": {
                        "type": "integer",
                        "description": "Omit for a one-shot task — most reminders are one-shot. Only set this when the user asks for something genuinely repeating, e.g. 86400 for daily, 604800 for weekly. Every run delivers a message to the chat, so short intervals read as spam."
                    }
                },
                "required": ["prompt", "run_at"]
            }),
        });
        definitions.push(ToolDefinition {
            name: "list_scheduled_tasks".to_owned(),
            description: "List this chat's pending scheduled tasks, each with the id needed to \
                          cancel it. Call this before answering any question about what is \
                          scheduled, and before cancelling anything."
                .to_owned(),
            parameters: json!({ "type": "object", "properties": {} }),
        });
        definitions.push(ToolDefinition {
            name: "cancel_scheduled_task".to_owned(),
            description: "Cancel a pending scheduled task by its id, as shown by \
                          list_scheduled_tasks. Use this whenever the user asks to stop, remove, \
                          or turn off a reminder — including \"stop reminding me\" and \"don't \
                          repeat that\". Saying you have stopped a reminder without calling this \
                          changes nothing, and it will keep firing."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "The task id from list_scheduled_tasks; a unique prefix is enough."
                    }
                },
                "required": ["id"]
            }),
        });
        definitions.push(ToolDefinition {
            name: "remember".to_owned(),
            description: "Save a fact about the user or their preferences permanently. Unlike \
                          conversation history, this persists across /new, session switches, and \
                          restarts, and is visible in every future conversation. Use it only when \
                          explicitly asked to remember something, or when the user states a clear, \
                          durable preference or fact about themselves. If a similar fact is already \
                          remembered, use update_memory instead of adding a duplicate."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "fact": {
                        "type": "string",
                        "description": "A short, self-contained fact, e.g. \"The user is a researcher.\" or \"Prefers concise answers.\""
                    }
                },
                "required": ["fact"]
            }),
        });
        definitions.push(ToolDefinition {
            name: "update_memory".to_owned(),
            description: "Replace a previously remembered fact that is now outdated or wrong. \
                          Find it by an unambiguous substring of its exact wording; if more than \
                          one remembered fact matches, this fails and lists the facts it matched, \
                          so retry with a substring that appears in only one of them."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "matching": {
                        "type": "string",
                        "description": "A substring that uniquely identifies the fact to replace, e.g. \"is a researcher\"."
                    },
                    "fact": {
                        "type": "string",
                        "description": "The corrected fact to store in its place."
                    }
                },
                "required": ["matching", "fact"]
            }),
        });
        definitions.push(ToolDefinition {
            name: "forget".to_owned(),
            description: "Permanently delete a previously remembered fact, found by an \
                          unambiguous substring of its exact wording (same matching rule as \
                          update_memory)."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "matching": {
                        "type": "string",
                        "description": "A substring that uniquely identifies the fact to delete."
                    }
                },
                "required": ["matching"]
            }),
        });
        definitions.push(ToolDefinition {
            name: "ask_user".to_owned(),
            description: "Pause and ask the user a clarifying question before proceeding, when \
                          a task is ambiguous or has multiple reasonable interpretations. This is \
                          not for approval of a risky action (run_command and delegate_to_kamui \
                          already ask for that automatically) — use it when you genuinely need \
                          more information to continue, e.g. which of several matching files was \
                          meant, or a preference between reasonable options. The user can tap one \
                          of the offered options or reply with free text instead."
                .to_owned(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "question": {
                        "type": "string",
                        "description": "The question to ask, e.g. \"Which file did you mean?\""
                    },
                    "options": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "2-4 short suggested answers shown as buttons, e.g. [\"src/main.rs\", \"src/lib.rs\"]. Optional — omit for an open-ended question with no natural fixed choices."
                    }
                },
                "required": ["question"]
            }),
        });
        // ask_user is dispatched specially by run_agent (it needs the bot and chat, which
        // ToolRegistry does not have), so it is not listed in requires_confirmation/dispatch below.
        definitions.extend(self.extra.iter().map(|tool| tool.definition()));
        definitions
    }

    pub fn requires_confirmation(&self, name: &str) -> bool {
        name == "run_command"
            || name == "delegate_to_kamui"
            || self
                .extra
                .iter()
                .find(|tool| tool.name() == name)
                .is_some_and(|tool| tool.requires_confirmation())
    }

    pub fn preview(&self, call: &ToolCall) -> Option<String> {
        if call.name == "run_command" {
            parse_command(&call.arguments)
                .ok()
                .map(|command| format!("Command: {command}"))
        } else if call.name == "delegate_to_kamui" {
            parse_task(&call.arguments)
                .ok()
                .map(|task| format!("Kamui task: {task}"))
        } else {
            self.extra
                .iter()
                .find(|tool| tool.name() == call.name)
                .and_then(|tool| tool.preview(&call.arguments))
        }
    }

    /// Dispatch a tool call made while answering `chat_id`. Only `schedule_task` needs the chat
    /// ID today (to know where to deliver the result later); every other tool ignores it.
    pub async fn dispatch(&self, chat_id: i64, call: &ToolCall) -> String {
        let result = match call.name.as_str() {
            "read_file" => self.read_file(&call.arguments),
            "list_directory" => self.list_directory(&call.arguments),
            "run_command" => self.run_command(&call.arguments).await,
            "delegate_to_kamui" => self.delegate_to_kamui(&call.arguments).await,
            "schedule_task" => self.schedule_task(chat_id, &call.arguments).await,
            "list_scheduled_tasks" => self.list_scheduled_tasks(chat_id).await,
            "cancel_scheduled_task" => self.cancel_scheduled_task(chat_id, &call.arguments).await,
            "remember" => self.remember(&call.arguments).await,
            "update_memory" => self.update_memory(&call.arguments).await,
            "forget" => self.forget(&call.arguments).await,
            _ => match self.extra.iter().find(|tool| tool.name() == call.name) {
                Some(tool) => tool.run(&call.arguments).await,
                None => Err(anyhow::anyhow!("unknown tool '{}'", call.name)),
            },
        };
        result.unwrap_or_else(|error| format!("Error: {error:#}"))
    }

    fn read_file(&self, arguments: &str) -> Result<String> {
        let arguments: PathArguments =
            serde_json::from_str(arguments).context("tool arguments were not valid JSON")?;
        let path = self.resolve(&arguments.path)?;
        let metadata = std::fs::metadata(&path)
            .with_context(|| format!("could not inspect {}", path.display()))?;
        if !metadata.is_file() {
            bail!("path is not a file: {}", arguments.path);
        }
        if metadata.len() > MAX_FILE_SIZE {
            bail!("file exceeds the 64 KiB limit: {}", arguments.path);
        }
        std::fs::read_to_string(&path)
            .with_context(|| format!("could not read UTF-8 file {}", arguments.path))
    }

    fn list_directory(&self, arguments: &str) -> Result<String> {
        let arguments: PathArguments =
            serde_json::from_str(arguments).context("tool arguments were not valid JSON")?;
        let path = self.resolve(&arguments.path)?;
        if !path.is_dir() {
            bail!("path is not a directory: {}", arguments.path);
        }

        let mut entries = std::fs::read_dir(&path)
            .with_context(|| format!("could not list {}", arguments.path))?
            .map(|entry| {
                let entry = entry?;
                let mut name = entry.file_name().to_string_lossy().into_owned();
                if entry.file_type()?.is_dir() {
                    name.push('/');
                }
                Ok(name)
            })
            .collect::<std::io::Result<Vec<_>>>()?;
        entries.sort();
        if entries.len() > MAX_DIRECTORY_ENTRIES {
            entries.truncate(MAX_DIRECTORY_ENTRIES);
            entries.push("... entry limit reached".to_owned());
        }
        Ok(entries.join("\n"))
    }

    fn resolve(&self, relative: &str) -> Result<PathBuf> {
        let relative = Path::new(relative);
        if relative.is_absolute() {
            bail!("path must be relative to the workspace");
        }
        let path = self
            .root
            .join(relative)
            .canonicalize()
            .with_context(|| format!("path does not exist: {relative:?}"))?;
        if !path.starts_with(&self.root) {
            bail!("path escapes the configured workspace");
        }
        Ok(path)
    }

    async fn run_command(&self, arguments: &str) -> Result<String> {
        let command = parse_command(arguments)?;
        let (shell, flag) = if cfg!(windows) {
            ("cmd", "/C")
        } else {
            ("sh", "-c")
        };
        let child = tokio::process::Command::new(shell)
            .arg(flag)
            .arg(&command)
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("failed to start the command")?;

        match tokio::time::timeout(COMMAND_TIMEOUT, child.wait_with_output()).await {
            Ok(result) => {
                let output = result.context("failed to run the command")?;
                Ok(format_command_output(&output))
            }
            Err(_) => Ok(format!(
                "Error: command timed out after {} seconds and was terminated",
                COMMAND_TIMEOUT.as_secs()
            )),
        }
    }

    /// Run a coding task through Kamui's non-interactive mode (`kamui -p <task> --auto-approve`)
    /// in the configured workspace. Kumo already gated this call on Telegram approval before
    /// dispatch, so Kamui's own tool approvals are bypassed with `--auto-approve` rather than
    /// asking twice for the same task.
    async fn delegate_to_kamui(&self, arguments: &str) -> Result<String> {
        let task = parse_task(arguments)?;
        let child = tokio::process::Command::new("kamui")
            .arg("-p")
            .arg(&task)
            .arg("--auto-approve")
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("failed to start kamui")?;

        match tokio::time::timeout(KAMUI_TIMEOUT, child.wait_with_output()).await {
            Ok(result) => {
                let output = result.context("failed to run kamui")?;
                Ok(summarize_kamui_output(&output))
            }
            Err(_) => Ok(format!(
                "Error: kamui timed out after {} seconds and was terminated",
                KAMUI_TIMEOUT.as_secs()
            )),
        }
    }

    /// Record a one-shot or recurring scheduled task. Actually running it (and, for a recurring
    /// task, rescheduling it after each run) is the scheduler loop's job (`src/scheduler.rs`);
    /// this only validates and persists the request.
    async fn schedule_task(&self, chat_id: i64, arguments: &str) -> Result<String> {
        let arguments: ScheduleArguments =
            serde_json::from_str(arguments).context("tool arguments were not valid JSON")?;
        let prompt = arguments.prompt.trim();
        if prompt.is_empty() {
            bail!("schedule_task requires a non-empty 'prompt' argument");
        }
        let run_at = DateTime::parse_from_rfc3339(&arguments.run_at).with_context(|| {
            format!(
                "run_at was not a valid RFC 3339 timestamp: {}",
                arguments.run_at
            )
        })?;
        let now = chrono::Utc::now();
        if run_at < now {
            bail!("run_at ({}) is in the past", arguments.run_at);
        }
        if run_at.signed_duration_since(now) > MAX_SCHEDULE_HORIZON {
            bail!(
                "run_at is more than {} days in the future; double-check the year",
                MAX_SCHEDULE_HORIZON.num_days()
            );
        }
        if let Some(interval) = arguments.repeat_interval_seconds
            && interval < MIN_REPEAT_INTERVAL.as_secs() as i64
        {
            bail!(
                "repeat_interval_seconds must be at least {} seconds",
                MIN_REPEAT_INTERVAL.as_secs()
            );
        }

        let database = self.database.lock().await;
        let pending = database.list_scheduled_tasks(chat_id)?.len();
        if pending >= MAX_PENDING_TASKS {
            bail!(
                "this chat already has {pending} pending scheduled tasks, which is the limit; \
                 cancel some with cancel_scheduled_task before adding another"
            );
        }
        let id = database.create_scheduled_task(
            chat_id,
            prompt,
            run_at.timestamp(),
            arguments.repeat_interval_seconds,
        )?;
        drop(database);

        let local_time = run_at.with_timezone(&self.timezone);
        let recurrence = match arguments.repeat_interval_seconds {
            Some(interval) => format!(", repeating every {interval} seconds"),
            None => String::new(),
        };
        Ok(format!(
            "scheduled ({}): will run at {} ({}){recurrence}",
            &id[..8],
            local_time.format("%Y-%m-%d %H:%M %Z"),
            self.timezone
        ))
    }

    /// The pending tasks for this chat, with the ids `cancel_scheduled_task` accepts. Without this
    /// the model could create reminders but never see or stop them, so "stop reminding me" could
    /// only ever be answered with an apology.
    async fn list_scheduled_tasks(&self, chat_id: i64) -> Result<String> {
        let tasks = self.database.lock().await.list_scheduled_tasks(chat_id)?;
        if tasks.is_empty() {
            return Ok("no pending scheduled tasks".to_owned());
        }

        let lines: Vec<String> = tasks
            .iter()
            .map(|task| {
                let when = DateTime::from_timestamp(task.run_at, 0)
                    .map(|value| {
                        value
                            .with_timezone(&self.timezone)
                            .format("%Y-%m-%d %H:%M")
                            .to_string()
                    })
                    .unwrap_or_else(|| "unknown time".to_owned());
                let recurrence = match task.repeat_interval_seconds {
                    Some(interval) => format!(", repeats every {interval}s"),
                    None => String::new(),
                };
                format!(
                    "{}: \"{}\" at {when}{recurrence}",
                    &task.id[..8],
                    task.prompt
                )
            })
            .collect();
        Ok(lines.join("\n"))
    }

    async fn cancel_scheduled_task(&self, chat_id: i64, arguments: &str) -> Result<String> {
        let arguments: CancelTaskArguments =
            serde_json::from_str(arguments).context("tool arguments were not valid JSON")?;
        let id = arguments.id.trim();
        if id.is_empty() {
            bail!("cancel_scheduled_task requires a non-empty 'id' argument");
        }

        if self
            .database
            .lock()
            .await
            .cancel_scheduled_task(chat_id, id)?
        {
            Ok(format!("cancelled scheduled task {id}"))
        } else {
            bail!(
                "no pending scheduled task in this chat matches \"{id}\"; call \
                 list_scheduled_tasks for the current ids"
            )
        }
    }

    /// Append a new global memory entry, capped so the system prompt (which carries every stored
    /// fact on every request) cannot grow unbounded.
    async fn remember(&self, arguments: &str) -> Result<String> {
        let arguments: FactArguments =
            serde_json::from_str(arguments).context("tool arguments were not valid JSON")?;
        let fact = arguments.fact.trim();
        if fact.is_empty() {
            bail!("remember requires a non-empty 'fact' argument");
        }

        let database = self.database.lock().await;
        let existing = database.total_memory_bytes()?;
        if existing + fact.len() as i64 > MAX_MEMORY_BYTES {
            bail!(
                "memory is full ({existing}/{MAX_MEMORY_BYTES} bytes); use update_memory or forget \
                 to make room before adding more"
            );
        }
        database.remember(fact)?;
        Ok(format!("remembered: {fact}"))
    }

    /// Replace an existing memory entry matched by an unambiguous substring.
    async fn update_memory(&self, arguments: &str) -> Result<String> {
        let arguments: UpdateMemoryArguments =
            serde_json::from_str(arguments).context("tool arguments were not valid JSON")?;
        let matching = arguments.matching.trim();
        let fact = arguments.fact.trim();
        if matching.is_empty() || fact.is_empty() {
            bail!("update_memory requires non-empty 'matching' and 'fact' arguments");
        }

        match self.database.lock().await.update_memory(matching, fact)? {
            MemoryMatch::One(_) => Ok(format!("updated memory matching \"{matching}\" to: {fact}")),
            MemoryMatch::None => bail!("no remembered fact contains \"{matching}\""),
            MemoryMatch::Ambiguous(entries) => bail!(ambiguous_memory_error(matching, &entries)),
        }
    }

    /// Delete an existing memory entry matched by an unambiguous substring.
    async fn forget(&self, arguments: &str) -> Result<String> {
        let arguments: ForgetArguments =
            serde_json::from_str(arguments).context("tool arguments were not valid JSON")?;
        let matching = arguments.matching.trim();
        if matching.is_empty() {
            bail!("forget requires a non-empty 'matching' argument");
        }

        match self.database.lock().await.forget(matching)? {
            MemoryMatch::One(_) => Ok(format!("forgot the fact matching \"{matching}\"")),
            MemoryMatch::None => bail!("no remembered fact contains \"{matching}\""),
            MemoryMatch::Ambiguous(entries) => bail!(ambiguous_memory_error(matching, &entries)),
        }
    }
}

/// Names the entries a substring collided with. Listing them is the whole point: a caller told only
/// that its substring was ambiguous has to guess a better one, while a caller shown the competing
/// facts can pick wording that appears in exactly one of them.
fn ambiguous_memory_error(matching: &str, entries: &[String]) -> String {
    let mut message = format!(
        "\"{matching}\" matches {} remembered facts; use a substring unique to one of them:",
        entries.len()
    );
    for entry in entries.iter().take(MAX_AMBIGUOUS_MEMORY_ENTRIES) {
        message.push_str(&format!("\n- {}", truncate_fact(entry)));
    }
    if let Some(rest) = entries.len().checked_sub(MAX_AMBIGUOUS_MEMORY_ENTRIES)
        && rest > 0
    {
        message.push_str(&format!("\n- ...and {rest} more"));
    }
    message
}

fn truncate_fact(fact: &str) -> String {
    if fact.chars().count() <= MAX_AMBIGUOUS_FACT_CHARS {
        return fact.to_owned();
    }
    let head: String = fact.chars().take(MAX_AMBIGUOUS_FACT_CHARS).collect();
    format!("{head}...")
}

/// Whether the `kamui` binary is reachable on `PATH`. Detected once per process; delegation is an
/// optional capability, never a requirement for Kumo's built-in tools.
pub fn kamui_available() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        std::process::Command::new("kamui")
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    })
}

#[derive(Deserialize)]
struct PathArguments {
    path: String,
}

#[derive(Deserialize)]
struct CommandArguments {
    command: String,
}

#[derive(Deserialize)]
struct TaskArguments {
    task: String,
}

#[derive(Deserialize)]
struct CancelTaskArguments {
    id: String,
}

#[derive(Deserialize)]
struct ScheduleArguments {
    prompt: String,
    run_at: String,
    #[serde(default)]
    repeat_interval_seconds: Option<i64>,
}

#[derive(Deserialize)]
struct FactArguments {
    fact: String,
}

#[derive(Deserialize)]
struct UpdateMemoryArguments {
    matching: String,
    fact: String,
}

#[derive(Deserialize)]
struct ForgetArguments {
    matching: String,
}

fn parse_command(arguments: &str) -> Result<String> {
    let arguments: CommandArguments =
        serde_json::from_str(arguments).context("tool arguments were not valid JSON")?;
    let command = arguments.command.trim();
    if command.is_empty() {
        bail!("run_command requires a non-empty 'command' argument");
    }
    Ok(command.to_owned())
}

fn parse_task(arguments: &str) -> Result<String> {
    let arguments: TaskArguments =
        serde_json::from_str(arguments).context("tool arguments were not valid JSON")?;
    let task = arguments.task.trim();
    if task.is_empty() {
        bail!("delegate_to_kamui requires a non-empty 'task' argument");
    }
    Ok(task.to_owned())
}

/// Turn a `kamui -p` invocation's raw output into something readable in Telegram: a one-line
/// summary of what happened (tool calls made, any errors) followed by Kamui's actual final
/// answer, instead of the exit code plus the full interleaved stdout/stderr `format_command_output`
/// would produce for a generic command. Kamui's own trace lines (`  → tool(...)`, `    ok (N
/// chars)`, `    ! error`) are counted rather than shown verbatim; the final answer is taken as
/// the text after the last such trace line, since `kamui -p` prints it as `\n{answer}` after the
/// tool loop and nothing else goes to stdout after that (its "resume this session" hint goes to
/// stderr, not stdout, specifically so this split works).
fn summarize_kamui_output(output: &std::process::Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();

    let is_trace_line = |line: &str| {
        line.starts_with("  \u{2192} ")
            || line.starts_with("    ok (")
            || line.starts_with("    ! ")
    };
    let last_trace = lines.iter().rposition(|line| is_trace_line(line));
    let tool_calls = lines
        .iter()
        .filter(|line| line.starts_with("  \u{2192} "))
        .count();
    let errors = lines
        .iter()
        .filter(|line| line.trim_start().starts_with('!'))
        .count();

    let answer_start = last_trace.map_or(0, |index| index + 1);
    let answer = lines[answer_start..]
        .iter()
        .map(|line| line.trim_end())
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned();

    let mut summary = match output.status.code() {
        Some(0) if tool_calls == 0 => String::new(),
        Some(0) => format!("({tool_calls} tool call(s), {errors} error(s))\n"),
        Some(code) => format!("(kamui exited with code {code}, {tool_calls} tool call(s))\n"),
        None => "(kamui was terminated by a signal)\n".to_owned(),
    };

    if answer.is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        summary.push_str("Error: kamui produced no answer.");
        if !stderr.trim().is_empty() {
            summary.push_str("\nstderr:\n");
            summary.push_str(stderr.trim());
        }
    } else {
        summary.push_str(&answer);
    }
    truncate_utf8(summary, MAX_COMMAND_OUTPUT)
}

fn format_command_output(output: &std::process::Output) -> String {
    let code = output.status.code().map_or_else(
        || "terminated by signal".to_owned(),
        |code| code.to_string(),
    );
    let mut result = format!("exit code: {code}");
    if !output.stdout.is_empty() {
        result.push_str("\nstdout:\n");
        result.push_str(&String::from_utf8_lossy(&output.stdout));
    }
    if !output.stderr.is_empty() {
        result.push_str("\nstderr:\n");
        result.push_str(&String::from_utf8_lossy(&output.stderr));
    }
    truncate_utf8(result, MAX_COMMAND_OUTPUT)
}

fn truncate_utf8(mut value: String, limit: usize) -> String {
    if value.len() <= limit {
        return value;
    }
    let mut boundary = limit;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
    value.push_str("\n... output truncated");
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    struct FakeExternalTool;

    #[async_trait]
    impl ExternalTool for FakeExternalTool {
        fn name(&self) -> &str {
            "fake__ping"
        }

        fn definition(&self) -> ToolDefinition {
            ToolDefinition {
                name: self.name().to_owned(),
                description: "Ping".to_owned(),
                parameters: json!({ "type": "object" }),
            }
        }

        fn requires_confirmation(&self) -> bool {
            true
        }

        fn preview(&self, arguments: &str) -> Option<String> {
            Some(format!("Fake {arguments}"))
        }

        async fn run(&self, _arguments: &str) -> Result<String> {
            Ok("pong".to_owned())
        }
    }

    fn workspace() -> PathBuf {
        let root = std::env::temp_dir().join(format!("kumo-tools-{}", Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("hello.txt"), "hello").unwrap();
        root
    }

    fn test_database() -> Arc<Mutex<Database>> {
        Arc::new(Mutex::new(Database::open_in_memory_for_tests()))
    }

    fn registry(root: PathBuf, extra: Vec<Arc<dyn ExternalTool>>) -> ToolRegistry {
        ToolRegistry::new(root, extra, test_database(), chrono_tz::UTC).unwrap()
    }

    #[tokio::test]
    async fn reads_and_lists_workspace_content() {
        let root = workspace();
        let tools = registry(root.clone(), Vec::new());

        assert_eq!(
            tools
                .dispatch(
                    42,
                    &ToolCall {
                        id: "1".into(),
                        name: "read_file".into(),
                        arguments: r#"{"path":"hello.txt"}"#.into(),
                    }
                )
                .await,
            "hello"
        );
        assert!(
            tools
                .dispatch(
                    42,
                    &ToolCall {
                        id: "2".into(),
                        name: "list_directory".into(),
                        arguments: r#"{"path":"."}"#.into(),
                    }
                )
                .await
                .contains("hello.txt")
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn rejects_paths_outside_workspace() {
        let root = workspace();
        let outside_name = format!("kumo-outside-{}.txt", Uuid::new_v4());
        let outside = root.parent().unwrap().join(&outside_name);
        std::fs::write(&outside, "secret").unwrap();
        let tools = registry(root.clone(), Vec::new());
        let output = tools
            .dispatch(
                42,
                &ToolCall {
                    id: "1".into(),
                    name: "read_file".into(),
                    arguments: format!(r#"{{"path":"../{outside_name}"}}"#),
                },
            )
            .await;

        assert!(output.contains("escapes the configured workspace"));
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_file(outside).unwrap();
    }

    #[tokio::test]
    async fn runs_commands_in_workspace() {
        let root = workspace();
        let tools = registry(root.clone(), Vec::new());
        let command = if cfg!(windows) {
            "type hello.txt"
        } else {
            "cat hello.txt"
        };
        let output = tools
            .dispatch(
                42,
                &ToolCall {
                    id: "1".into(),
                    name: "run_command".into(),
                    arguments: serde_json::json!({ "command": command }).to_string(),
                },
            )
            .await;

        assert!(output.starts_with("exit code: 0"));
        assert!(output.contains("hello"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn truncates_command_output_on_utf8_boundaries() {
        let value = format!("{}é", "a".repeat(MAX_COMMAND_OUTPUT));
        let output = truncate_utf8(value, MAX_COMMAND_OUTPUT + 1);

        assert!(output.ends_with("... output truncated"));
        assert!(output.is_char_boundary(MAX_COMMAND_OUTPUT));
    }

    #[tokio::test]
    async fn routes_external_tools_through_shared_policy() {
        let root = workspace();
        let tools = registry(root.clone(), vec![Arc::new(FakeExternalTool)]);
        let call = ToolCall {
            id: "1".into(),
            name: "fake__ping".into(),
            arguments: "{}".into(),
        };

        assert!(
            tools
                .definitions()
                .iter()
                .any(|tool| tool.name == call.name)
        );
        assert!(tools.requires_confirmation(&call.name));
        assert_eq!(tools.preview(&call).as_deref(), Some("Fake {}"));
        assert_eq!(tools.dispatch(42, &call).await, "pong");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn parse_task_rejects_empty_or_blank_input() {
        assert!(parse_task(r#"{"task":""}"#).is_err());
        assert!(parse_task(r#"{"task":"   "}"#).is_err());
        assert!(parse_task("not json").is_err());
    }

    #[test]
    fn parse_task_trims_and_returns_the_task() {
        assert_eq!(
            parse_task(r#"{"task":"  add a test  "}"#).unwrap(),
            "add a test"
        );
    }

    #[test]
    fn delegate_to_kamui_always_requires_confirmation() {
        let tools = registry(std::env::temp_dir(), Vec::new());
        assert!(tools.requires_confirmation("delegate_to_kamui"));
    }

    #[test]
    fn delegate_to_kamui_preview_shows_the_task() {
        let tools = registry(std::env::temp_dir(), Vec::new());
        let call = ToolCall {
            id: "1".into(),
            name: "delegate_to_kamui".into(),
            arguments: r#"{"task":"fix the failing test"}"#.into(),
        };

        assert_eq!(
            tools.preview(&call).as_deref(),
            Some("Kamui task: fix the failing test")
        );
    }

    #[tokio::test]
    async fn delegate_to_kamui_rejects_a_blank_task_before_spawning_kamui() {
        // Whether `kamui` is actually installed on the machine running the suite varies, so this
        // only exercises the argument-validation path shared with parse_task, which runs before
        // any process is spawned and is deterministic either way.
        let tools = registry(std::env::temp_dir(), Vec::new());
        let call = ToolCall {
            id: "1".into(),
            name: "delegate_to_kamui".into(),
            arguments: r#"{"task":""}"#.into(),
        };

        assert!(tools.dispatch(42, &call).await.starts_with("Error:"));
    }

    #[test]
    fn schedule_task_never_requires_confirmation() {
        let tools = registry(std::env::temp_dir(), Vec::new());
        assert!(!tools.requires_confirmation("schedule_task"));
    }

    fn schedule_call(prompt: &str, run_at: &str, repeat: Option<i64>) -> ToolCall {
        let mut arguments = serde_json::json!({ "prompt": prompt, "run_at": run_at });
        if let Some(repeat) = repeat {
            arguments["repeat_interval_seconds"] = repeat.into();
        }
        ToolCall {
            id: "1".into(),
            name: "schedule_task".into(),
            arguments: arguments.to_string(),
        }
    }

    fn soon() -> String {
        (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339()
    }

    #[tokio::test]
    async fn a_reminder_can_be_listed_and_cancelled_by_the_model() {
        let tools = registry(std::env::temp_dir(), Vec::new());
        tools
            .dispatch(42, &schedule_call("ping", &soon(), Some(3600)))
            .await;

        let listed = tools
            .dispatch(
                42,
                &ToolCall {
                    id: "1".into(),
                    name: "list_scheduled_tasks".into(),
                    arguments: "{}".into(),
                },
            )
            .await;
        assert!(listed.contains("ping"), "{listed}");
        assert!(listed.contains("repeats every 3600s"), "{listed}");

        let id = listed.split(':').next().unwrap().to_owned();
        let cancelled = tools
            .dispatch(
                42,
                &ToolCall {
                    id: "1".into(),
                    name: "cancel_scheduled_task".into(),
                    arguments: serde_json::json!({ "id": id }).to_string(),
                },
            )
            .await;
        assert!(cancelled.starts_with("cancelled"), "{cancelled}");

        let after = tools
            .dispatch(
                42,
                &ToolCall {
                    id: "1".into(),
                    name: "list_scheduled_tasks".into(),
                    arguments: "{}".into(),
                },
            )
            .await;
        assert_eq!(after, "no pending scheduled tasks");
    }

    #[tokio::test]
    async fn cancelling_is_scoped_to_the_asking_chat() {
        let tools = registry(std::env::temp_dir(), Vec::new());
        tools
            .dispatch(42, &schedule_call("mine", &soon(), None))
            .await;

        // Another chat must not see it, nor be able to cancel it by guessing a prefix.
        let listed = tools
            .dispatch(
                99,
                &ToolCall {
                    id: "1".into(),
                    name: "list_scheduled_tasks".into(),
                    arguments: "{}".into(),
                },
            )
            .await;
        assert_eq!(listed, "no pending scheduled tasks");
    }

    #[tokio::test]
    async fn a_per_minute_repeat_is_refused() {
        let tools = registry(std::env::temp_dir(), Vec::new());

        let output = tools
            .dispatch(42, &schedule_call("spam", &soon(), Some(60)))
            .await;

        assert!(output.starts_with("Error:"), "{output}");
        assert!(output.contains("at least 300 seconds"), "{output}");
    }

    #[tokio::test]
    async fn a_chat_cannot_pile_up_unbounded_pending_tasks() {
        let tools = registry(std::env::temp_dir(), Vec::new());
        for _ in 0..MAX_PENDING_TASKS {
            let output = tools
                .dispatch(42, &schedule_call("ping", &soon(), None))
                .await;
            assert!(output.starts_with("scheduled"), "{output}");
        }

        let output = tools
            .dispatch(42, &schedule_call("one too many", &soon(), None))
            .await;
        assert!(output.starts_with("Error:"), "{output}");
        assert!(output.contains("cancel_scheduled_task"), "{output}");
    }

    #[tokio::test]
    async fn schedule_task_rejects_an_invalid_timestamp() {
        let tools = registry(std::env::temp_dir(), Vec::new());
        let call = ToolCall {
            id: "1".into(),
            name: "schedule_task".into(),
            arguments: r#"{"prompt":"ping","run_at":"not a date"}"#.into(),
        };

        assert!(tools.dispatch(42, &call).await.starts_with("Error:"));
    }

    #[tokio::test]
    async fn schedule_task_rejects_a_time_in_the_past() {
        let tools = registry(std::env::temp_dir(), Vec::new());
        let call = ToolCall {
            id: "1".into(),
            name: "schedule_task".into(),
            arguments: r#"{"prompt":"ping","run_at":"2000-01-01T00:00:00+00:00"}"#.into(),
        };

        let output = tools.dispatch(42, &call).await;
        assert!(output.starts_with("Error:"), "{output}");
        assert!(output.contains("past"));
    }

    #[tokio::test]
    async fn schedule_task_rejects_a_time_too_far_in_the_future() {
        let tools = registry(std::env::temp_dir(), Vec::new());
        let call = ToolCall {
            id: "1".into(),
            name: "schedule_task".into(),
            arguments: r#"{"prompt":"ping","run_at":"2099-01-01T00:00:00+00:00"}"#.into(),
        };

        let output = tools.dispatch(42, &call).await;
        assert!(output.starts_with("Error:"), "{output}");
    }

    #[tokio::test]
    async fn schedule_task_persists_a_valid_future_task() {
        let database = test_database();
        let tools = ToolRegistry::new(
            std::env::temp_dir(),
            Vec::new(),
            database.clone(),
            chrono_tz::UTC,
        )
        .unwrap();
        let next_year = chrono::Utc::now() + chrono::Duration::days(30);
        let run_at = next_year.to_rfc3339();
        let call = ToolCall {
            id: "1".into(),
            name: "schedule_task".into(),
            arguments: serde_json::json!({ "prompt": "check the weather", "run_at": run_at })
                .to_string(),
        };

        let output = tools.dispatch(42, &call).await;
        assert!(output.starts_with("scheduled ("), "{output}");

        let due = database
            .lock()
            .await
            .claim_due_scheduled_tasks(next_year.timestamp() + 1)
            .unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].prompt, "check the weather");
        assert_eq!(due[0].telegram_chat_id, 42);
    }

    #[tokio::test]
    async fn schedule_task_persists_a_repeat_interval() {
        let database = test_database();
        let tools = ToolRegistry::new(
            std::env::temp_dir(),
            Vec::new(),
            database.clone(),
            chrono_tz::UTC,
        )
        .unwrap();
        let next_year = chrono::Utc::now() + chrono::Duration::days(30);
        let call = ToolCall {
            id: "1".into(),
            name: "schedule_task".into(),
            arguments: serde_json::json!({
                "prompt": "daily standup reminder",
                "run_at": next_year.to_rfc3339(),
                "repeat_interval_seconds": 86400,
            })
            .to_string(),
        };

        let output = tools.dispatch(42, &call).await;
        assert!(output.contains("repeating every 86400 seconds"), "{output}");

        let due = database
            .lock()
            .await
            .claim_due_scheduled_tasks(next_year.timestamp() + 1)
            .unwrap();
        assert_eq!(due[0].repeat_interval_seconds, Some(86400));
    }

    #[tokio::test]
    async fn schedule_task_rejects_a_repeat_interval_below_the_minimum() {
        let tools = registry(std::env::temp_dir(), Vec::new());
        let next_year = chrono::Utc::now() + chrono::Duration::days(30);
        let call = ToolCall {
            id: "1".into(),
            name: "schedule_task".into(),
            arguments: serde_json::json!({
                "prompt": "spam every second",
                "run_at": next_year.to_rfc3339(),
                "repeat_interval_seconds": 1,
            })
            .to_string(),
        };

        let output = tools.dispatch(42, &call).await;
        assert!(output.starts_with("Error:"), "{output}");
    }

    fn memory_call(name: &str, arguments: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "1".into(),
            name: name.into(),
            arguments: arguments.to_string(),
        }
    }

    #[tokio::test]
    async fn memory_tools_never_require_confirmation() {
        let tools = registry(std::env::temp_dir(), Vec::new());
        assert!(!tools.requires_confirmation("remember"));
        assert!(!tools.requires_confirmation("update_memory"));
        assert!(!tools.requires_confirmation("forget"));
    }

    #[tokio::test]
    async fn remember_persists_a_fact() {
        let database = test_database();
        let tools = ToolRegistry::new(
            std::env::temp_dir(),
            Vec::new(),
            database.clone(),
            chrono_tz::UTC,
        )
        .unwrap();

        let output = tools
            .dispatch(
                42,
                &memory_call(
                    "remember",
                    serde_json::json!({ "fact": "The user is a researcher." }),
                ),
            )
            .await;

        assert!(output.starts_with("remembered:"), "{output}");
        let entries = database.lock().await.list_memory().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].content, "The user is a researcher.");
    }

    #[tokio::test]
    async fn remember_rejects_a_blank_fact() {
        let tools = registry(std::env::temp_dir(), Vec::new());
        let output = tools
            .dispatch(
                42,
                &memory_call("remember", serde_json::json!({ "fact": "  " })),
            )
            .await;
        assert!(output.starts_with("Error:"), "{output}");
    }

    #[tokio::test]
    async fn remember_refuses_once_the_memory_cap_is_reached() {
        let database = test_database();
        let tools = ToolRegistry::new(
            std::env::temp_dir(),
            Vec::new(),
            database.clone(),
            chrono_tz::UTC,
        )
        .unwrap();
        database
            .lock()
            .await
            .remember(&"x".repeat(MAX_MEMORY_BYTES as usize))
            .unwrap();

        let output = tools
            .dispatch(
                42,
                &memory_call("remember", serde_json::json!({ "fact": "one more" })),
            )
            .await;

        assert!(
            output.starts_with("Error:") && output.contains("full"),
            "{output}"
        );
    }

    #[tokio::test]
    async fn update_memory_replaces_a_matched_fact() {
        let database = test_database();
        let tools = ToolRegistry::new(
            std::env::temp_dir(),
            Vec::new(),
            database.clone(),
            chrono_tz::UTC,
        )
        .unwrap();
        database
            .lock()
            .await
            .remember("The user is a researcher.")
            .unwrap();

        let output = tools
            .dispatch(
                42,
                &memory_call(
                    "update_memory",
                    serde_json::json!({ "matching": "is a researcher", "fact": "is a software engineer." }),
                ),
            )
            .await;

        assert!(output.starts_with("updated memory"), "{output}");
        let entries = database.lock().await.list_memory().unwrap();
        assert_eq!(entries[0].content, "is a software engineer.");
    }

    #[tokio::test]
    async fn update_memory_reports_an_error_when_nothing_matches() {
        let tools = registry(std::env::temp_dir(), Vec::new());
        let output = tools
            .dispatch(
                42,
                &memory_call(
                    "update_memory",
                    serde_json::json!({ "matching": "nonexistent", "fact": "x" }),
                ),
            )
            .await;
        assert!(output.starts_with("Error:"), "{output}");
    }

    #[tokio::test]
    async fn update_memory_names_the_facts_an_ambiguous_substring_matched() {
        let database = test_database();
        let tools = ToolRegistry::new(
            std::env::temp_dir(),
            Vec::new(),
            database.clone(),
            chrono_tz::UTC,
        )
        .unwrap();
        {
            let database = database.lock().await;
            database.remember("The user likes tea.").unwrap();
            database.remember("The user likes coffee.").unwrap();
        }

        let output = tools
            .dispatch(
                42,
                &memory_call(
                    "update_memory",
                    serde_json::json!({ "matching": "The user likes", "fact": "x" }),
                ),
            )
            .await;

        assert!(output.starts_with("Error:"), "{output}");
        assert!(output.contains("The user likes tea."), "{output}");
        assert!(output.contains("The user likes coffee."), "{output}");
        // The model needs enough to retry, and neither entry may be modified meanwhile.
        assert_eq!(database.lock().await.list_memory().unwrap().len(), 2);
    }

    #[test]
    fn an_ambiguous_match_lists_a_bounded_number_of_facts() {
        let entries: Vec<String> = (0..9).map(|index| format!("fact number {index}")).collect();

        let message = ambiguous_memory_error("fact", &entries);

        assert!(message.contains("matches 9 remembered facts"), "{message}");
        assert!(message.contains("fact number 4"), "{message}");
        assert!(!message.contains("fact number 5"), "{message}");
        assert!(message.contains("...and 4 more"), "{message}");
    }

    #[test]
    fn a_long_fact_is_truncated_in_an_ambiguous_match() {
        let long = "x".repeat(MAX_AMBIGUOUS_FACT_CHARS + 50);

        let message = ambiguous_memory_error("x", &[long, "short".to_owned()]);

        assert!(message.contains(&format!("{}...", "x".repeat(MAX_AMBIGUOUS_FACT_CHARS))));
        assert!(!message.contains(&"x".repeat(MAX_AMBIGUOUS_FACT_CHARS + 1)));
    }

    #[tokio::test]
    async fn forget_removes_a_matched_fact() {
        let database = test_database();
        let tools = ToolRegistry::new(
            std::env::temp_dir(),
            Vec::new(),
            database.clone(),
            chrono_tz::UTC,
        )
        .unwrap();
        database
            .lock()
            .await
            .remember("The user is a researcher.")
            .unwrap();

        let output = tools
            .dispatch(
                42,
                &memory_call("forget", serde_json::json!({ "matching": "researcher" })),
            )
            .await;

        assert!(output.starts_with("forgot"), "{output}");
        assert!(database.lock().await.list_memory().unwrap().is_empty());
    }

    #[tokio::test]
    async fn forget_reports_an_error_when_nothing_matches() {
        let tools = registry(std::env::temp_dir(), Vec::new());
        let output = tools
            .dispatch(
                42,
                &memory_call("forget", serde_json::json!({ "matching": "nonexistent" })),
            )
            .await;
        assert!(output.starts_with("Error:"), "{output}");
    }

    /// Builds a real `std::process::Output` with the given exit code, stdout, and stderr, using a
    /// throwaway shell invocation so the `ExitStatus` is constructed the same way the real
    /// `delegate_to_kamui` code path receives one (no private-field access or unsafe needed).
    fn fake_output(code: i32, stdout: &str, stderr: &str) -> std::process::Output {
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("exit {code}"))
            .status()
            .unwrap();
        std::process::Output {
            status,
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[test]
    fn summarizes_a_clean_run_with_no_tool_calls_as_just_the_answer() {
        let output = fake_output(0, "\nThe answer is 42.", "");
        assert_eq!(summarize_kamui_output(&output), "The answer is 42.");
    }

    #[test]
    fn summarizes_a_clean_run_with_tool_calls() {
        let stdout =
            "  \u{2192} read_file(src/main.rs)\n    ok (120 chars)\n\nDone reading the file.";
        let output = fake_output(0, stdout, "");
        let summary = summarize_kamui_output(&output);
        assert!(
            summary.starts_with("(1 tool call(s), 0 error(s))\n"),
            "{summary}"
        );
        assert!(summary.ends_with("Done reading the file."), "{summary}");
    }

    #[test]
    fn summarizes_a_run_with_a_tool_error() {
        let stdout = "  \u{2192} run_command(false)\n    ! exit code 1\n\nThe command failed.";
        let output = fake_output(0, stdout, "");
        let summary = summarize_kamui_output(&output);
        assert!(
            summary.starts_with("(1 tool call(s), 1 error(s))\n"),
            "{summary}"
        );
    }

    #[test]
    fn summarizes_a_nonzero_exit() {
        let output = fake_output(1, "  \u{2192} run_command(false)\n    ! exit code 1", "");
        let summary = summarize_kamui_output(&output);
        assert!(
            summary.starts_with("(kamui exited with code 1, 1 tool call(s))\n"),
            "{summary}"
        );
        assert!(summary.contains("Error: kamui produced no answer."));
    }

    #[test]
    fn falls_back_to_stderr_when_there_is_no_answer() {
        let output = fake_output(1, "", "panicked at src/main.rs:1");
        let summary = summarize_kamui_output(&output);
        assert!(summary.contains("Error: kamui produced no answer."));
        assert!(summary.contains("panicked at src/main.rs:1"));
    }
}
