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

    let ctx = branchmesh_actors::agent::build_context(mesh.repo(), "t", "bot", "claude-opus-5-5").unwrap();
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
    let lead_ctx = agent::build_context(mesh.repo(), "t", "lead", "claude-opus-5-5").unwrap();
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
            attachments: vec![],
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

#[tokio::test]
async fn system_notes_render_as_stable_system_messages() {
    let (_d, mesh) = setup(Arc::new(MockProvider::instant()), fast_cfg());
    mesh.save_persona(&persona("bot")).unwrap();
    mesh.repo().create_branch("t2", json!({})).unwrap();
    mesh.ask("bot", "t2", "user", "hi", Value::Null, None).await.unwrap();

    // A note arrives after the assistant reply: it is held until the next user turn.
    let note = agent::SystemRecord {
        kind: "operator_note",
        author: "operator:p1",
        author_name: Some("Mom"),
        preamble: Some("Note from Mom (private guidance):"),
        text: "Sam has a soccer game at 5pm.",
    };
    agent::append_system_record(mesh.repo(), "t2", note, vec![]).unwrap();
    let held = agent::build_context(mesh.repo(), "t2", "bot", "claude-opus-5-5").unwrap();
    assert_eq!(held.last().unwrap()["role"], "assistant", "note must not follow an assistant turn");

    let reply = mesh.ask("bot", "t2", "user", "what's today?", Value::Null, None).await.unwrap();
    assert!(reply.text.contains("Keeping in mind"), "{}", reply.text);
    let ctx = agent::build_context(mesh.repo(), "t2", "bot", "claude-opus-5-5").unwrap();
    let roles: Vec<&str> = ctx.iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(roles, vec!["user", "assistant", "user", "system", "assistant"]);
    assert!(ctx[3]["content"].as_str().unwrap().contains("soccer game"));
    assert!(ctx[3]["content"].as_str().unwrap().contains("Mom"));

    // Append-only: the next request's prefix equals the previous context.
    mesh.ask("bot", "t2", "user", "ok", Value::Null, None).await.unwrap();
    let ctx2 = agent::build_context(mesh.repo(), "t2", "bot", "claude-opus-5-5").unwrap();
    assert_eq!(&ctx2[..ctx.len()], &ctx[..]);

    // Models without mid-conversation system messages get a user-turn note instead.
    let other = agent::build_context(mesh.repo(), "t2", "bot", "google/gemini-3-flash").unwrap();
    assert!(other.iter().all(|m| m["role"] != "system"));
    assert!(serde_json::to_string(&other).unwrap().contains("soccer game"));
}

#[tokio::test]
async fn author_framing_is_configurable() {
    use branchmesh_actors::{AuthorFrame, ContextStyle};
    let (_d, mesh) = setup(Arc::new(MockProvider::instant()), fast_cfg());
    mesh.save_persona(&persona("bot")).unwrap();
    mesh.repo().create_branch("t3", json!({})).unwrap();
    mesh.set_context_style(
        ContextStyle::default()
            .with_author("child:", AuthorFrame::Direct)
            .with_author("event", AuthorFrame::Labeled("Device event".into())),
    );
    mesh.ask("bot", "t3", "child:sam", "hello", Value::Null, None).await.unwrap();
    let reply = mesh.ask("bot", "t3", "event", "arrived at school", Value::Null, None).await.unwrap();
    assert!(reply.text.contains("(Device event)"), "{}", reply.text);
    mesh.ask("bot", "t3", "stranger", "hey", Value::Null, None).await.unwrap();
    let ctx = agent::build_context_with(mesh.repo(), "t3", "bot", "claude-opus-5-5", &mesh.context_style()).unwrap();
    let text = serde_json::to_string(&ctx).unwrap();
    assert!(text.contains("\"hello\""), "{text}");
    assert!(text.contains("[Device event]\\narrived at school"), "{text}");
    assert!(text.contains("[message from stranger]"), "{text}");
}

