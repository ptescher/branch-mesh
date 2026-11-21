use branchmesh_server::{api, ui, ServeArgs};
use clap::{Parser, Subcommand};

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

#[derive(Parser)]
struct ServeOnly {
    #[command(flatten)]
    args: ServeArgs,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    branchmesh_server::init_tracing();
    let cli = Cli::parse();
    match cli.command.unwrap_or_else(|| Command::Serve(ServeOnly::parse_from(["branchmesh"]).args)) {
        Command::Healthcheck { url } => branchmesh_server::healthcheck(&url).await,
        Command::Serve(args) => {
            let rt = branchmesh_server::open_runtime(&args).await?;
            if args.api_token.is_none() {
                tracing::warn!("BRANCHMESH_API_TOKEN not set: the API is unauthenticated");
            }
            let app = api::router(api::AppState::new(rt, args.api_token.clone())).fallback(ui::static_handler);
            branchmesh_server::serve(app, args.bind).await
        }
    }
}
