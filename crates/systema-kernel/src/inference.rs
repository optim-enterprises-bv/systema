//! Client for titan-engine's OpenAI-compatible `/v1/chat/completions` endpoint.

use std::time::Duration;

use anyhow::Context as _;
use serde::Deserialize;
use serde_json::json;

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use crate::config::{InferenceConfig, ProviderConfig};
use crate::manifest::ModelTier;
use crate::session::ChatMessage;

#[derive(Clone)]
pub struct InferenceClient {
    http: reqwest::Client,
    cfg: InferenceConfig,
    /// Named providers beyond `cfg.endpoint`/`cfg.cloud_endpoint`, seeded from
    /// `[inference.providers.*]` and runtime-addable (Phase 5 of docs/IIi-FUSION.md -- the same
    /// hot-pluggable, operator-only pattern as `[mcp.*]` servers in Phase 4). `Arc` so the client
    /// stays `Clone`.
    providers: Arc<RwLock<BTreeMap<String, ProviderConfig>>>,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: ChatMessage,
}

#[derive(Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingDatum>,
}

#[derive(Deserialize)]
struct EmbeddingDatum {
    #[serde(default)]
    index: usize,
    embedding: Vec<f32>,
}

impl InferenceClient {
    pub fn new(cfg: InferenceConfig) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(cfg.timeout_secs)).build()?;
        let providers = Arc::new(RwLock::new(cfg.providers.clone()));
        Ok(Self { http, cfg, providers })
    }

    /// The configured providers, startup plus runtime-added (not the default `endpoint`, which
    /// has no name of its own).
    pub fn list_providers(&self) -> Vec<(String, ProviderConfig)> {
        self.providers.read().unwrap().iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    /// Add or replace a named provider at runtime.
    pub fn add_provider(&self, name: String, cfg: ProviderConfig) {
        self.providers.write().unwrap().insert(name, cfg);
    }

    /// Remove a provider. Returns whether one existed.
    pub fn remove_provider(&self, name: &str) -> bool {
        self.providers.write().unwrap().remove(name).is_some()
    }

    /// Cheap reachability check against a named provider (or the default endpoint if `name` is
    /// `None`), mirroring `health()`.
    pub async fn provider_health(&self, name: Option<&str>) -> anyhow::Result<()> {
        let endpoint = match name {
            None => self.cfg.endpoint.clone(),
            Some(n) => {
                self.providers.read().unwrap().get(n).map(|p| p.endpoint.clone()).with_context(|| format!("no provider named {n}"))?
            }
        };
        let url = format!("{}/models", endpoint.trim_end_matches('/'));
        self.http.get(url).timeout(Duration::from_secs(3)).send().await?.error_for_status()?;
        Ok(())
    }

    pub fn model_for(&self, tier: ModelTier) -> &str {
        match tier {
            ModelTier::Fast => &self.cfg.fast_model,
            ModelTier::Deep => &self.cfg.deep_model,
        }
    }

    /// One completion. `tools` is the OpenAI `tools` array (empty = no tools offered).
    /// Returns the assistant message, which may carry `tool_calls` instead of text. If the local
    /// engine is unreachable and a `cloud_endpoint` is configured, the request is retried there.
    pub async fn complete(
        &self,
        tier: ModelTier,
        messages: &[ChatMessage],
        tools: &[serde_json::Value],
    ) -> anyhow::Result<ChatMessage> {
        let local = self.complete_at(&self.cfg.endpoint, self.model_for(tier), None, messages, tools).await;
        match (local, &self.cfg.cloud_endpoint) {
            (Ok(m), _) => Ok(m),
            (Err(e), Some(cloud)) => {
                tracing::warn!("local inference failed ({e:#}); falling back to cloud at {cloud}");
                let model = self.cfg.cloud_model.as_deref().unwrap_or(self.model_for(tier));
                let key = std::env::var("SYSTEMA_CLOUD_API_KEY").ok();
                self.complete_at(cloud, model, key.as_deref(), messages, tools)
                    .await
                    .with_context(|| format!("cloud fallback at {cloud} also failed"))
            }
            (Err(e), None) => Err(e),
        }
    }

    /// One completion against a specific OpenAI-compatible endpoint.
    async fn complete_at(
        &self,
        endpoint: &str,
        model: &str,
        api_key: Option<&str>,
        messages: &[ChatMessage],
        tools: &[serde_json::Value],
    ) -> anyhow::Result<ChatMessage> {
        let url = format!("{}/chat/completions", endpoint.trim_end_matches('/'));
        let mut body = json!({ "model": model, "messages": messages, "stream": false });
        if !tools.is_empty() {
            body["tools"] = json!(tools);
        }
        if let Some(t) = self.cfg.temperature {
            body["temperature"] = json!(t);
        }
        if let Some(p) = self.cfg.top_p {
            body["top_p"] = json!(p);
        }
        let mut req = self.http.post(&url).json(&body);
        if let Some(key) = api_key {
            req = req.bearer_auth(key);
        }
        let resp = req
            .send()
            .await
            .with_context(|| format!("is an OpenAI-compatible server running at {endpoint}?"))?
            .error_for_status()?;
        let parsed: ChatResponse = resp.json().await?;
        let mut msg = parsed.choices.into_iter().next().context("no choices in completion")?.message;
        msg.content = msg.content.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        if msg.tool_calls.as_ref().is_some_and(Vec::is_empty) {
            msg.tool_calls = None;
        }
        if msg.content.is_none() && msg.tool_calls.is_none() {
            anyhow::bail!("empty completion");
        }
        Ok(msg)
    }

    /// Plain text completion with no tools.
    pub async fn chat(&self, tier: ModelTier, messages: &[ChatMessage]) -> anyhow::Result<String> {
        self.complete(tier, messages, &[]).await?.content.context("empty completion")
    }

    /// Plain text completion against an explicit named provider and model, bypassing tier
    /// routing entirely. `provider: None` uses the default `endpoint` (no auth). For a future
    /// per-message provider/model picker in the chat UI, and for `Control::ProviderTest`.
    pub async fn chat_with(&self, provider: Option<&str>, model: &str, messages: &[ChatMessage]) -> anyhow::Result<String> {
        let (endpoint, api_key) = match provider {
            None => (self.cfg.endpoint.clone(), None),
            Some(n) => {
                let p = self.providers.read().unwrap().get(n).cloned().with_context(|| format!("no provider named {n}"))?;
                let key = p.api_key_env.as_deref().and_then(|v| std::env::var(v).ok());
                (p.endpoint, key)
            }
        };
        self.complete_at(&endpoint, model, api_key.as_deref(), messages, &[]).await?.content.context("empty completion")
    }

    /// Whether an embeddings endpoint is configured.
    pub fn embeddings_enabled(&self) -> bool {
        self.cfg.embed_endpoint.is_some()
    }

    /// Embed one or more texts via the configured `embed_endpoint` (OpenAI `/embeddings` shape).
    /// Returns one vector per input, in order. Errors if no endpoint is configured.
    pub async fn embed(&self, inputs: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        let endpoint = self.cfg.embed_endpoint.as_deref().context("no embed_endpoint configured")?;
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        let url = format!("{}/embeddings", endpoint.trim_end_matches('/'));
        let body = json!({ "model": self.cfg.embed_model, "input": inputs });
        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("is the embeddings service running at {endpoint}?"))?
            .error_for_status()?;
        let parsed: EmbeddingResponse = resp.json().await?;
        if parsed.data.len() != inputs.len() {
            anyhow::bail!("embeddings endpoint returned {} vectors for {} inputs", parsed.data.len(), inputs.len());
        }
        // Keep the endpoint's order (index) to align vectors with inputs.
        let mut data = parsed.data;
        data.sort_by_key(|d| d.index);
        Ok(data.into_iter().map(|d| d.embedding).collect())
    }

    /// Cheap reachability check.
    pub async fn health(&self) -> anyhow::Result<()> {
        let url = format!("{}/models", self.cfg.endpoint.trim_end_matches('/'));
        self.http.get(url).timeout(Duration::from_secs(3)).send().await?.error_for_status()?;
        Ok(())
    }
}
