//runner.rs — drive Terraform as a child process, stream its JSONL log to disk, and classify the
//outcome. We spawn `terraform` directly (no Go, no terraform-exec), read its REAL ExitStatus (never
//pipe the child into a SIGPIPE-141), and do one stable string match to split a state-lock from broken HCL.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::Context;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

//Terraform's release-stable state-lock summary, identical in the plaintext and -json forms. Classified
//separately from broken HCL because it is transient — the caller retries with backoff, no task emitted.
const STATE_LOCK_SUMMARY: &str = "Error acquiring the state lock";

//Terraform's terminating error diagnostic is always among its final lines, so the tail is both what the
//emitter embeds and what classification scans.
const TAIL_LINES: usize = 50;

const STDERR_CAP_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    ApplySuccess,
    PlanNoChanges,
    PlanChanges,
    StateLock,
    ConfigError,
}

pub struct RunResult {
    pub outcome: Outcome,
    pub exit_code: i32,
    pub log_path: PathBuf,
    pub log_tail: String,
}

//real_dir is already canonicalized by the allowlist. Uses -chdir so the runner's own cwd never changes.
pub async fn run(
    real_dir: &Path,
    apply: bool,
    run_id: &str,
    runs_dir: &Path,
) -> anyhow::Result<RunResult> {
    let real_dir_str = real_dir
        .to_str()
        .with_context(|| format!("real_dir is not valid UTF-8: {}", real_dir.display()))?;

    let mut cmd = Command::new("terraform");
    cmd.arg(format!("-chdir={real_dir_str}"));
    if apply {
        //apply has no -detailed-exitcode; success is 0, failure non-zero.
        cmd.args(["apply", "-input=false", "-no-color", "-json", "-auto-approve"]);
    } else {
        //-detailed-exitcode is mandatory on plan: it splits 0 (no changes) from 2 (changes).
        cmd.args(["plan", "-input=false", "-no-color", "-json", "-detailed-exitcode"]);
    }

    //Hardened child env: empty CLI config (no host ~/.terraformrc dev_overrides leak in), TF_LOG removed
    //(provider debug logs can carry secrets), TF_IN_AUTOMATION set (silences interactive next-step hints).
    cmd.env("TF_CLI_CONFIG_FILE", "/etc/tf-apply/empty.tfrc");
    cmd.env_remove("TF_LOG");
    cmd.env("TF_IN_AUTOMATION", "1");

    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.kill_on_drop(true);

    tokio::fs::create_dir_all(runs_dir)
        .await
        .with_context(|| format!("creating runs dir {}", runs_dir.display()))?;
    let log_path = runs_dir.join(format!("{run_id}.jsonl"));

    let mut child = cmd.spawn().context("spawning `terraform` (is it on PATH?)")?;
    let stdout = child.stdout.take().context("capturing child stdout")?;
    let stderr = child.stderr.take().context("capturing child stderr")?;

    //Stream stdout to disk line-by-line — never buffer the whole log; keep only a bounded tail ring.
    let log_path_task = log_path.clone();
    let stdout_task = tokio::spawn(async move {
        let mut file = tokio::fs::File::create(&log_path_task)
            .await
            .with_context(|| format!("creating log {}", log_path_task.display()))?;
        let mut ring: VecDeque<String> = VecDeque::with_capacity(TAIL_LINES + 1);
        let mut reader = BufReader::new(stdout).lines();
        while let Some(line) = reader.next_line().await.context("reading terraform stdout")? {
            file.write_all(line.as_bytes()).await?;
            file.write_all(b"\n").await?;
            if ring.len() == TAIL_LINES {
                ring.pop_front();
            }
            ring.push_back(line);
        }
        file.flush().await?;
        anyhow::Ok(ring.into_iter().collect::<Vec<String>>())
    });

    //Bounded stderr for the non-JSON lock fallback.
    let stderr_task = tokio::spawn(async move {
        let mut reader = BufReader::new(stderr).lines();
        let mut collected = String::new();
        while let Some(line) = reader.next_line().await.context("reading terraform stderr")? {
            if collected.len() < STDERR_CAP_BYTES {
                collected.push_str(&line);
                collected.push('\n');
            }
        }
        anyhow::Ok(collected)
    });

    //Read the child's REAL exit status; both drains run concurrently so pipe buffers never back-pressure
    //the child into a hang.
    let status = child.wait().await.context("waiting on terraform")?;
    let tail = stdout_task.await.context("stdout streamer task panicked")??;
    let stderr_text = stderr_task.await.context("stderr collector task panicked")??;

    let exit_code = status.code().unwrap_or(-1);
    let outcome = classify(exit_code, apply, &tail, &stderr_text);

    //Prefer the stdout tail; fall back to stderr if terraform errored before emitting any -json.
    let log_tail = if tail.is_empty() && !stderr_text.trim().is_empty() {
        tail_of(&stderr_text, TAIL_LINES)
    } else {
        tail.join("\n")
    };

    Ok(RunResult {
        outcome,
        exit_code,
        log_path,
        log_tail,
    })
}

