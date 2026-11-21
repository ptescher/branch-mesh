//! # BranchMesh
//!
//! An embedded durable-execution and forkable multi-agent runtime.
//!
//! ```no_run
//! use branchmesh::{Runtime, AgentPersona, MergeStrategy, web_search_tool, code_interpreter_tool};
//!
//! # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
//! let runtime = Runtime::open("./data_store.db").await?;
//!
//! let assistant = AgentPersona::new("research-bot")
//!     .with_system_prompt("You are a thorough research analyst.")
//!     .with_toolset(vec![web_search_tool(), code_interpreter_tool()]);
//! let verifier = AgentPersona::new("citation-checker")
//!     .with_system_prompt("You verify citations rigorously.");
//!
//! let thread = runtime.spawn_thread(&assistant, "user_session_123").await?;
//! thread.send_message("Summarize recent work on CRDTs.").await?;
//!
//! // Fork state for a peer consultation handled by another persona.
//! let sub_branch = thread.fork("peer_consultation_branch").await?.with_persona(&verifier)?;
//! sub_branch.send_message("Agent B, verify these citations.").await?;
//!
//! // Merge results back into the main thread.
//! thread.merge(sub_branch, MergeStrategy::SemanticLLM).await?;
//! # Ok(()) }
//! ```

use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use branchmesh_actors as actors;
pub use branchmesh_core as core;

pub use branchmesh_actors::{
    ask_agent_tool, code_interpreter_tool, AuthorFrame, ContextStyle, current_time_tool, http_fetch_tool, memory_read_tool,
    memory_write_tool, provider_from_env, web_search_tool, ActorInfo, AgentPersona, AnthropicConfig,
    AnthropicProvider, Envelope, EnvelopeStatus, FnTool, LlmConflictResolver, LlmProvider, LlmRequest,
    LlmResponse, Mesh, MeshConfig, MockProvider, Reply, Speech, Synthesized, Tool, ToolContext, ToolRef, Transcript,
    VoiceConfig, DEFAULT_MODEL,
};
pub use branchmesh_core::repo::CommitOptions;
pub use branchmesh_core::{
    BranchInfo, BranchStatus, Change, Commit, Conflict, DiffEntry, Error, Event, EventBus, Execution,
    Hash, JournalEntry, MergeOptions, MergeOutcome, MergePreview, MergeStrategy, Repo, Result, Store,
};

/// A point in a branch's history (fork / rollback target).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BranchPoint {
    pub branch: String,
    pub commit: Hash,
}

pub struct RuntimeBuilder {
    path: PathBuf,
    llm: Option<Arc<dyn LlmProvider>>,
    config: MeshConfig,
    recover: bool,
    merge_model: Option<String>,
    speech: Option<Speech>,
}

impl RuntimeBuilder {
    /// Use a specific speech stack (default: OpenRouter if `OPENROUTER_API_KEY` is set, else mock).
    pub fn with_speech(mut self, speech: Speech) -> Self {
        self.speech = Some(speech);
        self
    }

    /// Use a specific LLM provider (default: Claude if `ANTHROPIC_API_KEY` is set, else the mock).
    pub fn with_llm(mut self, llm: Arc<dyn LlmProvider>) -> Self {
        self.llm = Some(llm);
        self
    }

    pub fn with_config(mut self, config: MeshConfig) -> Self {
        self.config = config;
        self
    }

    /// Model used for LLM-assisted semantic merges.
    pub fn with_merge_model(mut self, model: impl Into<String>) -> Self {
        self.merge_model = Some(model.into());
        self
    }

    /// Re-dispatch in-flight messages on open (default: true).
    pub fn recover(mut self, recover: bool) -> Self {
        self.recover = recover;
        self
    }

    pub async fn open(self) -> Result<Runtime> {
        let store = Store::open(&self.path)?;
        let repo = Repo::new(store, EventBus::default())?;
        let llm = self.llm.unwrap_or_else(provider_from_env);
        let mesh = Mesh::new(repo, llm, self.config);
        let merge_model = self
            .merge_model
            .or_else(|| std::env::var("BRANCHMESH_MERGE_MODEL").ok())
            .unwrap_or_else(|| DEFAULT_MODEL.to_string());
        let speech = self.speech.unwrap_or_else(Speech::from_env);
        let rt = Runtime { inner: Arc::new(RtInner { mesh, path: self.path, merge_model, speech }) };
        if self.recover {
            rt.inner.mesh.recover()?;
        }
        Ok(rt)
    }
}

struct RtInner {
    mesh: Mesh,
    path: PathBuf,
    merge_model: String,
    speech: Speech,
}

/// The embedded BranchMesh runtime. Cheap to clone.
#[derive(Clone)]
pub struct Runtime {
    inner: Arc<RtInner>,
}

impl Runtime {
    /// Open (or create) a runtime backed by a single database file.
    pub async fn open(path: impl AsRef<Path>) -> Result<Runtime> {
        Self::builder(path).open().await
    }

    pub fn builder(path: impl AsRef<Path>) -> RuntimeBuilder {
        RuntimeBuilder {
            path: path.as_ref().to_path_buf(),
            llm: None,
            config: MeshConfig::default(),
            recover: true,
            merge_model: None,
            speech: None,
        }
    }

    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    pub fn mesh(&self) -> &Mesh {
        &self.inner.mesh
    }

    pub fn repo(&self) -> &Repo {
        self.inner.mesh.repo()
    }

    pub fn events(&self) -> &EventBus {
        self.inner.mesh.events()
    }

    pub fn speech(&self) -> &Speech {
        &self.inner.speech
    }

