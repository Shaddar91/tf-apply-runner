//allowlist.rs — per-request path gate: runs after auth on the raw `dir`, returns the canonical real
//path used for BOTH the lock key and `terraform -chdir` (no TOCTOU gap). Fail-closed; the descendant
//test is component-wise (`Path::starts_with`): `/srv/work-secrets` is not a descendant of `/srv/work`.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AllowlistError {
    //Caller-fixable input: empty, non-absolute, or control/NUL chars → 400.
    BadRequest(String),
    //Resolves outside the allowlist, or does not resolve at all → 403.
    Forbidden(String),
    //In-allowlist but not a directory → 422.
    Unprocessable(String),
}

impl AllowlistError {
    pub fn status(&self) -> u16 {
        match self {
            AllowlistError::BadRequest(_) => 400,
            AllowlistError::Forbidden(_) => 403,
            AllowlistError::Unprocessable(_) => 422,
        }
    }

    //Safe to return in the error body — never echoes a secret.
    pub fn message(&self) -> &str {
        match self {
            AllowlistError::BadRequest(m)
            | AllowlistError::Forbidden(m)
            | AllowlistError::Unprocessable(m) => m,
        }
    }
}

//Returns the canonical `real` path on success; every rejection is fail-closed.
pub fn validate(raw_dir: &str, roots: &[PathBuf]) -> Result<PathBuf, AllowlistError> {
    if raw_dir.is_empty() {
        return Err(AllowlistError::BadRequest("dir must not be empty".into()));
    }
    if raw_dir.chars().any(|c| c == '\0' || c.is_control()) {
        return Err(AllowlistError::BadRequest(
            "dir contains NUL or control characters".into(),
        ));
    }
    let candidate = Path::new(raw_dir);
    if !candidate.is_absolute() {
        return Err(AllowlistError::BadRequest(
            "dir must be an absolute path".into(),
        ));
    }

    //canonicalize resolves symlinks and `..` before the descendant test; a path that does not resolve
    //is 403, not 404 — we never confirm or deny the existence of paths outside the allowlist.
    let real = std::fs::canonicalize(candidate)
        .map_err(|_| AllowlistError::Forbidden("dir does not resolve to a real path".into()))?;

    let in_allowlist = roots.iter().any(|root| real.starts_with(root));
    if !in_allowlist {
        return Err(AllowlistError::Forbidden(
            "dir is outside the allowed roots".into(),
        ));
    }

    if !real.is_dir() {
        return Err(AllowlistError::Unprocessable(
            "dir is not a directory".into(),
        ));
    }

    //An empty (no .tf) directory passes through; terraform surfaces that as its own error later.
    Ok(real)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn scratch(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("tf_apply_allow_{}_{tag}", std::process::id()));
        fs::create_dir_all(&base).unwrap();
        fs::canonicalize(&base).unwrap()
    }

    #[test]
    fn relative_path_is_400() {
        let root = scratch("rel");
        let err = validate("relative/dir", std::slice::from_ref(&root)).unwrap_err();
        assert_eq!(err.status(), 400, "{err:?}");
    }

    #[test]
    fn nul_or_control_char_is_400() {
        let root = scratch("ctl");
        let err = validate("/tmp/with\0nul", std::slice::from_ref(&root)).unwrap_err();
        assert_eq!(err.status(), 400, "{err:?}");
    }

    #[test]
    fn etc_traversal_outside_roots_is_403() {
        let root = scratch("etc");
        let err = validate("/etc", std::slice::from_ref(&root)).unwrap_err();
        assert_eq!(err.status(), 403, "{err:?}");
    }

    #[test]
    fn nonexistent_path_is_403() {
        let root = scratch("missing");
        let target = root.join("does-not-exist-xyz");
        let err = validate(target.to_str().unwrap(), std::slice::from_ref(&root)).unwrap_err();
        assert_eq!(err.status(), 403, "{err:?}");
    }

    #[test]
    fn prefix_sibling_is_403() {
        //Sibling `<base>/workspace-secrets` shares a STRING prefix with root `<base>/workspace` but is a
        //different path component — the core reason the test is component-wise, not str::starts_with.
        let base = scratch("sibling");
        let root = base.join("workspace");
        let sibling = base.join("workspace-secrets");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&sibling).unwrap();
        let root = fs::canonicalize(&root).unwrap();

        let err = validate(sibling.to_str().unwrap(), std::slice::from_ref(&root)).unwrap_err();
        assert_eq!(err.status(), 403, "prefix-sibling must be 403, got {err:?}");
    }

    #[test]
    fn file_target_inside_root_is_422() {
        let root = scratch("file");
        let file = root.join("main.tf");
        fs::write(&file, "resource \"terraform_data\" \"x\" {}\n").unwrap();
        let err = validate(file.to_str().unwrap(), std::slice::from_ref(&root)).unwrap_err();
        assert_eq!(err.status(), 422, "file target must be 422, got {err:?}");
    }

    #[test]
    fn directory_inside_root_is_accepted() {
        let root = scratch("ok");
        let inner = root.join("stack");
        fs::create_dir_all(&inner).unwrap();
        let real = validate(inner.to_str().unwrap(), std::slice::from_ref(&root)).unwrap();
        assert!(real.starts_with(&root));
        assert!(real.is_dir());
    }

    #[test]
    fn empty_root_list_rejects_everything() {
        let root = scratch("empty");
        let err = validate(root.to_str().unwrap(), &[]).unwrap_err();
        assert_eq!(err.status(), 403, "{err:?}");
    }
}