//Pure classifier — split out so it is unit-testable without spawning terraform.
fn classify(exit_code: i32, apply: bool, stdout_lines: &[String], stderr: &str) -> Outcome {
    match exit_code {
        0 => {
            if apply {
                Outcome::ApplySuccess
            } else {
                Outcome::PlanNoChanges
            }
        }
        2 => Outcome::PlanChanges,
        _ => {
            if is_state_lock(stdout_lines, stderr) {
                Outcome::StateLock
            } else {
                Outcome::ConfigError
            }
        }
    }
}

//A held state lock and a broken config both present as exit code 1, so match the stable summary: primary
//is a -json diagnostic on stdout, fallback is a plain substring on stderr (errored before -json engaged).
fn is_state_lock(stdout_lines: &[String], stderr: &str) -> bool {
    stdout_lines
        .iter()
        .any(|line| line_is_state_lock_diagnostic(line))
        || stderr.contains(STATE_LOCK_SUMMARY)
}

//True only for an error-severity diagnostic whose summary is the lock string — a warning-severity
//dev-override diagnostic carrying that summary must never be read as the lock.
fn line_is_state_lock_diagnostic(line: &str) -> bool {
    let line = line.trim();
    if line.is_empty() {
        return false;
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return false;
    };
    if v.get("type").and_then(serde_json::Value::as_str) != Some("diagnostic") {
        return false;
    }
    let Some(diag) = v.get("diagnostic") else {
        return false;
    };
    let severity = diag.get("severity").and_then(serde_json::Value::as_str);
    let summary = diag.get("summary").and_then(serde_json::Value::as_str);
    severity == Some("error") && summary == Some(STATE_LOCK_SUMMARY)
}

fn tail_of(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCK_LINE: &str = r#"{"@level":"error","@message":"Error: Error acquiring the state lock","diagnostic":{"severity":"error","summary":"Error acquiring the state lock","detail":"Error message: resource temporarily unavailable\nLock Info:\n  ID: 2d6052fb-c19c-0b90-1386-1e473b018ce7"},"type":"diagnostic"}"#;

    const CONFIG_ERR_LINE: &str = r#"{"@level":"error","@message":"Error: No configuration files","diagnostic":{"severity":"error","summary":"No configuration files","detail":"Plan requires configuration to be present."},"type":"diagnostic"}"#;

    #[test]
    fn lock_diagnostic_classifies_as_state_lock() {
        let lines = vec![LOCK_LINE.to_string()];
        assert_eq!(classify(1, false, &lines, ""), Outcome::StateLock);
    }

    #[test]
    fn no_configuration_files_classifies_as_config_error() {
        let lines = vec![CONFIG_ERR_LINE.to_string()];
        assert_eq!(classify(1, false, &lines, ""), Outcome::ConfigError);
    }

    #[test]
    fn non_json_stderr_lock_fallback() {
        assert_eq!(
            classify(1, false, &[], "Error: Error acquiring the state lock\n"),
            Outcome::StateLock
        );
    }

    #[test]
    fn exit_codes_map_without_diagnostics() {
        assert_eq!(classify(0, true, &[], ""), Outcome::ApplySuccess);
        assert_eq!(classify(0, false, &[], ""), Outcome::PlanNoChanges);
        assert_eq!(classify(2, false, &[], ""), Outcome::PlanChanges);
        assert_eq!(classify(1, false, &[], ""), Outcome::ConfigError);
    }

    #[test]
    fn warning_severity_is_not_a_lock() {
        let warn = r#"{"@level":"warning","diagnostic":{"severity":"warning","summary":"Error acquiring the state lock"},"type":"diagnostic"}"#;
        assert!(!line_is_state_lock_diagnostic(warn));
        assert_eq!(classify(1, false, &[warn.to_string()], ""), Outcome::ConfigError);
    }
}
