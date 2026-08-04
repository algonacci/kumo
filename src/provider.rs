use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::ProviderConfig;

#[derive(Clone)]
pub struct Provider {
    client: Client,
    config: ProviderConfig,
}

#[async_trait]
pub trait ModelProvider: Send + Sync {
    fn active_model(&self) -> &str;
    async fn chat(&self, messages: &[Message], tools: &[ToolDefinition]) -> Result<ChatResponse>;

    async fn summarize(&self, messages: &[Message]) -> Result<String> {
        let response = self.chat(messages, &[]).await?;
        if response.content.trim().is_empty() {
            bail!("provider returned an empty summary");
        }
        Ok(response.content)
    }
}

#[derive(Clone, Debug)]
pub struct Message {
    pub(crate) role: Role,
    pub(crate) content: String,
    pub(crate) images: Vec<ImageAttachment>,
    pub(crate) tool_calls: Vec<ToolCall>,
    pub(crate) tool_call_id: Option<String>,
}

/// An image attached to a user message, carried as base64 so it stays provider-independent until
/// the wire layer encodes it as a data URL.
#[derive(Clone, Debug)]
pub struct ImageAttachment {
    /// MIME type, e.g. `image/jpeg`.
    pub media_type: String,
    pub data: String,
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

    /// A user message with one or more images attached (e.g. a Telegram photo). `content` may be
    /// empty when the user sent an image with no caption.
    pub fn user_with_images(content: impl Into<String>, images: Vec<ImageAttachment>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            images,
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self::text(Role::Assistant, content)
    }

    pub fn tool_request(content: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            images: Vec::new(),
            tool_calls,
            tool_call_id: None,
        }
    }

    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            images: Vec::new(),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
        }
    }

    fn text(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            images: Vec::new(),
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
            // Images are per-request only, never persisted (see save_turn); a message loaded back
            // from storage never carries any.
            images: Vec::new(),
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

impl ToolDefinition {
    /// What this definition costs in a request, for compaction's accounting. The JSON scaffolding
    /// the wire layer wraps it in is a few dozen fixed bytes per tool, small enough against a
    /// schema to leave out of the estimate.
    pub fn payload_bytes(&self) -> usize {
        self.name.len() + self.description.len() + self.parameters.to_string().len()
    }
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
}

#[async_trait]
impl ModelProvider for Provider {
    fn active_model(&self) -> &str {
        &self.config.active_model
    }

    async fn chat(&self, messages: &[Message], tools: &[ToolDefinition]) -> Result<ChatResponse> {
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

/// One entry of a provider's model listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelInfo {
    pub id: String,
    /// The model's context window in tokens, when the provider reports one.
    pub context_window: Option<u64>,
}

pub async fn list_models(base_url: &str, api_key: &str) -> Result<Vec<ModelInfo>> {
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
    let models = model_listing(response);

    if models.is_empty() {
        bail!("provider returned no models");
    }
    Ok(models)
}

fn model_listing(response: ModelsResponse) -> Vec<ModelInfo> {
    let mut models = response
        .data
        .into_iter()
        .map(|model| ModelInfo {
            id: model.id,
            context_window: model.context_window.or(model.context_length),
        })
        .collect::<Vec<_>>();
    models.sort_by(|left, right| left.id.cmp(&right.id));
    models.dedup_by(|left, right| left.id == right.id);
    models
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
    content: Option<WireContent<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<WireToolCall<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<&'a str>,
}

/// Message content is a plain string unless images are attached, in which case OpenAI expects an
/// array of typed parts.
#[derive(Serialize)]
#[serde(untagged)]
enum WireContent<'a> {
    Text(&'a str),
    Parts(Vec<WirePart<'a>>),
}

#[derive(Serialize)]
#[serde(tag = "type")]
enum WirePart<'a> {
    #[serde(rename = "text")]
    Text { text: &'a str },
    #[serde(rename = "image_url")]
    ImageUrl { image_url: WireImageUrl },
}

#[derive(Serialize)]
struct WireImageUrl {
    url: String,
}

impl<'a> From<&'a Message> for WireMessage<'a> {
    fn from(message: &'a Message) -> Self {
        let content = if !message.images.is_empty() {
            // With images, content becomes an array of text and image parts.
            let mut parts = Vec::with_capacity(message.images.len() + 1);
            if !message.content.is_empty() {
                parts.push(WirePart::Text {
                    text: &message.content,
                });
            }
            for image in &message.images {
                parts.push(WirePart::ImageUrl {
                    image_url: WireImageUrl {
                        url: format!("data:{};base64,{}", image.media_type, image.data),
                    },
                });
            }
            Some(WireContent::Parts(parts))
        } else if message.content.is_empty() && !message.tool_calls.is_empty() {
            None
        } else {
            Some(WireContent::Text(&message.content))
        };
        Self {
            role: message.role_name(),
            content,
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
    /// Groq reports a model's window under this name, OpenRouter under `context_length`, and the
    /// OpenAI API itself reports neither — so both are optional and a provider that names it
    /// something else simply leaves Kumo on its conservative default.
    #[serde(default)]
    context_window: Option<u64>,
    #[serde(default)]
    context_length: Option<u64>,
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
    fn reads_a_context_window_under_either_provider_spelling() {
        let response: ModelsResponse = serde_json::from_str(
            r#"{"data": [
                {"id": "groq-style", "context_window": 131072},
                {"id": "openrouter-style", "context_length": 200000},
                {"id": "openai-style"}
            ]}"#,
        )
        .unwrap();

        let listing = model_listing(response);

        assert_eq!(
            listing,
            vec![
                ModelInfo {
                    id: "groq-style".into(),
                    context_window: Some(131_072)
                },
                ModelInfo {
                    id: "openai-style".into(),
                    context_window: None
                },
                ModelInfo {
                    id: "openrouter-style".into(),
                    context_window: Some(200_000)
                },
            ]
        );
    }

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

    #[test]
    fn plain_text_message_serializes_content_as_a_string() {
        let message = Message::user("hello");
        let value = serde_json::to_value(WireMessage::from(&message)).unwrap();

        assert_eq!(value["content"], "hello");
    }

    #[test]
    fn serializes_images_as_content_parts() {
        let message = Message::user_with_images(
            "what is this?",
            vec![ImageAttachment {
                media_type: "image/jpeg".into(),
                data: "AAAA".into(),
            }],
        );
        let value = serde_json::to_value(WireMessage::from(&message)).unwrap();

        assert_eq!(value["content"][0]["type"], "text");
        assert_eq!(value["content"][0]["text"], "what is this?");
        assert_eq!(value["content"][1]["type"], "image_url");
        assert_eq!(
            value["content"][1]["image_url"]["url"],
            "data:image/jpeg;base64,AAAA"
        );
    }

    #[test]
    fn an_image_with_no_caption_omits_the_text_part() {
        let message = Message::user_with_images(
            "",
            vec![ImageAttachment {
                media_type: "image/jpeg".into(),
                data: "AAAA".into(),
            }],
        );
        let value = serde_json::to_value(WireMessage::from(&message)).unwrap();

        // Only the image part should be present, no leading empty text part.
        assert_eq!(value["content"].as_array().unwrap().len(), 1);
        assert_eq!(value["content"][0]["type"], "image_url");
    }
}
