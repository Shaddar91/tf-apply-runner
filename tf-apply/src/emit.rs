//emit.rs — on a config-error or stale-lock outcome, write ONE terraform-expert task into the pipeline's
//_unrouted/ dir. Terraform output is never returned to the caller; the full log stays on disk and only a
//secret-scrubbed 50-line tail is embedded. scrub() runs before anything reaches disk.

use std::path::{Path, PathBuf};

use anyhow::Context;
use regex::Regex;

pub struct FailureReport<'a> {
    pub run_id: &'a str,
    pub real_dir: &'a Path,
    pub apply: bool,
    pub exit_code: i32,
    //Rendered verbatim from runner::RunResult — the emitter hardcodes no host path of its own.
    pub log_path: &'a Path,
    //Raw (unscrubbed) tail; scrub() is applied inside render.
    pub log_tail: &'a str,
}

//Redact anything AWS-credential-shaped from a log tail before it is embedded: secret_access_key
//assignments, AKIA access-key IDs, and any hex run >= 40. Over-redacts on purpose — a false positive
//costs the fixing agent context; a false negative leaks a secret into a git-adjacent queue file.
fn scrub(tail: &str) -> String {
    //Compiled per call — the emitter is on the cold error path, not a hot loop.
    let secret_kv = Regex::new(r"(?i)aws_secret_access_key\s*=\s*\S+").unwrap();
    let access_key = Regex::new(r"AKIA[0-9A-Z]{16}").unwrap();
    let long_hex = Regex::new(r"(?i)\b[0-9a-f]{40,}\b").unwrap();

    let out = secret_kv.replace_all(tail, "aws_secret_access_key = [REDACTED]");
    let out = access_key.replace_all(&out, "[REDACTED_AWS_ACCESS_KEY_ID]");
    let out = long_hex.replace_all(&out, "[REDACTED_HEX]");
    out.into_owned()
}

//Reconstruct the terraform invocation runner::run spawned; carries no credentials (the args never held any).
fn failing_command(real_dir: &Path, apply: bool) -> String {
    let dir = real_dir.display();
    if apply {
        format!("terraform -chdir={dir} apply -input=false -no-color -json -auto-approve")
    } else {
        format!("terraform -chdir={dir} plan -input=false -no-color -json -detailed-exitcode")
    }
}

//Pure/deterministic given `created` (the caller stamps the timestamp) so it renders without a clock or filesystem.
fn render_task(created: &str, report: &FailureReport<'_>) -> String {
    let real_dir = report.real_dir.display();
    let run_id = report.run_id;
    let exit_code = report.exit_code;
    let log_path = report.log_path.display();
    let failing_command = failing_command(report.real_dir, report.apply);
    let scrubbed_tail = scrub(report.log_tail);

    format!(
        r#"# Task: Fix failing Terraform config at {real_dir}

**Task ID:** task_tf_apply_fail_{run_id}
**Created:** {created}
**Priority:** High
**Type:** code
**Target Agent:** terraform-expert
**Agent Available:** Yes
**Ready Status:** READY_FOR_EXECUTION

## Summary
A tf-apply-runner run failed with a non-lock Terraform error. Fix the HCL so `terraform plan` succeeds.

## Context
- Directory: {real_dir}
- Failing command: {failing_command}
- Exit code: {exit_code}
- Run log: {log_path}
- Log tail (scrubbed):
```
{scrubbed_tail}
```

## Action Items
- [ ] Read the run log tail above and open {real_dir}
- [ ] Fix the Terraform configuration
- [ ] Re-verify locally: `terraform -chdir={real_dir} plan -input=false -no-color`

## Success Criteria
- `terraform -chdir={real_dir} plan -input=false -no-color` exits 0 or 2 (no error diagnostic)
"#
    )
}

//Local time as RFC 3339 (an ISO-8601 profile) with offset, seconds precision.
fn now_local_iso8601() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
}

//unrouted_dir is injected by the caller (no hardcoded host path); any write error propagates (fail-closed).
pub fn emit_config_error(unrouted_dir: &Path, report: &FailureReport<'_>) -> anyhow::Result<PathBuf> {
    let created = now_local_iso8601();
    let body = render_task(&created, report);
    let file = unrouted_dir.join(format!("task_tf_apply_fail_{}.md", report.run_id));
    std::fs::create_dir_all(unrouted_dir)
        .with_context(|| format!("creating unrouted dir {}", unrouted_dir.display()))?;
    std::fs::write(&file, body).with_context(|| format!("writing task file {}", file.display()))?;
    Ok(file)
}

