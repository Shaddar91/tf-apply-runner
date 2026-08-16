//routes.rs — the HTTP surface and run lifecycle. Three routes; POST /apply runs a strict order:
//auth -> allowlist -> lock -> spawn. A successful trigger returns ONLY {"run_id"} — the terraform run
//is detached, its output never reaches the caller; failures fan out to on-disk status + a pipeline task.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{Path as AxumPath, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use regex::Regex;
use serde::Deserialize;
use serde_json::json;

use crate::locks::{self, Admission, LockRegistry, RunStatus};
use crate::runner::{self, Outcome};
use crate::{allowlist, auth, emit};

//Bounded-backoff for a transient state lock: this many RETRIES, delays doubling from the initial value,
//capped. A lock that outlasts all of these is handed to a terraform-expert task, never auto-force-unlocked.
const STATE_LOCK_MAX_RETRIES: u32 = 3;
const STATE_LOCK_INITIAL_BACKOFF: Duration = Duration::from_secs(2);
const STATE_LOCK_MAX_BACKOFF: Duration = Duration::from_secs(30);

pub struct AppState {
    //Never logged.
    pub token: String,
    //Canonicalized allowed roots (main.rs drops non-existent ones at startup). Empty => fail-closed.
    pub roots: Vec<PathBuf>,
    pub runs_dir: PathBuf,
    pub unrouted_dir: PathBuf,
    pub locks: Arc<LockRegistry>,
}

pub type SharedState = Arc<AppState>;

pub fn router(state: SharedState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/apply", post(apply))
        .route("/status/:run_id", get(status))
        .with_state(state)
}

//Liveness only — no auth (it exposes nothing) and no dependency on the allowlist or creds.
async fn health() -> Json<serde_json::Value> {
    Json(json!({ "status": "ok" }))
}

#[derive(Deserialize)]
struct ApplyRequest {
    dir: String,
    //Absent => plan-only, the safe default.
    #[serde(default)]
    apply: bool,
}

async fn apply(
    State(state): State<SharedState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    //Auth BEFORE we parse the body or touch the filesystem — an unauthenticated caller learns nothing.
    let presented = headers.get("x-runner-token").and_then(|v| v.to_str().ok());
    let authed = matches!(auth::check_token(&state.token, presented), auth::AuthOutcome::Ok);
    tracing::info!(token_present = presented.is_some(), authorized = authed, "POST /apply");
    if !authed {
        return (StatusCode::UNAUTHORIZED, Json(json!({ "error": "unauthorized" }))).into_response();
    }

    let req: ApplyRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("invalid JSON body: {e}") })),
            )
                .into_response();
        }
    };

    //The canonical path is used for BOTH the lock key and -chdir (the thing checked == the thing run).
    let real = match allowlist::validate(&req.dir, &state.roots) {
        Ok(real) => real,
        Err(err) => {
            let code = StatusCode::from_u16(err.status()).unwrap_or(StatusCode::FORBIDDEN);
            tracing::warn!(status = err.status(), reason = err.message(), "apply rejected by allowlist");
            return (code, Json(json!({ "error": err.message() }))).into_response();
        }
    };

    //Per-dir 409 first, then global 429.
    let run_id = uuid::Uuid::new_v4().to_string();
    let (run_id, guard) = match state.locks.admit(&real, &run_id) {
        Admission::Admitted { run_id, guard } => (run_id, guard),
        Admission::InProgress { run_id } => {
            return (
                StatusCode::CONFLICT,
                Json(json!({ "error": "run in progress", "run_id": run_id })),
            )
                .into_response();
        }
        Admission::AtCapacity => {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Json(json!({ "error": "at capacity, retry later" })),
            )
                .into_response();
        }
    };

    //Persist "running" before we return so a restart or a GET /status sees the run immediately.
    let started = now_iso();
    let running = RunStatus {
        run_id: run_id.clone(),
        dir: real.display().to_string(),
        apply: req.apply,
        state: "running".to_string(),
        exit_code: None,
        started: started.clone(),
        finished: None,
    };
    if let Err(e) = locks::write_status(&state.runs_dir, &running) {
        tracing::error!(run_id = %run_id, error = %e, "failed to persist running status");
    }

    //Detached: the guard is moved into the task and releases the per-dir lock + global permit when it
    //ends (normal or panic). The caller gets the run id right now, nothing else.
    let bg_state = Arc::clone(&state);
    let bg_real = real.clone();
    let bg_run_id = run_id.clone();
    let apply_flag = req.apply;
    tokio::spawn(async move {
        let _guard = guard;
        execute_run(bg_state, bg_real, apply_flag, bg_run_id, started).await;
    });

    (StatusCode::OK, Json(json!({ "run_id": run_id }))).into_response()
}

