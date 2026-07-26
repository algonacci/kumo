//! MCP stdio client. Configured servers are launched at startup and their advertised tools join
//! Kumo's registry under qualified names so they use the same agent loop and Telegram approvals.

use std::{process::Stdio, sync::Arc};

use anyhow::{Context, Result};
use async_trait::async_trait;
use base64::Engine;
use rmcp::{
    model::CallToolRequestParam,
    service::{RoleClient, RunningService, ServiceExt},
    transport::TokioChildProcess,
};
use serde_json::Value;

use crate::{config::McpServerConfig, provider::ToolDefinition, tools::ExternalTool};

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
        match connect(name, server).await {
            Ok(mut connected) => {
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
            Err(error) => statuses.push(ConnectionStatus {
                name: name.clone(),
                tool_count: 0,
                trusted_count: 0,
                error: Some(format!("{error:#}")),
            }),
        }
    }
    Connections { tools, statuses }
}

async fn connect(name: &str, server: &McpServerConfig) -> Result<Vec<Arc<dyn ExternalTool>>> {
    let mut command = tokio::process::Command::new(&server.command);
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
        let result = self
            .service
            .call_tool(CallToolRequestParam {
                name: self.remote_name.clone().into(),
                arguments: value.as_object().cloned(),
            })
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
