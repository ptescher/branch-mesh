//! Claude provider over the Messages API (raw HTTP + SSE streaming; there is
//! no official Rust SDK). The same client talks to Anthropic directly or to
//! OpenRouter's Anthropic-compatible `POST /api/v1/messages` endpoint.
//!
//! Request shape decisions:
//! * `thinking: {type: "adaptive"}` on models that support it.
//! * `thinking.block_binding.prefix_mismatch_behavior = "drop_block"`
//!   (beta `thinking-binding-controls-2026-08-01`): BranchMesh forks and
//!   merges conversation histories by design, which can edit earlier turns.
//!   Dropping invalidated thinking blocks keeps those requests from failing.
//! * `fallbacks: "default"` (beta `server-side-fallback-2026-07-01`) on models
//!   that support server-side refusal fallback.
//! * Top-level `cache_control` for automatic prompt caching.

use crate::llm::{DeltaFn, LlmProvider, LlmRequest, LlmResponse};
use async_trait::async_trait;
use branchmesh_core::{Error, Result};
use futures::StreamExt;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Duration;

pub const API_VERSION: &str = "2026-10-21";
const BETA_FALLBACK: &str = "server-side-fallback-2026-07-01";
const BETA_BLOCK_BINDING: &str = "thinking-binding-controls-2026-08-01";

/// Which Messages-API-compatible service to call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    /// api.anthropic.com
    Anthropic,
    /// openrouter.ai `/api/v1/messages` (bearer auth, `anthropic/...` model ids,
    /// OpenRouter-routed `fallbacks: [{model}]`).
    OpenRouter,
}

#[derive(Debug, Clone)]
pub struct AnthropicConfig {
    pub flavor: Flavor,
    pub api_key: Option<String>,
    pub auth_token: Option<String>,
    pub base_url: String,
    /// Opt into server-side refusal fallbacks where the model supports them.
    pub fallbacks: bool,
    /// Drop thinking blocks invalidated by history edits (fork/merge).
    pub drop_invalid_thinking: bool,
    pub max_retries: u32,
    pub timeout: Duration,
    /// OpenRouter only: explicit fallback models (None = per-model defaults,
    /// empty = disabled).
    pub fallback_models: Option<Vec<String>>,
    /// OpenRouter only: app attribution (`HTTP-Referer`, `X-Title`).
    pub app_url: Option<String>,
    pub app_name: Option<String>,
}

fn env_flag(name: &str, default: bool) -> bool {
    std::env::var(name)
        .map(|v| !matches!(v.to_ascii_lowercase().as_str(), "0" | "false" | "off" | "no"))
        .unwrap_or(default)
}

impl AnthropicConfig {
    /// OpenRouter configuration from `OPENROUTER_API_KEY` (+ optional
    /// `OPENROUTER_BASE_URL`, `OPENROUTER_FALLBACK_MODELS`, `OPENROUTER_APP_URL`,
    /// `OPENROUTER_APP_NAME`).
    pub fn openrouter_from_env() -> Option<Self> {
        let key = std::env::var("OPENROUTER_API_KEY").ok().filter(|s| !s.is_empty())?;
        let fallback_models = std::env::var("OPENROUTER_FALLBACK_MODELS").ok().map(|v| {
            if matches!(v.trim().to_ascii_lowercase().as_str(), "" | "off" | "none" | "false") {
                vec![]
            } else {
                v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
            }
        });
        Some(AnthropicConfig {
            flavor: Flavor::OpenRouter,
            api_key: None,
            auth_token: Some(key),
            base_url: std::env::var("OPENROUTER_BASE_URL").unwrap_or_else(|_| "https://openrouter.ai/api".into()),
            fallbacks: env_flag("BRANCHMESH_LLM_FALLBACKS", true),
            drop_invalid_thinking: env_flag("BRANCHMESH_DROP_INVALID_THINKING", true),
            max_retries: 3,
            timeout: Duration::from_secs(600),
            fallback_models,
            app_url: std::env::var("OPENROUTER_APP_URL").ok(),
            app_name: Some(std::env::var("OPENROUTER_APP_NAME").unwrap_or_else(|_| "BranchMesh".into())),
        })
    }

