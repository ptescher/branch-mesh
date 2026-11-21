//! Offline, deterministic LLM stand-in so BranchMesh runs (and is testable)
//! without an API key. It understands a few phrases to exercise tools:
//!
//! * `remember <fact>`         -> `memory_write`
//! * `what do you remember`    -> `memory_read`
//! * `ask <persona> <question>`-> `ask_agent` (agent-to-agent consultation)
//! * `what time is it`         -> `current_time`
//!
//! Anything else is echoed back with the persona's first system-prompt sentence.

use crate::llm::{DeltaFn, LlmProvider, LlmRequest, LlmResponse};
use async_trait::async_trait;
use branchmesh_core::Result;
use serde_json::{json, Value};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct MockProvider {
    /// Delay between streamed words (for a visible streaming effect).
    pub word_delay: Duration,
}

impl Default for MockProvider {
    fn default() -> Self {
        MockProvider { word_delay: Duration::from_millis(12) }
    }
}

impl MockProvider {
    pub fn instant() -> Self {
        MockProvider { word_delay: Duration::ZERO }
    }
}

fn has_tool(req: &LlmRequest, name: &str) -> bool {
    req.tools.iter().any(|t| t["name"] == name)
}

fn text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn slug(s: &str) -> String {
    let words: Vec<String> = s
        .split_whitespace()
        .take(4)
        .map(|w| w.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_lowercase())
        .filter(|w| !w.is_empty())
        .collect();
    if words.is_empty() {
        "note".into()
    } else {
        words.join("-")
    }
}

impl MockProvider {
    fn decide(&self, req: &LlmRequest) -> (Vec<Value>, &'static str) {
        let n = req.messages.len();
        let last = req.messages.iter().rev().find(|m| m["role"] != "system").cloned().unwrap_or(json!({}));
        let note = req
            .messages
            .iter()
            .rev()
            .find(|m| m["role"] == "system")
            .and_then(|m| m["content"].as_str())
            .map(|t| t.lines().last().unwrap_or("").trim().to_string());

        if req.output_schema.is_some() {
            // Structured requests: return an empty instance of the schema
            // (arrays empty, strings blank) so callers use their fallbacks.
            let mut obj = serde_json::Map::new();
            if let Some(props) = req.output_schema.as_ref().and_then(|s| s["properties"].as_object()) {
                for (k, v) in props {
                    let empty = match v["type"].as_str() {
                        Some("array") => json!([]),
                        Some("object") => json!({}),
                        Some("boolean") => json!(false),
                        Some("number") | Some("integer") => json!(0),
                        _ => json!(""),
                    };
                    obj.insert(k.clone(), empty);
                }
            }
            return (vec![json!({"type": "text", "text": Value::Object(obj).to_string()})], "end_turn");
        }

        // Tool results came back: summarize them.
        if let Some(blocks) = last["content"].as_array() {
            let results: Vec<String> = blocks
                .iter()
                .filter(|b| b["type"] == "tool_result")
                .map(|b| text_of(&b["content"]).chars().take(400).collect())
                .collect();
            if !results.is_empty() {
                let text = format!("Done. Tool result: {}", results.join(" | "));
                return (vec![json!({"type": "text", "text": text})], "end_turn");
            }
        }

        let user_text = text_of(&last["content"]);
        // Only look at the newest line (consultation messages carry context above).
        let line = user_text.lines().last().unwrap_or("").trim().to_string();
        let lower = line.to_lowercase();
        // Byte offsets found in `lower` are only valid in `line` if lowercasing
        // preserved lengths (true for ASCII).
        let src: &str = if lower.len() == line.len() { &line } else { &lower };
        let tool_use = |name: &str, input: Value| {
            vec![
                json!({"type": "text", "text": format!("Using {name}.")}),
                json!({"type": "tool_use", "id": format!("toolu_mock_{n}"), "name": name, "input": input}),
            ]
        };

        if let Some(rest) = lower.strip_prefix("ask ") {
            if has_tool(req, "ask_agent") {
                let mut parts = rest.splitn(2, ' ');
                let persona = parts.next().unwrap_or("").trim_end_matches([':', ',']).to_string();
                let question = src[(4 + persona.len()).min(src.len())..].trim_start_matches([':', ',', ' ']).to_string();
                return (tool_use("ask_agent", json!({"persona": persona, "question": question})), "tool_use");
            }
        }
        if let Some(rest) = lower.find("remember ").map(|i| src[i + 9..].trim().to_string()) {
            if has_tool(req, "memory_write") && !lower.starts_with("what") {
                return (tool_use("memory_write", json!({"key": slug(&rest), "value": rest})), "tool_use");
            }
        }
        if (lower.contains("what do you remember") || lower.contains("recall")) && has_tool(req, "memory_read") {
            return (tool_use("memory_read", json!({})), "tool_use");
        }
        if lower.contains("what time") && has_tool(req, "current_time") {
            return (tool_use("current_time", json!({})), "tool_use");
        }

        let flavor = req.system.split(['.', '\n']).next().unwrap_or("").trim();
        // Labeled messages (`[label]\ntext`, see ContextStyle) get a relay-style echo.
        let label = user_text
            .strip_prefix('[')
            .and_then(|r| r.split_once("]\n"))
            .map(|(l, _)| l)
            .filter(|l| !l.contains('→') && !l.ends_with(" replied"));
        let mut text = if let Some(label) = label {
            format!("[mock] ({label}) {line}")
        } else if flavor.is_empty() {
            format!("[mock] I received: \"{line}\". Set an LLM API key to talk to a real model.")
        } else {
            format!("[mock · {flavor}] I received: \"{line}\". Set an LLM API key to talk to a real model.")
        };
        if let Some(note) = note.filter(|n| !n.is_empty()) {
            text.push_str(&format!(" (Keeping in mind: {note})"));
        }
        (vec![json!({"type": "text", "text": text})], "end_turn")
    }
}

#[async_trait]
impl LlmProvider for MockProvider {
    fn name(&self) -> &str {
        "mock"
    }

    async fn complete(&self, req: &LlmRequest, on_delta: DeltaFn<'_>) -> Result<LlmResponse> {
        let (content, stop_reason) = self.decide(req);
        for block in &content {
            if let Some(text) = block["text"].as_str() {
                for (i, word) in text.split(' ').enumerate() {
                    let piece = if i == 0 { word.to_string() } else { format!(" {word}") };
                    on_delta(&piece);
                    if !self.word_delay.is_zero() {
                        tokio::time::sleep(self.word_delay).await;
                    }
                }
            }
        }
        let out_tokens: usize = content.iter().map(|b| b.to_string().len() / 4).sum();
        let in_tokens: usize = req.messages.iter().map(|m| m.to_string().len() / 4).sum::<usize>() + req.system.len() / 4;
        Ok(LlmResponse {
            id: format!("msg_mock_{}", branchmesh_core::new_id()),
            model: format!("mock/{}", req.model),
            provider: "mock".into(),
            content,
            stop_reason: stop_reason.into(),
            stop_details: Value::Null,
            usage: json!({"input_tokens": in_tokens, "output_tokens": out_tokens}),
        })
    }
}
