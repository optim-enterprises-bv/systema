//! Client for titan-engine's OpenAI-compatible `/v1/chat/completions` endpoint.

use std::time::Duration;

use anyhow::Context as _;
use serde::Deserialize;
use serde_json::json;

use crate::config::InferenceConfig;
use crate::manifest::ModelTier;
use crate::session::ChatMessage;

#[derive(Clone)]
pub struct InferenceClient {
    http: reqwest::Client,
    cfg: InferenceConfig,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: RespMessage,
}

#[derive(Deserialize)]
struct RespMessage {
    content: Option<String>,
}

impl InferenceClient {
    pub fn new(cfg: InferenceConfig) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(cfg.timeout_secs)).build()?;
        Ok(Self { http, cfg })
    }

    pub fn model_for(&self, tier: ModelTier) -> &str {
        match tier {
            ModelTier::Fast => &self.cfg.fast_model,
            ModelTier::Deep => &self.cfg.deep_model,
        }
    }

    pub async fn chat(&self, tier: ModelTier, messages: &[ChatMessage]) -> anyhow::Result<String> {
        let url = format!("{}/chat/completions", self.cfg.endpoint.trim_end_matches('/'));
        let mut body = json!({ "model": self.model_for(tier), "messages": messages, "stream": false });
        if let Some(t) = self.cfg.temperature {
            body["temperature"] = json!(t);
        }
        if let Some(p) = self.cfg.top_p {
            body["top_p"] = json!(p);
        }
        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("is titan-engine running at {}?", self.cfg.endpoint))?
            .error_for_status()?;
        let parsed: ChatResponse = resp.json().await?;
        parsed
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .context("empty completion")
    }

    /// Cheap reachability check.
    pub async fn health(&self) -> anyhow::Result<()> {
        let url = format!("{}/models", self.cfg.endpoint.trim_end_matches('/'));
        self.http.get(url).timeout(Duration::from_secs(3)).send().await?.error_for_status()?;
        Ok(())
    }
}
