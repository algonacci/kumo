use anyhow::{Context, Result, bail};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::ProviderConfig;

#[derive(Clone)]
pub struct Provider {
    client: Client,
    config: ProviderConfig,
}

#[derive(Clone, Debug)]
pub struct Message {
    pub(crate) role: Role,
    pub(crate) content: String,
    pub(crate) tool_calls: Vec<ToolCall>,
    pub(crate) tool_call_id: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) enum Role {
    System,
    User,
    Assistant,
    Tool,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self::text(Role::System, content)
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::text(Role::User, content)
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self::text(Role::Assistant, content)
    }

    pub fn tool_request(content: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_calls,
            tool_call_id: None,
        }
    }

    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
        }
    }

    fn text(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    pub(crate) fn role_name(&self) -> &'static str {
        match self.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }

    pub(crate) fn from_stored(
        role: &str,
        content: String,
        tool_calls: Vec<ToolCall>,
        tool_call_id: Option<String>,
    ) -> Result<Self> {
        let role = match role {
            "system" => Role::System,
            "user" => Role::User,
            "assistant" => Role::Assistant,
            "tool" => Role::Tool,
            _ => bail!("unknown stored message role: {role}"),
        };
        Ok(Self {
            role,
            content,
            tool_calls,
            tool_call_id,
        })
    }
}

#[derive(Clone, Debug)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

pub struct ChatResponse {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
    pub finish_reason: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
}

impl Provider {
    pub fn new(config: ProviderConfig) -> Self {
        Self {
            client: Client::new(),
            config,
        }
    }

    pub fn active_model(&self) -> &str {
        &self.config.active_model
    }

    pub async fn chat(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<ChatResponse> {
        let response = authorized(
            self.client
                .post(endpoint(&self.config.base_url, "chat/completions")),
            &self.config.api_key,
        )
        .json(&ChatRequest {
            model: &self.config.active_model,
            messages: messages.iter().map(WireMessage::from).collect(),
            tools: tools.iter().map(WireTool::from).collect(),
        })
        .send()
        .await
        .context("could not reach the model provider")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            bail!("provider returned {status}: {}", error_message(&body));
        }

        let mut response: WireChatResponse = response
            .json()
            .await
            .context("provider returned an invalid chat response")?;
        let choice = response
            .choices
            .pop()
            .context("provider returned no choices")?;
        Ok(ChatResponse {
            content: choice.message.content.unwrap_or_default(),
            tool_calls: choice
                .message
                .tool_calls
                .into_iter()
                .map(|call| ToolCall {
                    id: call.id,
                    name: call.function.name,
                    arguments: call.function.arguments,
                })
                .collect(),
            usage: response.usage,
            finish_reason: choice.finish_reason,
        })
    }
}

pub async fn list_models(base_url: &str, api_key: &str) -> Result<Vec<String>> {
    let response = authorized(Client::new().get(endpoint(base_url, "models")), api_key)
        .send()
        .await
        .context("could not reach the provider")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        bail!("provider returned {status}: {}", error_message(&body));
    }

    let response: ModelsResponse = response
        .json()
        .await
        .context("provider returned an invalid models response")?;
    let mut models = response
        .data
        .into_iter()
        .map(|model| model.id)
        .collect::<Vec<_>>();
    models.sort();
    models.dedup();

    if models.is_empty() {
        bail!("provider returned no models");
    }
    Ok(models)
}

fn endpoint(base_url: &str, path: &str) -> String {
    format!("{}/{}", base_url.trim_end_matches('/'), path)
}

fn authorized(request: reqwest::RequestBuilder, api_key: &str) -> reqwest::RequestBuilder {
    if api_key.is_empty() {
        request
    } else {
        request.bearer_auth(api_key)
    }
}

fn error_message(body: &str) -> String {
    serde_json::from_str::<ErrorResponse>(body)
        .ok()
        .map(|response| response.error.message)
        .filter(|message| !message.trim().is_empty())
        .unwrap_or_else(|| body.chars().take(300).collect())
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<WireMessage<'a>>,
    tools: Vec<WireTool<'a>>,
}

#[derive(Serialize)]
struct WireMessage<'a> {
    role: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<&'a str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<WireToolCall<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<&'a str>,
}

impl<'a> From<&'a Message> for WireMessage<'a> {
    fn from(message: &'a Message) -> Self {
        Self {
            role: message.role_name(),
            content: if message.content.is_empty() && !message.tool_calls.is_empty() {
                None
            } else {
                Some(&message.content)
            },
            tool_calls: message.tool_calls.iter().map(WireToolCall::from).collect(),
            tool_call_id: message.tool_call_id.as_deref(),
        }
    }
}

#[derive(Serialize)]
struct WireToolCall<'a> {
    id: &'a str,
    #[serde(rename = "type")]
    kind: &'static str,
    function: WireFunctionCall<'a>,
}

impl<'a> From<&'a ToolCall> for WireToolCall<'a> {
    fn from(call: &'a ToolCall) -> Self {
        Self {
            id: &call.id,
            kind: "function",
            function: WireFunctionCall {
                name: &call.name,
                arguments: &call.arguments,
            },
        }
    }
}

#[derive(Serialize)]
struct WireFunctionCall<'a> {
    name: &'a str,
    arguments: &'a str,
}

#[derive(Serialize)]
struct WireTool<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    function: WireToolFunction<'a>,
}

impl<'a> From<&'a ToolDefinition> for WireTool<'a> {
    fn from(tool: &'a ToolDefinition) -> Self {
        Self {
            kind: "function",
            function: WireToolFunction {
                name: &tool.name,
                description: &tool.description,
                parameters: &tool.parameters,
            },
        }
    }
}

#[derive(Serialize)]
struct WireToolFunction<'a> {
    name: &'a str,
    description: &'a str,
    parameters: &'a Value,
}

#[derive(Deserialize)]
struct WireChatResponse {
    choices: Vec<Choice>,
    #[serde(default)]
    usage: Usage,
}

#[derive(Deserialize)]
struct Choice {
    message: AssistantMessage,
    #[serde(default)]
    finish_reason: String,
}

#[derive(Deserialize)]
struct AssistantMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ResponseToolCall>,
}

#[derive(Deserialize)]
struct ResponseToolCall {
    id: String,
    function: ResponseFunctionCall,
}

#[derive(Deserialize)]
struct ResponseFunctionCall {
    name: String,
    arguments: String,
}

#[derive(Deserialize)]
struct ModelsResponse {
    data: Vec<Model>,
}

#[derive(Deserialize)]
struct Model {
    id: String,
}

#[derive(Deserialize)]
struct ErrorResponse {
    error: ProviderError,
}

#[derive(Deserialize)]
struct ProviderError {
    message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_provider_endpoints() {
        assert_eq!(
            endpoint("https://api.example.com/v1/", "models"),
            "https://api.example.com/v1/models"
        );
    }

    #[test]
    fn extracts_structured_provider_errors() {
        assert_eq!(
            error_message(r#"{"error":{"message":"bad key"}}"#),
            "bad key"
        );
    }

    #[test]
    fn tool_only_assistant_message_uses_null_content() {
        let message = Message::tool_request(
            "",
            vec![ToolCall {
                id: "1".into(),
                name: "read_file".into(),
                arguments: "{}".into(),
            }],
        );
        let value = serde_json::to_value(WireMessage::from(&message)).unwrap();

        assert!(value.get("content").is_none());
        assert_eq!(value["tool_calls"][0]["function"]["name"], "read_file");
    }
}
