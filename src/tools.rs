use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::json;

use crate::provider::{ToolCall, ToolDefinition};

const MAX_FILE_SIZE: u64 = 64 * 1024;
const MAX_DIRECTORY_ENTRIES: usize = 200;

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
        ]
    }

    pub fn dispatch(&self, call: &ToolCall) -> String {
        let result = match call.name.as_str() {
            "read_file" => self.read_file(&call.arguments),
            "list_directory" => self.list_directory(&call.arguments),
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
}

#[derive(Deserialize)]
struct PathArguments {
    path: String,
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

    #[test]
    fn reads_and_lists_workspace_content() {
        let root = workspace();
        let tools = ToolRegistry::new(root.clone()).unwrap();

        assert_eq!(
            tools.dispatch(&ToolCall {
                id: "1".into(),
                name: "read_file".into(),
                arguments: r#"{"path":"hello.txt"}"#.into(),
            }),
            "hello"
        );
        assert!(
            tools
                .dispatch(&ToolCall {
                    id: "2".into(),
                    name: "list_directory".into(),
                    arguments: r#"{"path":"."}"#.into(),
                })
                .contains("hello.txt")
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_paths_outside_workspace() {
        let root = workspace();
        let outside_name = format!("kumo-outside-{}.txt", Uuid::new_v4());
        let outside = root.parent().unwrap().join(&outside_name);
        std::fs::write(&outside, "secret").unwrap();
        let tools = ToolRegistry::new(root.clone()).unwrap();
        let output = tools.dispatch(&ToolCall {
            id: "1".into(),
            name: "read_file".into(),
            arguments: format!(r#"{{"path":"../{outside_name}"}}"#),
        });

        assert!(output.contains("escapes the configured workspace"));
        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_file(outside).unwrap();
    }
}