    pub fn from_env() -> Option<Self> {
        let api_key = std::env::var("ANTHROPIC_API_KEY").ok().filter(|s| !s.is_empty());
        let auth_token = std::env::var("ANTHROPIC_AUTH_TOKEN").ok().filter(|s| !s.is_empty());
        if api_key.is_none() && auth_token.is_none() {
            return None;
        }
        let flag = env_flag;
        Some(AnthropicConfig {
            flavor: Flavor::Anthropic,
            api_key,
            auth_token,
            base_url: std::env::var("ANTHROPIC_BASE_URL").unwrap_or_else(|_| "https://api.anthropic.com".into()),
            fallbacks: flag("BRANCHMESH_LLM_FALLBACKS", true),
            drop_invalid_thinking: flag("BRANCHMESH_DROP_INVALID_THINKING", true),
            max_retries: 3,
            timeout: Duration::from_secs(600),
            fallback_models: None,
            app_url: None,
            app_name: None,
        })
    }
}

/// Normalize a model id to Anthropic's form: `anthropic/claude-opus-5.5` -> `claude-opus-5-5`.
pub fn anthropic_model_id(model: &str) -> String {
    model.strip_prefix("anthropic/").unwrap_or(model).replace('.', "-")
}

/// Map a model id to OpenRouter's form: `claude-opus-5-5` -> `anthropic/claude-opus-5.5`.
/// Ids that already contain a `/` (any OpenRouter model) pass through unchanged.
pub fn openrouter_model_id(model: &str) -> String {
    if model.contains('/') {
        return model.to_string();
    }
    let mut id = model.to_string();
    // A trailing "-<major>-<minor>" version becomes "<major>.<minor>".
    let parts: Vec<&str> = model.rsplitn(3, '-').collect();
    if parts.len() == 3
        && parts[0].chars().all(|c| c.is_ascii_digit())
        && parts[1].chars().all(|c| c.is_ascii_digit())
        && !parts[0].is_empty()
        && !parts[1].is_empty()
    {
        id = format!("{}-{}.{}", parts[2], parts[1], parts[0]);
    }
    format!("anthropic/{id}")
}

/// Default OpenRouter fallback chain for a Claude model (mirrors Anthropic's
/// recommended refusal fallbacks; OpenRouter has no `"default"` mode).
fn openrouter_default_fallbacks(model: &str) -> Vec<String> {
    let m = anthropic_model_id(model);
    if m.starts_with("claude-opus-5-5") || m.starts_with("claude-fable-5") {
        vec!["anthropic/claude-opus-5".into()]
    } else if m.starts_with("claude-sonnet-5-5") {
        vec!["anthropic/claude-sonnet-5".into()]
    } else {
        vec![]
    }
}

pub struct AnthropicProvider {
    cfg: AnthropicConfig,
    http: reqwest::Client,
}

fn supports_adaptive_thinking(model: &str) -> bool {
    if model.contains('/') && !model.starts_with("anthropic/") {
        return false;
    }
    let model = &anthropic_model_id(model);
    [
        "claude-opus-5", "claude-sonnet-5", "claude-haiku-5", "claude-fable", "claude-mythos",
        "claude-opus-4-6", "claude-opus-4-7", "claude-opus-4-8", "claude-sonnet-4-6",
    ]
    .iter()
    .any(|p| model.starts_with(p))
}

fn supports_server_fallback(model: &str) -> bool {
    let model = &anthropic_model_id(model);
    ["claude-opus-5", "claude-fable-5", "claude-sonnet-5-5"]
        .iter()
        .any(|p| model.starts_with(p))
}

