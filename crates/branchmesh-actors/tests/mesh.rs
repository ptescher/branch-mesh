use async_trait::async_trait;
use branchmesh_actors::llm::DeltaFn;
use branchmesh_actors::*;
use branchmesh_core::store::ns;
use branchmesh_core::{journal, EventBus, Repo, Store};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Mock LLM that fails specific call numbers (1-based) once each.
struct Flaky {
    inner: MockProvider,
    calls: AtomicU32,
    fail_on: Vec<u32>,
    always_fail: bool,
}

#[async_trait]
impl LlmProvider for Flaky {
    fn name(&self) -> &str {
        "flaky"
    }
    async fn complete(&self, req: &LlmRequest, d: DeltaFn<'_>) -> branchmesh_core::Result<LlmResponse> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if self.always_fail || self.fail_on.contains(&n) {
            return Err(branchmesh_core::Error::Other(format!("simulated crash on LLM call {n}")));
        }
        self.inner.complete(req, d).await
    }
}

fn setup(llm: Arc<dyn LlmProvider>, cfg: MeshConfig) -> (tempfile::TempDir, Mesh) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("m.db")).unwrap();
    let repo = Repo::new(store, EventBus::default()).unwrap();
    (dir, Mesh::new(repo, llm, cfg))
}

fn fast_cfg() -> MeshConfig {
    MeshConfig {
        idle_timeout: Duration::from_millis(200),
        retry_backoff: Duration::from_millis(20),
        reply_timeout: Duration::from_secs(20),
        ..MeshConfig::default()
    }
}

fn persona(id: &str) -> AgentPersona {
    AgentPersona::new(id)
        .with_system_prompt(format!("You are {id}."))
        .with_tool_names(["memory_write", "memory_read", "ask_agent", "current_time"])
}

#[tokio::test]
async fn message_roundtrip_and_memory_tool() {
    let (_d, mesh) = setup(Arc::new(MockProvider::instant()), fast_cfg());
    mesh.save_persona(&persona("bot")).unwrap();
    mesh.repo().create_branch("t", json!({})).unwrap();

    let reply = mesh.ask("bot", "t", "user", "hello there", Value::Null, None).await.unwrap();
    assert!(reply.text.contains("hello there"));

    let reply = mesh.ask("bot", "t", "user", "remember the sky is blue", Value::Null, None).await.unwrap();
    assert_eq!(reply.steps, 2);
    assert_eq!(mesh.repo().get("t", "memory/the-sky-is-blue").unwrap(), Some(json!("the sky is blue")));

    let ctx = branchmesh_actors::agent::build_context(mesh.repo(), "t", "bot").unwrap();
    // user, assistant, user, assistant(tool_use), user(tool_result), assistant
    assert_eq!(ctx.len(), 6);
    assert_eq!(ctx[4]["content"][0]["type"], "tool_result");
}

#[tokio::test]
async fn crashed_turn_replays_journal_instead_of_recalling_llm_or_tools() {
    // Call 1 = tool_use (current_time), call 2 crashes, retry: call 3 succeeds.
    let flaky = Arc::new(Flaky { inner: MockProvider::instant(), calls: AtomicU32::new(0), fail_on: vec![2], always_fail: false });
    let (_d, mesh) = setup(flaky.clone(), fast_cfg());
    let tool_calls = Arc::new(AtomicU32::new(0));
    let counter = tool_calls.clone();
    mesh.register_tool(FnTool::new("current_time", "time", json!({"type": "object", "properties": {}}), move |_, _| {
        let counter = counter.clone();
        async move {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(json!({"unix_ms": 1}))
        }
    }));
    mesh.save_persona(&persona("bot")).unwrap();
    mesh.repo().create_branch("t", json!({})).unwrap();

    let reply = mesh.ask("bot", "t", "user", "what time is it?", Value::Null, None).await.unwrap();
    assert!(reply.text.contains("Tool result"), "{}", reply.text);
    assert_eq!(flaky.calls.load(Ordering::SeqCst), 3, "first completion replayed, not re-requested");
    assert_eq!(tool_calls.load(Ordering::SeqCst), 1, "tool ran exactly once across attempts");

    let exec = journal::execution(mesh.repo(), &reply.execution).unwrap().unwrap();
    assert_eq!(exec.attempts, 2);
    assert!(exec.replayed >= 3, "receive, llm:0, append, tool were replayed: {}", exec.replayed);
    // Exactly one copy of each message on the branch despite the retry.
    let msgs = mesh.repo().list("t", "messages").unwrap();
    assert_eq!(msgs.len(), 4);
}

