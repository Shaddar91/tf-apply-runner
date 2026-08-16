//tf-apply — the long-lived HTTP executor. Reads config from the environment, canonicalizes the
//allowlist roots once at startup, reconciles any run left mid-flight by a prior process, and serves
//three routes on loopback ONLY (127.0.0.1). It owns the run registry, locks, and AWS creds — none reach a caller.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

mod runner;
mod emit;
mod auth;
mod allowlist;
mod locks;
mod routes;

const DEFAULT_PORT: u16 = 1937;
const DEFAULT_MAX_CONCURRENT_RUNS: usize = 4;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    //Logs to stderr at INFO. The token value is never logged anywhere.
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_writer(std::io::stderr)
        .init();

    let port: u16 = env_parse("TF_RUNNER_PORT", DEFAULT_PORT);
    let max_concurrent: usize = env_parse("TF_MAX_CONCURRENT_RUNS", DEFAULT_MAX_CONCURRENT_RUNS);
    let token = std::env::var("X_RUNNER_TOKEN").unwrap_or_default();
    let runs_dir = PathBuf::from(
        std::env::var("RUNS_DIR").unwrap_or_else(|_| "runs".to_string()),
    );
    let unrouted_dir = PathBuf::from(std::env::var("UNROUTED_DIR").unwrap_or_default());
    let roots = canonicalize_roots(&std::env::var("TF_ALLOWED_ROOTS").unwrap_or_default());

    //Fail-closed: the service still boots (so /health answers) but /apply rejects until these are set.
    if token.is_empty() {
        tracing::warn!("X_RUNNER_TOKEN is empty — every POST /apply will be rejected 401 (fail-closed)");
    }
    if roots.is_empty() {
        tracing::warn!("TF_ALLOWED_ROOTS resolved to zero roots — every POST /apply will be rejected 403 (fail-closed)");
    }
    if unrouted_dir.as_os_str().is_empty() {
        tracing::warn!("UNROUTED_DIR is unset — failure tasks cannot be emitted");
    }

    locks::reconcile_on_startup(&runs_dir);

    let state: routes::SharedState = Arc::new(routes::AppState {
        token,
        roots,
        runs_dir,
        unrouted_dir,
        locks: locks::LockRegistry::new(max_concurrent),
    });

    //Loopback ONLY — never exposed on a routable interface.
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(
        %addr,
        max_concurrent,
        "tf-apply serve listening (loopback only)"
    );
    axum::serve(listener, routes::router(state)).await?;
    Ok(())
}

fn env_parse<T: std::str::FromStr + std::fmt::Display>(key: &str, default: T) -> T {
    match std::env::var(key) {
        Ok(v) => match v.parse::<T>() {
            Ok(parsed) => parsed,
            Err(_) => {
                tracing::warn!(key, value = %v, %default, "unparsable env value — using default");
                default
            }
        },
        Err(_) => default,
    }
}

//Canonicalize each root here so every allowlist descendant test later compares canonical-vs-canonical;
//drop (with a log) any that do not exist.
fn canonicalize_roots(raw: &str) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for entry in raw.split(':').map(str::trim).filter(|s| !s.is_empty()) {
        match std::fs::canonicalize(entry) {
            Ok(path) => {
                tracing::info!(root = %path.display(), "allowed root");
                roots.push(path);
            }
            Err(e) => {
                tracing::warn!(entry, error = %e, "dropping non-existent allowed root");
            }
        }
    }
    roots
}
