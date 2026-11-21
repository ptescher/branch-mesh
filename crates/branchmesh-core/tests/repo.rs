use async_trait::async_trait;
use branchmesh_core::merge::MergeContext;
use branchmesh_core::repo::CommitOptions;
use branchmesh_core::*;
use serde_json::json;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

fn repo() -> (tempfile::TempDir, Repo) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("t.db")).unwrap();
    (dir, Repo::new(store, EventBus::default()).unwrap())
}

fn opts(msg: &str) -> CommitOptions {
    CommitOptions::new("test", msg)
}

#[test]
fn commit_read_and_structural_sharing() {
    let (_d, r) = repo();
    r.create_branch("main", json!({})).unwrap();
    r.commit("main", &[Change::set("memory/a", json!(1)), Change::set("memory/b", json!({"x": 2}))], opts("c1")).unwrap();
    assert_eq!(r.get("main", "memory/a").unwrap(), Some(json!(1)));
    assert_eq!(r.get("main", "memory").unwrap(), Some(json!({"a": 1, "b": {"x": 2}})));

    // Fork is O(1): no new objects.
    let before = r.store().object_count().unwrap();
    r.fork("main", "side", json!({})).unwrap();
    assert_eq!(r.store().object_count().unwrap(), before);
    assert_eq!(r.head("side").unwrap(), r.head("main").unwrap());

    // A write on the fork rewrites only the changed path: blob + memory tree + root tree + commit.
    r.commit("side", &[Change::set("memory/a", json!(42))], opts("c2")).unwrap();
    assert_eq!(r.store().object_count().unwrap(), before + 4);
    assert_eq!(r.get("main", "memory/a").unwrap(), Some(json!(1)));
    assert_eq!(r.get("side", "memory/a").unwrap(), Some(json!(42)));

    // Unchanged subtree hashes are shared between branches.
    let main_tree = r.commit_object(&r.head("main").unwrap()).unwrap().tree;
    let side_tree = r.commit_object(&r.head("side").unwrap()).unwrap().tree;
    let b_main = r.tree_entry_at(&main_tree, "memory/b").unwrap().unwrap();
    let b_side = r.tree_entry_at(&side_tree, "memory/b").unwrap().unwrap();
    assert_eq!(b_main.hash, b_side.hash);

    // No-op commits do not create history.
    let head = r.head("side").unwrap();
    assert_eq!(r.commit("side", &[Change::set("memory/a", json!(42))], opts("noop")).unwrap(), head);

    // Deleting the last entry of a directory prunes it.
    r.commit("side", &[Change::delete("memory/b"), Change::delete("memory/a")], opts("del")).unwrap();
    assert_eq!(r.get("side", "memory").unwrap(), None);
}

#[test]
fn diff_log_and_rollback() {
    let (_d, r) = repo();
    r.create_branch("main", json!({})).unwrap();
    let c1 = r.commit("main", &[Change::set("k", json!("v1"))], opts("c1")).unwrap();
    let c2 = r.commit("main", &[Change::set("k", json!("v2")), Change::set("n", json!(1))], opts("c2")).unwrap();
    let d = r.diff(&c1, &c2).unwrap();
    assert_eq!(d.len(), 2);
    assert!(d.iter().any(|e| e.path == "k" && e.kind == DiffKind::Modified));
    assert!(d.iter().any(|e| e.path == "n" && e.kind == DiffKind::Added));

    let log = r.log(&c2, 10).unwrap();
    assert_eq!(log[0].0, c2);
    assert_eq!(log[1].0, c1);
    assert_eq!(log.last().unwrap().0, Repo::genesis());

    let rb = r.rollback("main", &c1, "test").unwrap();
    assert_eq!(r.get("main", "k").unwrap(), Some(json!("v1")));
    assert_eq!(r.get("main", "n").unwrap(), None);
    assert_eq!(r.commit_object(&rb).unwrap().parents, vec![c2]);
}

#[tokio::test]
async fn clean_three_way_merge() {
    let (_d, r) = repo();
    r.create_branch("main", json!({})).unwrap();
    r.commit("main", &[Change::set("shared", json!({"a": 1, "list": [1]}))], opts("base")).unwrap();
    r.fork("main", "side", json!({})).unwrap();
    r.commit("main", &[Change::set("main_only", json!(true)), Change::set("shared", json!({"a": 2, "list": [1, 2]}))], opts("m")).unwrap();
    r.commit("side", &[Change::set("side_only", json!(true)), Change::set("shared", json!({"a": 1, "b": 9, "list": [1, 3]}))], opts("s")).unwrap();

    let preview = r.merge_preview("side", "main").unwrap();
    assert!(preview.conflicts.is_empty());
    let out = r.merge("side", "main", MergeOptions::new(MergeStrategy::ThreeWay)).await.unwrap();
    let state = r.state("main").unwrap();
    assert_eq!(state["main_only"], json!(true));
    assert_eq!(state["side_only"], json!(true));
    assert_eq!(state["shared"], json!({"a": 2, "b": 9, "list": [1, 2, 3]}));
    let mc = r.commit_object(&out.commit).unwrap();
    assert_eq!(mc.parents.len(), 2);

    // Merging again is a no-op.
    assert!(r.merge("side", "main", MergeOptions::new(MergeStrategy::ThreeWay)).await.unwrap().up_to_date);
}

fn conflicting() -> (tempfile::TempDir, Repo) {
    let (d, r) = repo();
    r.create_branch("main", json!({})).unwrap();
    r.commit("main", &[Change::set("fact", json!("base"))], opts("base")).unwrap();
    r.fork("main", "side", json!({})).unwrap();
    r.commit("main", &[Change::set("fact", json!("ours"))], opts("m")).unwrap();
    r.commit("side", &[Change::set("fact", json!("theirs"))], opts("s")).unwrap();
    (d, r)
}

