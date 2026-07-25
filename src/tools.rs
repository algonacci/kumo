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
    storage::Database,
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
            description: "Schedule a one-shot message to yourself for a future time. At the \
                          scheduled time, the given prompt is sent through your own agent loop \
                          (with all the same tools) as if the user had just sent it, and the \
                          result is delivered to the user in this chat. Use the user's configured \
                          timezone (given in the system prompt) to compute run_at."
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
                        "description": "When to run, as RFC 3339 with a UTC offset, e.g. \"2026-07-27T09:00:00+07:00\"."
                    }
                },
                "required": ["prompt", "run_at"]
            }),
        });
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
                Ok(format_command_output(&output))
            }
            Err(_) => Ok(format!(
                "Error: kamui timed out after {} seconds and was terminated",
                KAMUI_TIMEOUT.as_secs()
            )),
        }
    }

    /// Record a one-shot scheduled task. Actually running it later is the scheduler loop's job
    /// (`src/scheduler.rs`); this only validates and persists the request.
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

        let id = self.database.lock().await.create_scheduled_task(
            chat_id,
            prompt,
            run_at.timestamp(),
        )?;
        let local_time = run_at.with_timezone(&self.timezone);
        Ok(format!(
            "scheduled ({}): will run at {} ({})",
            &id[..8],
            local_time.format("%Y-%m-%d %H:%M %Z"),
            self.timezone
        ))
    }
}

/// Whether the `kamui` binary is reachable on `PATH`. Detected once per process; delegation is an
/// optional capability, never a requirement for Kumo's built-in tools.
fn kamui_available() -> bool {
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
struct ScheduleArguments {
    prompt: String,
    run_at: String,
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
            .due_scheduled_tasks(next_year.timestamp() + 1)
            .unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].prompt, "check the weather");
        assert_eq!(due[0].telegram_chat_id, 42);
    }
}
