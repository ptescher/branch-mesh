use branchmesh::{AgentPersona, Runtime};

/// Create demo personas and a welcome thread on a fresh database.
pub fn seed_if_empty(rt: &Runtime) -> branchmesh::Result<()> {
    if !rt.mesh().personas()?.is_empty() {
        return Ok(());
    }
    let personas = [
        AgentPersona::new("research-bot")
            .with_name("Research Bot")
            .with_description("Lead analyst. Delegates verification to fact-checker.")
            .with_system_prompt(
                "You are a thorough research analyst. Keep answers concise. Save durable findings \
                 with memory_write. When a claim needs independent verification, consult the \
                 'fact-checker' persona with ask_agent; for long material ask 'summarizer'.",
            )
            .with_tool_names(["memory_write", "memory_read", "ask_agent", "current_time", "web_search"]),
        AgentPersona::new("fact-checker")
            .with_name("Fact Checker")
            .with_description("Verifies claims and citations; records verdicts in memory.")
            .with_system_prompt(
                "You verify claims and citations rigorously. State what you could and could not \
                 confirm. Record each verdict with memory_write under 'verdicts/<topic>'.",
            )
            .with_tool_names(["memory_write", "memory_read", "http_fetch"]),
        AgentPersona::new("summarizer")
            .with_name("Summarizer")
            .with_description("Condenses long threads into crisp summaries.")
            .with_system_prompt("You condense conversations and documents into crisp bullet-point summaries.")
            .with_tool_names(["memory_read"])
            .with_effort("low"),
    ];
    for p in &personas {
        rt.register_persona(p)?;
    }
    rt.repo().get_or_create_branch(
        "threads/welcome",
        serde_json::json!({ "kind": "thread", "persona": "research-bot", "thread_id": "welcome" }),
    )?;
    tracing::info!("seeded demo personas: research-bot, fact-checker, summarizer");
    Ok(())
}
