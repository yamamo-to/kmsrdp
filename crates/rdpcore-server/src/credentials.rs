//! Username/password validation, shaped after `ironrdp-server`'s
//! `Credentials`/`ExactMatchCredentialValidator` so kmsrdp's existing
//! construction (`ExactMatchCredentialValidator::new(Credentials { .. })`)
//! ports unchanged.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credentials {
    pub username: String,
    pub password: String,
    pub domain: Option<String>,
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
        if !client_user.eq_ignore_ascii_case(&self.expected.username) || password != self.expected.password {
            return false;
        }
        match &self.expected.domain {
            Some(expected_domain) => client_domain.eq_ignore_ascii_case(expected_domain),
            None => true,
        }
    }
}

/// mstsc may send `DOMAIN\user` or `user@domain` entirely in the username
/// field (with an empty domain), or prefix local accounts as `.\user`.
fn normalize_client_identity(username: &str, domain: &str) -> (String, String) {
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
}
