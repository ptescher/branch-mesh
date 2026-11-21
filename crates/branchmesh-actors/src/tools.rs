use crate::mesh::Mesh;
use async_trait::async_trait;
use branchmesh_core::repo::CommitOptions;
use branchmesh_core::{Change, EntryKind};
use serde_json::{json, Value};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

/// What a tool invocation can see and touch.
#[derive(Clone)]
pub struct ToolContext {
    pub mesh: Mesh,
    /// Branch the calling actor runs on.
    pub branch: String,
    /// Calling persona id.
    pub persona: String,
    /// Durable execution id of the caller.
    pub execution: String,
    /// The model's tool_use id (stable across replays).
    pub call_id: String,
    /// Consultation nesting depth of the calling branch.
    pub depth: u32,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str {
        ""
    }
    fn input_schema(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    /// Definition of an Anthropic server-side tool (executed by the API, not
    /// by BranchMesh). When `Some`, [`Tool::call`] is never invoked.
    fn server_definition(&self) -> Option<Value> {
        None
    }
    /// Journal category for this tool's side effects.
    fn journal_kind(&self) -> EntryKind {
        EntryKind::ToolCall
    }
    /// Execute the tool. `Err` is reported to the model as an error result.
    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<Value, String>;
}

pub type ToolRef = Arc<dyn Tool>;

/// API tool definition for a tool.
pub fn definition(tool: &dyn Tool) -> Value {
    tool.server_definition().unwrap_or_else(|| {
        json!({
            "name": tool.name(),
            "description": tool.description(),
            "input_schema": tool.input_schema(),
        })
    })
}

type ToolFuture = Pin<Box<dyn Future<Output = Result<Value, String>> + Send>>;
type ToolFn = Arc<dyn Fn(ToolContext, Value) -> ToolFuture + Send + Sync>;

/// A tool backed by an async closure.
pub struct FnTool {
    name: String,
    description: String,
    schema: Value,
    kind: EntryKind,
    f: ToolFn,
}

impl FnTool {
    /// Returns a ready-to-register [`ToolRef`] rather than `Self`.
    #[allow(clippy::new_ret_no_self)]
    pub fn new<F, Fut>(name: &str, description: &str, schema: Value, f: F) -> ToolRef
    where
        F: Fn(ToolContext, Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Value, String>> + Send + 'static,
    {
        Self::with_kind(name, description, schema, EntryKind::ToolCall, f)
    }

    pub fn with_kind<F, Fut>(name: &str, description: &str, schema: Value, kind: EntryKind, f: F) -> ToolRef
    where
        F: Fn(ToolContext, Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Value, String>> + Send + 'static,
    {
        Arc::new(FnTool {
            name: name.into(),
            description: description.into(),
            schema,
            kind,
            f: Arc::new(move |ctx, input| Box::pin(f(ctx, input))),
        })
    }
}

#[async_trait]
impl Tool for FnTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn input_schema(&self) -> Value {
        self.schema.clone()
    }
    fn journal_kind(&self) -> EntryKind {
        self.kind
    }
    async fn call(&self, ctx: &ToolContext, input: Value) -> Result<Value, String> {
        (self.f)(ctx.clone(), input).await
    }
}

/// An Anthropic server-side tool.
pub struct ServerTool {
    name: String,
    definition: Value,
}

#[async_trait]
impl Tool for ServerTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn server_definition(&self) -> Option<Value> {
        Some(self.definition.clone())
    }
    async fn call(&self, _: &ToolContext, _: Value) -> Result<Value, String> {
        Err(format!("{} is executed server-side by the model provider", self.name))
    }
}

fn sanitize_key(key: &str) -> String {
    key.trim_matches('/')
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/') { c } else { '-' })
        .collect()
}

// ------------------------------------------------------------------ built-ins

/// Persist a fact into the agent's branch state at `memory/<key>`.
pub fn memory_write_tool() -> ToolRef {
    FnTool::with_kind(
        "memory_write",
        "Store a fact or note in your long-term memory on the current branch. Use a short, stable key.",
        json!({
            "type": "object",
            "properties": {
                "key": {"type": "string", "description": "Short key, e.g. 'user-preferences' or 'citations/smith-2021'"},
                "value": {"description": "Any JSON value to store"}
            },
            "required": ["key", "value"]
        }),
        EntryKind::StateWrite,
        |ctx, input| async move {
            let key = sanitize_key(input["key"].as_str().ok_or("missing 'key'")?);
            if key.is_empty() {
                return Err("empty key".into());
            }
            let commit = ctx
                .mesh
                .repo()
                .commit(
                    &ctx.branch,
                    &[Change::set(format!("memory/{key}"), input["value"].clone())],
                    CommitOptions::new(format!("agent:{}", ctx.persona), format!("memory_write {key}"))
                        .execution(&ctx.execution),
                )
                .map_err(|e| e.to_string())?;
            Ok(json!({"ok": true, "key": key, "commit": commit}))
        },
    )
}

/// Read memory entries (one key, or everything).
pub fn memory_read_tool() -> ToolRef {
    FnTool::new(
        "memory_read",
        "Read your long-term memory on the current branch. Omit 'key' to list everything.",
        json!({
            "type": "object",
            "properties": {"key": {"type": "string"}}
        }),
        |ctx, input| async move {
            let repo = ctx.mesh.repo();
            match input["key"].as_str() {
                Some(k) => {
                    let v = repo.get(&ctx.branch, &format!("memory/{}", sanitize_key(k))).map_err(|e| e.to_string())?;
                    Ok(json!({"key": k, "value": v}))
                }
                None => {
                    let items = repo.list(&ctx.branch, "memory").map_err(|e| e.to_string())?;
                    let map: serde_json::Map<String, Value> = items
                        .into_iter()
                        .map(|(p, v)| (p.trim_start_matches("memory/").to_string(), v))
                        .collect();
                    Ok(Value::Object(map))
                }
            }
        },
    )
}

/// Current wall-clock time (a classic non-deterministic input: journaled, so
/// replays see the same time).
pub fn current_time_tool() -> ToolRef {
    FnTool::new(
        "current_time",
        "Get the current UTC time.",
        json!({"type": "object", "properties": {}}),
        |_, _| async move {
            let ms = branchmesh_core::now_ms();
            Ok(json!({"unix_ms": ms}))
        },
    )
}

/// HTTP GET a URL (journaled side effect). Disable with BRANCHMESH_HTTP_FETCH=off.
pub fn http_fetch_tool() -> ToolRef {
    FnTool::with_kind(
        "http_fetch",
        "Fetch a URL over HTTP(S) with GET and return the status and (truncated) body text.",
        json!({
            "type": "object",
            "properties": {"url": {"type": "string", "description": "http:// or https:// URL"}},
            "required": ["url"]
        }),
        EntryKind::HttpCall,
        |_, input| async move {
            if std::env::var("BRANCHMESH_HTTP_FETCH").map(|v| v == "off").unwrap_or(false) {
                return Err("http_fetch is disabled on this server".into());
            }
            let url = input["url"].as_str().ok_or("missing 'url'")?;
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                return Err("only http(s) URLs are allowed".into());
            }
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(20))
                .build()
                .map_err(|e| e.to_string())?;
            let resp = client.get(url).send().await.map_err(|e| e.to_string())?;
            let status = resp.status().as_u16();
            let ctype = resp
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            let body = resp.text().await.map_err(|e| e.to_string())?;
            let truncated: String = body.chars().take(20_000).collect();
            Ok(json!({"status": status, "content_type": ctype, "body": truncated, "truncated": body.len() > truncated.len()}))
        },
    )
}

