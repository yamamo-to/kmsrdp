//! Username/password validation, shaped after `ironrdp-server`'s
//! `Credentials`/`ExactMatchCredentialValidator` so kmsrdp's existing
//! construction (`ExactMatchCredentialValidator::new(Credentials { .. })`)
//! ports unchanged.

use subtle::ConstantTimeEq as _;

#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub username: String,
    pub password: String,
    pub domain: Option<String>,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("username", &self.username)
            .field("password", &"[REDACTED]")
            .field("domain", &self.domain)
            .finish()
    }
}

pub trait CredentialValidator: Send + Sync {
    fn validate(&self, username: &str, password: &str, domain: &str) -> bool;
}

pub struct ExactMatchCredentialValidator {
    expected: Credentials,
}

impl ExactMatchCredentialValidator {
    pub fn new(expected: Credentials) -> Self {
        Self { expected }
    }
}

impl CredentialValidator for ExactMatchCredentialValidator {
    fn validate(&self, username: &str, password: &str, domain: &str) -> bool {
        let (client_domain, client_user) = normalize_client_identity(username, domain);
        let password_matches: bool = self
            .expected
            .password
            .as_bytes()
            .ct_eq(password.as_bytes())
            .into();
        if !eq_ignore_ascii_case_ct(&client_user, &self.expected.username) || !password_matches {
            return false;
        }
        match &self.expected.domain {
            Some(expected_domain) => eq_ignore_ascii_case_ct(&client_domain, expected_domain),
            None => true,
        }
    }
}

/// mstsc may send `DOMAIN\user` or `user@domain` entirely in the username
/// field (with an empty domain), or prefix local accounts as `.\user`.
pub fn normalize_client_identity(username: &str, domain: &str) -> (String, String) {
    let username = username.trim();
    let domain = domain.trim();

    if let Some(user) = username.strip_prefix(".\\") {
        return (String::new(), user.to_owned());
    }
    if let Some((d, u)) = username.split_once('\\') {
        return (d.to_owned(), u.to_owned());
    }
    if let Some((u, d)) = username.rsplit_once('@') {
        return (d.to_owned(), u.to_owned());
    }
    (domain.to_owned(), username.to_owned())
}

/// ASCII case-insensitive equality that does not short-circuit on the first
/// differing byte (length is still visible). Used for usernames/domains so
/// the password `ct_eq` is not preceded by an early `return`.
pub fn eq_ignore_ascii_case_ct(a: &str, b: &str) -> bool {
    let a: Vec<u8> = a.bytes().map(|c| c.to_ascii_lowercase()).collect();
    let b: Vec<u8> = b.bytes().map(|c| c.to_ascii_lowercase()).collect();
    let n = a.len().max(b.len());
    let mut pa = vec![0u8; n];
    let mut pb = vec![0u8; n];
    pa[..a.len()].copy_from_slice(&a);
    pb[..b.len()].copy_from_slice(&b);
    let body: bool = pa.ct_eq(&pb).into();
    let lens: bool = (a.len() as u64).ct_eq(&(b.len() as u64)).into();
    body && lens
}

#[cfg(test)]
mod tests {
    use super::*;

    fn validator(user: &str, pass: &str) -> ExactMatchCredentialValidator {
        ExactMatchCredentialValidator::new(Credentials {
            username: user.to_owned(),
            password: pass.to_owned(),
            domain: None,
        })
    }

    #[test]
    fn accepts_plain_username() {
        let v = validator("kmsrdp", "hunter2");
        assert!(v.validate("kmsrdp", "hunter2", ""));
    }

    #[test]
    fn accepts_domain_backslash_username() {
        let v = validator("kmsrdp", "hunter2");
        assert!(v.validate(r"WORKGROUP\kmsrdp", "hunter2", ""));
    }

    #[test]
    fn accepts_local_dot_backslash_username() {
        let v = validator("kmsrdp", "hunter2");
        assert!(v.validate(r".\kmsrdp", "hunter2", ""));
    }

    #[test]
    fn accepts_split_domain_and_username() {
        let v = validator("kmsrdp", "hunter2");
        assert!(v.validate("kmsrdp", "hunter2", "WORKGROUP"));
    }

    #[test]
    fn username_match_is_case_insensitive() {
        let v = validator("kmsrdp", "hunter2");
        assert!(v.validate("KMSRDP", "hunter2", ""));
    }

    #[test]
    fn rejects_wrong_password() {
        let v = validator("kmsrdp", "hunter2");
        assert!(!v.validate("kmsrdp", "wrong", ""));
    }

    #[test]
    fn rejects_partial_password_or_wrong_case() {
        let v = validator("kmsrdp", "Hunter2");
        // Prefix mismatch
        assert!(!v.validate("kmsrdp", "Hunter", ""));
        // Suffix extra
        assert!(!v.validate("kmsrdp", "Hunter22", ""));
        // Case mismatch (password must be case sensitive)
        assert!(!v.validate("kmsrdp", "hunter2", ""));
        // Empty password
        assert!(!v.validate("kmsrdp", "", ""));
    }

    #[test]
    fn credentials_debug_redacts_password() {
        let creds = Credentials {
            username: "admin".to_string(),
            password: "super_secret_password".to_string(),
            domain: Some("DOMAIN".to_string()),
        };
        let formatted = format!("{creds:?}");
        assert!(!formatted.contains("super_secret_password"));
        assert!(formatted.contains("[REDACTED]"));
    }

    #[test]
    fn username_ct_eq_is_case_insensitive_and_rejects_mismatch() {
        assert!(eq_ignore_ascii_case_ct("kmsrdp", "KMSRDP"));
        assert!(!eq_ignore_ascii_case_ct("kmsrdp", "kmsrd"));
        assert!(!eq_ignore_ascii_case_ct("kmsrdp", "kmsrdpx"));
        assert!(!eq_ignore_ascii_case_ct("abc", "abd"));
    }
}
