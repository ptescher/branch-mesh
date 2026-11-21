use crate::agent;
use crate::llm::{LlmProvider, LlmResponse};
use crate::persona::AgentPersona;
use crate::tools::{builtin_tools, ToolContext, ToolRef};
use branchmesh_core::store::ns;
use branchmesh_core::{Error, EventBus, Hash, MergeOptions, MergeStrategy, Repo, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, Clone)]
pub struct MeshConfig {
    /// Hibernate an actor after this long without messages.
    pub idle_timeout: Duration,
    /// Attempts before an envelope is moved to the dead-letter box.
    pub max_attempts: u32,
    /// Base delay between retries (multiplied by the attempt number).
    pub retry_backoff: Duration,
    /// Maximum nesting of agent-to-agent consultations.
    pub max_consult_depth: u32,
    /// How long `ask` waits for a reply.
    pub reply_timeout: Duration,
}

impl Default for MeshConfig {
    fn default() -> Self {
        MeshConfig {
            idle_timeout: Duration::from_secs(30),
            max_attempts: 3,
            retry_backoff: Duration::from_secs(2),
            max_consult_depth: 3,
            reply_timeout: Duration::from_secs(900),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EnvelopeStatus {
    Pending,
    Processing,
    Done,
    DeadLetter,
}

/// A durable mailbox message addressed to the actor `(to, branch)`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub id: String,
    /// Target persona.
    pub to: String,
    pub branch: String,
    /// `user`, `agent:<persona>` or `system`.
    pub from: String,
    pub content: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub status: EnvelopeStatus,
    pub attempts: u32,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub result: Option<Value>,
    /// Durable execution handling this envelope (stable across retries).
    pub execution: String,
    #[serde(default)]
    pub meta: Value,
}

impl Envelope {
    pub fn actor_key(&self) -> String {
        actor_key(&self.to, &self.branch)
    }
}

pub fn actor_key(persona: &str, branch: &str) -> String {
    format!("{persona}@{branch}")
}

/// The outcome of a processed message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reply {
    pub envelope: String,
    pub execution: String,
    pub persona: String,
    pub branch: String,
    pub text: String,
    pub stop_reason: String,
    pub commit: Option<Hash>,
    pub steps: u64,
}

impl Reply {
    fn from_envelope(env: &Envelope) -> Reply {
        let r = env.result.clone().unwrap_or(Value::Null);
        Reply {
            envelope: env.id.clone(),
            execution: env.execution.clone(),
            persona: env.to.clone(),
            branch: env.branch.clone(),
            text: r["text"].as_str().unwrap_or("").to_string(),
            stop_reason: r["stop_reason"].as_str().unwrap_or("").to_string(),
            commit: serde_json::from_value(r["commit"].clone()).ok(),
            steps: r["steps"].as_u64().unwrap_or(0),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ActorStats {
    pub processed: u64,
    pub failed: u64,
    pub llm_calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub llm_ms: u64,
    pub activations: u64,
    pub last_active: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ActorInfo {
    pub key: String,
    pub persona: String,
    pub branch: String,
    /// In RAM with a live mailbox task (otherwise hibernating on disk).
    pub active: bool,
    pub current: Option<String>,
    pub activated_at: Option<i64>,
    pub stats: ActorStats,
    pub pending: usize,
    pub processing: usize,
    pub done: usize,
    pub dead_letter: usize,
}

struct ActorSlot {
    tx: mpsc::UnboundedSender<String>,
    activated_at: i64,
    current: Arc<Mutex<Option<String>>>,
}

type Waiter = oneshot::Sender<std::result::Result<Reply, String>>;

struct Inner {
    repo: Repo,
    llm: RwLock<Arc<dyn LlmProvider>>,
    tools: RwLock<HashMap<String, ToolRef>>,
    actors: Mutex<HashMap<String, ActorSlot>>,
    stats: Mutex<HashMap<String, ActorStats>>,
    waiters: Mutex<HashMap<String, Vec<Waiter>>>,
    style: RwLock<crate::context::ContextStyle>,
    config: MeshConfig,
}

/// The virtual actor mesh. Cheap to clone.
#[derive(Clone)]
pub struct Mesh {
    inner: Arc<Inner>,
}

impl Mesh {
    pub fn new(repo: Repo, llm: Arc<dyn LlmProvider>, config: MeshConfig) -> Self {
        let tools = builtin_tools().into_iter().map(|t| (t.name().to_string(), t)).collect();
        Mesh {
            inner: Arc::new(Inner {
                repo,
                llm: RwLock::new(llm),
                tools: RwLock::new(tools),
                actors: Mutex::new(HashMap::new()),
                stats: Mutex::new(HashMap::new()),
                waiters: Mutex::new(HashMap::new()),
                style: RwLock::new(Default::default()),
                config,
            }),
        }
    }

    pub fn repo(&self) -> &Repo {
        &self.inner.repo
    }

    pub fn events(&self) -> &EventBus {
        self.inner.repo.events()
    }

    pub fn config(&self) -> &MeshConfig {
        &self.inner.config
    }

    pub fn llm(&self) -> Arc<dyn LlmProvider> {
        self.inner.llm.read().unwrap().clone()
    }

    pub fn set_llm(&self, llm: Arc<dyn LlmProvider>) {
        *self.inner.llm.write().unwrap() = llm;
    }

    /// How stored records are framed for the model (see [`crate::context`]).
    pub fn context_style(&self) -> crate::context::ContextStyle {
        self.inner.style.read().unwrap().clone()
    }

    pub fn set_context_style(&self, style: crate::context::ContextStyle) {
        *self.inner.style.write().unwrap() = style;
    }

    // ------------------------------------------------------------------ tools

    pub fn register_tool(&self, tool: ToolRef) {
        self.inner.tools.write().unwrap().insert(tool.name().to_string(), tool);
    }

    pub fn tool(&self, name: &str) -> Option<ToolRef> {
        self.inner.tools.read().unwrap().get(name).cloned()
    }

    pub fn tools(&self) -> Vec<ToolRef> {
        let mut v: Vec<ToolRef> = self.inner.tools.read().unwrap().values().cloned().collect();
        v.sort_by(|a, b| a.name().cmp(b.name()));
        v
    }

    // --------------------------------------------------------------- personas

    pub fn save_persona(&self, persona: &AgentPersona) -> Result<AgentPersona> {
        if persona.id.is_empty()
            || !persona.id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            return Err(Error::Invalid(format!("invalid persona id '{}'", persona.id)));
        }
        for t in &persona.tool_impls {
            self.register_tool(t.clone());
        }
        let now = branchmesh_core::now_ms();
        let mut p = persona.clone();
        let existing: Option<AgentPersona> = self.repo().store().get_doc(ns::PERSONAS, &p.id)?;
        p.created_at = existing.map(|e| e.created_at).filter(|t| *t > 0).unwrap_or(now);
        p.updated_at = now;
        if p.name.is_empty() {
            p.name = p.id.clone();
        }
        self.repo().store().put_doc(ns::PERSONAS, &p.id, &p)?;
        self.events().emit("persona_saved", json!({ "persona": p }));
        Ok(p)
    }

    pub fn persona(&self, id: &str) -> Result<AgentPersona> {
        self.repo()
            .store()
            .get_doc(ns::PERSONAS, id)?
            .ok_or_else(|| Error::NotFound(format!("persona '{id}'")))
    }

    pub fn personas(&self) -> Result<Vec<AgentPersona>> {
        Ok(self
            .repo()
            .store()
            .list_docs::<AgentPersona>(ns::PERSONAS, "")?
            .into_iter()
            .map(|(_, p)| p)
            .collect())
    }

    pub fn delete_persona(&self, id: &str) -> Result<()> {
        if !self.repo().store().delete_doc(ns::PERSONAS, id)? {
            return Err(Error::NotFound(format!("persona '{id}'")));
        }
        self.events().emit("persona_deleted", json!({ "persona": id }));
        Ok(())
    }

    // ---------------------------------------------------------------- mailbox

    /// Durably enqueue a message for `(to, branch)` and dispatch it. With an
    /// explicit `id`, sending is idempotent (an existing envelope is reused).
    pub fn send(&self, to: &str, branch: &str, from: &str, content: &str, meta: Value, id: Option<String>) -> Result<Envelope> {
        self.persona(to)?;
        self.repo().branch(branch)?;
        let id = id.unwrap_or_else(|| format!("env-{}", branchmesh_core::new_id()));
        if let Some(existing) = self.envelope(&id)? {
            if matches!(existing.status, EnvelopeStatus::Pending | EnvelopeStatus::Processing) {
                self.dispatch(&existing);
            }
            return Ok(existing);
        }
        let now = branchmesh_core::now_ms();
        let env = Envelope {
            execution: format!("exec-{}", id.trim_start_matches("env-")),
            id: id.clone(),
            to: to.into(),
            branch: branch.into(),
            from: from.into(),
            content: content.into(),
            created_at: now,
            updated_at: now,
            status: EnvelopeStatus::Pending,
            attempts: 0,
            last_error: None,
            result: None,
            meta,
        };
        self.repo().store().put_doc(ns::MAILBOX, &id, &env)?;
        self.events().emit("mailbox_enqueued", json!({ "envelope": env }));
        self.dispatch(&env);
        Ok(env)
    }

    /// Send and wait for the reply.
    pub async fn ask(&self, to: &str, branch: &str, from: &str, content: &str, meta: Value, id: Option<String>) -> Result<Reply> {
        let id = id.unwrap_or_else(|| format!("env-{}", branchmesh_core::new_id()));
        let rx = self.register_waiter(&id);
        let env = self.send(to, branch, from, content, meta, Some(id.clone()))?;
        self.await_reply(&env.id, rx).await
    }

    /// Wait for an already-sent envelope.
    pub async fn wait_for(&self, id: &str) -> Result<Reply> {
        let rx = self.register_waiter(id);
        self.await_reply(id, rx).await
    }

    fn register_waiter(&self, id: &str) -> oneshot::Receiver<std::result::Result<Reply, String>> {
        let (tx, rx) = oneshot::channel();
        self.inner.waiters.lock().unwrap().entry(id.to_string()).or_default().push(tx);
        rx
    }

    async fn await_reply(&self, id: &str, rx: oneshot::Receiver<std::result::Result<Reply, String>>) -> Result<Reply> {
        if let Some(env) = self.envelope(id)? {
            match env.status {
                EnvelopeStatus::Done => return Ok(Reply::from_envelope(&env)),
                EnvelopeStatus::DeadLetter => {
                    return Err(Error::Other(format!(
                        "message {id} dead-lettered: {}",
                        env.last_error.unwrap_or_default()
                    )))
                }
                _ => {}
            }
        }
        match tokio::time::timeout(self.inner.config.reply_timeout, rx).await {
            Ok(Ok(Ok(reply))) => Ok(reply),
            Ok(Ok(Err(e))) => Err(Error::Other(e)),
            Ok(Err(_)) => Err(Error::Other("reply channel closed".into())),
            Err(_) => Err(Error::Other(format!("timed out waiting for reply to {id}"))),
        }
    }

    fn notify(&self, id: &str, result: std::result::Result<Reply, String>) {
        if let Some(waiters) = self.inner.waiters.lock().unwrap().remove(id) {
            for w in waiters {
                let _ = w.send(result.clone());
            }
        }
    }

    pub fn envelope(&self, id: &str) -> Result<Option<Envelope>> {
        self.repo().store().get_doc(ns::MAILBOX, id)
    }

    /// Envelopes, newest first, optionally filtered by status.
    pub fn envelopes(&self, status: Option<EnvelopeStatus>) -> Result<Vec<Envelope>> {
        let mut all: Vec<Envelope> = self
            .repo()
            .store()
            .list_docs::<Envelope>(ns::MAILBOX, "")?
            .into_iter()
            .map(|(_, e)| e)
            .filter(|e| status.map(|s| s == e.status).unwrap_or(true))
            .collect();
        all.sort_by_key(|e| std::cmp::Reverse(e.created_at));
        Ok(all)
    }

    fn update_envelope(&self, id: &str, f: impl FnOnce(&mut Envelope)) -> Result<Envelope> {
        let env = self.repo().store().update_doc::<Envelope, _, _>(ns::MAILBOX, id, |cur| {
            let mut e = cur.ok_or_else(|| Error::NotFound(format!("envelope {id}")))?;
            f(&mut e);
            e.updated_at = branchmesh_core::now_ms();
            Ok((Some(e.clone()), e))
        })?;
        self.events().emit("mailbox_updated", json!({ "envelope": env }));
        Ok(env)
    }

    /// Re-drive a dead-lettered (or stuck) envelope.
    pub fn retry(&self, id: &str) -> Result<Envelope> {
        let env = self.update_envelope(id, |e| {
            e.status = EnvelopeStatus::Pending;
            e.attempts = 0;
            e.last_error = None;
        })?;
        self.dispatch(&env);
        Ok(env)
    }

    /// Re-dispatch everything that was pending or in flight when the process
    /// stopped. In-flight executions resume by replaying their journals.
    pub fn recover(&self) -> Result<usize> {
        let mut n = 0;
        let mut items: Vec<Envelope> = self
            .envelopes(None)?
            .into_iter()
            .filter(|e| matches!(e.status, EnvelopeStatus::Pending | EnvelopeStatus::Processing))
            .collect();
        items.sort_by_key(|e| e.created_at);
        for env in items {
            self.dispatch(&env);
            n += 1;
        }
        if n > 0 {
            tracing::info!(count = n, "recovered in-flight messages");
            self.events().emit("recovered", json!({ "count": n }));
        }
        Ok(n)
    }

    // ----------------------------------------------------------------- actors

    fn dispatch(&self, env: &Envelope) {
        let key = env.actor_key();
        let mut actors = self.inner.actors.lock().unwrap();
        if let Some(slot) = actors.get(&key) {
            if slot.tx.send(env.id.clone()).is_ok() {
                return;
            }
        }
        let (tx, rx) = mpsc::unbounded_channel();
        let _ = tx.send(env.id.clone());
        let current = Arc::new(Mutex::new(None));
        let now = branchmesh_core::now_ms();
        actors.insert(key.clone(), ActorSlot { tx, activated_at: now, current: current.clone() });
        drop(actors);
        self.inner.stats.lock().unwrap().entry(key.clone()).or_default().activations += 1;
        self.events().emit(
            "actor_activated",
            json!({ "actor": key, "persona": env.to, "branch": env.branch }),
        );
        let mesh = self.clone();
        tokio::spawn(async move { mesh.actor_loop(key, rx, current).await });
    }

    async fn actor_loop(self, key: String, mut rx: mpsc::UnboundedReceiver<String>, current: Arc<Mutex<Option<String>>>) {
        let idle = self.inner.config.idle_timeout;
        loop {
            let next = match tokio::time::timeout(idle, rx.recv()).await {
                Ok(Some(id)) => id,
                Ok(None) => break,
                Err(_) => {
                    // Idle: hibernate unless a message raced in. Holding the
                    // actors lock makes this atomic with respect to dispatch.
                    let mut actors = self.inner.actors.lock().unwrap();
                    match rx.try_recv() {
                        Ok(id) => {
                            drop(actors);
                            id
                        }
                        Err(_) => {
                            actors.remove(&key);
                            drop(actors);
                            self.events().emit("actor_hibernated", json!({ "actor": key }));
                            return;
                        }
                    }
                }
            };
            *current.lock().unwrap() = Some(next.clone());
            self.process(&key, &next).await;
            *current.lock().unwrap() = None;
        }
    }

    async fn process(&self, key: &str, id: &str) {
        let env = match self.envelope(id) {
            Ok(Some(e)) if matches!(e.status, EnvelopeStatus::Pending | EnvelopeStatus::Processing) => e,
            _ => return,
        };
        let env = match self.update_envelope(&env.id, |e| {
            e.status = EnvelopeStatus::Processing;
            e.attempts += 1;
        }) {
            Ok(e) => e,
            Err(e) => {
                tracing::error!(error = %e, "failed to mark envelope processing");
                return;
            }
        };

        // Run in a separate task so a panic becomes a retryable failure.
        let mesh = self.clone();
        let env2 = env.clone();
        let result = tokio::spawn(async move { agent::run_turn(&mesh, &env2).await })
            .await
            .unwrap_or_else(|e| Err(Error::Other(format!("agent task panicked: {e}"))));

        let now = branchmesh_core::now_ms();
        match result {
            Ok(value) => {
                {
                    let mut stats = self.inner.stats.lock().unwrap();
                    let s = stats.entry(key.to_string()).or_default();
                    s.processed += 1;
                    s.last_active = now;
                }
                self.events().emit(
                    "reply",
                    json!({
                        "branch": env.branch, "persona": env.to, "envelope": env.id, "execution": env.execution,
                        "from": env.from, "text": value["text"], "stop_reason": value["stop_reason"], "meta": env.meta,
                    }),
                );
                match self.update_envelope(&env.id, |e| {
                    e.status = EnvelopeStatus::Done;
                    e.result = Some(value);
                    e.last_error = None;
                }) {
                    Ok(done) => self.notify(&env.id, Ok(Reply::from_envelope(&done))),
                    Err(e) => self.notify(&env.id, Err(e.to_string())),
                }
            }
            Err(err) => {
                let msg = err.to_string();
                tracing::warn!(envelope = %env.id, attempt = env.attempts, error = %msg, "message processing failed");
                {
                    let mut stats = self.inner.stats.lock().unwrap();
                    let s = stats.entry(key.to_string()).or_default();
                    s.failed += 1;
                    s.last_active = now;
                }
                let dead = env.attempts >= self.inner.config.max_attempts
                    || matches!(err, Error::NonDeterminism { .. } | Error::NotFound(_) | Error::BranchNotFound(_));
                let updated = self.update_envelope(&env.id, |e| {
                    e.status = if dead { EnvelopeStatus::DeadLetter } else { EnvelopeStatus::Pending };
                    e.last_error = Some(msg.clone());
                });
                if dead {
                    self.notify(&env.id, Err(msg));
                } else if let Ok(updated) = updated {
                    let mesh = self.clone();
                    let delay = self.inner.config.retry_backoff * env.attempts;
                    tokio::spawn(async move {
                        tokio::time::sleep(delay).await;
                        mesh.dispatch(&updated);
                    });
                }
            }
        }
    }

    pub(crate) fn record_llm(&self, key: &str, resp: &LlmResponse, ms: u64) {
        let mut stats = self.inner.stats.lock().unwrap();
        let s = stats.entry(key.to_string()).or_default();
        s.llm_calls += 1;
        s.input_tokens += resp.input_tokens();
        s.output_tokens += resp.output_tokens();
        s.llm_ms += ms;
        s.last_active = branchmesh_core::now_ms();
    }

    /// All known actors: live ones plus hibernating ones with mailbox history.
    pub fn actors(&self) -> Result<Vec<ActorInfo>> {
        let mut map: HashMap<String, ActorInfo> = HashMap::new();
        let blank = |key: &str, persona: &str, branch: &str| ActorInfo {
            key: key.to_string(),
            persona: persona.to_string(),
            branch: branch.to_string(),
            active: false,
            current: None,
            activated_at: None,
            stats: ActorStats::default(),
            pending: 0,
            processing: 0,
            done: 0,
            dead_letter: 0,
        };
        for env in self.envelopes(None)? {
            let key = env.actor_key();
            let a = map.entry(key.clone()).or_insert_with(|| blank(&key, &env.to, &env.branch));
            match env.status {
                EnvelopeStatus::Pending => a.pending += 1,
                EnvelopeStatus::Processing => a.processing += 1,
                EnvelopeStatus::Done => a.done += 1,
                EnvelopeStatus::DeadLetter => a.dead_letter += 1,
            }
        }
        {
            let actors = self.inner.actors.lock().unwrap();
            for (key, slot) in actors.iter() {
                let (persona, branch) = key.split_once('@').unwrap_or((key, ""));
                let a = map.entry(key.clone()).or_insert_with(|| blank(key, persona, branch));
                a.active = true;
                a.activated_at = Some(slot.activated_at);
                a.current = slot.current.lock().unwrap().clone();
            }
        }
        {
            let stats = self.inner.stats.lock().unwrap();
            for (key, s) in stats.iter() {
                if let Some(a) = map.get_mut(key) {
                    a.stats = s.clone();
                }
            }
        }
        let mut out: Vec<ActorInfo> = map.into_values().collect();
        out.sort_by(|a, b| b.active.cmp(&a.active).then(b.stats.last_active.cmp(&a.stats.last_active)).then(a.key.cmp(&b.key)));
        Ok(out)
    }

    // ---------------------------------------------------------- consultation

    /// Agent-to-agent consultation: fork the caller's branch, let `target`
    /// answer on the fork, then merge the fork back (deterministic strategy,
    /// so it never blocks on conflicts). Idempotent per tool call, so a
    /// crashed caller resumes the same consultation instead of starting over.
    pub async fn consult(&self, ctx: &ToolContext, target: &str, question: &str) -> Result<Value> {
        if ctx.depth >= self.inner.config.max_consult_depth {
            return Err(Error::Invalid(format!(
                "consultation depth limit ({}) reached",
                self.inner.config.max_consult_depth
            )));
        }
        if target == ctx.persona {
            return Err(Error::Invalid("an agent cannot consult itself".into()));
        }
        self.persona(target)?;
        let cid = Hash::of(format!("{}:{}", ctx.execution, ctx.call_id).as_bytes()).to_hex();
        let mut fork_name = format!("{}/consult/{}-{}", ctx.branch, target, &cid[..8]);
        if fork_name.len() > 190 {
            fork_name = format!("consult/{}-{}", target, &cid[..16]);
        }
        let meta = json!({
            "kind": "consultation",
            "consult_of": ctx.persona,
            "persona": target,
            "requested_by": ctx.execution,
            "depth": ctx.depth + 1,
            "question": question,
        });
        match self.repo().fork(&ctx.branch, &fork_name, meta) {
            Ok(_) | Err(Error::BranchExists(_)) => {}
            Err(e) => return Err(e),
        }
        self.events().emit(
            "consultation_started",
            json!({ "from": ctx.persona, "to": target, "branch": ctx.branch, "fork": fork_name }),
        );
        let reply = self
            .ask(
                target,
                &fork_name,
                &format!("agent:{}", ctx.persona),
                question,
                json!({ "consult_of": ctx.persona, "depth": ctx.depth + 1 }),
                Some(format!("env-consult-{}", &cid[..24])),
            )
            .await?;
        let mut opts = MergeOptions::new(MergeStrategy::Deterministic);
        opts.author = format!("agent:{}", ctx.persona);
        opts.message = Some(format!("merge consultation with {target}"));
        opts.close_source = true;
        let outcome = self.repo().merge(&fork_name, &ctx.branch, opts).await?;
        self.events().emit(
            "consultation_finished",
            json!({ "from": ctx.persona, "to": target, "branch": ctx.branch, "fork": fork_name, "merge_commit": outcome.commit }),
        );
        let memory_changes: Vec<&str> = outcome
            .changes
            .iter()
            .filter(|c| c.path.starts_with("memory/"))
            .map(|c| c.path.as_str())
            .collect();
        Ok(json!({
            "persona": target,
            "reply": reply.text,
            "fork": fork_name,
            "merge_commit": outcome.commit,
            "merged_memory": memory_changes,
        }))
    }
}
