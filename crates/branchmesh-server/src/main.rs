mod api;
mod seed;
mod ui;

use branchmesh::{MeshConfig, Runtime};
use clap::{Parser, Subcommand};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "branchmesh", version, about = "BranchMesh durable multi-agent runtime + web UI")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the API server and web UI (default).
    Serve(ServeArgs),
    /// Probe a running server's health endpoint (used by the Docker HEALTHCHECK).
    Healthcheck {
        #[arg(long, env = "BRANCHMESH_HEALTH_URL", default_value = "http://127.0.0.1:8080/api/health")]
        url: String,
    },
}

#[derive(Parser, Clone)]
struct ServeArgs {
    /// Database file.
    #[arg(long, env = "BRANCHMESH_DATA", default_value = "./data/branchmesh.db")]
    data: PathBuf,
    /// Listen address.
    #[arg(long, env = "BRANCHMESH_BIND", default_value = "0.0.0.0:8080")]
    bind: SocketAddr,
    /// Bearer token required for /api (unset = no auth).
    #[arg(long, env = "BRANCHMESH_API_TOKEN")]
    api_token: Option<String>,
    /// Seconds an idle actor stays in RAM before hibernating.
    #[arg(long, env = "BRANCHMESH_IDLE_TIMEOUT", default_value_t = 30)]
    idle_timeout: u64,
    /// Attempts before a message is dead-lettered.
    #[arg(long, env = "BRANCHMESH_MAX_ATTEMPTS", default_value_t = 3)]
    max_attempts: u32,
    /// Don't create demo personas on an empty database.
    #[arg(long, env = "BRANCHMESH_NO_SEED", default_value_t = false)]
    no_seed: bool,
}

impl Default for ServeArgs {
    fn default() -> Self {
        ServeArgs::parse_from(["branchmesh"])
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,tower_http=warn".into()),
        )
        .init();

    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Serve(ServeArgs::default())) {
        Command::Healthcheck { url } => {
            let resp = reqwest::Client::builder()
                .timeout(Duration::from_secs(3))
                .build()?
                .get(&url)
                .send()
                .await?;
            if resp.status().is_success() {
                println!("ok");
                Ok(())
            } else {
                anyhow::bail!("unhealthy: {}", resp.status())
            }
        }
        Command::Serve(args) => serve(args).await,
    }
}

async fn serve(args: ServeArgs) -> anyhow::Result<()> {
    let config = MeshConfig {
        idle_timeout: Duration::from_secs(args.idle_timeout),
        max_attempts: args.max_attempts,
        ..MeshConfig::default()
    };
    let rt = Runtime::builder(&args.data).with_config(config).open().await?;
    tracing::info!(data = %args.data.display(), provider = rt.mesh().llm().name(), "runtime opened");
    if !args.no_seed {
        seed::seed_if_empty(&rt)?;
    }

    let app = api::router(api::AppState::new(rt, args.api_token.clone())).fallback(ui::static_handler);
    let listener = tokio::net::TcpListener::bind(args.bind).await?;
    tracing::info!("BranchMesh listening on http://{}", args.bind);
    if args.api_token.is_none() {
        tracing::warn!("BRANCHMESH_API_TOKEN not set: the API is unauthenticated");
    }
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let ctrl_c = tokio::signal::ctrl_c();
            #[cfg(unix)]
            {
                let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
                tokio::select! { _ = ctrl_c => {}, _ = term.recv() => {} }
            }
            #[cfg(not(unix))]
            let _ = ctrl_c.await;
            tracing::info!("shutting down");
        })
        .await?;
    Ok(())
}
