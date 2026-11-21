//! How stored records are framed when a branch is rendered as an LLM
//! conversation. Applications built on BranchMesh customize this per mesh
//! (e.g. who counts as the end user, how relayed messages are introduced,
//! how standing facts are headed) without the core knowing their domain.

use serde_json::Value;

/// How messages from one class of author are shown to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthorFrame {
    /// Treated like the end user: shown verbatim when addressed to the
    /// persona, `[user → other]` otherwise.
    Direct,
    /// Shown as `[<label>]\n<text>`.
    Labeled(String),
}

/// Rendering rules for a mesh. Cheap to clone.
#[derive(Debug, Clone)]
pub struct ContextStyle {
    /// Author prefix -> framing, matched in order (`author.starts_with(prefix)`).
    /// Unmatched authors are shown as `[message from <author>]`. The author
    /// `"user"` is always [`AuthorFrame::Direct`].
    pub authors: Vec<(String, AuthorFrame)>,
    /// Heading for the standing-facts snapshot (see [`ContextStyle::facts_prefix`]).
    pub facts_heading: String,
    /// Shown when all standing facts were removed.
    pub facts_cleared: String,
    /// Branch path prefix holding standing facts (`<prefix>/<key>` -> `{text}`).
    pub facts_prefix: String,
}

impl Default for ContextStyle {
    fn default() -> Self {
        ContextStyle {
            authors: vec![],
            facts_heading: "Current standing facts (authoritative; they replace any earlier version):".into(),
            facts_cleared: "All previously shared standing facts have been cleared.".into(),
            facts_prefix: FACTS_PREFIX.into(),
        }
    }
}

/// Default branch path prefix for standing facts.
pub const FACTS_PREFIX: &str = "memory/facts";

impl ContextStyle {
    /// Add a framing rule for authors starting with `prefix`.
    pub fn with_author(mut self, prefix: impl Into<String>, frame: AuthorFrame) -> Self {
        self.authors.push((prefix.into(), frame));
        self
    }

    pub fn with_facts(mut self, prefix: impl Into<String>, heading: impl Into<String>, cleared: impl Into<String>) -> Self {
        self.facts_prefix = prefix.into();
        self.facts_heading = heading.into();
        self.facts_cleared = cleared.into();
        self
    }

    pub fn frame_for(&self, author: &str) -> Option<&AuthorFrame> {
        if author == "user" {
            return Some(&AuthorFrame::Direct);
        }
        self.authors.iter().find(|(p, _)| author.starts_with(p.as_str())).map(|(_, f)| f)
    }

    /// Text of a stored `role: "system"` record: an optional `preamble`
    /// field (written by the application) followed by the content.
    pub fn system_text(&self, record: &Value, content: String) -> String {
        match record["preamble"].as_str().filter(|p| !p.is_empty()) {
            Some(p) => format!("{p}\n{content}"),
            None => content,
        }
    }
}
