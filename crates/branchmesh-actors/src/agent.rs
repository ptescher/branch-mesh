//! The durable agent loop: one inbound message -> LLM/tool steps -> reply.
//!
//! Conversation state lives on the actor's branch under `messages/<key>`
//! (keys sort chronologically). Every step goes through the execution
//! journal, so a crashed turn resumes by replaying recorded LLM completions
//! and tool outputs instead of calling them again.

use crate::llm::{LlmRequest, LlmResponse};
use crate::mesh::{Envelope, Mesh};
use crate::persona::AgentPersona;
use crate::tools::{definition, ToolContext, ToolRef};
use branchmesh_core::repo::CommitOptions;
use branchmesh_core::{Change, DurableContext, EntryKind, Hash, Repo, Result};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

fn content_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

fn as_blocks(content: &Value) -> Vec<Value> {
    match content {
        Value::String(s) => vec![json!({"type": "text", "text": s})],
        Value::Array(a) => a.clone(),
        _ => vec![],
    }
}

/// Render the branch's stored messages as an API conversation from the
/// point of view of `persona`. Deterministic for a given state, so the
/// prompt prefix stays stable (good for caching and preserved thinking).
pub fn build_context(repo: &Repo, branch: &str, persona: &str) -> Result<Vec<Value>> {
    let stored = repo.list(branch, "messages")?;
    let mut turns: Vec<(String, Vec<Value>)> = Vec::new();
    let mut push = |role: &str, blocks: Vec<Value>| {
        if blocks.is_empty() {
            return;
        }
        match turns.last_mut() {
            Some((r, b)) if r == role => b.extend(blocks),
            _ => turns.push((role.to_string(), blocks)),
        }
    };

    for (_, m) in &stored {
        // Messages from a consultation this persona started: the outcome is
        // already in its tool_result, so don't show the side conversation.
        if m["consult_of"].as_str() == Some(persona) {
            continue;
        }
        let own = m["persona"].as_str() == Some(persona);
        let role = m["role"].as_str().unwrap_or("user");
        let kind = m["kind"].as_str().unwrap_or("message");
        match (role, kind) {
            ("assistant", _) if own => push("assistant", as_blocks(&m["content"])),
            ("assistant", _) => {
                let text = content_text(&m["content"]);
                if !text.trim().is_empty() {
                    let who = m["persona"].as_str().unwrap_or("another agent");
                    push("user", vec![json!({"type": "text", "text": format!("[{who} replied]\n{text}")})]);
                }
            }
            (_, "tool_results") if own => push("user", as_blocks(&m["content"])),
            (_, "tool_results") => {}
            _ => {
                let text = content_text(&m["content"]);
                let from = m["author"].as_str().unwrap_or("user");
                let to = m["persona"].as_str();
                let rendered = if from == "user" && (to.is_none() || to == Some(persona)) {
                    text
                } else if from == "user" {
                    format!("[user → {}]\n{text}", to.unwrap_or("?"))
                } else {
                    format!("[message from {from}]\n{text}")
                };
                push("user", vec![json!({"type": "text", "text": rendered})]);
            }
        }
    }

    // Repair dangling tool calls (e.g. a turn that was dead-lettered between
    // the model's tool_use and our tool_result) with synthetic error results.
    let mut i = 0;
    while i < turns.len() {
        if turns[i].0 == "assistant" {
            let ids: Vec<String> = turns[i]
                .1
                .iter()
                .filter(|b| b["type"] == "tool_use")
                .filter_map(|b| b["id"].as_str().map(String::from))
                .collect();
            if !ids.is_empty() {
                let answered: HashSet<String> = turns
                    .get(i + 1)
                    .filter(|t| t.0 == "user")
                    .map(|t| {
                        t.1.iter()
                            .filter(|b| b["type"] == "tool_result")
                            .filter_map(|b| b["tool_use_id"].as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                let missing: Vec<Value> = ids
                    .iter()
                    .filter(|id| !answered.contains(*id))
                    .map(|id| json!({"type": "tool_result", "tool_use_id": id, "content": "tool call interrupted", "is_error": true}))
                    .collect();
                if !missing.is_empty() {
                    if turns.get(i + 1).map(|t| t.0 == "user").unwrap_or(false) {
                        let mut blocks = missing;
                        blocks.extend(std::mem::take(&mut turns[i + 1].1));
                        turns[i + 1].1 = blocks;
                    } else {
                        turns.insert(i + 1, ("user".into(), missing));
                    }
                }
            }
        }
        i += 1;
    }

    // tool_result blocks must lead a user turn.
    for (role, blocks) in turns.iter_mut() {
        if role == "user" {
            let (mut results, rest): (Vec<Value>, Vec<Value>) =
                std::mem::take(blocks).into_iter().partition(|b| b["type"] == "tool_result");
            results.extend(rest);
            *blocks = results;
        }
    }
    // The API requires the conversation to start with a user turn.
    while turns.first().map(|t| t.0 == "assistant").unwrap_or(false) {
        turns.remove(0);
    }
    Ok(turns.into_iter().map(|(role, content)| json!({"role": role, "content": content})).collect())
}

struct Turn<'a> {
    mesh: &'a Mesh,
    durable: &'a DurableContext,
    env: &'a Envelope,
    persona: &'a AgentPersona,
    consult_of: Option<String>,
}

impl Turn<'_> {
    fn write_message(&self, label: &str, mut record: Value, message: &str) -> Result<Hash> {
        let ts = branchmesh_core::now_ms();
        let short: String = self.durable.id().trim_start_matches("exec-").chars().take(8).collect();
        let key = format!("{ts:013}-{short}-{label}");
        record["id"] = json!(key);
        record["ts"] = json!(ts);
        record["execution"] = json!(self.durable.id());
        record["branch"] = json!(self.env.branch);
        if let Some(c) = &self.consult_of {
            record["consult_of"] = json!(c);
        }
        let author = record["author"].as_str().unwrap_or("system").to_string();
        self.mesh.repo().commit(
            &self.env.branch,
            &[Change::set(format!("messages/{key}"), record)],
            CommitOptions::new(author, message).execution(self.durable.id()),
        )
    }
}

/// Process one envelope end-to-end. Returns the reply payload stored on the envelope.
pub async fn run_turn(mesh: &Mesh, env: &Envelope) -> Result<Value> {
    let repo = mesh.repo();
    let persona = mesh.persona(&env.to)?;
    let branch = repo.branch(&env.branch)?;
    let (durable, _exec) = DurableContext::begin(
        repo,
        &env.execution,
        &persona.id,
        &env.branch,
        Some(env.id.clone()),
        json!({ "from": env.from, "content": env.content }),
    )?;
    let turn = Turn {
        mesh,
        durable: &durable,
        env,
        persona: &persona,
        consult_of: branch.meta["consult_of"].as_str().map(String::from),
    };
    let depth = branch.meta["depth"].as_u64().unwrap_or(0) as u32;
    let result = run_inner(&turn, depth).await;
    match &result {
        Ok(v) => {
            durable.complete(v.clone())?;
        }
        Err(e) => {
            durable.fail(&e.to_string())?;
        }
    }
    result
}

fn resolve_tools(mesh: &Mesh, persona: &AgentPersona) -> (Vec<Value>, HashMap<String, ToolRef>) {
    let mut defs = Vec::new();
    let mut map = HashMap::new();
    let has_web_search = persona.tools.iter().any(|t| t == "web_search");
    for name in &persona.tools {
        // web_search_20260209 runs code internally; a second execution
        // environment confuses the model.
        if name == "code_execution" && has_web_search {
            continue;
        }
        match mesh.tool(name) {
            Some(t) => {
                defs.push(definition(t.as_ref()));
                map.insert(name.clone(), t);
            }
            None => tracing::warn!(persona = %persona.id, tool = %name, "unknown tool; skipping"),
        }
    }
    (defs, map)
}

async fn run_inner(turn: &Turn<'_>, depth: u32) -> Result<Value> {
    let mesh = turn.mesh;
    let repo = mesh.repo();
    let env = turn.env;
    let persona = turn.persona;
    let durable = turn.durable;
    let actor_key = env.actor_key();

    // 1. Record the inbound message on the branch.
    let _: Hash = durable
        .run(
            EntryKind::MessageReceived,
            "receive",
            json!({ "from": env.from, "content": env.content }),
            || async {
                turn.write_message(
                    "in",
                    json!({
                        "role": "user",
                        "kind": "message",
                        "author": env.from,
                        "persona": persona.id,
                        "content": env.content,
                    }),
                    &format!("message from {}", env.from),
                )
            },
        )
        .await?;

    let (tool_defs, tool_map) = resolve_tools(mesh, persona);
    let tool_names: Vec<&String> = tool_map.keys().collect();
    let events = mesh.events().clone();
    let (branch_name, persona_id, exec_id) = (env.branch.clone(), persona.id.clone(), durable.id().to_string());
    let on_delta = move |text: &str| {
        events.emit(
            "llm_delta",
            json!({ "branch": branch_name, "persona": persona_id, "execution": exec_id, "text": text }),
        );
    };

    let mut last: Option<LlmResponse> = None;
    let mut steps = 0u64;
    for step in 0..persona.max_steps.max(1) {
        steps += 1;
        let messages = build_context(repo, &env.branch, &persona.id)?;
        let context_commit = repo.head(&env.branch)?;
        let req = LlmRequest {
            model: persona.model.clone(),
            system: persona.system_prompt.clone(),
            messages,
            tools: tool_defs.clone(),
            max_tokens: persona.max_tokens,
            effort: persona.effort.clone(),
            output_schema: None,
        };
        let replaying = durable.is_replaying();
        let t0 = std::time::Instant::now();
        let llm = mesh.llm();
        let resp: LlmResponse = durable
            .run(
                EntryKind::LlmCompletion,
                &format!("llm:{step}"),
                json!({
                    "model": req.model,
                    "provider": llm.name(),
                    "context_commit": context_commit,
                    "messages": req.messages.len(),
                    "last_message": req.messages.last(),
                    "tools": tool_names,
                }),
                || async { llm.complete(&req, &on_delta).await },
            )
            .await?;
        if !replaying {
            mesh.record_llm(&actor_key, &resp, t0.elapsed().as_millis() as u64);
        }

        if resp.stop_reason == "refusal" {
            // Partial output of a declined turn is discarded, not stored.
            last = Some(resp);
            break;
        }

        let _: Hash = durable
            .run(
                EntryKind::StateWrite,
                &format!("append_assistant:{step}"),
                json!({ "stop_reason": resp.stop_reason, "model": resp.model }),
                || async {
                    turn.write_message(
                        &format!("a{step:02}"),
                        json!({
                            "role": "assistant",
                            "kind": "message",
                            "author": format!("agent:{}", persona.id),
                            "persona": persona.id,
                            "model": resp.model,
                            "stop_reason": resp.stop_reason,
                            "usage": resp.usage,
                            "content": resp.content,
                        }),
                        &format!("{} replied ({})", persona.id, resp.stop_reason),
                    )
                },
            )
            .await?;

        match resp.stop_reason.as_str() {
            "tool_use" => {
                let mut results = Vec::new();
                for block in resp.tool_uses() {
                    let name = block["name"].as_str().unwrap_or("").to_string();
                    let id = block["id"].as_str().unwrap_or("").to_string();
                    let input = block["input"].clone();
                    let tool = tool_map.get(&name).cloned();
                    let kind = tool.as_ref().map(|t| t.journal_kind()).unwrap_or(EntryKind::ToolCall);
                    let ctx = ToolContext {
                        mesh: mesh.clone(),
                        branch: env.branch.clone(),
                        persona: persona.id.clone(),
                        execution: durable.id().to_string(),
                        call_id: id.clone(),
                        depth,
                    };
                    let out: Value = durable
                        .run(kind, &format!("tool:{name}:{id}"), input.clone(), || async {
                            let r = match &tool {
                                Some(t) => t.call(&ctx, input.clone()).await,
                                None => Err(format!("unknown tool '{name}'")),
                            };
                            Ok(match r {
                                Ok(v) => json!({ "ok": true, "output": v }),
                                Err(e) => json!({ "ok": false, "error": e }),
                            })
                        })
                        .await?;
                    let ok = out["ok"].as_bool().unwrap_or(false);
                    let payload = if ok { &out["output"] } else { &out["error"] };
                    let text = match payload {
                        Value::String(s) => s.clone(),
                        v => serde_json::to_string_pretty(v).unwrap_or_default(),
                    };
                    let mut result = json!({ "type": "tool_result", "tool_use_id": id, "content": text });
                    if !ok {
                        result["is_error"] = json!(true);
                    }
                    results.push(result);
                }
                let _: Hash = durable
                    .run(
                        EntryKind::StateWrite,
                        &format!("append_tool_results:{step}"),
                        json!({ "count": results.len() }),
                        || async {
                            turn.write_message(
                                &format!("t{step:02}"),
                                json!({
                                    "role": "user",
                                    "kind": "tool_results",
                                    "author": "system",
                                    "persona": persona.id,
                                    "content": results,
                                }),
                                &format!("tool results for {}", persona.id),
                            )
                        },
                    )
                    .await?;
                last = Some(resp);
            }
            // Server-side tool loop paused; re-send to let it continue.
            "pause_turn" => last = Some(resp),
            _ => {
                last = Some(resp);
                break;
            }
        }
    }

    let resp = last.unwrap_or_default();
    let text = if resp.stop_reason == "refusal" {
        let category = resp.stop_details["category"].as_str().unwrap_or("unspecified");
        format!("[request declined by the model's safety system (category: {category})]")
    } else {
        resp.text()
    };
    Ok(json!({
        "text": text,
        "stop_reason": resp.stop_reason,
        "model": resp.model,
        "steps": steps,
        "commit": repo.head(&env.branch)?,
    }))
}
