//auth.rs — the X-Runner-Token gate. Constant-time compare (subtle::ConstantTimeEq, never `==`) so a
//wrong token cannot be recovered byte-by-byte through timing; runs before any allowlist/lock/fs work;
//the token value is never logged.

use subtle::ConstantTimeEq;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthOutcome {
    Ok,
    Unauthorized,
}

//Fail-closed: an empty configured or presented token is always Unauthorized, so an unset X_RUNNER_TOKEN
//rejects every request. ct_eq leaks only length (the accepted standard), never the byte contents.
pub fn check_token(configured: &str, presented: Option<&str>) -> AuthOutcome {
    if configured.is_empty() {
        return AuthOutcome::Unauthorized;
    }
    let presented = match presented {
        Some(p) if !p.is_empty() => p,
        _ => return AuthOutcome::Unauthorized,
    };
    if bool::from(configured.as_bytes().ct_eq(presented.as_bytes())) {
        AuthOutcome::Ok
    } else {
        AuthOutcome::Unauthorized
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "s3cr3t-runner-token-0123456789abcdef";

    #[test]
    fn correct_token_authorizes() {
        assert_eq!(check_token(SECRET, Some(SECRET)), AuthOutcome::Ok);
    }

    //Same-length-but-different exercises the content-comparison branch, not the length short-circuit.
    #[test]
    fn wrong_token_is_rejected_constant_time() {
        let mut wrong = SECRET.to_string();
        wrong.replace_range(0..1, "X");
        assert_eq!(wrong.len(), SECRET.len());
        assert_eq!(check_token(SECRET, Some(&wrong)), AuthOutcome::Unauthorized);
    }

    #[test]
    fn missing_token_is_rejected() {
        assert_eq!(check_token(SECRET, None), AuthOutcome::Unauthorized);
    }

    #[test]
    fn empty_presented_token_is_rejected() {
        assert_eq!(check_token(SECRET, Some("")), AuthOutcome::Unauthorized);
    }

    #[test]
    fn empty_configured_secret_rejects_everything() {
        assert_eq!(check_token("", Some("anything")), AuthOutcome::Unauthorized);
        assert_eq!(check_token("", Some("")), AuthOutcome::Unauthorized);
    }

    #[test]
    fn length_mismatch_is_rejected() {
        assert_eq!(check_token(SECRET, Some("short")), AuthOutcome::Unauthorized);
        assert_eq!(
            check_token(SECRET, Some(&format!("{SECRET}extra"))),
            AuthOutcome::Unauthorized
        );
    }
}