/// After a mid-output fallback, blocks before the final `fallback` boundary
/// that the next model must not see (thinking, unanswered tool calls) are
/// removed so the turn can be echoed back verbatim on the next request.
pub fn sanitize_fallback_content(content: Vec<Value>) -> Vec<Value> {
    let Some(boundary) = content.iter().rposition(|b| b["type"] == "fallback") else {
        return content;
    };
    let result_ids: Vec<String> = content
        .iter()
        .filter(|b| b["type"].as_str().map(|t| t.ends_with("_tool_result")).unwrap_or(false))
        .filter_map(|b| b["tool_use_id"].as_str().map(String::from))
        .collect();
    content
        .into_iter()
        .enumerate()
        .filter(|(i, b)| {
            if *i >= boundary {
                return true;
            }
            match b["type"].as_str().unwrap_or("") {
                "text" | "fallback" => true,
                "thinking" | "redacted_thinking" | "tool_use" => false,
                "server_tool_use" => b["id"].as_str().map(|id| result_ids.iter().any(|r| r == id)).unwrap_or(false),
                t if t.ends_with("_tool_result") => true,
                _ => false,
            }
        })
        .map(|(_, b)| b)
        .collect()
}

impl AnthropicProvider {
    pub fn new(cfg: AnthropicConfig) -> Self {
        let http = reqwest::Client::builder()
            .timeout(cfg.timeout)
            .build()
            .expect("http client");
        AnthropicProvider { cfg, http }
    }

    fn build_body(&self, req: &LlmRequest) -> (Value, Vec<&'static str>) {
        let mut betas = Vec::new();
        let model = match self.cfg.flavor {
            Flavor::Anthropic => anthropic_model_id(&req.model),
            Flavor::OpenRouter => openrouter_model_id(&req.model),
        };
        let mut body = json!({
            "model": model,
            "max_tokens": req.max_tokens,
            "messages": req.messages,
            "stream": true,
            "cache_control": {"type": "ephemeral"},
        });
        if !req.system.is_empty() {
            body["system"] = json!(req.system);
        }
        if !req.tools.is_empty() {
            body["tools"] = json!(req.tools);
        }
        if supports_adaptive_thinking(&req.model) {
            let mut thinking = json!({"type": "adaptive"});
            if self.cfg.drop_invalid_thinking {
                thinking["block_binding"] = json!({"prefix_mismatch_behavior": "drop_block"});
                betas.push(BETA_BLOCK_BINDING);
            }
            body["thinking"] = thinking;
        }
        let mut output_config = serde_json::Map::new();
        if let Some(effort) = &req.effort {
            output_config.insert("effort".into(), json!(effort));
        }
        if let Some(schema) = &req.output_schema {
            output_config.insert("format".into(), json!({"type": "json_schema", "schema": schema}));
        }
        if !output_config.is_empty() {
            body["output_config"] = Value::Object(output_config);
        }
        if self.cfg.fallbacks {
            match self.cfg.flavor {
                Flavor::Anthropic if supports_server_fallback(&req.model) => {
                    body["fallbacks"] = json!("default");
                    betas.push(BETA_FALLBACK);
                }
                Flavor::OpenRouter => {
                    let models = self
                        .cfg
                        .fallback_models
                        .clone()
                        .unwrap_or_else(|| openrouter_default_fallbacks(&req.model));
                    let models: Vec<Value> = models
                        .into_iter()
                        .filter(|m| *m != model)
                        .take(3)
                        .map(|m| json!({ "model": m }))
                        .collect();
                    if !models.is_empty() {
                        body["fallbacks"] = Value::Array(models);
                    }
                }
                _ => {}
            }
        }
        (body, betas)
    }

