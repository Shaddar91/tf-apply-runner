//locks.rs — in-memory ADMISSION gate (not Terraform's state lock): per-canonical-dir (dup => 409) and
//a global TF_MAX_CONCURRENT_RUNS cap (over => 429). The map is lost on restart and rebuilt from
//runs/*.status via reconcile_on_startup; the runner never auto-force-unlocks.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub struct RunHandle {
    pub run_id: String,
}

pub enum Admission {
    //The guard, moved into the run task, frees the per-dir lock AND the global permit on drop (incl. panic).
    Admitted { run_id: String, guard: RunGuard },
    InProgress { run_id: String },
    AtCapacity,
}

//RAII cleanup: making it a Drop impl means a panic in the run task cannot leak a permanently-locked dir.
pub struct RunGuard {
    registry: Arc<LockRegistry>,
    dir: PathBuf,
    _permit: OwnedSemaphorePermit,
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        self.registry.release(&self.dir);
    }
}

pub struct LockRegistry {
    //std Mutex — every critical section is synchronous (no await held across the lock).
    inflight: Mutex<HashMap<PathBuf, RunHandle>>,
    sem: Arc<Semaphore>,
}

impl LockRegistry {
    pub fn new(max_concurrent: usize) -> Arc<Self> {
        //Clamp to >= 1 so a misconfigured TF_MAX_CONCURRENT_RUNS=0 degrades to serial, not dead-at-429.
        let permits = max_concurrent.max(1);
        Arc::new(Self {
            inflight: Mutex::new(HashMap::new()),
            sem: Arc::new(Semaphore::new(permits)),
        })
    }

    //Per-dir 409 FIRST, then global 429 — both under one lock so two same-dir triggers cannot both slip through.
    pub fn admit(self: &Arc<Self>, real_dir: &Path, run_id: &str) -> Admission {
        let mut map = self.inflight.lock().expect("lock registry mutex poisoned");
        if let Some(handle) = map.get(real_dir) {
            return Admission::InProgress {
                run_id: handle.run_id.clone(),
            };
        }
        let permit = match Arc::clone(&self.sem).try_acquire_owned() {
            Ok(p) => p,
            Err(_) => return Admission::AtCapacity,
        };
        map.insert(
            real_dir.to_path_buf(),
            RunHandle {
                run_id: run_id.to_string(),
            },
        );
        Admission::Admitted {
            run_id: run_id.to_string(),
            guard: RunGuard {
                registry: Arc::clone(self),
                dir: real_dir.to_path_buf(),
                _permit: permit,
            },
        }
    }

    fn release(&self, real_dir: &Path) {
        self.inflight
            .lock()
            .expect("lock registry mutex poisoned")
            .remove(real_dir);
    }

    #[cfg(test)]
    pub fn inflight_len(&self) -> usize {
        self.inflight.lock().unwrap().len()
    }
}

//The durable, non-secret run record at runs/<run_id>.status — GET /status returns it verbatim. Carries
//NO log content and NO credentials.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunStatus {
    pub run_id: String,
    pub dir: String,
    pub apply: bool,
    //running | apply_success | plan_no_changes | plan_changes | config_error | state_lock_unresolved | driver_error | interrupted
    pub state: String,
    pub exit_code: Option<i32>,
    pub started: String,
    pub finished: Option<String>,
}

impl RunStatus {
    pub fn is_terminal(&self) -> bool {
        self.state != "running"
    }
}

//Constrain the filesystem-facing run_id to a UUID-safe token so it can never traverse out of runs_dir.
pub fn is_valid_run_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn status_path(runs_dir: &Path, run_id: &str) -> PathBuf {
    runs_dir.join(format!("{run_id}.status"))
}