#[tokio::test]
async fn standing_facts_snapshot_only_when_changed() {
    let (_d, mesh) = setup(Arc::new(MockProvider::instant()), fast_cfg());
    mesh.save_persona(&persona("bot")).unwrap();
    mesh.repo().create_branch("t2", json!({})).unwrap();
    let repo = mesh.repo();
    let snapshots = || {
        repo.list("t2", "messages").unwrap().into_iter().filter(|(_, m)| m["kind"] == "memory_snapshot").count()
    };
    mesh.ask("bot", "t2", "user", "hi", Value::Null, None).await.unwrap();
    assert_eq!(snapshots(), 0, "no facts, no snapshot");

    repo.commit("t2", &[branchmesh_core::Change::set("memory/facts/allergies", json!({"text": "peanuts"}))],
        branchmesh_core::repo::CommitOptions::new("parent", "fact")).unwrap();
    mesh.ask("bot", "t2", "user", "snack ideas?", Value::Null, None).await.unwrap();
    assert_eq!(snapshots(), 1);
    mesh.ask("bot", "t2", "user", "thanks", Value::Null, None).await.unwrap();
    assert_eq!(snapshots(), 1, "unchanged facts are not repeated");

    repo.commit("t2", &[branchmesh_core::Change::set("memory/facts/bedtime", json!({"text": "8pm"}))],
        branchmesh_core::repo::CommitOptions::new("parent", "fact")).unwrap();
    mesh.ask("bot", "t2", "user", "when do I sleep?", Value::Null, None).await.unwrap();
    assert_eq!(snapshots(), 2);
    let ctx = agent::build_context(repo, "t2", "bot", "claude-opus-5-5").unwrap();
    let last_sys = ctx.iter().rev().find(|m| m["role"] == "system").unwrap();
    let text = last_sys["content"].as_str().unwrap();
    assert!(text.contains("peanuts") && text.contains("8pm"), "{text}");
}

/// A tiny valid PNG (1x1).
const PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0x0D, 0x49, 0x48, 0x44, 0x52, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0,
    0, 0, 0x1F, 0x15, 0xC4, 0x89, 0, 0, 0, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0, 1, 0, 0, 5, 0, 1, 0x0D, 0x0A,
    0x2D, 0xB4, 0, 0, 0, 0, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
];

#[tokio::test]
async fn images_are_stored_by_reference_and_sent_to_the_model() {
    let (_d, mesh) = setup(Arc::new(MockProvider::instant()), fast_cfg());
    mesh.save_persona(&persona("bot")).unwrap();
    mesh.repo().create_branch("t", json!({})).unwrap();
    let store = mesh.repo().store();
    let media_type = branchmesh_core::media::validate_image(PNG).unwrap();
    let img = store.put_media(PNG, media_type).unwrap();
    assert_eq!(store.get_media(&img.hash).unwrap().unwrap(), PNG);

    let reply = mesh.ask_with("bot", "t", "user", "what is this?", vec![img.clone()], Value::Null, None).await.unwrap();
    assert!(reply.text.contains("I can see 1 image(s): image/png"), "{}", reply.text);

    // The branch keeps a reference, never the bytes.
    let msgs = mesh.repo().list("t", "messages").unwrap();
    let inbound = &msgs[0].1;
    assert_eq!(inbound["content"][0]["source"], json!({"type": "media", "hash": img.hash, "media_type": "image/png"}));
    assert_eq!(inbound["content"][1]["text"], "what is this?");
    // So does the journal.
    let journal = serde_json::to_string(&journal::entries(mesh.repo(), &reply.execution).unwrap()).unwrap();
    assert!(!journal.contains("\"base64\""), "journal must not hold image bytes");

    // Image-only messages work, and only the newest images are shown.
    let style = ContextStyle { max_images: 1, ..ContextStyle::default() };
    mesh.ask_with("bot", "t", "user", "", vec![img.clone()], Value::Null, None).await.unwrap();
    let ctx = agent::build_context_with(mesh.repo(), "t", "bot", "claude-opus-5-5", &style).unwrap();
    let images: Vec<&Value> = ctx.iter().flat_map(|m| m["content"].as_array().unwrap().iter()).filter(|b| b["type"] == "image").collect();
    assert_eq!(images.len(), 1);
    assert!(serde_json::to_string(&ctx).unwrap().contains("an earlier image, no longer shown"));

    // Unknown media is refused at send time.
    let bogus = branchmesh_core::MediaRef { hash: "00".repeat(32), media_type: "image/png".into(), size: 1 };
    assert!(mesh.send_with("bot", "t", "user", "hi", vec![bogus], Value::Null, None).is_err());
}