#[tokio::test]
async fn consultation_forks_and_merges_back() {
    let (_d, mesh) = setup(Arc::new(MockProvider::instant()), fast_cfg());
    mesh.save_persona(&persona("lead")).unwrap();
    mesh.save_persona(&persona("checker")).unwrap();
    mesh.repo().create_branch("t", json!({})).unwrap();

    let reply = mesh.ask("lead", "t", "user", "ask checker remember verified fact", Value::Null, None).await.unwrap();
    assert!(reply.text.contains("checker"));
    // The checker's memory write happened on the fork and was merged back.
    assert_eq!(mesh.repo().get("t", "memory/verified-fact").unwrap(), Some(json!("verified fact")));
    let forks: Vec<_> = mesh.repo().branches().unwrap().into_iter().filter(|b| b.parent.as_deref() == Some("t")).collect();
    assert_eq!(forks.len(), 1);
    assert!(matches!(forks[0].status, branchmesh_core::BranchStatus::Merged { .. }));

    // The lead does not see the side conversation (its result is in the tool_result) ...
    let lead_ctx = agent::build_context(mesh.repo(), "t", "lead").unwrap();
    let text = serde_json::to_string(&lead_ctx).unwrap();
    assert!(!text.contains("[message from agent:lead]"));
    // ... but the merged messages are stored on the thread for auditing.
    let all = mesh.repo().list("t", "messages").unwrap();
    assert!(all.iter().any(|(_, m)| m["consult_of"] == "lead"));
}

#[tokio::test]
async fn idle_actors_hibernate() {
    let (_d, mesh) = setup(Arc::new(MockProvider::instant()), fast_cfg());
    mesh.save_persona(&persona("bot")).unwrap();
    mesh.repo().create_branch("t", json!({})).unwrap();
    mesh.ask("bot", "t", "user", "hi", Value::Null, None).await.unwrap();
    assert!(mesh.actors().unwrap()[0].active);
    tokio::time::sleep(Duration::from_millis(600)).await;
    let a = &mesh.actors().unwrap()[0];
    assert!(!a.active, "actor should hibernate after idle timeout");
    assert_eq!(a.done, 1);
    // Re-activation is transparent.
    mesh.ask("bot", "t", "user", "again", Value::Null, None).await.unwrap();
}

#[tokio::test]
async fn failures_dead_letter_and_can_be_retried() {
    let flaky = Arc::new(Flaky { inner: MockProvider::instant(), calls: AtomicU32::new(0), fail_on: vec![], always_fail: true });
    let (_d, mesh) = setup(flaky.clone(), fast_cfg());
    mesh.save_persona(&persona("bot")).unwrap();
    mesh.repo().create_branch("t", json!({})).unwrap();
    let err = mesh.ask("bot", "t", "user", "hi", Value::Null, None).await.unwrap_err();
    assert!(err.to_string().contains("dead-lettered") || err.to_string().contains("simulated"), "{err}");
    let dead = mesh.envelopes(Some(EnvelopeStatus::DeadLetter)).unwrap();
    assert_eq!(dead.len(), 1);
    assert_eq!(dead[0].attempts, 3);

    // Redrive after the outage ends.
    let healthy: Arc<dyn LlmProvider> = Arc::new(MockProvider::instant());
    mesh.set_llm(healthy);
    let id = dead[0].id.clone();
    mesh.retry(&id).unwrap();
    let reply = mesh.wait_for(&id).await.unwrap();
    assert!(reply.text.contains("hi"));
}

#[tokio::test]
async fn pending_messages_recover_after_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("r.db");
    {
        let repo = Repo::new(Store::open(&path).unwrap(), EventBus::default()).unwrap();
        let mesh = Mesh::new(repo, Arc::new(MockProvider::instant()), fast_cfg());
        mesh.save_persona(&persona("bot")).unwrap();
        mesh.repo().create_branch("t", json!({})).unwrap();
        // Simulate a process that died right after accepting a message.
        let env = Envelope {
            id: "env-crash".into(),
            to: "bot".into(),
            branch: "t".into(),
            from: "user".into(),
            content: "survive the crash".into(),
            created_at: 1,
            updated_at: 1,
            status: EnvelopeStatus::Processing,
            attempts: 1,
            last_error: None,
            result: None,
            execution: "exec-crash".into(),
            meta: Value::Null,
        };
        mesh.repo().store().put_doc(ns::MAILBOX, &env.id, &env).unwrap();
    }
    let repo = Repo::new(Store::open(&path).unwrap(), EventBus::default()).unwrap();
    let mesh = Mesh::new(repo, Arc::new(MockProvider::instant()), fast_cfg());
    assert_eq!(mesh.recover().unwrap(), 1);
    let reply = mesh.wait_for("env-crash").await.unwrap();
    assert!(reply.text.contains("survive the crash"));
}