//GET /status/:run_id — the on-disk status record (never the log). run_id is validated so it cannot
//traverse out of runs_dir.
async fn status(State(state): State<SharedState>, AxumPath(run_id): AxumPath<String>) -> Response {
    if !locks::is_valid_run_id(&run_id) {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "invalid run_id" }))).into_response();
    }
    match locks::read_status(&state.runs_dir, &run_id) {
        Some(status) => (StatusCode::OK, Json(json!(status))).into_response(),
        None => (StatusCode::NOT_FOUND, Json(json!({ "error": "unknown run_id" }))).into_response(),
    }
}

//Drive terraform (retrying only a transient state lock), then record the terminal status and, on a
//broken config or an unresolved lock, emit a terraform-expert pipeline task.
async fn execute_run(state: SharedState, real: PathBuf, apply: bool, run_id: String, started: String) {
    let mut retries_left = STATE_LOCK_MAX_RETRIES;
    let mut backoff = STATE_LOCK_INITIAL_BACKOFF;

    let final_result = loop {
        match runner::run(&real, apply, &run_id, &state.runs_dir).await {
            Ok(result) => {
                if result.outcome == Outcome::StateLock && retries_left > 0 {
                    tracing::info!(
                        run_id = %run_id,
                        retries_left,
                        backoff_secs = backoff.as_secs(),
                        "state lock held — retrying with backoff"
                    );
                    tokio::time::sleep(backoff).await;
                    retries_left -= 1;
                    backoff = (backoff * 2).min(STATE_LOCK_MAX_BACKOFF);
                    continue;
                }
                break Ok(result);
            }
            Err(e) => break Err(e),
        }
    };

    match final_result {
        Ok(result) => finalize_outcome(&state, &real, apply, &run_id, &started, result).await,
        Err(e) => {
            //Driver fault (e.g. terraform not on PATH), not broken HCL — record it, emit NO task.
            tracing::error!(run_id = %run_id, error = %e, "terraform driver failed to run");
            finalize(&state, &run_id, &real, apply, "driver_error", None, &started);
        }
    }
}

//Map the final outcome to a terminal status, emitting a pipeline task on the two failure outcomes.
async fn finalize_outcome(
    state: &SharedState,
    real: &Path,
    apply: bool,
    run_id: &str,
    started: &str,
    result: runner::RunResult,
) {
    let state_str = match result.outcome {
        Outcome::ApplySuccess => "apply_success",
        Outcome::PlanNoChanges => "plan_no_changes",
        Outcome::PlanChanges => "plan_changes",
        Outcome::ConfigError => {
            let report = emit::FailureReport {
                run_id,
                real_dir: real,
                apply,
                exit_code: result.exit_code,
                log_path: &result.log_path,
                log_tail: &result.log_tail,
            };
            match emit::emit_config_error(&state.unrouted_dir, &report) {
                Ok(path) => tracing::warn!(run_id, task = %path.display(), "config error — emitted terraform-expert task"),
                Err(e) => tracing::error!(run_id, error = %e, "failed to emit config-error task"),
            }
            "config_error"
        }
        Outcome::StateLock => {
            //Retries exhausted — finalize_outcome only sees StateLock once the loop gives up.
            let info = parse_lock_info(&result.log_tail);
            let report = emit::StaleLockReport {
                run_id,
                real_dir: real,
                apply,
                exit_code: result.exit_code,
                log_path: &result.log_path,
                lock_id: info.id.as_deref(),
                lock_who: info.who.as_deref(),
                lock_created: info.created.as_deref(),
                log_tail: &result.log_tail,
            };
            match emit::emit_stale_lock(&state.unrouted_dir, &report) {
                Ok(path) => tracing::warn!(run_id, task = %path.display(), "state lock unresolved — emitted terraform-expert task"),
                Err(e) => tracing::error!(run_id, error = %e, "failed to emit stale-lock task"),
            }
            "state_lock_unresolved"
        }
    };
    finalize(state, run_id, real, apply, state_str, Some(result.exit_code), started);
}

