use crate::hash::Hash;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Every immutable thing BranchMesh stores is an `Object`, addressed by the
/// SHA-256 of its canonical JSON encoding. `serde_json` (without
/// `preserve_order`) sorts object keys, so encoding is deterministic.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Object {
    /// A JSON value at a leaf path in a state tree.
    Blob { value: Value },
    /// A directory node: name -> child.
    Tree { entries: BTreeMap<String, TreeEntry> },
    /// A snapshot of a state tree plus its history.
    Commit(Commit),
    /// An execution-journal entry (LLM completion, tool output, ...).
    Journal(crate::journal::JournalEntry),
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EntryType {
    Blob,
    Tree,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct TreeEntry {
    pub kind: EntryType,
    pub hash: Hash,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Commit {
    pub tree: Hash,
    pub parents: Vec<Hash>,
    pub author: String,
    pub message: String,
    pub timestamp: i64,
    /// The durable execution that produced this commit, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<String>,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub meta: Value,
}

impl Object {
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("object serialization cannot fail")
    }

    pub fn hash(&self) -> Hash {
        Hash::of(&self.encode())
    }
}
