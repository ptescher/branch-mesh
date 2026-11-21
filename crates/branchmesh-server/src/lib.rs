//! BranchMesh HTTP API server with the embedded operator web UI.
//!
//! Usable as a library so applications can serve the operator API next to
//! their own routes and UI:
//!
//! ```ignore
//! let rt = branchmesh_server::open_runtime(&args).await?;
//! let state = branchmesh_server::AppState::new(rt.clone(), args.api_token.clone());
//! let app = branchmesh_server::router(state)
//!     .merge(my_routes(rt))
//!     .fallback(branchmesh_server::ui::static_handler);
//! branchmesh_server::serve(app, args.bind).await?;
//! ```

pub mod api;
pub mod seed;
pub mod ui;

pub use api::{bad, router, ApiError, ApiResult, AppState, TokenAuthority, TokenCheck};

use branchmesh::{MeshConfig, Runtime};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

/// Common server flags (also read from the environment).
#[derive(clap::Args, Clone, Debug)]
pub struct ServeArgs {
    /// Database file.
    #[arg(long, env = "BRANCHMESH_DATA", default_value = "./data/branchmesh.db")]
    pub data: PathBuf,
    /// Listen address.
    #[arg(long, env = "BRANCHMESH_BIND", default_value = "0.0.0.0:8080")]
    pub bind: SocketAddr,
    /// Bearer token required for the operator API (unset = no auth).
    #[arg(long, env = "BRANCHMESH_API_TOKEN")]
    pub api_token: Option<String>,
    /// Seconds an idle actor stays in RAM before hibernating.
    #[arg(long, env = "BRANCHMESH_IDLE_TIMEOUT", default_value_t = 30)]
    pub idle_timeout: u64,
    /// Attempts before a message is dead-lettered.
    #[arg(long, env = "BRANCHMESH_MAX_ATTEMPTS", default_value_t = 3)]
    pub max_attempts: u32,
    /// Don't create demo personas on an empty database.
    #[arg(long, env = "BRANCHMESH_NO_SEED", default_value_t = false, num_args = 0..=1,
          default_missing_value = "true", value_parser = clap::builder::BoolishValueParser::new())]
    pub no_seed: bool,
}

/// Log to stderr, `RUST_LOG` filter (default `info,tower_http=warn`).
pub fn init_tracing() {
    tracing_subscriber::fmt()
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,tower_http=warn".into()),
        )
        .init();
}

/// Open the runtime described by `args` (and seed demo personas unless disabled).
pub async fn open_runtime(args: &ServeArgs) -> anyhow::Result<Runtime> {
    let config = MeshConfig {
        idle_timeout: Duration::from_secs(args.idle_timeout),
        max_attempts: args.max_attempts,
        ..MeshConfig::default()
    };
    let rt = Runtime::builder(&args.data).with_config(config).open().await?;
    tracing::info!(
        data = %args.data.display(),
        llm = rt.mesh().llm().name(),
        stt = rt.speech().stt.name(),
        tts = rt.speech().tts.name(),
        "runtime opened"
    );
    if !args.no_seed {
        seed::seed_if_empty(&rt)?;
    }
    Ok(rt)
}

/// Serve `app` on `bind` until Ctrl-C / SIGTERM.
pub async fn serve(app: axum::Router, bind: SocketAddr) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!("listening on http://{bind}");
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

/// Probe a running server's health endpoint (used by Docker HEALTHCHECKs).
pub async fn healthcheck(url: &str) -> anyhow::Result<()> {
    let resp = reqwest::Client::builder().timeout(Duration::from_secs(3)).build()?.get(url).send().await?;
    if resp.status().is_success() {
        println!("ok");
        Ok(())
    } else {
        anyhow::bail!("unhealthy: {}", resp.status())
    }
}