    async fn send(&self, body: &Value, betas: &[&str]) -> Result<reqwest::Response> {
        let url = format!("{}/v1/messages", self.cfg.base_url.trim_end_matches('/'));
        let mut attempt = 0;
        loop {
            let mut rb = self
                .http
                .post(&url)
                .header("content-type", "application/json")
                .header("anthropic-version", API_VERSION)
                .json(body);
            if let Some(key) = &self.cfg.api_key {
                rb = rb.header("x-api-key", key);
            } else if let Some(token) = &self.cfg.auth_token {
                rb = rb.bearer_auth(token);
            }
            if let Some(u) = &self.cfg.app_url {
                rb = rb.header("HTTP-Referer", u);
            }
            if let Some(n) = &self.cfg.app_name {
                rb = rb.header("X-Title", n);
            }
            if !betas.is_empty() {
                rb = rb.header("anthropic-beta", betas.join(","));
            }
            let result = rb.send().await;
            let retry_after = |resp: &reqwest::Response| {
                resp.headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.parse::<u64>().ok())
                    .map(Duration::from_secs)
            };
            match result {
                Ok(resp) if resp.status().is_success() => return Ok(resp),
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    let should_retry = resp
                        .headers()
                        .get("x-should-retry")
                        .and_then(|v| v.to_str().ok())
                        .map(|v| v == "true")
                        .unwrap_or(matches!(status, 408 | 409 | 429 | 500..=599));
                    let wait = retry_after(&resp);
                    let text = resp.text().await.unwrap_or_default();
                    if should_retry && attempt < self.cfg.max_retries {
                        attempt += 1;
                        let delay = wait.unwrap_or(Duration::from_millis(1000 * 2u64.pow(attempt - 1)));
                        tracing::warn!(status, attempt, "anthropic request failed; retrying in {:?}", delay);
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    return Err(Error::Other(format!("{} API error {status}: {text}", self.name())));
                }
                Err(e) if attempt < self.cfg.max_retries && (e.is_connect() || e.is_timeout()) => {
                    attempt += 1;
                    tokio::time::sleep(Duration::from_millis(1000 * 2u64.pow(attempt - 1))).await;
                }
                Err(e) => return Err(Error::Other(format!("anthropic request failed: {}", branchmesh_core::error_chain(&e)))),
            }
        }
    }
}

/// Accumulates SSE events into a complete message.
#[derive(Default)]
struct StreamState {
    message: Value,
    blocks: Vec<Value>,
    partial_json: HashMap<usize, String>,
    stop_reason: String,
    stop_details: Value,
    usage: Value,
}

