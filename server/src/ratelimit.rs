//! In-memory rate limits (DESIGN.md §7), kept per client address and per
//! account for each kind of action.
//!
//! Each limit is a token bucket holding up to `burst` tokens, refilled with
//! one token every `interval`. It is stored as the time at which the bucket
//! will next be full (the "generic cell rate algorithm"), which needs no
//! floating point and makes the bound exact: over any period of length `d`
//! at most `burst + d / interval` requests are allowed.
//!
//! The state lives in this one process (DESIGN.md §1), so a restart resets
//! every limit.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Something that is rate-limited.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    /// Logging in with a password.
    Login,
    /// Asking for a magic link.
    MagicLink,
    /// Typing in the code from a magic-link email.
    MagicCode,
    /// Asking for a password reset.
    PasswordReset,
    /// Registering with an invite.
    Redeem,
    /// Trying setup tokens.
    Setup,
    /// Sending a verification email again.
    VerifyEmail,
}

/// Whose allowance a request uses up.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Scope {
    /// The client's address.
    Ip,
    /// The account (or email address) the request is about.
    Account,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rule {
    pub burst: u32,
    pub interval: Duration,
}

impl Rule {
    const fn new(burst: u32, interval_secs: u64) -> Rule {
        Rule {
            burst,
            interval: Duration::from_secs(interval_secs),
        }
    }
}

/// The limits for each action.
pub fn rule(action: Action, scope: Scope) -> Rule {
    match (action, scope) {
        (Action::Login, Scope::Ip) => Rule::new(20, 30),
        (Action::Login, Scope::Account) => Rule::new(10, 60),
        (Action::MagicLink | Action::PasswordReset, Scope::Ip) => Rule::new(5, 120),
        (Action::MagicLink | Action::PasswordReset, Scope::Account) => Rule::new(3, 600),
        (Action::MagicCode, _) => Rule::new(10, 30),
        (Action::Redeem, _) => Rule::new(10, 300),
        (Action::Setup, _) => Rule::new(10, 60),
        (Action::VerifyEmail, _) => Rule::new(3, 600),
    }
}

/// One token bucket, as the time at which it will be full again.
#[derive(Clone, Copy, Debug)]
pub struct Bucket {
    full_at: Instant,
}

impl Bucket {
    pub fn new(now: Instant) -> Bucket {
        Bucket { full_at: now }
    }

    /// Takes a token if there is one.
    pub fn take(&mut self, rule: Rule, now: Instant) -> bool {
        let full_at = self.full_at.max(now);
        // The bucket is short of (full_at - now) / interval tokens; there is
        // a token to take if that is at most burst - 1.
        let tolerance = rule.interval * rule.burst.saturating_sub(1);
        if full_at.duration_since(now) > tolerance || rule.burst == 0 {
            return false;
        }
        self.full_at = full_at + rule.interval;
        true
    }

    fn is_full(&self, now: Instant) -> bool {
        self.full_at <= now
    }
}

/// How many buckets to hold before throwing away the full ones.
const PRUNE_AT: usize = 10_000;

pub struct RateLimiter {
    buckets: Mutex<HashMap<(Action, Scope, String), Bucket>>,
    rules: fn(Action, Scope) -> Rule,
}

impl Default for RateLimiter {
    fn default() -> Self {
        RateLimiter::new(rule)
    }
}

impl RateLimiter {
    pub fn new(rules: fn(Action, Scope) -> Rule) -> RateLimiter {
        RateLimiter {
            buckets: Mutex::new(HashMap::new()),
            rules,
        }
    }

    /// Whether a request may go ahead, using up one of its allowance if so.
    pub fn check(&self, action: Action, scope: Scope, key: &str) -> bool {
        self.check_at(action, scope, key, Instant::now())
    }

    pub fn check_at(&self, action: Action, scope: Scope, key: &str, now: Instant) -> bool {
        let rule = (self.rules)(action, scope);
        let mut buckets = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        if buckets.len() >= PRUNE_AT {
            buckets.retain(|_, bucket| !bucket.is_full(now));
        }
        buckets
            .entry((action, scope, key.to_lowercase()))
            .or_insert_with(|| Bucket::new(now))
            .take(rule, now)
    }