//Distinct from a ConfigError: the HCL may be fine, the state is held by a stale lock — the fix is a
//human/agent force-unlock decision, NEVER an automatic one. Any lock field may be absent.
pub struct StaleLockReport<'a> {
    pub run_id: &'a str,
    pub real_dir: &'a Path,
    pub apply: bool,
    pub exit_code: i32,
    pub log_path: &'a Path,
    pub lock_id: Option<&'a str>,
    pub lock_who: Option<&'a str>,
    pub lock_created: Option<&'a str>,
    //Raw (unscrubbed) tail; scrub() is applied inside render.
    pub log_tail: &'a str,
}

//The force-unlock re-trigger for the fixing agent — the runner NEVER runs it itself.
fn force_unlock_command(real_dir: &Path, lock_id: Option<&str>) -> String {
    let dir = real_dir.display();
    match lock_id {
        Some(id) => format!("terraform -chdir={dir} force-unlock {id}"),
        None => format!("terraform -chdir={dir} force-unlock <LOCK_ID-from-run-log>"),
    }
}

//Pure/deterministic given `created`; carries the lock identity fields and the exact force-unlock re-trigger.
fn render_stale_lock_task(created: &str, report: &StaleLockReport<'_>) -> String {
    let real_dir = report.real_dir.display();
    let run_id = report.run_id;
    let exit_code = report.exit_code;
    let log_path = report.log_path.display();
    let failing_command = failing_command(report.real_dir, report.apply);
    let force_unlock = force_unlock_command(report.real_dir, report.lock_id);
    let lock_id = report.lock_id.unwrap_or("(not captured — read from run log)");
    let lock_who = report.lock_who.unwrap_or("(not captured — read from run log)");
    let lock_created = report.lock_created.unwrap_or("(not captured — read from run log)");
    let scrubbed_tail = scrub(report.log_tail);

    format!(
        r#"# Task: Resolve stale Terraform state lock at {real_dir}

**Task ID:** task_tf_apply_lock_{run_id}
**Created:** {created}
**Priority:** High
**Type:** maintenance
**Target Agent:** terraform-expert
**Agent Available:** Yes
**Ready Status:** READY_FOR_EXECUTION

## Summary
A tf-apply-runner run could not acquire the Terraform state lock and it did not clear after bounded
retries. The HCL may be fine — confirm no other run holds the lock, then force-unlock and re-trigger.

## Context
- Directory: {real_dir}
- Failing command: {failing_command}
- Exit code: {exit_code}
- Run log: {log_path}
- Lock ID: {lock_id}
- Held by (Who): {lock_who}
- Lock created: {lock_created}
- Log tail (scrubbed):
```
{scrubbed_tail}
```

## Action Items
- [ ] Confirm NO other terraform process legitimately holds this lock (check other agents/CI first)
- [ ] Only if the lock is confirmed stale, release it: `{force_unlock}`
- [ ] Re-verify locally: `terraform -chdir={real_dir} plan -input=false -no-color`

## Success Criteria
- `terraform -chdir={real_dir} plan -input=false -no-color` acquires the lock and exits 0 or 2
"#
    )
}

