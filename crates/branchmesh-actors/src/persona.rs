use crate::tools::ToolRef;
use serde::{Deserialize, Serialize};

fn default_model() -> String {
    std::env::var("BRANCHMESH_DEFAULT_MODEL").unwrap_or_else(|_| crate::DEFAULT_MODEL.to_string())
}
fn default_max_tokens() -> u32 {
    16000
}
fn default_max_steps() -> u32 {
    12
}

/// An agent personality: prompt, model settings and the names of the tools
/// it may use. Stored in the database so actors can be re-activated after a
/// restart; tool implementations live in the mesh's tool registry.
#[derive(Clone, Serialize, Deserialize)]
pub struct AgentPersona {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub system_prompt: String,
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    /// `low` | `medium` | `high` | `xhigh` | `max` (None = model default).
    #[serde(default)]
    pub effort: Option<String>,
    #[serde(default)]
    pub tools: Vec<String>,
    /// Upper bound on LLM round-trips per inbound message.
    #[serde(default = "default_max_steps")]
    pub max_steps: u32,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub updated_at: i64,
    /// Tool implementations to register alongside the persona.
    #[serde(skip)]
    pub tool_impls: Vec<ToolRef>,
}

impl std::fmt::Debug for AgentPersona {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentPersona")
            .field("id", &self.id)
            .field("model", &self.model)
            .field("tools", &self.tools)
            .finish()
    }
}

impl AgentPersona {
    pub fn new(id: impl Into<String>) -> Self {
        let id = id.into();
        AgentPersona {
            name: id.clone(),
            id,
            description: String::new(),
            system_prompt: String::new(),
            model: default_model(),
            max_tokens: default_max_tokens(),
            effort: None,
            tools: vec![],
            max_steps: default_max_steps(),
            created_at: 0,
            updated_at: 0,
            tool_impls: vec![],
        }
    }

    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    pub fn with_description(mut self, d: impl Into<String>) -> Self {
        self.description = d.into();
        self
    }

    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = prompt.into();
        self
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    pub fn with_effort(mut self, effort: impl Into<String>) -> Self {
        self.effort = Some(effort.into());
        self
    }

    pub fn with_max_tokens(mut self, n: u32) -> Self {
        self.max_tokens = n;
        self
    }

    pub fn with_max_steps(mut self, n: u32) -> Self {
        self.max_steps = n;
        self
    }

    /// Attach tool implementations; their names are recorded on the persona.
    pub fn with_toolset(mut self, tools: Vec<ToolRef>) -> Self {
        for t in tools {
            let name = t.name().to_string();
            if !self.tools.contains(&name) {
                self.tools.push(name);
            }
            self.tool_impls.push(t);
        }
        self
    }

    /// Reference already-registered tools by name.
    pub fn with_tool_names<I: IntoIterator<Item = S>, S: Into<String>>(mut self, names: I) -> Self {
        for n in names {
            let n = n.into();
            if !self.tools.contains(&n) {
                self.tools.push(n);
            }
        }
        self
    }
}
