use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::json;

use crate::provider::{ToolCall, ToolDefinition};

const MAX_FILE_SIZE: u64 = 64 * 1024;
const MAX_DIRECTORY_ENTRIES: usize = 200;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_COMMAND_OUTPUT: usize = 16 * 1024;

#[derive(Clone)]
pub struct ToolRegistry {
    root: PathBuf,
}

impl ToolRegistry {
    pub fn new(root: PathBuf) -> Result<Self> {
        let root = root
            .canonicalize()
            .with_context(|| format!("could not resolve workspace {}", root.display()))?;
        if !root.is_dir() {
            bail!("workspace is not a directory: {}", root.display());
        }
        Ok(Self { root })
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        vec![
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
        ]
    }

    pub fn requires_confirmation(&self, name: &str) -> bool {
        name == "run_command"
    }

    pub fn preview(&self, call: &ToolCall) -> Option<String> {
        (call.name == "run_command")
            .then(|| parse_command(&call.arguments).ok())
            .flatten()
    }

    pub async fn dispatch(&self, call: &ToolCall) -> String {
        let result = match call.name.as_str() {
            "read_file" => self.read_file(&call.arguments),
            "list_directory" => self.list_directory(&call.arguments),
            "run_command" => self.run_command(&call.arguments).await,
            _ => Err(anyhow::anyhow!("unknown tool '{}'", call.name)),
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
}

#[derive(Deserialize)]
struct PathArguments {
    path: String,
}

#[derive(Deserialize)]
struct CommandArguments {
    command: String,
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

    fn workspace() -> PathBuf {
        let root = std::env::temp_dir().join(format!("kumo-tools-{}", Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("hello.txt"), "hello").unwrap();
        root
    }

    #[tokio::test]
    async fn reads_and_lists_workspace_content() {
        let root = workspace();
        let tools = ToolRegistry::new(root.clone()).unwrap();

        assert_eq!(
            tools
                .dispatch(&ToolCall {
                    id: "1".into(),
                    name: "read_file".into(),
                    arguments: r#"{"path":"hello.txt"}"#.into(),
                })
                .await,
            "hello"
        );
        assert!(
            tools
                .dispatch(&ToolCall {
                    id: "2".into(),
                    name: "list_directory".into(),
                    arguments: r#"{"path":"."}"#.into(),
                })
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
        let tools = ToolRegistry::new(root.clone()).unwrap();
        let output = tools
            .dispatch(&ToolCall {
                id: "1".into(),
                name: "read_file".into(),
                arguments: format!(r#"{{"path":"../{outside_name}"}}"#),
            })
            .await;

        assert!(output.contains("escapes the configured workspace"));
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_file(outside).unwrap();
    }

    #[tokio::test]
    async fn runs_commands_in_workspace() {
        let root = workspace();
        let tools = ToolRegistry::new(root.clone()).unwrap();
        let command = if cfg!(windows) {
            "type hello.txt"
        } else {
            "cat hello.txt"
        };
        let output = tools
            .dispatch(&ToolCall {
                id: "1".into(),
                name: "run_command".into(),
                arguments: serde_json::json!({ "command": command }).to_string(),
            })
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
}