    /// Checks several limits at once; the request goes ahead only if all
    /// of them allow it. Every limit is charged either way, so that a
    /// client can't probe one limit without paying for the others.
    pub fn check_all(&self, checks: &[(Action, Scope, &str)]) -> bool {
        let now = Instant::now();
        let results: Vec<bool> = checks
            .iter()
            .map(|(action, scope, key)| self.check_at(*action, *scope, key, now))
            .collect();
        results.into_iter().all(|ok| ok)
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.buckets.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hegel::TestCase;
    use hegel::generators::{self as gs, Generator};

    fn rule_from(tc: &TestCase) -> Rule {
        Rule {
            burst: tc.draw(gs::integers::<u32>().max_value(20)),
            interval: Duration::from_millis(
                tc.draw(gs::integers::<u64>().min_value(1).max_value(10_000)),
            ),
        }
    }

    /// Request times in milliseconds after the start, in order.
    fn times(tc: &TestCase) -> Vec<u64> {
        let gaps = tc.draw(gs::vecs(gs::integers::<u64>().max_value(5_000)).max_size(200));
        gaps.iter()
            .scan(0u64, |t, gap| {
                *t += gap;
                Some(*t)
            })
            .collect()
    }

    #[hegel::test]
    fn never_allows_more_than_burst_plus_refill(tc: TestCase) {
        let rule = rule_from(&tc);
        let start = Instant::now();
        let times = times(&tc);
        let mut bucket = Bucket::new(start);
        let allowed: Vec<u64> = times
            .iter()
            .copied()
            .filter(|&t| bucket.take(rule, start + Duration::from_millis(t)))
            .collect();
        let interval = rule.interval.as_millis() as u64;
        for (i, &from) in allowed.iter().enumerate() {
            for (j, &to) in allowed.iter().enumerate().skip(i) {
                let in_window = (j - i + 1) as u64;
                assert!(
                    in_window <= u64::from(rule.burst) + (to - from) / interval,
                    "{in_window} requests allowed between {from}ms and {to}ms with {rule:?}"
                );
            }
        }
    }

    #[hegel::test]
    fn an_idle_bucket_allows_a_full_burst(tc: TestCase) {
        let rule = rule_from(&tc);
        let start = Instant::now();
        let mut bucket = Bucket::new(start);
        let mut last = 0;
        for t in times(&tc) {
            bucket.take(rule, start + Duration::from_millis(t));
            last = t;
        }
        let rested = start + Duration::from_millis(last) + rule.interval * rule.burst;
        for _ in 0..rule.burst {
            assert!(bucket.take(rule, rested));
        }
        assert!(!bucket.take(rule, rested));
    }

    #[hegel::test]
    fn keys_are_independent(tc: TestCase) {
        let limiter = RateLimiter::default();
        let now = Instant::now();
        let key = tc.draw(gs::text().max_size(20));
        let other = tc.draw(gs::text().max_size(20).filter({
            let key = key.clone();
            move |other: &String| other.to_lowercase() != key.to_lowercase()
        }));
        let burst = rule(Action::Login, Scope::Account).burst;
        for _ in 0..burst {
            assert!(limiter.check_at(Action::Login, Scope::Account, &key, now));
        }
        assert!(!limiter.check_at(Action::Login, Scope::Account, &key, now));
        // Keys that differ only in case share a bucket.
        assert!(!limiter.check_at(Action::Login, Scope::Account, &key.to_lowercase(), now));
        if key.to_uppercase().to_lowercase() == key.to_lowercase() {
            assert!(!limiter.check_at(Action::Login, Scope::Account, &key.to_uppercase(), now));
        }
        assert!(limiter.check_at(Action::Login, Scope::Account, &other, now));
        assert!(limiter.check_at(Action::Login, Scope::Ip, &key, now));
        assert!(limiter.check_at(Action::MagicLink, Scope::Account, &key, now));
    }

    #[test]
    fn full_buckets_are_pruned() {
        let limiter = RateLimiter::default();
        let now = Instant::now();
        for i in 0..PRUNE_AT {
            limiter.check_at(Action::Login, Scope::Ip, &i.to_string(), now);
        }
        assert_eq!(limiter.len(), PRUNE_AT);
        let later = now + Duration::from_secs(3600);
        limiter.check_at(Action::Login, Scope::Ip, "new", later);
        assert_eq!(limiter.len(), 1);
    }

    #[test]
    fn check_all_charges_every_limit() {
        let limiter = RateLimiter::default();
        let ip_burst = rule(Action::MagicLink, Scope::Ip).burst;
        let mut allowed = 0;
        for i in 0..ip_burst + 2 {
            let email = format!("person{i}@example.com");
            if limiter.check_all(&[
                (Action::MagicLink, Scope::Ip, "1.2.3.4"),
                (Action::MagicLink, Scope::Account, &email),
            ]) {
                allowed += 1;
            }
        }
        assert_eq!(allowed, ip_burst);
    }
}
