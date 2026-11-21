//! LLM-assisted semantic conflict resolution for merges.

use crate::llm::{LlmProvider, LlmRequest};
use async_trait::async_trait;
use branchmesh_core::merge::MergeContext;
use branchmesh_core::{Conflict, ConflictResolver, Resolution, Result};
use serde_json::{json, Map, Value};
use std::sync::Arc;

const SYSTEM: &str = "You resolve merge conflicts in the persisted state of AI agents. Each conflict \
is a JSON value at a path that two branches changed differently since their common base. \
'ours' is the target branch (usually the main thread), 'theirs' is the branch being merged in \
(usually a side-consultation or sub-agent). Produce, for every path, a single value that \
preserves the intent and information of both sides: combine lists and notes, keep corrections, \
and prefer the more specific or more recent fact when they truly contradict. \
Return value_json as a JSON-encoded string of the merged value. Set delete=true only if the \
path should be removed.";

/// Resolves conflicts by asking an LLM (structured output), with a
/// deterministic heuristic fallback for anything it leaves unresolved.
pub struct LlmConflictResolver {
    llm: Arc<dyn LlmProvider>,
    model: String,
}

impl LlmConflictResolver {
    pub fn new(llm: Arc<dyn LlmProvider>, model: impl Into<String>) -> Self {
        LlmConflictResolver { llm, model: model.into() }
    }
}

/// Union-style merge: objects merge recursively, arrays are unioned, scalar
/// disagreements take `theirs` (the newer, side-branch value).
pub fn heuristic_merge(ours: Option<&Value>, theirs: Option<&Value>) -> Option<Value> {
    match (ours, theirs) {
        (None, t) => t.cloned(),
        (o, None) => o.cloned(),
        (Some(Value::Object(o)), Some(Value::Object(t))) => {
            let mut out = Map::new();
            for k in o.keys().chain(t.keys()) {
                if out.contains_key(k) {
                    continue;
                }
                if let Some(v) = heuristic_merge(o.get(k), t.get(k)) {
                    out.insert(k.clone(), v);
                }
            }
            Some(Value::Object(out))
        }
        (Some(Value::Array(o)), Some(Value::Array(t))) => {
            let mut out = o.clone();
            for item in t {
                if !out.contains(item) {
                    out.push(item.clone());
                }
            }
            Some(Value::Array(out))
        }
        (_, Some(t)) => Some(t.clone()),
    }
}

fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "resolutions": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "delete": {"type": "boolean"},
                        "value_json": {"type": "string"},
                        "rationale": {"type": "string"}
                    },
                    "required": ["path", "delete", "value_json", "rationale"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["resolutions"],
        "additionalProperties": false
    })
}

#[async_trait]
impl ConflictResolver for LlmConflictResolver {
    async fn resolve(&self, conflicts: &[Conflict], ctx: &MergeContext) -> Result<Vec<Resolution>> {
        let payload = json!({
            "merge": { "source (theirs)": ctx.source, "target (ours)": ctx.target },
            "conflicts": conflicts,
        });
        let req = LlmRequest {
            model: self.model.clone(),
            system: SYSTEM.into(),
            messages: vec![json!({
                "role": "user",
                "content": format!("Resolve these conflicts:\n\n{}", serde_json::to_string_pretty(&payload)?),
            })],
            tools: vec![],
            max_tokens: 16000,
            effort: Some("medium".into()),
            output_schema: Some(schema()),
        };
        let noop = |_: &str| {};
        let mut resolved: Vec<Resolution> = Vec::new();
        match self.llm.complete(&req, &noop).await {
            Ok(resp) if resp.stop_reason != "refusal" => {
                let parsed: Value = serde_json::from_str(&resp.text()).unwrap_or(Value::Null);
                if let Some(items) = parsed["resolutions"].as_array() {
                    for item in items {
                        let Some(path) = item["path"].as_str() else { continue };
                        if !conflicts.iter().any(|c| c.path == path) {
                            continue;
                        }
                        let value = if item["delete"].as_bool().unwrap_or(false) {
                            None
                        } else {
                            match item["value_json"].as_str().map(serde_json::from_str::<Value>) {
                                Some(Ok(v)) => Some(v),
                                _ => continue,
                            }
                        };
                        resolved.push(Resolution {
                            path: path.to_string(),
                            value,
                            rationale: item["rationale"].as_str().map(String::from),
                            resolved_by: format!("llm:{}", resp.model),
                        });
                    }
                }
            }
            Ok(_) => tracing::warn!("semantic merge request was declined; using heuristic merge"),
            Err(e) => tracing::warn!(error = %e, "semantic merge LLM call failed; using heuristic merge"),
        }
        for c in conflicts {
            if !resolved.iter().any(|r| r.path == c.path) {
                resolved.push(Resolution {
                    path: c.path.clone(),
                    value: heuristic_merge(c.ours.as_ref(), c.theirs.as_ref()),
                    rationale: Some("heuristic union merge (LLM gave no resolution)".into()),
                    resolved_by: "heuristic".into(),
                });
            }
        }
        Ok(resolved)
    }
}
