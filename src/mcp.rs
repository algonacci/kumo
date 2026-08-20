//! MCP stdio client. Configured servers are launched at startup and their advertised tools join
//! Kumo's registry under qualified names so they use the same agent loop and Telegram approvals.

use std::{process::Stdio, sync::Arc, time::Instant};

use anyhow::{Context, Result};
use async_trait::async_trait;
use base64::Engine;
use rmcp::{
    model::CallToolRequestParams,
    service::{RoleClient, RunningService, ServiceExt},
    transport::TokioChildProcess,
};
use serde_json::Value;

use crate::{config::McpServerConfig, logging, provider::ToolDefinition, tools::ExternalTool};

const MEDIA_RESULT_PREFIX: &str = "__KUMO_MCP_MEDIA__";
const MAX_MCP_IMAGE_BYTES: usize = 10 * 1024 * 1024;

pub(crate) struct McpImage {
    pub(crate) mime_type: String,
    pub(crate) data: Vec<u8>,
}

pub struct ConnectionStatus {
    pub name: String,
    pub tool_count: usize,
    /// How many of this server's tools skip the approval prompt — all of them when the server
    /// itself is `trusted`, otherwise however many `trusted_tools` named.
    pub trusted_count: usize,
    pub error: Option<String>,
}

impl ConnectionStatus {
    /// " [trusted]" when nothing this server offers needs approval, " [n trusted]" when only some
    /// of it does, and nothing at all when every call is gated.
    pub fn trust_label(&self) -> String {
        match self.trusted_count {
            0 => String::new(),
            count if count == self.tool_count => " [trusted]".to_owned(),
            count => format!(" [{count} trusted]"),
        }
    }
}

pub struct Connections {
    pub tools: Vec<Arc<dyn ExternalTool>>,
    pub statuses: Vec<ConnectionStatus>,
}

pub async fn connect_all(
    servers: &std::collections::BTreeMap<String, McpServerConfig>,
) -> Connections {
    let mut tools = Vec::new();
    let mut statuses = Vec::new();
    for (name, server) in servers {
        let started = Instant::now();
        logging::info("mcp", format!("server={name} status=connecting"));
        match connect(name, server).await {
            Ok(mut connected) => {
                logging::info(
                    "mcp",
                    format!(
                        "server={name} status=initialized tools={} duration_ms={}",
                        connected.len(),
                        started.elapsed().as_millis()
                    ),
                );
                statuses.push(ConnectionStatus {
                    name: name.clone(),
                    tool_count: connected.len(),
                    trusted_count: connected
                        .iter()
                        .filter(|tool| !tool.requires_confirmation())
                        .count(),
                    error: None,
                });
                tools.append(&mut connected);
            }
            Err(error) => {
                logging::error(
                    "mcp",
                    format!(
                        "server={name} status=failed duration_ms={}",
                        started.elapsed().as_millis()
                    ),
                    &error,
                );
                statuses.push(ConnectionStatus {
                    name: name.clone(),
                    tool_count: 0,
                    trusted_count: 0,
                    error: Some(format!("{error:#}")),
                });
            }
        }
    }
    Connections { tools, statuses }
}

/// Resolve a bare program name the way a shell would on Windows, and only there.
///
/// `Command::new` spawns a program; it does not consult `PATHEXT`. Most of the MCP ecosystem is
/// launched through `npx`, `uvx` or `npm`, which on Windows are `.cmd` shims rather than `.exe`
/// files — so the README's own `command = "npx"` example failed with "program not found" while
/// the shim sat on `PATH`. Spawning through `cmd /C` would fix that too, but it would also hand
/// every configured argument to a shell for re-parsing, which is a quoting hazard nobody asked
/// for. Finding the file the shell would have found keeps the spawn direct.
///
/// A name that already carries a path or an extension is left alone, and so is one nothing
/// matches — that falls through to the original spawn error, which names the command the user
/// actually wrote.
#[cfg(windows)]
fn resolve_program(command: &str) -> std::ffi::OsString {
    use std::path::Path;

    if command.contains(['/', '\\']) || Path::new(command).extension().is_some() {
        return command.into();
    }
    let extensions = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_owned());
    let Some(path) = std::env::var_os("PATH") else {
        return command.into();
    };
    for directory in std::env::split_paths(&path) {
        for extension in extensions.split(';').filter(|ext| !ext.is_empty()) {
            let candidate = directory.join(format!("{command}{extension}"));
            if candidate.is_file() {
                return candidate.into_os_string();
            }
        }
    }
    command.into()
}

#[cfg(not(windows))]
fn resolve_program(command: &str) -> std::ffi::OsString {
    command.into()
}

async fn connect(name: &str, server: &McpServerConfig) -> Result<Vec<Arc<dyn ExternalTool>>> {
    let mut command = tokio::process::Command::new(resolve_program(&server.command));
    command.args(&server.args);
    let (transport, _stderr) = TokioChildProcess::builder(command)
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("failed to start '{}'", server.command))?;
    let service = ().serve(transport).await.context("MCP initialization failed")?;
    let listed = service
        .list_all_tools()
        .await
        .context("could not list MCP tools")?;
    let service = Arc::new(service);

    Ok(listed
        .into_iter()
        .map(|tool| {
            Arc::new(McpTool {
                qualified_name: format!("{name}__{}", tool.name),
                remote_name: tool.name.to_string(),
                description: tool.description.as_deref().unwrap_or_default().to_string(),
                schema: Value::Object(tool.input_schema.as_ref().clone()),
                trusted: server.trusts(&tool.name),
                service: service.clone(),
            }) as Arc<dyn ExternalTool>
        })
        .collect())
}

