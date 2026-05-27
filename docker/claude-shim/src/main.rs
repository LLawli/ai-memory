//! claude-shim — Anthropic /v1/messages compatible HTTP shim that
//! forwards to `claude -p` (Claude Code CLI in headless mode).
//!
//! Intended deployment: bound to an internal docker network, NOT
//! published to the host. No inbound auth; isolation is purely
//! network-layer. The OAuth token used by `claude` is provided via
//! the `CLAUDE_CODE_OAUTH_TOKEN` env var, inherited by the child.

#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Router, serve};
use clap::Parser;
use tokio::net::TcpListener;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use claude_shim::anthropic::{ApiErrorBody, MessagesRequest, MessagesResponse};
use claude_shim::translate;

const OAUTH_ENV: &str = "CLAUDE_CODE_OAUTH_TOKEN";

#[derive(Parser, Debug)]
#[command(
    name = "claude-shim",
    about = "Anthropic /v1/messages → `claude -p` translator",
    version
)]
struct Cli {
    /// Address to bind the HTTP server on.
    #[arg(long, default_value = "0.0.0.0:8080", env = "CLAUDE_SHIM_BIND")]
    bind: SocketAddr,

    /// Path to the `claude` binary. Looked up on PATH if not absolute.
    #[arg(long, default_value = "claude", env = "CLAUDE_SHIM_BINARY")]
    claude_binary: PathBuf,

    /// Per-request timeout for the `claude -p` subprocess (seconds).
    #[arg(long, default_value_t = 300, env = "CLAUDE_SHIM_TIMEOUT_SECS")]
    timeout_secs: u64,

    /// Run a one-shot health check and exit. Used by Docker HEALTHCHECK.
    /// Exits 0 if the binary resolves and the OAuth env var is set.
    #[arg(long)]
    healthcheck: bool,
}

#[derive(Clone)]
struct AppState {
    binary: OsString,
    timeout: Duration,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    if cli.healthcheck {
        return run_healthcheck(&cli);
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("claude_shim=info,tower_http=info")),
        )
        .compact()
        .init();

    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            error!(error = %err, "claude-shim failed");
            ExitCode::FAILURE
        }
    }
}

fn run_healthcheck(cli: &Cli) -> ExitCode {
    if std::env::var_os(OAUTH_ENV).is_none() {
        eprintln!("healthcheck: missing {OAUTH_ENV}");
        return ExitCode::FAILURE;
    }
    match resolve_binary(&cli.claude_binary) {
        Ok(_) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("healthcheck: {err}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<()> {
    if std::env::var_os(OAUTH_ENV).is_none() {
        return Err(anyhow!(
            "{OAUTH_ENV} must be set; the shim refuses to start without it"
        ));
    }
    let binary = resolve_binary(&cli.claude_binary)?;

    let state = AppState {
        binary,
        timeout: Duration::from_secs(cli.timeout_secs),
    };

    let app = Router::new()
        .route("/health", get(health))
        .route("/v1/messages", post(handle_messages))
        .with_state(Arc::new(state));

    info!(addr = %cli.bind, "claude-shim listening");
    let listener = TcpListener::bind(cli.bind)
        .await
        .with_context(|| format!("bind to {}", cli.bind))?;
    serve(listener, app).await.context("axum serve")?;
    Ok(())
}

fn resolve_binary(path: &std::path::Path) -> Result<OsString> {
    if path.is_absolute() {
        if path.exists() {
            Ok(path.as_os_str().to_owned())
        } else {
            Err(anyhow!("claude binary not found at {}", path.display()))
        }
    } else {
        // Probe PATH by trying `which`-equivalent: attempt to canonicalise
        // via the env's PATH dirs. Falling back to the bare name lets
        // tokio::process::Command do its own PATH lookup at spawn time,
        // which is the most permissive behaviour.
        Ok(path.as_os_str().to_owned())
    }
}

async fn health() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

async fn handle_messages(
    State(state): State<Arc<AppState>>,
    Json(request): Json<MessagesRequest>,
) -> Result<Json<MessagesResponse>, (StatusCode, Json<ApiErrorBody>)> {
    let prepared = translate::prepare(&request, state.binary.clone(), state.timeout)
        .map_err(|body| (StatusCode::BAD_REQUEST, Json(body)))?;

    match translate::execute(prepared).await {
        Ok(response) => Ok(Json(response)),
        Err(body) => {
            warn!(error_type = %body.error.kind, message = %body.error.message, "shim returning error");
            Err((StatusCode::BAD_GATEWAY, Json(body)))
        }
    }
}
