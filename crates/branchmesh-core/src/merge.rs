//! 3-way merge of state trees.
//!
//! Merging happens at two levels:
//!
//! 1. **Tree level** — for every leaf path changed on the source side since
//!    the merge base, take the source value if the target did not touch it.
//! 2. **Value level** — when both sides changed the same leaf, attempt a
//!    recursive JSON 3-way merge: objects merge key by key, arrays that were
//!    only appended to on both sides are concatenated. Anything else is a
//!    [`Conflict`], handed to the selected [`MergeStrategy`].

use crate::error::{Error, Result};
use crate::hash::Hash;
use crate::repo::{BranchStatus, Change, CommitOptions, Repo};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum MergeStrategy {
    /// Automatic 3-way merge; fail if any conflict remains unresolved.
    #[default]
    #[serde(rename = "three_way")]
    ThreeWay,
    /// On conflict keep the target branch's value.
    #[serde(rename = "ours")]
    Ours,
    /// On conflict take the source branch's value.
    #[serde(rename = "theirs")]
    Theirs,
    /// CouchDB-style: pick a deterministic winner (independent of merge
    /// direction) and keep the losing revision under `_conflicts/<path>`.
    #[serde(rename = "deterministic")]
    Deterministic,
    /// Ask an LLM to produce a semantically merged value for each conflict.
    #[serde(rename = "semantic_llm", alias = "semantic")]
    SemanticLLM,
}

