use async_trait::async_trait;
use branchmesh_core::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

/// A provider-neutral completion request. Messages and tool definitions use
/// the Anthropic Messages API wire format (content blocks).
#[derive(Debug, Clone, Default)]
pub struct LlmRequest {
    pub model: String,
    pub system: String,
    pub messages: Vec<Value>,
    pub tools: Vec<Value>,
    pub max_tokens: u32,
    pub effort: Option<String>,
    /// JSON schema for structured output (`output_config.format`).
    pub output_schema: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LlmResponse {
    pub id: String,
    pub model: String,
    pub provider: String,
    /// Raw content blocks (text, thinking, tool_use, server tool blocks, fallback, ...).
    pub content: Vec<Value>,
    pub stop_reason: String,
    #[serde(default)]
    pub stop_details: Value,
    #[serde(default)]
    pub usage: Value,
}

impl LlmResponse {
    /// Concatenated visible text.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("")
    }

    /// Client-side tool calls the caller must execute.
    pub fn tool_uses(&self) -> Vec<&Value> {
        self.content.iter().filter(|b| b["type"] == "tool_use").collect()
    }

    pub fn input_tokens(&self) -> u64 {
        self.usage["input_tokens"].as_u64().unwrap_or(0)
            + self.usage["cache_read_input_tokens"].as_u64().unwrap_or(0)
            + self.usage["cache_creation_input_tokens"].as_u64().unwrap_or(0)
    }

    pub fn output_tokens(&self) -> u64 {
        self.usage["output_tokens"].as_u64().unwrap_or(0)
    }
}

/// Callback for streamed text deltas.
pub type DeltaFn<'a> = &'a (dyn Fn(&str) + Send + Sync);

#[async_trait]
pub trait LlmProvider: Send + Sync {
    fn name(&self) -> &str;
    async fn complete(&self, req: &LlmRequest, on_delta: DeltaFn<'_>) -> Result<LlmResponse>;
}

/// Pick the LLM provider from the environment:
///
/// * `BRANCHMESH_LLM=mock|anthropic|openrouter` forces a provider;
/// * otherwise `ANTHROPIC_API_KEY`/`ANTHROPIC_AUTH_TOKEN` -> Anthropic directly,
///   `OPENROUTER_API_KEY` -> OpenRouter, else the offline mock.
pub fn provider_from_env() -> Arc<dyn LlmProvider> {
    use crate::anthropic::{AnthropicConfig, AnthropicProvider};
    let forced = std::env::var("BRANCHMESH_LLM").unwrap_or_default().to_ascii_lowercase();
    let anthropic = || AnthropicConfig::from_env();
    let openrouter = || AnthropicConfig::openrouter_from_env();
    let cfg = match forced.as_str() {
        "mock" => None,
        "anthropic" => anthropic(),
        "openrouter" => openrouter(),
        _ => anthropic().or_else(openrouter),
    };
    match cfg {
        Some(cfg) => {
            tracing::info!(flavor = ?cfg.flavor, base_url = %cfg.base_url, "using Messages API provider");
            Arc::new(AnthropicProvider::new(cfg))
        }
        None => {
            if forced != "mock" {
                tracing::warn!("no ANTHROPIC_API_KEY or OPENROUTER_API_KEY set; using the offline mock LLM provider");
            }
            Arc::new(crate::mock::MockProvider::default())
        }
    }
}