struct McpTool {
    qualified_name: String,
    remote_name: String,
    description: String,
    schema: Value,
    trusted: bool,
    service: Arc<RunningService<RoleClient, ()>>,
}

#[async_trait]
impl ExternalTool for McpTool {
    fn name(&self) -> &str {
        &self.qualified_name
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.qualified_name.clone(),
            description: self.description.clone(),
            parameters: self.schema.clone(),
        }
    }

    fn requires_confirmation(&self) -> bool {
        !self.trusted
    }

    fn preview(&self, arguments: &str) -> Option<String> {
        Some(format!("MCP {} {}", self.remote_name, arguments.trim()))
    }

    async fn run(&self, arguments: &str) -> Result<String> {
        let value: Value =
            serde_json::from_str(arguments).context("tool arguments were not valid JSON")?;
        let mut request = CallToolRequestParams::new(self.remote_name.clone());
        if let Some(arguments) = value.as_object().cloned() {
            request = request.with_arguments(arguments);
        }
        let result = self
            .service
            .call_tool(request)
            .await
            .with_context(|| format!("MCP tool '{}' failed", self.qualified_name))?;
        Ok(render_result(&result))
    }
}

fn render_result<T: serde::Serialize>(result: &T) -> String {
    let value = serde_json::to_value(result).unwrap_or(Value::Null);
    let is_error = value
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let texts: Vec<&str> = value
        .get("content")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("text").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default();
    let images: Vec<Value> = value
        .get("content")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|item| item.get("type").and_then(Value::as_str) == Some("image"))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    let body = if texts.is_empty() {
        value.to_string()
    } else {
        texts.join("\n")
    };
    let body = if is_error {
        format!("Error: {body}")
    } else {
        body
    };
    if images.is_empty() {
        body
    } else {
        format!(
            "{MEDIA_RESULT_PREFIX}{}",
            serde_json::json!({"text": body, "images": images})
        )
    }
}

pub(crate) fn extract_media(output: String) -> (String, Vec<McpImage>) {
    let Some(payload) = output.strip_prefix(MEDIA_RESULT_PREFIX) else {
        return (output, Vec::new());
    };
    let Ok(value) = serde_json::from_str::<Value>(payload) else {
        return (output, Vec::new());
    };
    let text = value
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let images = value
        .get("images")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|image| {
            let mime_type = image.get("mimeType")?.as_str()?.to_owned();
            let encoded = image.get("data")?.as_str()?;
            let data = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .ok()?;
            (data.len() <= MAX_MCP_IMAGE_BYTES).then_some(McpImage { mime_type, data })
        })
        .collect();
    (text, images)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The failure this guards against is Windows-only, and so is the fix: `Command::new("npx")`
    /// reports "program not found" while `npx.cmd` sits on `PATH`, because spawning does not
    /// consult `PATHEXT`.
    #[cfg(windows)]
    #[test]
    fn a_bare_shim_name_resolves_to_the_file_a_shell_would_run() {
        let resolved = resolve_program("cmd");
        let resolved = std::path::Path::new(&resolved);
        assert!(
            resolved.is_file(),
            "a bare name on PATH must resolve to a real file, got {}",
            resolved.display()
        );
        assert!(
            resolved.extension().is_some(),
            "and it must carry the extension"
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_name_that_matches_nothing_is_left_for_the_spawn_error_to_report() {
        assert_eq!(
            resolve_program("kumo-no-such-program"),
            std::ffi::OsString::from("kumo-no-such-program")
        );
    }

    #[cfg(windows)]
    #[test]
    fn an_explicit_path_or_extension_is_never_rewritten() {
        assert_eq!(
            resolve_program("node.exe"),
            std::ffi::OsString::from("node.exe")
        );
        assert_eq!(
            resolve_program("C:/tools/thing"),
            std::ffi::OsString::from("C:/tools/thing")
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn resolution_is_a_passthrough_off_windows() {
        assert_eq!(resolve_program("npx"), std::ffi::OsString::from("npx"));
    }

    #[test]
    fn renders_text_content_parts() {
        let result = json!({
            "content": [{"type": "text", "text": "first"}, {"type": "text", "text": "second"}]
        });
        assert_eq!(render_result(&result), "first\nsecond");
    }

    #[test]
    fn marks_error_results() {
        let result = json!({
            "content": [{"type": "text", "text": "failed"}],
            "isError": true
        });
        assert_eq!(render_result(&result), "Error: failed");
    }

    #[test]
    fn extracts_image_content_without_leaking_base64_into_text() {
        let result = json!({
            "content": [
                {"type": "text", "text": "chart metadata"},
                {"type": "image", "mimeType": "image/png", "data": "iVBORw=="}
            ]
        });
        let (text, images) = extract_media(render_result(&result));
        assert_eq!(text, "chart metadata");
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].mime_type, "image/png");
        assert_eq!(images[0].data, b"\x89PNG");
    }
}
