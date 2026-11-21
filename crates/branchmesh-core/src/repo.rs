use crate::error::{Error, Result};
use crate::events::EventBus;
use crate::hash::Hash;
use crate::object::{Commit, EntryType, Object, TreeEntry};
use crate::store::{ns, Store};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};

/// A mutation to a state tree. Paths are `/`-separated.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Change {
    Set { path: String, value: Value },
    Delete { path: String },
}

impl Change {
    pub fn set(path: impl Into<String>, value: Value) -> Self {
        Change::Set { path: path.into(), value }
    }
    pub fn delete(path: impl Into<String>) -> Self {
        Change::Delete { path: path.into() }
    }
    fn path(&self) -> &str {
        match self {
            Change::Set { path, .. } | Change::Delete { path } => path,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum BranchStatus {
    Active,
    Merged { into: String, commit: Hash, at: i64 },
    Abandoned { at: i64 },
}

/// A mutable branch pointer. Forking copies only this record (O(1)).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BranchInfo {
    pub name: String,
    pub head: Hash,
    /// Branch this one was forked from.
    pub parent: Option<String>,
    /// Commit this branch was forked at.
    pub fork_point: Option<Hash>,
    pub created_at: i64,
    pub updated_at: i64,
    pub status: BranchStatus,
    #[serde(default)]
    pub meta: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DiffKind {
    Added,
    Removed,
    Modified,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffEntry {
    pub path: String,
    pub kind: DiffKind,
    pub old: Option<Value>,
    pub new: Option<Value>,
}

/// Options describing a commit.
#[derive(Debug, Clone, Default)]
pub struct CommitOptions {
    pub author: String,
    pub message: String,
    pub execution: Option<String>,
    pub meta: Value,
    /// Create a commit even if the tree did not change.
    pub allow_empty: bool,
}

impl CommitOptions {
    pub fn new(author: impl Into<String>, message: impl Into<String>) -> Self {
        CommitOptions {
            author: author.into(),
            message: message.into(),
            ..Default::default()
        }
    }
    pub fn execution(mut self, id: impl Into<String>) -> Self {
        self.execution = Some(id.into());
        self
    }
    pub fn meta(mut self, meta: Value) -> Self {
        self.meta = meta;
        self
    }
}

/// Git-like repository over the content-addressed [`Store`].
#[derive(Clone)]
pub struct Repo {
    pub(crate) store: Store,
    pub(crate) events: EventBus,
}

pub(crate) fn split_path(path: &str) -> Result<Vec<&str>> {
    let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if parts.is_empty() {
        return Err(Error::Invalid(format!("empty path '{path}'")));
    }
    Ok(parts)
}

fn validate_branch_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 200
        && !name.starts_with('/')
        && !name.ends_with('/')
        && !name.contains("//")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':' | '@'));
    if ok {
        Ok(())
    } else {
        Err(Error::Invalid(format!(
            "invalid branch name '{name}' (allowed: A-Z a-z 0-9 - _ . / : @)"
        )))
    }
}

/// In-memory working tree used to apply a batch of changes with structural sharing.
enum Node {
    /// Unchanged subtree / blob from the store.
    Ref(TreeEntry),
    /// New blob value.
    Blob(Value),
    /// Loaded (and possibly modified) directory.
    Dir(BTreeMap<String, Node>),
}

impl Repo {
    pub fn new(store: Store, events: EventBus) -> Result<Self> {
        let repo = Repo { store, events };
        // Make sure the shared genesis commit exists.
        repo.store.put_objects(&[Self::empty_tree_object(), Self::genesis_object()])?;
        Ok(repo)
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn events(&self) -> &EventBus {
        &self.events
    }

    fn empty_tree_object() -> Object {
        Object::Tree { entries: BTreeMap::new() }
    }

    fn genesis_object() -> Object {
        Object::Commit(Commit {
            tree: Self::empty_tree_object().hash(),
            parents: vec![],
            author: "branchmesh".into(),
            message: "genesis".into(),
            timestamp: 0,
            execution: None,
            meta: Value::Null,
        })
    }

    pub fn empty_tree() -> Hash {
        Self::empty_tree_object().hash()
    }

    /// The root commit every branch descends from (deterministic hash).
    pub fn genesis() -> Hash {
        Self::genesis_object().hash()
    }

    // ------------------------------------------------------------------ objects

    pub fn commit_object(&self, hash: &Hash) -> Result<Commit> {
        match self.store.get_object(hash)? {
            Object::Commit(c) => Ok(c),
            _ => Err(Error::Invalid(format!("{hash} is not a commit"))),
        }
    }

    fn tree_entries(&self, hash: &Hash) -> Result<BTreeMap<String, TreeEntry>> {
        match self.store.get_object(hash)? {
            Object::Tree { entries } => Ok(entries),
            _ => Err(Error::Invalid(format!("{hash} is not a tree"))),
        }
    }

    fn blob_value(&self, hash: &Hash) -> Result<Value> {
        match self.store.get_object(hash)? {
            Object::Blob { value } => Ok(value),
            _ => Err(Error::Invalid(format!("{hash} is not a blob"))),
        }
    }

    /// Apply changes to a tree, returning the new root tree hash. Only nodes
    /// along changed paths are rewritten; everything else is shared.
    pub fn apply_changes(&self, root: &Hash, changes: &[Change]) -> Result<Hash> {
        let mut dir = self.load_dir(root)?;
        for change in changes {
            let parts = split_path(change.path())?;
            match change {
                Change::Set { value, .. } => self.node_set(&mut dir, &parts, value.clone())?,
                Change::Delete { .. } => self.node_delete(&mut dir, &parts)?,
            }
        }
        let mut new_objects = Vec::new();
        let entry = Self::finalize(Node::Dir(dir), &mut new_objects)?;
        self.store.put_objects(&new_objects)?;
        Ok(entry.map(|e| e.hash).unwrap_or_else(Self::empty_tree))
    }

    fn load_dir(&self, hash: &Hash) -> Result<BTreeMap<String, Node>> {
        Ok(self
            .tree_entries(hash)?
            .into_iter()
            .map(|(k, v)| (k, Node::Ref(v)))
            .collect())
    }

    fn node_set(&self, dir: &mut BTreeMap<String, Node>, parts: &[&str], value: Value) -> Result<()> {
        let (head, rest) = (parts[0], &parts[1..]);
        if rest.is_empty() {
            dir.insert(head.to_string(), Node::Blob(value));
            return Ok(());
        }
        let child = dir.remove(head);
        let mut sub = match child {
            Some(Node::Dir(d)) => d,
            Some(Node::Ref(TreeEntry { kind: EntryType::Tree, hash })) => self.load_dir(&hash)?,
            _ => BTreeMap::new(),
        };
        self.node_set(&mut sub, rest, value)?;
        dir.insert(head.to_string(), Node::Dir(sub));
        Ok(())
    }

    fn node_delete(&self, dir: &mut BTreeMap<String, Node>, parts: &[&str]) -> Result<()> {
        let (head, rest) = (parts[0], &parts[1..]);
        if rest.is_empty() {
            dir.remove(head);
            return Ok(());
        }
        let child = dir.remove(head);
        let mut sub = match child {
            Some(Node::Dir(d)) => d,
            Some(Node::Ref(TreeEntry { kind: EntryType::Tree, hash })) => self.load_dir(&hash)?,
            Some(other) => {
                dir.insert(head.to_string(), other);
                return Ok(());
            }
            None => return Ok(()),
        };
        self.node_delete(&mut sub, rest)?;
        dir.insert(head.to_string(), Node::Dir(sub));
        Ok(())
    }

    fn finalize(node: Node, out: &mut Vec<Object>) -> Result<Option<TreeEntry>> {
        match node {
            Node::Ref(e) => Ok(Some(e)),
            Node::Blob(value) => {
                let obj = Object::Blob { value };
                let hash = obj.hash();
                out.push(obj);
                Ok(Some(TreeEntry { kind: EntryType::Blob, hash }))
            }
            Node::Dir(children) => {
                let mut entries = BTreeMap::new();
                for (name, child) in children {
                    if let Some(e) = Self::finalize(child, out)? {
                        entries.insert(name, e);
                    }
                }
                if entries.is_empty() {
                    return Ok(None);
                }
                let obj = Object::Tree { entries };
                let hash = obj.hash();
                out.push(obj);
                Ok(Some(TreeEntry { kind: EntryType::Tree, hash }))
            }
        }
    }

    /// Look up the entry at `path` inside a tree.
    pub fn tree_entry_at(&self, root: &Hash, path: &str) -> Result<Option<TreeEntry>> {
        let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        if parts.is_empty() {
            return Ok(Some(TreeEntry { kind: EntryType::Tree, hash: *root }));
        }
        let mut current = *root;
        for (i, part) in parts.iter().enumerate() {
            let entries = self.tree_entries(&current)?;
            match entries.get(*part) {
                None => return Ok(None),
                Some(e) if i == parts.len() - 1 => return Ok(Some(*e)),
                Some(TreeEntry { kind: EntryType::Tree, hash }) => current = *hash,
                Some(_) => return Ok(None),
            }
        }
        Ok(None)
    }

    /// Materialize a tree (or blob) entry as JSON. Directories become objects.
    pub fn materialize(&self, entry: &TreeEntry) -> Result<Value> {
        match entry.kind {
            EntryType::Blob => self.blob_value(&entry.hash),
            EntryType::Tree => {
                let mut map = Map::new();
                for (k, e) in self.tree_entries(&entry.hash)? {
                    map.insert(k, self.materialize(&e)?);
                }
                Ok(Value::Object(map))
            }
        }
    }

    /// All blob paths under a tree, mapped to their blob hashes.
    pub fn flatten(&self, root: &Hash, prefix: &str) -> Result<BTreeMap<String, Hash>> {
        let mut out = BTreeMap::new();
        self.flatten_into(root, prefix.trim_matches('/'), &mut out)?;
        Ok(out)
    }

    fn flatten_into(&self, tree: &Hash, prefix: &str, out: &mut BTreeMap<String, Hash>) -> Result<()> {
        for (name, e) in self.tree_entries(tree)? {
            let path = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
            match e.kind {
                EntryType::Blob => {
                    out.insert(path, e.hash);
                }
                EntryType::Tree => self.flatten_into(&e.hash, &path, out)?,
            }
        }
        Ok(())
    }

    /// Paths whose blob differs between two trees. Identical subtrees are
    /// skipped by hash comparison, so this is proportional to the change size.
    pub fn changed_paths(&self, a: &Hash, b: &Hash) -> Result<BTreeSet<String>> {
        let mut out = BTreeSet::new();
        self.changed_into(
            Some(TreeEntry { kind: EntryType::Tree, hash: *a }),
            Some(TreeEntry { kind: EntryType::Tree, hash: *b }),
            "",
            &mut out,
        )?;
        Ok(out)
    }

    fn changed_into(
        &self,
        a: Option<TreeEntry>,
        b: Option<TreeEntry>,
        path: &str,
        out: &mut BTreeSet<String>,
    ) -> Result<()> {
        if a == b {
            return Ok(());
        }
        let as_tree = |e: &Option<TreeEntry>| -> Result<BTreeMap<String, TreeEntry>> {
            match e {
                Some(TreeEntry { kind: EntryType::Tree, hash }) => self.tree_entries(hash),
                _ => Ok(BTreeMap::new()),
            }
        };
        let a_blob = matches!(a, Some(TreeEntry { kind: EntryType::Blob, .. }));
        let b_blob = matches!(b, Some(TreeEntry { kind: EntryType::Blob, .. }));
        if (a_blob || b_blob) && !path.is_empty() {
            out.insert(path.to_string());
        }
        let ta = as_tree(&a)?;
        let tb = as_tree(&b)?;
        let names: BTreeSet<&String> = ta.keys().chain(tb.keys()).collect();
        for name in names {
            let child = if path.is_empty() { name.clone() } else { format!("{path}/{name}") };
            self.changed_into(ta.get(name).copied(), tb.get(name).copied(), &child, out)?;
        }
        Ok(())
    }

    /// Blob value at an exact path (`None` if absent or a directory).
    pub fn blob_at(&self, tree: &Hash, path: &str) -> Result<Option<(Hash, Value)>> {
        match self.tree_entry_at(tree, path)? {
            Some(TreeEntry { kind: EntryType::Blob, hash }) => Ok(Some((hash, self.blob_value(&hash)?))),
            _ => Ok(None),
        }
    }

    // ----------------------------------------------------------------- branches

    pub fn branch(&self, name: &str) -> Result<BranchInfo> {
        self.store
            .get_doc(ns::REFS, name)?
            .ok_or_else(|| Error::BranchNotFound(name.to_string()))
    }

    pub fn try_branch(&self, name: &str) -> Result<Option<BranchInfo>> {
        self.store.get_doc(ns::REFS, name)
    }

    pub fn branches(&self) -> Result<Vec<BranchInfo>> {
        Ok(self
            .store
            .list_docs::<BranchInfo>(ns::REFS, "")?
            .into_iter()
            .map(|(_, b)| b)
            .collect())
    }

    /// Create a branch at the genesis commit (or reuse it if it exists and
    /// `get_or_create` is set).
    pub fn create_branch(&self, name: &str, meta: Value) -> Result<BranchInfo> {
        validate_branch_name(name)?;
        let now = crate::now_ms();
        let info = BranchInfo {
            name: name.to_string(),
            head: Self::genesis(),
            parent: None,
            fork_point: None,
            created_at: now,
            updated_at: now,
            status: BranchStatus::Active,
            meta,
        };
        self.insert_branch(info)
    }

    pub fn get_or_create_branch(&self, name: &str, meta: Value) -> Result<BranchInfo> {
        match self.try_branch(name)? {
            Some(b) => Ok(b),
            None => match self.create_branch(name, meta) {
                Err(Error::BranchExists(_)) => self.branch(name),
                other => other,
            },
        }
    }

    fn insert_branch(&self, info: BranchInfo) -> Result<BranchInfo> {
        let name = info.name.clone();
        let created = self.store.update_doc::<BranchInfo, _, _>(ns::REFS, &name, |cur| {
            if cur.is_some() {
                return Err(Error::BranchExists(name.clone()));
            }
            Ok((Some(info.clone()), info))
        })?;
        self.events.emit("branch_created", json!({ "branch": created }));
        Ok(created)
    }

    /// O(1) fork: a new ref pointing at the source head. No data is copied;
    /// subsequent writes on either side create new nodes (copy-on-write).
    pub fn fork(&self, source: &str, name: &str, meta: Value) -> Result<BranchInfo> {
        validate_branch_name(name)?;
        let src = self.branch(source)?;
        let now = crate::now_ms();
        let info = BranchInfo {
            name: name.to_string(),
            head: src.head,
            parent: Some(source.to_string()),
            fork_point: Some(src.head),
            created_at: now,
            updated_at: now,
            status: BranchStatus::Active,
            meta,
        };
        self.insert_branch(info)
    }

    /// Fork from an arbitrary commit (time-travel branching).
    pub fn fork_at(&self, commit: &Hash, name: &str, parent: Option<String>, meta: Value) -> Result<BranchInfo> {
        validate_branch_name(name)?;
        self.commit_object(commit)?;
        let now = crate::now_ms();
        self.insert_branch(BranchInfo {
            name: name.to_string(),
            head: *commit,
            parent,
            fork_point: Some(*commit),
            created_at: now,
            updated_at: now,
            status: BranchStatus::Active,
            meta,
        })
    }

    pub fn set_status(&self, name: &str, status: BranchStatus) -> Result<BranchInfo> {
        let updated = self.store.update_doc::<BranchInfo, _, _>(ns::REFS, name, |cur| {
            let mut b = cur.ok_or_else(|| Error::BranchNotFound(name.to_string()))?;
            b.status = status;
            b.updated_at = crate::now_ms();
            Ok((Some(b.clone()), b))
        })?;
        self.events.emit("branch_updated", json!({ "branch": updated }));
        Ok(updated)
    }

    pub fn update_meta(&self, name: &str, f: impl FnOnce(&mut Value)) -> Result<BranchInfo> {
        self.store.update_doc::<BranchInfo, _, _>(ns::REFS, name, |cur| {
            let mut b = cur.ok_or_else(|| Error::BranchNotFound(name.to_string()))?;
            if !b.meta.is_object() {
                b.meta = json!({});
            }
            f(&mut b.meta);
            Ok((Some(b.clone()), b))
        })
    }

    pub fn delete_branch(&self, name: &str) -> Result<()> {
        if !self.store.delete_doc(ns::REFS, name)? {
            return Err(Error::BranchNotFound(name.to_string()));
        }
        self.events.emit("branch_deleted", json!({ "branch": name }));
        Ok(())
    }

    /// Move a branch ref with compare-and-swap semantics.
    pub fn update_ref(&self, name: &str, expected: Hash, new: Hash) -> Result<BranchInfo> {
        self.store.update_doc::<BranchInfo, _, _>(ns::REFS, name, |cur| {
            let mut b = cur.ok_or_else(|| Error::BranchNotFound(name.to_string()))?;
            if b.head != expected {
                return Err(Error::RefConflict(name.to_string()));
            }
            b.head = new;
            b.updated_at = crate::now_ms();
            Ok((Some(b.clone()), b))
        })
    }

    // ------------------------------------------------------------------ commits

    /// Write a commit with an explicit tree and parents, without moving any ref.
    pub fn write_commit(&self, tree: Hash, parents: Vec<Hash>, opts: &CommitOptions) -> Result<Hash> {
        let commit = Object::Commit(Commit {
            tree,
            parents,
            author: opts.author.clone(),
            message: opts.message.clone(),
            timestamp: crate::now_ms(),
            execution: opts.execution.clone(),
            meta: opts.meta.clone(),
        });
        self.store.put_object(&commit)
    }

    /// Apply `changes` on top of the branch head and advance the branch.
    /// Retries on concurrent ref updates by re-applying the changes to the new head.
    pub fn commit(&self, branch: &str, changes: &[Change], opts: CommitOptions) -> Result<Hash> {
        for _ in 0..32 {
            let info = self.branch(branch)?;
            let head = self.commit_object(&info.head)?;
            let tree = self.apply_changes(&head.tree, changes)?;
            if tree == head.tree && !opts.allow_empty {
                return Ok(info.head);
            }
            let hash = self.write_commit(tree, vec![info.head], &opts)?;
            match self.update_ref(branch, info.head, hash) {
                Ok(_) => {
                    self.events.emit(
                        "commit",
                        json!({
                            "branch": branch,
                            "commit": hash,
                            "parent": info.head,
                            "author": opts.author,
                            "message": opts.message,
                            "execution": opts.execution,
                            "paths": changes.iter().map(|c| c.path()).collect::<Vec<_>>(),
                        }),
                    );
                    return Ok(hash);
                }
                Err(Error::RefConflict(_)) => continue,
                Err(e) => return Err(e),
            }
        }
        Err(Error::RefConflict(branch.to_string()))
    }

    pub fn head(&self, branch: &str) -> Result<Hash> {
        Ok(self.branch(branch)?.head)
    }

    /// Full materialized state of a commit as a nested JSON object.
    pub fn state_at(&self, commit: &Hash) -> Result<Value> {
        let c = self.commit_object(commit)?;
        self.materialize(&TreeEntry { kind: EntryType::Tree, hash: c.tree })
    }

    pub fn state(&self, branch: &str) -> Result<Value> {
        self.state_at(&self.head(branch)?)
    }

    /// Value at a path (blob or directory) on a branch head.
    pub fn get(&self, branch: &str, path: &str) -> Result<Option<Value>> {
        self.get_at(&self.head(branch)?, path)
    }

    pub fn get_at(&self, commit: &Hash, path: &str) -> Result<Option<Value>> {
        let c = self.commit_object(commit)?;
        match self.tree_entry_at(&c.tree, path)? {
            Some(e) => Ok(Some(self.materialize(&e)?)),
            None => Ok(None),
        }
    }

    /// All leaf values under `prefix` at a commit, in path order.
    pub fn list_at(&self, commit: &Hash, prefix: &str) -> Result<Vec<(String, Value)>> {
        let c = self.commit_object(commit)?;
        let prefix = prefix.trim_matches('/');
        let sub = match self.tree_entry_at(&c.tree, prefix)? {
            Some(TreeEntry { kind: EntryType::Tree, hash }) => hash,
            Some(TreeEntry { kind: EntryType::Blob, hash }) => {
                return Ok(vec![(prefix.to_string(), self.blob_value(&hash)?)])
            }
            None => return Ok(vec![]),
        };
        self.flatten(&sub, prefix)?
            .into_iter()
            .map(|(p, h)| Ok((p, self.blob_value(&h)?)))
            .collect()
    }

    pub fn list(&self, branch: &str, prefix: &str) -> Result<Vec<(String, Value)>> {
        self.list_at(&self.head(branch)?, prefix)
    }

    /// Commit history reachable from `from`, newest first.
    pub fn log(&self, from: &Hash, limit: usize) -> Result<Vec<(Hash, Commit)>> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        // Max-heap on timestamp for a stable, readable order across merges.
        let mut heap = std::collections::BinaryHeap::new();
        let c = self.commit_object(from)?;
        heap.push((c.timestamp, *from));
        while let Some((_, h)) = heap.pop() {
            if !seen.insert(h) {
                continue;
            }
            let c = self.commit_object(&h)?;
            for p in &c.parents {
                if !seen.contains(p) {
                    let pc = self.commit_object(p)?;
                    heap.push((pc.timestamp, *p));
                }
            }
            out.push((h, c));
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }

    fn ancestors(&self, from: &Hash) -> Result<HashSet<Hash>> {
        let mut seen = HashSet::new();
        let mut queue = VecDeque::from([*from]);
        while let Some(h) = queue.pop_front() {
            if !seen.insert(h) {
                continue;
            }
            for p in self.commit_object(&h)?.parents {
                queue.push_back(p);
            }
        }
        Ok(seen)
    }

    pub fn is_ancestor(&self, ancestor: &Hash, of: &Hash) -> Result<bool> {
        Ok(ancestor == of || self.ancestors(of)?.contains(ancestor))
    }

    /// Lowest common ancestor of two commits (best common ancestor by
    /// recency; all branches share the genesis commit, so one always exists).
    pub fn merge_base(&self, a: &Hash, b: &Hash) -> Result<Hash> {
        let anc_a = self.ancestors(a)?;
        let anc_b = self.ancestors(b)?;
        let common: HashSet<Hash> = anc_a.intersection(&anc_b).copied().collect();
        // Every ancestor of a common commit is itself common, so the "best"
        // common ancestors are exactly the common commits that are not a
        // parent of another common commit. O(n).
        let mut dominated = HashSet::new();
        for c in &common {
            for p in self.commit_object(c)?.parents {
                dominated.insert(p);
            }
        }
        let mut best: Vec<Hash> = common.difference(&dominated).copied().collect();
        best.sort_by_key(|h| std::cmp::Reverse(self.commit_object(h).map(|c| c.timestamp).unwrap_or(0)));
        Ok(best.first().copied().unwrap_or_else(Self::genesis))
    }

    /// Structural diff between two commits.
    pub fn diff(&self, from: &Hash, to: &Hash) -> Result<Vec<DiffEntry>> {
        let a = self.commit_object(from)?.tree;
        let b = self.commit_object(to)?.tree;
        let mut out = Vec::new();
        for path in self.changed_paths(&a, &b)? {
            let old = self.blob_at(&a, &path)?.map(|x| x.1);
            let new = self.blob_at(&b, &path)?.map(|x| x.1);
            let kind = match (&old, &new) {
                (None, Some(_)) => DiffKind::Added,
                (Some(_), None) => DiffKind::Removed,
                _ => DiffKind::Modified,
            };
            out.push(DiffEntry { path, kind, old, new });
        }
        Ok(out)
    }

    /// Non-destructive rollback: a new commit on `branch` whose tree equals
    /// the tree of `target`. History is preserved for auditing.
    pub fn rollback(&self, branch: &str, target: &Hash, author: &str) -> Result<Hash> {
        let target_commit = self.commit_object(target)?;
        for _ in 0..32 {
            let info = self.branch(branch)?;
            let opts = CommitOptions::new(author, format!("rollback to {}", target.short()))
                .meta(json!({ "rollback": { "target": target } }));
            let hash = self.write_commit(target_commit.tree, vec![info.head], &opts)?;
            match self.update_ref(branch, info.head, hash) {
                Ok(_) => {
                    self.events.emit(
                        "rollback",
                        json!({ "branch": branch, "commit": hash, "target": target }),
                    );
                    return Ok(hash);
                }
                Err(Error::RefConflict(_)) => continue,
                Err(e) => return Err(e),
            }
        }
        Err(Error::RefConflict(branch.to_string()))
    }
}