impl StreamState {
    fn apply(&mut self, ev: &Value, on_delta: DeltaFn<'_>) -> Result<bool> {
        match ev["type"].as_str().unwrap_or("") {
            "message_start" => {
                self.message = ev["message"].clone();
                self.usage = ev["message"]["usage"].clone();
            }
            "content_block_start" => {
                let idx = ev["index"].as_u64().unwrap_or(self.blocks.len() as u64) as usize;
                while self.blocks.len() <= idx {
                    self.blocks.push(Value::Null);
                }
                self.blocks[idx] = ev["content_block"].clone();
            }
            "content_block_delta" => {
                let idx = ev["index"].as_u64().unwrap_or(0) as usize;
                let delta = &ev["delta"];
                let Some(block) = self.blocks.get_mut(idx) else { return Ok(false) };
                match delta["type"].as_str().unwrap_or("") {
                    "text_delta" => {
                        let t = delta["text"].as_str().unwrap_or("");
                        let cur = block["text"].as_str().unwrap_or("").to_string();
                        block["text"] = json!(cur + t);
                        on_delta(t);
                    }
                    "thinking_delta" => {
                        let t = delta["thinking"].as_str().unwrap_or("");
                        let cur = block["thinking"].as_str().unwrap_or("").to_string();
                        block["thinking"] = json!(cur + t);
                    }
                    "signature_delta" => {
                        block["signature"] = delta["signature"].clone();
                    }
                    "input_json_delta" => {
                        self.partial_json
                            .entry(idx)
                            .or_default()
                            .push_str(delta["partial_json"].as_str().unwrap_or(""));
                    }
                    "citations_delta" => {
                        if !block["citations"].is_array() {
                            block["citations"] = json!([]);
                        }
                        block["citations"].as_array_mut().unwrap().push(delta["citation"].clone());
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let idx = ev["index"].as_u64().unwrap_or(0) as usize;
                if let Some(raw) = self.partial_json.remove(&idx) {
                    let input = if raw.trim().is_empty() {
                        json!({})
                    } else {
                        serde_json::from_str(&raw).map_err(|e| {
                            Error::Other(format!("invalid tool input JSON from model: {e}"))
                        })?
                    };
                    if let Some(block) = self.blocks.get_mut(idx) {
                        block["input"] = input;
                    }
                }
            }
            "message_delta" => {
                if let Some(sr) = ev["delta"]["stop_reason"].as_str() {
                    self.stop_reason = sr.to_string();
                }
                if !ev["delta"]["stop_details"].is_null() {
                    self.stop_details = ev["delta"]["stop_details"].clone();
                }
                if let (Some(u), Some(dst)) = (ev["usage"].as_object(), self.usage.as_object_mut()) {
                    for (k, v) in u {
                        if !v.is_null() {
                            dst.insert(k.clone(), v.clone());
                        }
                    }
                }
            }
            "message_stop" => return Ok(true),
            "error" => {
                return Err(Error::Other(format!("anthropic stream error: {}", ev["error"])));
            }
            _ => {}
        }
        Ok(false)
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    fn name(&self) -> &str {
        match self.cfg.flavor {
            Flavor::Anthropic => "anthropic",
            Flavor::OpenRouter => "openrouter",
        }
    }

    async fn complete(&self, req: &LlmRequest, on_delta: DeltaFn<'_>) -> Result<LlmResponse> {
        let (body, betas) = self.build_body(req);
        let resp = self.send(&body, &betas).await?;
        let mut stream = resp.bytes_stream();
        let mut buf = String::new();
        let mut state = StreamState::default();
        let mut done = false;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| Error::Other(format!("stream interrupted: {e}")))?;
            buf.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(pos) = buf.find("\n\n") {
                let raw: String = buf.drain(..pos + 2).collect();
                let data: String = raw
                    .lines()
                    .filter_map(|l| l.strip_prefix("data:"))
                    .map(|l| l.trim_start())
                    .collect::<Vec<_>>()
                    .join("\n");
                if data.is_empty() {
                    continue;
                }
                let ev: Value = serde_json::from_str(&data)?;
                if state.apply(&ev, on_delta)? {
                    done = true;
                }
            }
        }
        if !done && state.stop_reason.is_empty() {
            return Err(Error::Other("stream ended before message_stop".into()));
        }
        let content: Vec<Value> = state.blocks.into_iter().filter(|b| !b.is_null()).collect();
        Ok(LlmResponse {
            id: state.message["id"].as_str().unwrap_or("").to_string(),
            model: state.message["model"].as_str().unwrap_or(&req.model).to_string(),
            provider: self.name().into(),
            content: sanitize_fallback_content(content),
            stop_reason: state.stop_reason,
            stop_details: state.stop_details,
            usage: state.usage,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stream_events() {
        let mut s = StreamState::default();
        let noop = |_: &str| {};
        let events = [
            json!({"type":"message_start","message":{"id":"msg_1","model":"claude-opus-5-5","usage":{"input_tokens":10,"output_tokens":1}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hel"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo"}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"t1","name":"x","input":{}}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"a\":"}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"1}"}}),
            json!({"type":"content_block_stop","index":1}),
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":7}}),
            json!({"type":"message_stop"}),
        ];
        let mut done = false;
        for e in &events {
            done |= s.apply(e, &noop).unwrap();
        }
        assert!(done);
        assert_eq!(s.blocks[0]["text"], "Hello");
        assert_eq!(s.blocks[1]["input"], json!({"a": 1}));
        assert_eq!(s.stop_reason, "tool_use");
        assert_eq!(s.usage["output_tokens"], 7);
        assert_eq!(s.usage["input_tokens"], 10);
    }

    #[test]
    fn request_body_shape() {
        let cfg = AnthropicConfig {
            flavor: Flavor::Anthropic,
            fallback_models: None,
            app_url: None,
            app_name: None,
            api_key: Some("k".into()),
            auth_token: None,
            base_url: "http://x".into(),
            fallbacks: true,
            drop_invalid_thinking: true,
            max_retries: 0,
            timeout: Duration::from_secs(1),
        };
        let p = AnthropicProvider::new(cfg);
        let req = LlmRequest {
            model: "claude-opus-5-5".into(),
            system: "sys".into(),
            messages: vec![json!({"role": "user", "content": "hi"})],
            tools: vec![json!({"name": "t", "description": "", "input_schema": {"type": "object"}})],
            max_tokens: 16000,
            effort: Some("high".into()),
            output_schema: None,
        };
        let (body, betas) = p.build_body(&req);
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["thinking"]["block_binding"]["prefix_mismatch_behavior"], "drop_block");
        assert_eq!(body["fallbacks"], "default");
        assert_eq!(body["output_config"]["effort"], "high");
        assert_eq!(body["cache_control"]["type"], "ephemeral");
        assert_eq!(body["stream"], true);
        assert!(body.get("temperature").is_none());
        assert_eq!(betas, vec![BETA_BLOCK_BINDING, BETA_FALLBACK]);

        // Haiku 5.5 has no server-side fallback.
        let (body, betas) = p.build_body(&LlmRequest { model: "claude-haiku-5-5".into(), ..req });
        assert!(body.get("fallbacks").is_none());
        assert_eq!(betas, vec![BETA_BLOCK_BINDING]);
    }