//Write the terminal status file. Carries no log content and no credentials.
fn finalize(
    state: &SharedState,
    run_id: &str,
    real: &Path,
    apply: bool,
    state_str: &str,
    exit_code: Option<i32>,
    started: &str,
) {
    let status = RunStatus {
        run_id: run_id.to_string(),
        dir: real.display().to_string(),
        apply,
        state: state_str.to_string(),
        exit_code,
        started: started.to_string(),
        finished: Some(now_iso()),
    };
    if let Err(e) = locks::write_status(&state.runs_dir, &status) {
        tracing::error!(run_id, error = %e, "failed to persist terminal status");
    }
}

struct LockInfo {
    id: Option<String>,
    who: Option<String>,
    created: Option<String>,
}

//Pull ID/Who/Created out of a run-log tail: prefer the structured -json diagnostic `detail`, fall back
//to regexing the raw tail for the plaintext-error case. Any field may be absent.
fn parse_lock_info(log_tail: &str) -> LockInfo {
    for line in log_tail.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        let is_lock = v
            .get("diagnostic")
            .and_then(|d| d.get("summary"))
            .and_then(|s| s.as_str())
            == Some("Error acquiring the state lock");
        if is_lock {
            if let Some(detail) = v
                .get("diagnostic")
                .and_then(|d| d.get("detail"))
                .and_then(|s| s.as_str())
            {
                let info = extract_lock_fields(detail);
                if info.id.is_some() || info.who.is_some() || info.created.is_some() {
                    return info;
                }
            }
        }
    }
    extract_lock_fields(log_tail)
}

fn extract_lock_fields(text: &str) -> LockInfo {
    let first_capture = |pattern: &str| -> Option<String> {
        Regex::new(pattern)
            .ok()
            .and_then(|re| re.captures(text).map(|c| c[1].trim().to_string()))
            .filter(|s| !s.is_empty())
    };
    LockInfo {
        id: first_capture(r"(?m)^\s*ID:\s*(\S+)"),
        who: first_capture(r"(?m)^\s*Who:\s*(.+)$"),
        created: first_capture(r"(?m)^\s*Created:\s*(.+)$"),
    }
}

fn now_iso() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_lock_info_from_json_detail() {
        let detail = "Error message: resource temporarily unavailable\\nLock Info:\\n  ID:        2d6052fb-c19c-0b90-1386-1e473b018ce7\\n  Who:       user@host\\n  Created:   2026-08-15 20:00:00.1 +0000 UTC";
        let line = format!(
            r#"{{"@level":"error","diagnostic":{{"severity":"error","summary":"Error acquiring the state lock","detail":"{detail}"}},"type":"diagnostic"}}"#
        );
        let info = parse_lock_info(&line);
        assert_eq!(info.id.as_deref(), Some("2d6052fb-c19c-0b90-1386-1e473b018ce7"));
        assert_eq!(info.who.as_deref(), Some("user@host"));
        assert_eq!(info.created.as_deref(), Some("2026-08-15 20:00:00.1 +0000 UTC"));
    }

    #[test]
    fn parse_lock_info_from_plaintext_fallback() {
        let text = "Error: Error acquiring the state lock\nLock Info:\n  ID: abc-123\n  Who: ci@runner\n";
        let info = parse_lock_info(text);
        assert_eq!(info.id.as_deref(), Some("abc-123"));
        assert_eq!(info.who.as_deref(), Some("ci@runner"));
        assert!(info.created.is_none());
    }

    #[test]
    fn parse_lock_info_absent_is_all_none() {
        let info = parse_lock_info("nothing lock-like here");
        assert!(info.id.is_none() && info.who.is_none() && info.created.is_none());
    }
}