    pub fn register_tool(&self, tool: ToolRef) {
        self.inner.mesh.register_tool(tool);
    }

    pub fn register_persona(&self, persona: &AgentPersona) -> Result<AgentPersona> {
        self.inner.mesh.save_persona(persona)
    }

    /// Start (or resume) a user session on branch `threads/<thread_id>`.
    pub async fn spawn_thread(&self, persona: &AgentPersona, thread_id: &str) -> Result<Thread> {
        self.register_persona(persona)?;
        let branch = format!("threads/{thread_id}");
        self.repo().get_or_create_branch(
            &branch,
            serde_json::json!({ "kind": "thread", "persona": persona.id, "thread_id": thread_id }),
        )?;
        Ok(Thread { rt: self.clone(), branch, persona: persona.id.clone() })
    }

    /// Attach to an existing branch as `persona`.
    pub fn thread(&self, branch: &str, persona: &str) -> Result<Thread> {
        self.repo().branch(branch)?;
        self.inner.mesh.persona(persona)?;
        Ok(Thread { rt: self.clone(), branch: branch.into(), persona: persona.into() })
    }

    /// The conflict resolver used for [`MergeStrategy::SemanticLLM`].
    pub fn resolver(&self) -> Arc<LlmConflictResolver> {
        Arc::new(LlmConflictResolver::new(self.inner.mesh.llm(), self.inner.merge_model.clone()))
    }

    /// Merge `source` into `target`. The semantic resolver is attached automatically.
    pub async fn merge(&self, source: &str, target: &str, mut opts: MergeOptions) -> Result<MergeOutcome> {
        if opts.resolver.is_none() {
            opts.resolver = Some(self.resolver());
        }
        self.repo().merge(source, target, opts).await
    }
}

/// A conversation bound to a branch and a persona.
#[derive(Clone)]
pub struct Thread {
    rt: Runtime,
    branch: String,
    persona: String,
}

impl Thread {
    pub fn branch(&self) -> &str {
        &self.branch
    }

    pub fn persona(&self) -> &str {
        &self.persona
    }

    pub fn runtime(&self) -> &Runtime {
        &self.rt
    }

    /// Address this branch's messages to another persona.
    pub fn with_persona(self, persona: &AgentPersona) -> Result<Thread> {
        self.rt.register_persona(persona)?;
        Ok(Thread { persona: persona.id.clone(), ..self })
    }

    /// Send a user message and wait for the agent's reply.
    pub async fn send_message(&self, text: &str) -> Result<Reply> {
        self.send_message_from("user", text).await
    }

    pub async fn send_message_from(&self, from: &str, text: &str) -> Result<Reply> {
        self.rt
            .mesh()
            .ask(&self.persona, &self.branch, from, text, serde_json::Value::Null, None)
            .await
    }

    /// Enqueue a message without waiting.
    pub fn post(&self, text: &str) -> Result<Envelope> {
        self.rt.mesh().send(&self.persona, &self.branch, "user", text, serde_json::Value::Null, None)
    }

    /// O(1) fork into `<branch>/<name>` (or `name` if it contains a `/`).
    pub async fn fork(&self, name: &str) -> Result<Thread> {
        let fork_name = if name.contains('/') { name.to_string() } else { format!("{}/{}", self.branch, name) };
        self.rt.repo().fork(
            &self.branch,
            &fork_name,
            serde_json::json!({ "kind": "fork", "persona": self.persona }),
        )?;
        Ok(Thread { rt: self.rt.clone(), branch: fork_name, persona: self.persona.clone() })
    }

    /// Fork from a historical commit (time travel).
    pub async fn fork_at(&self, point: &BranchPoint, name: &str) -> Result<Thread> {
        self.rt.repo().fork_at(
            &point.commit,
            name,
            Some(point.branch.clone()),
            serde_json::json!({ "kind": "fork", "persona": self.persona }),
        )?;
        Ok(Thread { rt: self.rt.clone(), branch: name.into(), persona: self.persona.clone() })
    }

    /// Merge `other` into this thread and mark it merged.
    pub async fn merge(&self, other: Thread, strategy: MergeStrategy) -> Result<MergeOutcome> {
        let mut opts = MergeOptions::new(strategy);
        opts.author = "user".into();
        opts.close_source = true;
        self.rt.merge(&other.branch, &self.branch, opts).await
    }

    pub fn merge_preview(&self, other: &Thread) -> Result<MergePreview> {
        self.rt.repo().merge_preview(&other.branch, &self.branch)
    }

    pub fn head(&self) -> Result<Hash> {
        self.rt.repo().head(&self.branch)
    }

    pub fn branch_point(&self) -> Result<BranchPoint> {
        Ok(BranchPoint { branch: self.branch.clone(), commit: self.head()? })
    }

    pub fn state(&self) -> Result<serde_json::Value> {
        self.rt.repo().state(&self.branch)
    }

    /// Stored message records, oldest first.
    pub fn messages(&self) -> Result<Vec<serde_json::Value>> {
        Ok(self.rt.repo().list(&self.branch, "messages")?.into_iter().map(|(_, v)| v).collect())
    }

    pub fn memory(&self) -> Result<serde_json::Value> {
        Ok(self.rt.repo().get(&self.branch, "memory")?.unwrap_or(serde_json::json!({})))
    }

    pub fn log(&self, limit: usize) -> Result<Vec<(Hash, Commit)>> {
        self.rt.repo().log(&self.head()?, limit)
    }

    /// Non-destructive rollback to an earlier commit.
    pub fn rollback(&self, commit: &Hash) -> Result<Hash> {
        self.rt.repo().rollback(&self.branch, commit, "user")
    }
}
