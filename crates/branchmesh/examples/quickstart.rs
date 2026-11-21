//! Mirrors the README example. Runs offline with the mock LLM unless
//! ANTHROPIC_API_KEY is set.
//!
//!     cargo run -p branchmesh --example quickstart

use branchmesh::{memory_read_tool, memory_write_tool, AgentPersona, MergeStrategy, Runtime};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    let dir = std::env::temp_dir().join("branchmesh-quickstart");
    let _ = std::fs::remove_dir_all(&dir);

    // 1. Initialize the embedded durable engine
    let runtime = Runtime::open(dir.join("data_store.db")).await?;

    // 2. Define agent personalities
    let assistant = AgentPersona::new("research-bot")
        .with_system_prompt("You are a thorough research analyst. Keep answers brief.")
        .with_toolset(vec![memory_write_tool(), memory_read_tool()]);
    let checker = AgentPersona::new("citation-checker")
        .with_system_prompt("You verify citations rigorously. Record verified citations in memory.")
        .with_toolset(vec![memory_write_tool()]);

    // 3. Start a user session branch
    let thread = runtime.spawn_thread(&assistant, "user_session_123").await?;
    let reply = thread.send_message("remember the paper is Shapiro et al. 2011 on CRDTs").await?;
    println!("research-bot: {}", reply.text);

    // 4. Fork state for a peer consultation
    let sub_branch = thread.fork("peer_consultation_branch").await?.with_persona(&checker)?;
    let reply = sub_branch.send_message("remember citation verified: Shapiro 2011, INRIA RR-7506").await?;
    println!("citation-checker: {}", reply.text);

    // 5. Merge results back into main thread
    let outcome = thread.merge(sub_branch, MergeStrategy::SemanticLLM).await?;
    println!("merged {} change(s) into {} at {}", outcome.changes.len(), thread.branch(), outcome.commit.short());
    println!("memory after merge: {}", serde_json::to_string_pretty(&thread.memory()?)?);
    Ok(())
}
