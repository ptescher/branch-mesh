//! Deterministic execution journal.
//!
//! Every non-deterministic step an agent takes (LLM completion, tool call,
//! HTTP request, state write) runs through [`DurableContext::run`]. The first
//! time, the closure executes and its output is persisted as an immutable,
//! content-addressed journal entry. If the worker crashes and the execution is
//! retried, `run` returns the recorded output instead of executing again, so
//! the agent deterministically fast-forwards to the failure point and resumes.

use crate::error::{Error, Result};
use crate::hash::Hash;
use crate::object::Object;
use crate::repo::Repo;
use crate::store::ns;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    MessageReceived,
    LlmCompletion,
    ToolCall,
    HttpCall,
    StateWrite,
    MessageSent,
    Fork,
    Merge,
    Custom,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct JournalEntry {
    pub execution: String,
    pub seq: u64,
    pub kind: EntryKind,
    pub name: String,
    pub input: Value,
    pub output: Value,
    pub started_at: i64,
    pub duration_ms: u64,
    /// Commit produced by this step, if it wrote state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<Hash>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionStatus {
    Running,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Execution {
    pub id: String,
    pub actor: String,
    pub branch: String,
    #[serde(default)]
    pub envelope: Option<String>,
    pub status: ExecutionStatus,
    pub started_at: i64,
    pub updated_at: i64,
    #[serde(default)]
    pub finished_at: Option<i64>,
    pub attempts: u32,
    pub entries: u64,
    /// Entries served from the journal (not re-executed) on the latest attempt.
    #[serde(default)]
    pub replayed: u64,
    pub input: Value,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<String>,
}

fn entry_key(execution: &str, seq: u64) -> String {
    format!("{execution}/{seq:010}")
}

/// All journal entries of an execution, in order.
pub fn entries(repo: &Repo, execution: &str) -> Result<Vec<JournalEntry>> {
    let index: Vec<(String, Hash)> = repo.store.list_docs(ns::JOURNAL, &format!("{execution}/"))?;
    index
        .into_iter()
        .map(|(_, h)| match repo.store.get_object(&h)? {
            Object::Journal(e) => Ok(e),
            _ => Err(Error::Invalid(format!("{h} is not a journal entry"))),
        })
        .collect()
}

pub fn execution(repo: &Repo, id: &str) -> Result<Option<Execution>> {
    repo.store.get_doc(ns::EXECUTIONS, id)
}

/// All executions, newest first.
pub fn executions(repo: &Repo) -> Result<Vec<Execution>> {
    let mut all: Vec<Execution> = repo
        .store
        .list_docs::<Execution>(ns::EXECUTIONS, "")?
        .into_iter()
        .map(|(_, e)| e)
        .collect();
    all.sort_by_key(|e| std::cmp::Reverse(e.started_at));
    Ok(all)
}

/// Handle for one durable execution attempt.
pub struct DurableContext {
    repo: Repo,
    id: String,
    recorded: Vec<JournalEntry>,
    cursor: AtomicU64,
    replayed: AtomicU64,
}

impl DurableContext {
    /// Start (or resume) an execution. Resuming loads the journal so that
    /// subsequent `run` calls replay recorded results.
    pub fn begin(repo: &Repo, id: &str, actor: &str, branch: &str, envelope: Option<String>, input: Value) -> Result<(Self, Execution)> {
        let now = crate::now_ms();
        let exec = repo.store.update_doc::<Execution, _, _>(ns::EXECUTIONS, id, |cur| {
            let e = match cur {
                Some(mut e) => {
                    e.status = ExecutionStatus::Running;
                    e.attempts += 1;
                    e.updated_at = now;
                    e.error = None;
                    e
                }
                None => Execution {
                    id: id.to_string(),
                    actor: actor.to_string(),
                    branch: branch.to_string(),
                    envelope: envelope.clone(),
                    status: ExecutionStatus::Running,
                    started_at: now,
                    updated_at: now,
                    finished_at: None,
                    attempts: 1,
                    entries: 0,
                    replayed: 0,
                    input: input.clone(),
                    result: None,
                    error: None,
                },
            };
            Ok((Some(e.clone()), e))
        })?;
        let recorded = entries(repo, id)?;
        repo.events.emit(
            "execution_started",
            json!({ "execution": exec, "replaying": recorded.len() }),
        );
        Ok((
            DurableContext {
                repo: repo.clone(),
                id: id.to_string(),
                recorded,
                cursor: AtomicU64::new(0),
                replayed: AtomicU64::new(0),
            },
            exec,
        ))
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn is_replaying(&self) -> bool {
        (self.cursor.load(Ordering::SeqCst) as usize) < self.recorded.len()
    }

    /// Run a journaled step. On replay, returns the recorded output; the
    /// step name must match what was recorded at this position or the
    /// execution is rejected as non-deterministic. Failed steps are not
    /// journaled, so a retry executes them again.
    pub async fn run<T, F, Fut>(&self, kind: EntryKind, name: &str, input: Value, f: F) -> Result<T>
    where
        T: Serialize + DeserializeOwned,
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let seq = self.cursor.fetch_add(1, Ordering::SeqCst);
        if let Some(entry) = self.recorded.get(seq as usize) {
            if entry.name != name {
                return Err(Error::NonDeterminism {
                    execution: self.id.clone(),
                    seq,
                    recorded: entry.name.clone(),
                    requested: name.to_string(),
                });
            }
            self.replayed.fetch_add(1, Ordering::SeqCst);
            self.repo.events.emit(
                "journal_replayed",
                json!({ "execution": self.id, "seq": seq, "name": name, "kind": entry.kind }),
            );
            return Ok(serde_json::from_value(entry.output.clone())?);
        }

        let started_at = crate::now_ms();
        let t0 = std::time::Instant::now();
        let out = f().await?;
        let output = serde_json::to_value(&out)?;
        let commit = output
            .get("commit")
            .cloned()
            .or_else(|| if kind == EntryKind::StateWrite { Some(output.clone()) } else { None })
            .and_then(|v| serde_json::from_value::<Hash>(v).ok());
        let entry = JournalEntry {
            execution: self.id.clone(),
            seq,
            kind,
            name: name.to_string(),
            input,
            output,
            started_at,
            duration_ms: t0.elapsed().as_millis() as u64,
            commit,
        };
        let hash = self.repo.store.put_object(&Object::Journal(entry.clone()))?;
        self.repo.store.put_doc(ns::JOURNAL, &entry_key(&self.id, seq), &hash)?;
        self.repo.store.update_doc::<Execution, _, _>(ns::EXECUTIONS, &self.id, |cur| {
            let mut e = cur.ok_or_else(|| Error::NotFound(self.id.clone()))?;
            e.entries = e.entries.max(seq + 1);
            e.updated_at = crate::now_ms();
            Ok((Some(e), ()))
        })?;
        self.repo.events.emit("journal", json!({ "execution": self.id, "hash": hash, "entry": entry }));
        Ok(out)
    }

    fn finish(&self, status: ExecutionStatus, result: Option<Value>, error: Option<String>) -> Result<Execution> {
        let replayed = self.replayed.load(Ordering::SeqCst);
        let exec = self.repo.store.update_doc::<Execution, _, _>(ns::EXECUTIONS, &self.id, |cur| {
            let mut e = cur.ok_or_else(|| Error::NotFound(self.id.clone()))?;
            let now = crate::now_ms();
            e.status = status;
            e.updated_at = now;
            e.replayed = replayed;
            if status != ExecutionStatus::Running {
                e.finished_at = Some(now);
            }
            e.result = result;
            e.error = error;
            Ok((Some(e.clone()), e))
        })?;
        self.repo.events.emit("execution_finished", json!({ "execution": exec }));
        Ok(exec)
    }

    pub fn complete(&self, result: Value) -> Result<Execution> {
        self.finish(ExecutionStatus::Completed, Some(result), None)
    }

    pub fn fail(&self, error: &str) -> Result<Execution> {
        self.finish(ExecutionStatus::Failed, None, Some(error.to_string()))
    }
}
