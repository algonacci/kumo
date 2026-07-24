//! MCP stdio client. Configured servers are launched at startup and their advertised tools join
//! Kumo's registry under qualified names so they use the same agent loop and Telegram approvals.

use std::{process::Stdio, sync::Arc};

use anyhow::{Context, Result};
use async_trait::async_trait;
use rmcp::{
    model::CallToolRequestParam,
    service::{RoleClient, RunningService, ServiceExt},
    transport::TokioChildProcess,
};
use serde_json::Value;

use crate::{config::McpServerConfig, provider::ToolDefinition, tools::ExternalTool};

pub struct ConnectionStatus {
    pub name: String,
    pub tool_count: usize,
    pub trusted: bool,
    pub error: Option<String>,
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
                    trusted: server.trusted,
                    error: None,
                });
                tools.append(&mut connected);
            }
            Err(error) => statuses.push(ConnectionStatus {
                name: name.clone(),
                tool_count: 0,
                trusted: server.trusted,
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
                trusted: server.trusted,
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
    let body = if texts.is_empty() {
        value.to_string()
    } else {
        texts.join("\n")
    };
    if is_error {
        format!("Error: {body}")
    } else {
        body
    }
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
}
