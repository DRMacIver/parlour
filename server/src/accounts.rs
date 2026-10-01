//! Accounts (DESIGN.md §7): roles, the rules for usernames, display names,
//! email addresses and passwords, and password hashing with Argon2id.

use std::fmt;
use std::sync::LazyLock;

use argon2::Argon2;
use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use serde::{Deserialize, Serialize};

use crate::mail;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Moderator,
    Admin,
}

impl Role {
    pub fn parse(s: &str) -> Option<Role> {
        match s {
            "user" => Some(Role::User),
            "moderator" => Some(Role::Moderator),
            "admin" => Some(Role::Admin),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Moderator => "moderator",
            Role::Admin => "admin",
        }
    }

    /// The role's name, for choosing one.
    pub fn label(self) -> &'static str {
        match self {
            Role::User => "Member",
            Role::Moderator => "Moderator",
            Role::Admin => "Admin",
        }
    }

    /// Moderators and admins.
    pub fn is_staff(self) -> bool {
        self >= Role::Moderator
    }

    /// The label shown next to a staff member's name.
    pub fn badge(self) -> Option<&'static str> {
        match self {
            Role::User => None,
            Role::Moderator => Some("Moderator"),
            Role::Admin => Some("Admin"),
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Usernames that would let someone pass as the site or its staff.
const RESERVED_USERNAMES: &[&str] = &[
    "admin",
    "administrator",
    "moderator",
    "mod",
    "staff",
    "system",
    "parlour",
    "root",
    "support",
    "me",
];

/// Checks a new username. The database enforces the same format.
pub fn validate_username(username: &str) -> Result<(), &'static str> {
    if !(2..=24).contains(&username.len()) {
        return Err("Usernames are 2 to 24 characters long.");
    }
    if !username
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Err("Usernames can only contain letters, digits and underscores.");
    }
    if RESERVED_USERNAMES
        .iter()
        .any(|r| r.eq_ignore_ascii_case(username))
    {
        return Err("That username is reserved.");
    }
    Ok(())
}

/// Tidies and checks a display name.
pub fn clean_display_name(name: &str) -> Result<String, &'static str> {
    let name: String = name.split_whitespace().collect::<Vec<_>>().join(" ");
    if name.is_empty() {
        return Err("Enter the name to show next to your posts.");
    }
    if name.chars().count() > 50 {
        return Err("Names are at most 50 characters long.");
    }
    if name.chars().any(char::is_control) {
        return Err("Names can't contain control characters.");
    }
    Ok(name)
}

/// Tidies and checks an email address.
pub fn clean_email(email: &str) -> Result<String, &'static str> {
    let email = email.trim();
    if mail::valid_address(email) {
        Ok(email.to_owned())
    } else {
        Err("Enter a valid email address.")
    }
}

pub const MIN_PASSWORD_LENGTH: usize = 10;
/// Long enough for any passphrase, short enough that hashing it is cheap.
pub const MAX_PASSWORD_BYTES: usize = 256;

pub fn validate_password(password: &str) -> Result<(), &'static str> {
    if password.chars().count() < MIN_PASSWORD_LENGTH {
        return Err("Passwords must be at least 10 characters long.");
    }
    if password.len() > MAX_PASSWORD_BYTES {
        return Err("Passwords can be at most 256 bytes long.");
    }
    Ok(())
}

/// Hashes a password with Argon2id (the default parameters: 19 MiB, two
/// passes). It takes a noticeable fraction of a second, so it runs off the
/// async worker threads.
pub async fn hash_password(password: &str) -> String {
    let password = password.to_owned();
    tokio::task::spawn_blocking(move || hash_password_blocking(&password))
        .await
        .expect("password hashing panicked")
}

pub fn hash_password_blocking(password: &str) -> String {
    Argon2::default()
        .hash_password(password.as_bytes())
        .expect("hashing a password with fresh salt can't fail")
        .to_string()
}

/// A hash of a password nobody knows, checked against when there is no
/// account, so that a failed login takes as long either way.
static DUMMY_HASH: LazyLock<String> =
    LazyLock::new(|| hash_password_blocking("no account has this password"));

/// Whether `password` matches `hash`. With no hash (no such account, or an
/// account that only uses magic links) it still does the work of a check,
/// and returns false.
pub async fn verify_password(hash: Option<String>, password: &str) -> bool {
    if password.len() > MAX_PASSWORD_BYTES {
        return false;
    }
    let password = password.to_owned();
    tokio::task::spawn_blocking(move || {
        let (hash, real) = match &hash {
            Some(hash) => (hash.as_str(), true),
            None => (DUMMY_HASH.as_str(), false),
        };
        let Ok(parsed) = PasswordHash::new(hash) else {
            return false;
        };
        Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok()
            && real
    })
    .await
    .expect("password checking panicked")
}

#[cfg(test)]
mod tests {
    use super::*;
    use hegel::TestCase;
    use hegel::generators as gs;

    #[test]
    fn roles() {
        assert!(!Role::User.is_staff());
        assert!(Role::Moderator.is_staff());
        assert!(Role::Admin.is_staff());
        for role in [Role::User, Role::Moderator, Role::Admin] {
            assert_eq!(Role::parse(role.as_str()), Some(role));
        }
        assert_eq!(Role::parse("owner"), None);
    }

    #[test]
    fn usernames() {
        assert!(validate_username("ruth").is_ok());
        assert!(validate_username("cheap_deals77").is_ok());
        assert!(validate_username("r").is_err());
        assert!(validate_username("ruth smith").is_err());
        assert!(validate_username("rüth").is_err());
        assert!(validate_username("Admin").is_err());
        assert!(validate_username(&"a".repeat(25)).is_err());
    }

    #[hegel::test]
    fn accepted_usernames_fit_the_database_format(tc: TestCase) {
        let name = tc.draw(gs::text().max_size(30));
        if validate_username(&name).is_ok() {
            assert!((2..=24).contains(&name.len()));
            assert!(name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'));
        }
    }

    #[hegel::test]
    fn display_names_are_tidy(tc: TestCase) {
        let name = tc.draw(gs::text());
        if let Ok(clean) = clean_display_name(&name) {
            assert!(!clean.is_empty() && clean.chars().count() <= 50);
            assert_eq!(clean.trim(), clean);
            assert!(!clean.contains("  "));
            assert_eq!(clean_display_name(&clean), Ok(clean.clone()));
        }
    }

    #[test]
    fn passwords() {
        assert!(validate_password("correct horse").is_ok());
        assert!(validate_password("short").is_err());
        assert!(validate_password(&"x".repeat(257)).is_err());
    }

    #[tokio::test]
    async fn password_hashes_verify() {
        let hash = hash_password("correct horse battery").await;
        assert!(hash.starts_with("$argon2id$"));
        assert!(verify_password(Some(hash.clone()), "correct horse battery").await);
        assert!(!verify_password(Some(hash.clone()), "correct horse battery!").await);
        assert!(!verify_password(Some("not a hash".into()), "anything").await);
        assert!(!verify_password(None, "no account has this password").await);
    }
}