//Write-then-rename for a single-writer-per-run model. run_id is validated by the caller, so the join is safe.
pub fn write_status(runs_dir: &Path, status: &RunStatus) -> anyhow::Result<()> {
    std::fs::create_dir_all(runs_dir)
        .with_context(|| format!("creating runs dir {}", runs_dir.display()))?;
    let final_path = status_path(runs_dir, &status.run_id);
    let tmp_path = runs_dir.join(format!("{}.status.tmp", status.run_id));
    let json = serde_json::to_string_pretty(status).context("serializing run status")?;
    std::fs::write(&tmp_path, json)
        .with_context(|| format!("writing {}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, &final_path)
        .with_context(|| format!("renaming into {}", final_path.display()))?;
    Ok(())
}

//None if absent/unreadable/corrupt. Callers must pre-validate run_id.
pub fn read_status(runs_dir: &Path, run_id: &str) -> Option<RunStatus> {
    let data = std::fs::read_to_string(status_path(runs_dir, run_id)).ok()?;
    serde_json::from_str(&data).ok()
}

//A status still "running" belongs to a run the prior process was executing; its terraform child died with
//it (kill_on_drop), so mark it "interrupted" (terminal) to free the dir. The live lock map starts empty.
pub fn reconcile_on_startup(runs_dir: &Path) {
    let entries = match std::fs::read_dir(runs_dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("status") {
            continue;
        }
        let Some(mut status) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|d| serde_json::from_str::<RunStatus>(&d).ok())
        else {
            continue;
        };
        if !status.is_terminal() {
            status.state = "interrupted".to_string();
            if status.finished.is_none() {
                status.finished = Some(status.started.clone());
            }
            if let Err(e) = write_status(runs_dir, &status) {
                tracing::warn!(run_id = %status.run_id, error = %e, "failed to reconcile stale run status");
            } else {
                tracing::info!(run_id = %status.run_id, "reconciled interrupted run from prior process");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp_runs(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("tf_apply_locks_{}_{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[tokio::test]
    async fn same_dir_second_trigger_is_in_progress() {
        let reg = LockRegistry::new(4);
        let dir = PathBuf::from("/tmp/tf-apply-lock-dir-a");

        let first = reg.admit(&dir, "run-1");
        let guard = match first {
            Admission::Admitted { run_id, guard } => {
                assert_eq!(run_id, "run-1");
                guard
            }
            _ => panic!("first admit should be Admitted"),
        };

        match reg.admit(&dir, "run-2") {
            Admission::InProgress { run_id } => assert_eq!(run_id, "run-1"),
            _ => panic!("same-dir second trigger should be InProgress"),
        }

        drop(guard);
        assert_eq!(reg.inflight_len(), 0);
        assert!(matches!(
            reg.admit(&dir, "run-3"),
            Admission::Admitted { .. }
        ));
    }

    #[tokio::test]
    async fn different_dirs_run_concurrently_until_cap() {
        let reg = LockRegistry::new(2);
        let g1 = match reg.admit(Path::new("/tmp/d1"), "r1") {
            Admission::Admitted { guard, .. } => guard,
            _ => panic!("d1 should admit"),
        };
        let _g2 = match reg.admit(Path::new("/tmp/d2"), "r2") {
            Admission::Admitted { guard, .. } => guard,
            _ => panic!("d2 should admit"),
        };
        assert!(matches!(
            reg.admit(Path::new("/tmp/d3"), "r3"),
            Admission::AtCapacity
        ));
        drop(g1);
        assert!(matches!(
            reg.admit(Path::new("/tmp/d3"), "r3"),
            Admission::Admitted { .. }
        ));
    }

    #[test]
    fn status_round_trips() {
        let runs = tmp_runs("status");
        let s = RunStatus {
            run_id: "abc-123".into(),
            dir: "/tmp/stack".into(),
            apply: false,
            state: "running".into(),
            exit_code: None,
            started: "2026-08-15T22:00:00+02:00".into(),
            finished: None,
        };
        write_status(&runs, &s).unwrap();
        let back = read_status(&runs, "abc-123").unwrap();
        assert_eq!(back.run_id, "abc-123");
        assert_eq!(back.state, "running");
        assert!(!back.is_terminal());
        let _ = std::fs::remove_dir_all(&runs);
    }

    #[test]
    fn reconcile_marks_running_as_interrupted() {
        let runs = tmp_runs("reconcile");
        let running = RunStatus {
            run_id: "stuck-1".into(),
            dir: "/tmp/stack".into(),
            apply: true,
            state: "running".into(),
            exit_code: None,
            started: "2026-08-15T22:00:00+02:00".into(),
            finished: None,
        };
        let done = RunStatus {
            run_id: "done-1".into(),
            dir: "/tmp/stack2".into(),
            apply: false,
            state: "plan_no_changes".into(),
            exit_code: Some(0),
            started: "2026-08-15T21:00:00+02:00".into(),
            finished: Some("2026-08-15T21:00:05+02:00".into()),
        };
        write_status(&runs, &running).unwrap();
        write_status(&runs, &done).unwrap();

        reconcile_on_startup(&runs);

        assert_eq!(read_status(&runs, "stuck-1").unwrap().state, "interrupted");
        assert_eq!(read_status(&runs, "done-1").unwrap().state, "plan_no_changes");
        let _ = std::fs::remove_dir_all(&runs);
    }

    #[test]
    fn run_id_validation_blocks_traversal() {
        assert!(is_valid_run_id("2d6052fb-c19c-0b90-1386-1e473b018ce7"));
        assert!(!is_valid_run_id("../../etc/passwd"));
        assert!(!is_valid_run_id("a/b"));
        assert!(!is_valid_run_id("a.b"));
        assert!(!is_valid_run_id(""));
    }
}
