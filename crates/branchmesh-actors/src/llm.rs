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

/// Claude if `ANTHROPIC_API_KEY` (or `ANTHROPIC_AUTH_TOKEN`) is set, otherwise
/// the offline mock provider. `BRANCHMESH_LLM=mock` forces the mock.
pub fn provider_from_env() -> Arc<dyn LlmProvider> {
    if std::env::var("BRANCHMESH_LLM").map(|v| v == "mock").unwrap_or(false) {
        return Arc::new(crate::mock::MockProvider::default());
    }
    match crate::anthropic::AnthropicConfig::from_env() {
        Some(cfg) => {
            tracing::info!(base_url = %cfg.base_url, "using Anthropic provider");
            Arc::new(crate::anthropic::AnthropicProvider::new(cfg))
        }
        None => {
            tracing::warn!("ANTHROPIC_API_KEY not set; using the offline mock LLM provider");
            Arc::new(crate::mock::MockProvider::default())
        }
    }
}