#[tokio::test]
async fn conflict_strategies() {
    let (_d, r) = conflicting();
    match r.merge("side", "main", MergeOptions::new(MergeStrategy::ThreeWay)).await {
        Err(Error::MergeConflict(c)) => assert_eq!(c[0].path, "fact"),
        other => panic!("expected conflict, got {other:?}"),
    }
    // Manual resolution wins over the strategy.
    let mut o = MergeOptions::new(MergeStrategy::ThreeWay);
    o.resolutions.insert("fact".into(), Some(json!("manual")));
    r.merge("side", "main", o).await.unwrap();
    assert_eq!(r.get("main", "fact").unwrap(), Some(json!("manual")));

    let (_d, r) = conflicting();
    r.merge("side", "main", MergeOptions::new(MergeStrategy::Ours)).await.unwrap();
    assert_eq!(r.get("main", "fact").unwrap(), Some(json!("ours")));

    let (_d, r) = conflicting();
    r.merge("side", "main", MergeOptions::new(MergeStrategy::Theirs)).await.unwrap();
    assert_eq!(r.get("main", "fact").unwrap(), Some(json!("theirs")));
}

#[tokio::test]
async fn deterministic_strategy_is_direction_independent() {
    let (_d, r1) = conflicting();
    r1.merge("side", "main", MergeOptions::new(MergeStrategy::Deterministic)).await.unwrap();
    let (_d2, r2) = conflicting();
    r2.merge("main", "side", MergeOptions::new(MergeStrategy::Deterministic)).await.unwrap();
    let w1 = r1.get("main", "fact").unwrap();
    let w2 = r2.get("side", "fact").unwrap();
    assert_eq!(w1, w2, "same winner regardless of merge direction");
    // The losing revision is preserved CouchDB-style.
    let loser = r1.get("main", "_conflicts/fact").unwrap().unwrap();
    assert_ne!(Some(loser["value"].clone()), w1);
}

struct CountingResolver(AtomicU32);

#[async_trait]
impl ConflictResolver for CountingResolver {
    async fn resolve(&self, conflicts: &[Conflict], _ctx: &MergeContext) -> Result<Vec<Resolution>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(conflicts
            .iter()
            .map(|c| Resolution {
                path: c.path.clone(),
                value: Some(json!(format!("{} + {}", c.ours.as_ref().unwrap(), c.theirs.as_ref().unwrap()))),
                rationale: Some("combined".into()),
                resolved_by: "test".into(),
            })
            .collect())
    }
}

#[tokio::test]
async fn semantic_strategy_uses_resolver() {
    let (_d, r) = conflicting();
    let resolver = Arc::new(CountingResolver(AtomicU32::new(0)));
    let mut o = MergeOptions::new(MergeStrategy::SemanticLLM);
    o.resolver = Some(resolver.clone());
    o.close_source = true;
    let out = r.merge("side", "main", o).await.unwrap();
    assert_eq!(out.resolutions.len(), 1);
    assert_eq!(r.get("main", "fact").unwrap(), Some(json!("\"ours\" + \"theirs\"")));
    assert_eq!(resolver.0.load(Ordering::SeqCst), 1);
    assert!(matches!(r.branch("side").unwrap().status, BranchStatus::Merged { .. }));
}

#[tokio::test]
async fn journal_replays_instead_of_reexecuting() {
    let (_d, r) = repo();
    let calls = Arc::new(AtomicU32::new(0));
    async fn step(ctx: &DurableContext, calls: Arc<AtomicU32>) -> Result<serde_json::Value> {
        ctx.run(EntryKind::Custom, "roll_dice", json!({}), move || async move {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(json!({"value": branchmesh_core::now_ms() % 6, "nonce": branchmesh_core::new_id()}))
        })
        .await
    }

    let (ctx, exec) = DurableContext::begin(&r, "e1", "actor", "main", None, json!({})).unwrap();
    assert_eq!(exec.attempts, 1);
    let first: serde_json::Value = step(&ctx, calls.clone()).await.unwrap();
    // Simulate a crash before completion, then a retry.
    drop(ctx);
    let (ctx, exec) = DurableContext::begin(&r, "e1", "actor", "main", None, json!({})).unwrap();
    assert_eq!(exec.attempts, 2);
    assert!(ctx.is_replaying());
    let second: serde_json::Value = step(&ctx, calls.clone()).await.unwrap();
    assert_eq!(first, second);
    assert_eq!(calls.load(Ordering::SeqCst), 1, "replayed step must not execute again");
    ctx.complete(json!("ok")).unwrap();

    // A different step at the same position is rejected.
    let (ctx, _) = DurableContext::begin(&r, "e1", "actor", "main", None, json!({})).unwrap();
    let res: Result<serde_json::Value> = ctx.run(EntryKind::Custom, "something_else", json!({}), || async { Ok(json!(1)) }).await;
    assert!(matches!(res, Err(Error::NonDeterminism { .. })));

    let entries = branchmesh_core::journal::entries(&r, "e1").unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, "roll_dice");
}

#[test]
fn branch_names_are_validated() {
    let (_d, r) = repo();
    assert!(r.create_branch("bad name", json!({})).is_err());
    assert!(r.create_branch("/lead", json!({})).is_err());
    r.create_branch("ok/name-1", json!({})).unwrap();
    assert!(matches!(r.create_branch("ok/name-1", json!({})), Err(Error::BranchExists(_))));
}