    #[test]
    fn openrouter_body_and_model_ids() {
        assert_eq!(openrouter_model_id("claude-opus-5-5"), "anthropic/claude-opus-5.5");
        assert_eq!(openrouter_model_id("claude-opus-5"), "anthropic/claude-opus-5");
        assert_eq!(openrouter_model_id("claude-haiku-4-5"), "anthropic/claude-haiku-4.5");
        assert_eq!(openrouter_model_id("google/gemini-3-flash"), "google/gemini-3-flash");
        assert_eq!(anthropic_model_id("anthropic/claude-sonnet-5.5"), "claude-sonnet-5-5");

        let p = AnthropicProvider::new(AnthropicConfig {
            flavor: Flavor::OpenRouter,
            api_key: None,
            auth_token: Some("k".into()),
            base_url: "http://x".into(),
            fallbacks: true,
            drop_invalid_thinking: true,
            max_retries: 0,
            timeout: Duration::from_secs(1),
            fallback_models: None,
            app_url: None,
            app_name: Some("BranchMesh".into()),
        });
        let req = LlmRequest {
            model: "claude-opus-5-5".into(),
            messages: vec![json!({"role": "user", "content": "hi"})],
            max_tokens: 100,
            ..Default::default()
        };
        let (body, betas) = p.build_body(&req);
        assert_eq!(body["model"], "anthropic/claude-opus-5.5");
        assert_eq!(body["fallbacks"], json!([{"model": "anthropic/claude-opus-5"}]));
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(betas, vec![BETA_BLOCK_BINDING]);
        assert_eq!(p.name(), "openrouter");

        // Non-Claude models get no Claude-specific thinking config.
        let (body, _) = p.build_body(&LlmRequest { model: "google/gemini-3-flash".into(), ..req });
        assert!(body.get("thinking").is_none());
        assert!(body.get("fallbacks").is_none());
    }

    #[test]
    fn strips_pre_fallback_thinking() {
        let content = vec![
            json!({"type":"thinking","thinking":"","signature":"x"}),
            json!({"type":"text","text":"partial"}),
            json!({"type":"fallback","from":{"model":"a"},"to":{"model":"b"}}),
            json!({"type":"thinking","thinking":"","signature":"y"}),
            json!({"type":"text","text":"final"}),
        ];
        let out = sanitize_fallback_content(content);
        assert_eq!(out.len(), 4);
        assert_eq!(out[0]["type"], "text");
        assert_eq!(out[2]["type"], "thinking");
    }
}
