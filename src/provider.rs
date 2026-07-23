use anyhow::{Context, Result, bail};
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::config::ProviderConfig;

#[derive(Clone)]
pub struct Provider {
    client: Client,
    config: ProviderConfig,
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

    pub async fn chat(&self, prompt: &str) -> Result<String> {
        let response = authorized(
            self.client
                .post(endpoint(&self.config.base_url, "chat/completions")),
            &self.config.api_key,
        )
        .json(&ChatRequest {
            model: &self.config.active_model,
            messages: [ChatMessage {
                role: "user",
                content: prompt,
            }],
        })
        .send()
        .await
        .context("could not reach the model provider")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            bail!("provider returned {status}: {}", error_message(&body));
        }

        let response: ChatResponse = response
            .json()
            .await
            .context("provider returned an invalid chat response")?;
        response
            .choices
            .into_iter()
            .next()
            .map(|choice| choice.message.content)
            .filter(|content| !content.trim().is_empty())
            .context("provider returned an empty response")
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

#[derive(Deserialize)]
struct ModelsResponse {
    data: Vec<Model>,
}

#[derive(Deserialize)]
struct Model {
    id: String,
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: [ChatMessage<'a>; 1],
}

#[derive(Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: AssistantMessage,
}

#[derive(Deserialize)]
struct AssistantMessage {
    content: String,
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
}
