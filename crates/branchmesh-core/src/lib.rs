//! # branchmesh-core
//!
//! The storage heart of BranchMesh:
//!
//! * [`Store`] — an embedded `redb` database holding immutable, SHA-256
//!   content-addressed objects plus a small set of mutable documents (refs,
//!   executions, mailboxes).
//! * [`Repo`] — Git-like operations on top of the store: hierarchical trees
//!   with structural sharing (copy-on-write), commits, O(1) branch forks,
//!   diffs, rollbacks and 3-way merges.
//! * [`journal`] — the deterministic execution journal used to replay
//!   non-deterministic side effects (LLM calls, HTTP, tools) after a crash.
//! * [`merge`] — CouchDB/Dolt-style 3-way JSON merge with pluggable conflict
//!   resolution (including LLM-assisted semantic resolution).

pub mod error;
pub mod events;
pub mod hash;
pub mod journal;
pub mod merge;
pub mod object;
pub mod repo;
pub mod store;

pub use error::{Error, Result};
pub use events::{Event, EventBus};
pub use hash::Hash;
pub use journal::{DurableContext, EntryKind, Execution, ExecutionStatus, JournalEntry};
pub use merge::{
    Conflict, ConflictResolver, MergeOptions, MergeOutcome, MergePreview, MergeStrategy, Resolution,
};
pub use object::{Commit, EntryType, Object, TreeEntry};
pub use repo::{BranchInfo, BranchStatus, Change, DiffEntry, DiffKind, Repo};
pub use store::Store;

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Generate a new random identifier (uuid v4, simple form).
pub fn new_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// Render an error with its full `source()` chain (e.g. the TLS or DNS cause
/// behind "error sending request").
pub fn error_chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut cur = e.source();
    while let Some(s) = cur {
        out.push_str(": ");
        out.push_str(&s.to_string());
        cur = s.source();
    }
    out
}