/// Consult another agent persona. The current branch is forked, the other
/// agent answers on the fork, and the fork is merged back.
pub fn ask_agent_tool() -> ToolRef {
    FnTool::with_kind(
        "ask_agent",
        "Consult another agent persona. They see a snapshot of this conversation, work on a forked branch, \
         and their findings (including memory updates) are merged back. Returns their reply.",
        json!({
            "type": "object",
            "properties": {
                "persona": {"type": "string", "description": "Persona id of the agent to consult"},
                "question": {"type": "string", "description": "What you want them to do or answer"}
            },
            "required": ["persona", "question"]
        }),
        EntryKind::MessageSent,
        |ctx, input| async move {
            let persona = input["persona"].as_str().ok_or("missing 'persona'")?.to_string();
            let question = input["question"].as_str().ok_or("missing 'question'")?.to_string();
            let reply = ctx.mesh.consult(&ctx, &persona, &question).await.map_err(|e| e.to_string())?;
            Ok(reply)
        },
    )
}

/// Anthropic server-side web search (`web_search_20260209`).
pub fn web_search_tool() -> ToolRef {
    Arc::new(ServerTool {
        name: "web_search".into(),
        definition: json!({"type": "web_search_20260209", "name": "web_search", "max_uses": 5}),
    })
}

/// Anthropic server-side code execution sandbox (`code_execution_20260521`).
/// Note: not sent alongside `web_search`, which already runs code internally.
pub fn code_interpreter_tool() -> ToolRef {
    Arc::new(ServerTool {
        name: "code_execution".into(),
        definition: json!({"type": "code_execution_20260521", "name": "code_execution"}),
    })
}

pub fn builtin_tools() -> Vec<ToolRef> {
    vec![
        memory_write_tool(),
        memory_read_tool(),
        current_time_tool(),
        http_fetch_tool(),
        ask_agent_tool(),
        web_search_tool(),
        code_interpreter_tool(),
    ]
}
