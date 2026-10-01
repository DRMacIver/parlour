//! Random secrets for sessions, emailed links and invites. Each is 32 (or
//! for invites 16) random bytes from the operating system, written in
//! URL-safe base64. Only a SHA-256 of each is stored, so a copy of the
//! database can't be used to log in or redeem anything; the secrets have
//! enough entropy that a fast hash is enough.
//!
//! The one exception is email waiting to be sent: a queued email holds its
//! link in plain text until it is sent, or until it has failed for the
//! last time (about an hour of retries), when the link is removed. See
//! [`crate::mail::scrub`].

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// A new random secret of `bytes` bytes, as URL-safe text.
pub fn random_token(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).expect("the operating system's random number generator failed");
    URL_SAFE_NO_PAD.encode(buf)
}

/// A session token or emailed-link token.
pub fn new_token() -> String {
    random_token(32)
}

/// The hash stored in place of a secret.
pub fn hash(secret: &str) -> Vec<u8> {
    Sha256::digest(secret.as_bytes()).to_vec()
}

/// A random number with `digits` decimal digits (leading zeros allowed), for
/// codes people type in.
pub fn random_code(digits: u32) -> String {
    let modulus = 10u64.pow(digits);
    // Rejection sampling keeps every code equally likely.
    let limit = u64::MAX - u64::MAX % modulus;
    loop {
        let mut buf = [0u8; 8];
        getrandom::fill(&mut buf).expect("the operating system's random number generator failed");
        let n = u64::from_le_bytes(buf);
        if n < limit {
            return format!("{:0width$}", n % modulus, width = digits as usize);
        }
    }
}

/// Compares two secrets (or hashes) in constant time.
pub fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && bool::from(a.ct_eq(b))
}

/// Whether `text` could be a token from [`random_token`]: URL-safe base64
/// of a sensible length. Lets handlers reject junk without a query.
pub fn plausible(text: &str) -> bool {
    (16..=64).contains(&text.len())
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;
    use hegel::TestCase;
    use hegel::generators as gs;

    #[test]
    fn tokens_are_url_safe_and_distinct() {
        let a = new_token();
        let b = new_token();
        assert_ne!(a, b);
        assert_eq!(a.len(), 43);
        assert!(plausible(&a));
        assert!(plausible(&random_token(16)));
    }

    #[test]
    fn codes_have_the_requested_digits() {
        for _ in 0..100 {
            let code = random_code(6);
            assert_eq!(code.len(), 6);
            assert!(code.bytes().all(|b| b.is_ascii_digit()));
        }
    }

    #[hegel::test]
    fn hashes_match_only_their_secret(tc: TestCase) {
        let a = tc.draw(gs::text());
        let b = tc.draw(gs::text());
        assert_eq!(same(&hash(&a), &hash(&b)), a == b);
    }

    #[hegel::test]
    fn plausible_tokens_are_safe_in_a_path(tc: TestCase) {
        let text = tc.draw(gs::text());
        if plausible(&text) {
            assert!(!text.contains(['/', '?', '#', '%', '.']));
        }
    }
}