//Named task_tf_apply_lock_ so it never collides with the config-error task for the same run.
pub fn emit_stale_lock(
    unrouted_dir: &Path,
    report: &StaleLockReport<'_>,
) -> anyhow::Result<PathBuf> {
    let created = now_local_iso8601();
    let body = render_stale_lock_task(&created, report);
    let file = unrouted_dir.join(format!("task_tf_apply_lock_{}.md", report.run_id));
    std::fs::create_dir_all(unrouted_dir)
        .with_context(|| format!("creating unrouted dir {}", unrouted_dir.display()))?;
    std::fs::write(&file, body).with_context(|| format!("writing task file {}", file.display()))?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    //A tail carrying a leaked AWS access key ID (AKIA + 16) and a separate 64-char hex secret — assert
    //the RENDERED task holds neither literal.
    #[test]
    fn emit_render_redacts_akia_and_long_hex() {
        let access_key = "AKIAIOSFODNN7EXAMPLE";
        let hex64 = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
        assert_eq!(access_key.len(), 20);
        assert_eq!(hex64.len(), 64);

        let tail = format!(
            "{{\"@level\":\"error\",\"@message\":\"Error: bad provider config\"}}\n\
             provider \"aws\" access_key = {access_key}\n\
             aws_secret_access_key = {hex64}\n\
             session_token = {hex64}"
        );

        let report = FailureReport {
            run_id: "testrun123",
            real_dir: Path::new("/tmp/tf-scratch"),
            apply: false,
            exit_code: 1,
            log_path: Path::new("/tmp/tf-apply-test/runs/testrun123.jsonl"),
            log_tail: &tail,
        };

        let rendered = render_task("2026-08-15T22:00:00+02:00", &report);

        assert!(
            !rendered.contains(access_key),
            "AKIA access key leaked into the emitted task:\n{rendered}"
        );
        assert!(
            !rendered.contains(hex64),
            "64-char hex secret leaked into the emitted task:\n{rendered}"
        );
        assert!(rendered.contains("**Target Agent:** terraform-expert"));
        assert!(rendered.contains("task_tf_apply_fail_testrun123"));
        assert!(rendered.contains("- Exit code: 1"));
    }

    #[test]
    fn emit_config_error_writes_scrubbed_task_file() {
        let hex64 = "abad1deaabad1deaabad1deaabad1deaabad1deaabad1deaabad1deaabad1dea";
        assert_eq!(hex64.len(), 64);
        let tail = format!("aws_secret_access_key = {hex64}");

        let dir = std::env::temp_dir().join(format!("tf_apply_emit_{}", std::process::id()));
        let report = FailureReport {
            run_id: "run_e2e_001",
            real_dir: Path::new("/tmp/tf-scratch"),
            apply: false,
            exit_code: 1,
            log_path: Path::new("/tmp/runs/run_e2e_001.jsonl"),
            log_tail: &tail,
        };

        let path = emit_config_error(&dir, &report).expect("emit must write the task file");
        assert_eq!(path, dir.join("task_tf_apply_fail_run_e2e_001.md"));

        let written = std::fs::read_to_string(&path).expect("task file must be readable");
        assert!(!written.contains(hex64), "secret leaked to disk:\n{written}");
        assert!(written.contains("aws_secret_access_key = [REDACTED]"));
        assert!(written.contains("**Target Agent:** terraform-expert"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stale_lock_task_carries_id_and_force_unlock() {
        let hex64 = "feedfacefeedfacefeedfacefeedfacefeedfacefeedfacefeedfacefeedface";
        assert_eq!(hex64.len(), 64);
        let tail = format!("Lock Info:\n  ID: 2d6052fb-c19c-0b90-1386-1e473b018ce7\ntoken={hex64}");
        let report = StaleLockReport {
            run_id: "lockrun1",
            real_dir: Path::new("/tmp/tf-scratch"),
            apply: false,
            exit_code: 1,
            log_path: Path::new("/tmp/runs/lockrun1.jsonl"),
            lock_id: Some("2d6052fb-c19c-0b90-1386-1e473b018ce7"),
            lock_who: Some("user@host"),
            lock_created: Some("2026-08-15 20:00:00 +0000 UTC"),
            log_tail: &tail,
        };
        let rendered = render_stale_lock_task("2026-08-15T22:00:00+02:00", &report);
        assert!(rendered.contains(
            "terraform -chdir=/tmp/tf-scratch force-unlock 2d6052fb-c19c-0b90-1386-1e473b018ce7"
        ));
        assert!(rendered.contains("**Target Agent:** terraform-expert"));
        assert!(rendered.contains("task_tf_apply_lock_lockrun1"));
        assert!(rendered.contains("Held by (Who): user@host"));
        assert!(!rendered.contains(hex64), "secret leaked into stale-lock task:\n{rendered}");
    }

    #[test]
    fn stale_lock_task_without_id_uses_placeholder() {
        let report = StaleLockReport {
            run_id: "lockrun2",
            real_dir: Path::new("/tmp/tf-scratch"),
            apply: true,
            exit_code: 1,
            log_path: Path::new("/tmp/runs/lockrun2.jsonl"),
            lock_id: None,
            lock_who: None,
            lock_created: None,
            log_tail: "Error acquiring the state lock",
        };
        let rendered = render_stale_lock_task("2026-08-15T22:00:00+02:00", &report);
        assert!(rendered.contains("force-unlock <LOCK_ID-from-run-log>"));
        assert!(rendered.contains("read from run log"));
    }
}