impl std::fmt::Display for MergeStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = serde_json::to_value(self).ok().and_then(|v| v.as_str().map(String::from));
        f.write_str(&s.unwrap_or_default())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conflict {
    pub path: String,
    pub base: Option<Value>,
    /// Target branch value.
    pub ours: Option<Value>,
    /// Source branch value.
    pub theirs: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resolution {
    pub path: String,
    /// `None` deletes the path.
    pub value: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rationale: Option<String>,
    pub resolved_by: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct MergeContext {
    pub source: String,
    pub target: String,
    pub base: Hash,
}

/// Pluggable conflict resolution (e.g. an LLM).
#[async_trait]
pub trait ConflictResolver: Send + Sync {
    async fn resolve(&self, conflicts: &[Conflict], ctx: &MergeContext) -> Result<Vec<Resolution>>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedChange {
    pub path: String,
    /// `take_theirs` | `auto_merged` | `delete`
    pub action: String,
    pub value: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergePreview {
    pub source: String,
    pub target: String,
    pub base: Hash,
    pub ours: Hash,
    pub theirs: Hash,
    pub up_to_date: bool,
    pub fast_forward: bool,
    pub changes: Vec<PlannedChange>,
    pub conflicts: Vec<Conflict>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeOutcome {
    pub commit: Hash,
    pub source: String,
    pub target: String,
    pub base: Hash,
    pub strategy: MergeStrategy,
    pub up_to_date: bool,
    pub changes: Vec<PlannedChange>,
    pub resolutions: Vec<Resolution>,
}

#[derive(Clone, Default)]
pub struct MergeOptions {
    pub strategy: MergeStrategy,
    /// Manual resolutions (path -> value, `None` = delete). Applied before the strategy.
    pub resolutions: BTreeMap<String, Option<Value>>,
    pub author: String,
    pub message: Option<String>,
    /// Mark the source branch as merged afterwards.
    pub close_source: bool,
    pub resolver: Option<Arc<dyn ConflictResolver>>,
}

impl MergeOptions {
    pub fn new(strategy: MergeStrategy) -> Self {
        MergeOptions { strategy, author: "branchmesh".into(), ..Default::default() }
    }
}

/// Both sides changed a value in ways that cannot be combined automatically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValueConflict;

/// Recursive 3-way JSON merge. `Err(ValueConflict)` means an irreconcilable
/// conflict; `Ok(None)` means the value is deleted.
pub fn merge_values(base: Option<&Value>, ours: Option<&Value>, theirs: Option<&Value>) -> Result<Option<Value>, ValueConflict> {
    if ours == theirs {
        return Ok(ours.cloned());
    }
    if base == ours {
        return Ok(theirs.cloned());
    }
    if base == theirs {
        return Ok(ours.cloned());
    }
    match (ours, theirs) {
        (Some(Value::Object(o)), Some(Value::Object(t))) => {
            let empty = Map::new();
            let b = match base {
                Some(Value::Object(b)) => b,
                _ => &empty,
            };
            let mut out = Map::new();
            let keys: std::collections::BTreeSet<&String> = b.keys().chain(o.keys()).chain(t.keys()).collect();
            for k in keys {
                if let Some(v) = merge_values(b.get(k), o.get(k), t.get(k))? {
                    out.insert(k.clone(), v);
                }
            }
            Ok(Some(Value::Object(out)))
        }
        (Some(Value::Array(o)), Some(Value::Array(t))) => {
            let empty = Vec::new();
            let b = match base {
                Some(Value::Array(b)) => b,
                None => &empty,
                _ => return Err(ValueConflict),
            };
            if o.starts_with(b) && t.starts_with(b) {
                let mut out = b.clone();
                let ours_tail = &o[b.len()..];
                out.extend(ours_tail.iter().cloned());
                for item in &t[b.len()..] {
                    if !ours_tail.contains(item) {
                        out.push(item.clone());
                    }
                }
                Ok(Some(Value::Array(out)))
            } else {
                Err(ValueConflict)
            }
        }
        _ => Err(ValueConflict),
    }
}

fn blob_hash(v: &Option<Value>) -> Option<Hash> {
    v.as_ref().map(|v| crate::object::Object::Blob { value: v.clone() }.hash())
}

impl Repo {
    /// Compute what merging `source` into `target` would do, without writing anything.
    pub fn merge_preview(&self, source: &str, target: &str) -> Result<MergePreview> {
        let theirs = self.head(source)?;
        let ours = self.head(target)?;
        if self.is_ancestor(&theirs, &ours)? {
            return Ok(MergePreview {
                source: source.into(),
                target: target.into(),
                base: theirs,
                ours,
                theirs,
                up_to_date: true,
                fast_forward: false,
                changes: vec![],
                conflicts: vec![],
            });
        }
        let base = self.merge_base(&ours, &theirs)?;
        let tb = self.commit_object(&base)?.tree;
        let to = self.commit_object(&ours)?.tree;
        let tt = self.commit_object(&theirs)?.tree;

        let mut changes = Vec::new();
        let mut conflicts = Vec::new();
        for path in self.changed_paths(&tb, &tt)? {
            let b = self.blob_at(&tb, &path)?.map(|x| x.1);
            let o = self.blob_at(&to, &path)?.map(|x| x.1);
            let t = self.blob_at(&tt, &path)?.map(|x| x.1);
            if o == t {
                continue;
            }
            if o == b {
                changes.push(PlannedChange {
                    path,
                    action: if t.is_some() { "take_theirs" } else { "delete" }.into(),
                    value: t,
                });
                continue;
            }
            match merge_values(b.as_ref(), o.as_ref(), t.as_ref()) {
                Ok(v) => changes.push(PlannedChange { path, action: "auto_merged".into(), value: v }),
                Err(ValueConflict) => conflicts.push(Conflict { path, base: b, ours: o, theirs: t }),
            }
        }
        Ok(MergePreview {
            source: source.into(),
            target: target.into(),
            base,
            ours,
            theirs,
            up_to_date: false,
            fast_forward: base == ours,
            changes,
            conflicts,
        })
    }

    /// Merge `source` into `target`, creating a merge commit with both heads as parents.
    pub async fn merge(&self, source: &str, target: &str, opts: MergeOptions) -> Result<MergeOutcome> {
        if source == target {
            return Err(Error::Invalid("cannot merge a branch into itself".into()));
        }
        // Resolutions are cached across CAS retries so an LLM is asked once per conflict.
        let mut cache: BTreeMap<String, Resolution> = BTreeMap::new();
        for _ in 0..8 {
            let preview = self.merge_preview(source, target)?;
            if preview.up_to_date {
                return Ok(MergeOutcome {
                    commit: preview.ours,
                    source: source.into(),
                    target: target.into(),
                    base: preview.base,
                    strategy: opts.strategy,
                    up_to_date: true,
                    changes: vec![],
                    resolutions: vec![],
                });
            }

            let mut resolutions = Vec::new();
            let mut pending = Vec::new();
            for c in &preview.conflicts {
                if let Some(v) = opts.resolutions.get(&c.path) {
                    resolutions.push(Resolution {
                        path: c.path.clone(),
                        value: v.clone(),
                        rationale: None,
                        resolved_by: "manual".into(),
                    });
                } else if let Some(r) = cache.get(&c.path) {
                    resolutions.push(r.clone());
                } else {
                    pending.push(c.clone());
                }
            }

            let mut extra_changes = Vec::new();
            match opts.strategy {
                MergeStrategy::ThreeWay if !pending.is_empty() => return Err(Error::MergeConflict(pending)),
                MergeStrategy::ThreeWay => {}
                MergeStrategy::Ours => {
                    for c in &pending {
                        resolutions.push(Resolution {
                            path: c.path.clone(),
                            value: c.ours.clone(),
                            rationale: Some("kept target value".into()),
                            resolved_by: "ours".into(),
                        });
                    }
                }
                MergeStrategy::Theirs => {
                    for c in &pending {
                        resolutions.push(Resolution {
                            path: c.path.clone(),
                            value: c.theirs.clone(),
                            rationale: Some("took source value".into()),
                            resolved_by: "theirs".into(),
                        });
                    }
                }
                MergeStrategy::Deterministic => {
                    for c in &pending {
                        // Non-deleted revisions beat deletions; otherwise the larger
                        // content hash wins. Same result regardless of direction.
                        let ours_wins = match (&c.ours, &c.theirs) {
                            (Some(_), None) => true,
                            (None, Some(_)) => false,
                            _ => blob_hash(&c.ours) > blob_hash(&c.theirs),
                        };
                        let (winner, loser, loser_branch) = if ours_wins {
                            (c.ours.clone(), c.theirs.clone(), source)
                        } else {
                            (c.theirs.clone(), c.ours.clone(), target)
                        };
                        if let Some(l) = loser {
                            extra_changes.push(Change::set(
                                format!("_conflicts/{}", c.path),
                                json!({ "value": l, "branch": loser_branch, "at": crate::now_ms() }),
                            ));
                        }
                        resolutions.push(Resolution {
                            path: c.path.clone(),
                            value: winner,
                            rationale: Some("deterministic winner; losing revision kept in _conflicts/".into()),
                            resolved_by: "deterministic".into(),
                        });
                    }
                }
                MergeStrategy::SemanticLLM => {
                    if !pending.is_empty() {
                        let resolver = opts
                            .resolver
                            .clone()
                            .ok_or_else(|| Error::Invalid("semantic merge requires a conflict resolver".into()))?;
                        let ctx = MergeContext { source: source.into(), target: target.into(), base: preview.base };
                        let resolved = resolver.resolve(&pending, &ctx).await?;
                        for c in &pending {
                            let r = resolved
                                .iter()
                                .find(|r| r.path == c.path)
                                .cloned()
                                .ok_or_else(|| Error::MergeConflict(vec![c.clone()]))?;
                            cache.insert(c.path.clone(), r.clone());
                            resolutions.push(r);
                        }
                    }
                }
            }

            // Build the merged tree on top of the target tree.
            let mut tree_changes: Vec<Change> = Vec::new();
            for ch in &preview.changes {
                tree_changes.push(match &ch.value {
                    Some(v) => Change::set(&ch.path, v.clone()),
                    None => Change::delete(&ch.path),
                });
            }
            for r in &resolutions {
                tree_changes.push(match &r.value {
                    Some(v) => Change::set(&r.path, v.clone()),
                    None => Change::delete(&r.path),
                });
            }
            tree_changes.extend(extra_changes);

            let target_tree = self.commit_object(&preview.ours)?.tree;
            let tree = self.apply_changes(&target_tree, &tree_changes)?;
            let message = opts
                .message
                .clone()
                .unwrap_or_else(|| format!("merge {source} into {target} ({})", opts.strategy));
            let commit_opts = CommitOptions::new(opts.author.clone(), message).meta(json!({
                "merge": {
                    "source": source,
                    "target": target,
                    "base": preview.base,
                    "strategy": opts.strategy,
                    "auto_changes": preview.changes.len(),
                    "resolutions": resolutions,
                }
            }));
            let commit = self.write_commit(tree, vec![preview.ours, preview.theirs], &commit_opts)?;
            match self.update_ref(target, preview.ours, commit) {
                Ok(_) => {}
                Err(Error::RefConflict(_)) => continue,
                Err(e) => return Err(e),
            }
            if opts.close_source {
                self.set_status(
                    source,
                    BranchStatus::Merged { into: target.into(), commit, at: crate::now_ms() },
                )?;
            }
            let outcome = MergeOutcome {
                commit,
                source: source.into(),
                target: target.into(),
                base: preview.base,
                strategy: opts.strategy,
                up_to_date: false,
                changes: preview.changes,
                resolutions,
            };
            self.events.emit("merge", serde_json::to_value(&outcome)?);
            return Ok(outcome);
        }
        Err(Error::RefConflict(target.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_merge_objects_and_arrays() {
        let base = json!({"a": 1, "list": [1, 2], "keep": true});
        let ours = json!({"a": 2, "list": [1, 2, 3], "keep": true});
        let theirs = json!({"a": 1, "b": "new", "list": [1, 2, 4], "keep": true});
        let merged = merge_values(Some(&base), Some(&ours), Some(&theirs)).unwrap().unwrap();
        assert_eq!(merged, json!({"a": 2, "b": "new", "list": [1, 2, 3, 4], "keep": true}));
    }

    #[test]
    fn json_merge_scalar_conflict() {
        assert!(merge_values(Some(&json!(1)), Some(&json!(2)), Some(&json!(3))).is_err());
    }
}
