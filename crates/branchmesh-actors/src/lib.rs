//! # branchmesh-actors
//!
//! Agent personalities as virtual actors.
//!
//! * An **actor** is a `(persona, branch)` pair. It is activated (a tokio task
//!   with a mailbox) when a message arrives and hibernates after an idle
//!   timeout. All of its state lives on its branch in the DAG store, so
//!   hibernation costs nothing and activation is just spawning a task.
//! * **Mailboxes are durable**: envelopes are persisted before dispatch and
//!   re-dispatched on restart. Each envelope maps to one durable execution
//!   whose journal makes retries replay instead of re-calling the LLM.
//! * Agents talk to each other with the `ask_agent` tool: the caller's
//!   branch is forked, the callee works on the fork, and the fork is merged
//!   back when it replies.

pub mod agent;
pub mod anthropic;
pub mod context;
pub mod llm;
pub mod mesh;
pub mod mock;
pub mod persona;
pub mod resolver;
pub mod speech;
pub mod tools;

pub use anthropic::{AnthropicConfig, AnthropicProvider, Flavor};
pub use context::{AuthorFrame, ContextStyle};
pub use llm::{provider_from_env, LlmProvider, LlmRequest, LlmResponse};
pub use mesh::{ActorInfo, Envelope, EnvelopeStatus, Mesh, MeshConfig, Reply};
pub use mock::MockProvider;
pub use persona::AgentPersona;
pub use resolver::LlmConflictResolver;
pub use speech::{Speech, SpeechToText, Synthesized, TextToSpeech, Transcript, VoiceConfig};
pub use tools::{
    ask_agent_tool, code_interpreter_tool, current_time_tool, http_fetch_tool, memory_read_tool,
    memory_write_tool, web_search_tool, FnTool, Tool, ToolContext, ToolRef,
};

pub use branchmesh_core::{Error, Result};

/// Default model for new personas.
pub const DEFAULT_MODEL: &str = "claude-opus-5-5";
