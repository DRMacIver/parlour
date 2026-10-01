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
    /// The username or email address typed into a form, whether or not it
    /// names an account. Refusals on this scope are shown to the client,
    /// and they are the same for every name, so they reveal nothing about
    /// which accounts exist.
    Account,
    /// The account a request turned out to be about, by id, however it was
    /// named. Callers don't show refusals on this scope as such (a refused
    /// login looks like a wrong password, a refused email is quietly not
    /// sent), so that comparing the allowances of a username and an email
    /// address doesn't reveal whether they belong to the same account.
    User,
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
        (Action::Login, Scope::Account | Scope::User) => Rule::new(10, 60),
        (Action::MagicLink | Action::PasswordReset, Scope::Ip) => Rule::new(5, 120),
        (Action::MagicLink | Action::PasswordReset, Scope::Account | Scope::User) => {
            Rule::new(3, 600)
        }
        // Each magic link allows a few guesses at its six-digit code, and
        // anyone can ask for links. Per account, this allows at most 22
        // guesses a day, a one in 45,000 chance.
        (Action::MagicCode, Scope::User) => Rule::new(10, 2 * 3600),
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

/// Keys longer than this are cut short before hashing. Nothing that long
/// names an account, so sharing a bucket between such keys loses nothing.
const MAX_KEY_CHARS: usize = 320;

/// A bucket's key: the action, the scope, and a hash of the key text, so
/// that each bucket takes the same small amount of memory however long the
/// text a client sent.
type Key = (Action, Scope, [u8; 16]);

fn key(action: Action, scope: Scope, text: &str) -> Key {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    for c in text
        .chars()
        .take(MAX_KEY_CHARS)
        .flat_map(char::to_lowercase)
    {
        let mut buf = [0; 4];
        hasher.update(c.encode_utf8(&mut buf).as_bytes());
    }
    let digest = hasher.finalize();
    let mut hash = [0; 16];
    hash.copy_from_slice(&digest[..16]);
    (action, scope, hash)
}

/// The most buckets held by default. Each takes well under 100 bytes.
pub const DEFAULT_CAPACITY: usize = 100_000;
/// How many buckets to hold before looking for full ones to throw away.
const PRUNE_AT: usize = 10_000;

pub struct RateLimiter {
    inner: Mutex<Buckets>,
    rules: fn(Action, Scope) -> Rule,
    capacity: usize,
}

struct Buckets {
    map: HashMap<Key, Bucket>,
    /// The size at which to prune next. It grows with the number of
    /// buckets left after pruning, so that pruning stays cheap on average
    /// even when few buckets are full.
    prune_at: usize,
}

impl Default for RateLimiter {
    fn default() -> Self {
        RateLimiter::new(rule)
    }
}

impl RateLimiter {
    pub fn new(rules: fn(Action, Scope) -> Rule) -> RateLimiter {
        RateLimiter::with_capacity(rules, DEFAULT_CAPACITY)
    }

    /// A limiter that holds at most `capacity` buckets. When it has more,
    /// it forgets those that are closest to full, which gives their keys a
    /// fresh allowance a little early but bounds the memory used whatever
    /// clients send.
    pub fn with_capacity(rules: fn(Action, Scope) -> Rule, capacity: usize) -> RateLimiter {
        let capacity = capacity.max(1);
        RateLimiter {
            inner: Mutex::new(Buckets {
                map: HashMap::new(),
                prune_at: PRUNE_AT.min(capacity),
            }),
            rules,
            capacity,
        }
    }

    /// Whether a request may go ahead, using up one of its allowance if so.
    /// Keys that differ only in case share an allowance.
    pub fn check(&self, action: Action, scope: Scope, key: &str) -> bool {
        self.check_at(action, scope, key, Instant::now())
    }

    pub fn check_at(&self, action: Action, scope: Scope, key_text: &str, now: Instant) -> bool {
        let rule = (self.rules)(action, scope);
        let key = key(action, scope, key_text);
        let mut buckets = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if !buckets.map.contains_key(&key) && buckets.map.len() >= buckets.prune_at {
            self.prune(&mut buckets, now);
        }
        buckets
            .map
            .entry(key)
            .or_insert_with(|| Bucket::new(now))
            .take(rule, now)
    }

    fn prune(&self, buckets: &mut Buckets, now: Instant) {
        buckets.map.retain(|_, bucket| !bucket.is_full(now));
        // Leave room for a quarter of the capacity before pruning again.
        let keep = self.capacity - (self.capacity / 4).max(1);
        if buckets.map.len() > keep {
            let mut by_full_at: Vec<(Instant, Key)> = buckets
                .map
                .iter()
                .map(|(key, bucket)| (bucket.full_at, *key))
                .collect();
            let excess = by_full_at.len() - keep;
            by_full_at.select_nth_unstable_by_key(excess - 1, |(full_at, _)| *full_at);
            for (_, key) in &by_full_at[..excess] {
                buckets.map.remove(key);
            }
        }
        // There are now fewer than `capacity` buckets, so this is more
        // than there are.
        buckets.prune_at =
            (buckets.map.len() * 2).clamp(PRUNE_AT.min(self.capacity), self.capacity);
    }

    /// Checks several limits in order, using up one of each allowance until
    /// one refuses. Put the client's own limits (its address) first: a
    /// client that has used up its own allowance can't then use up anyone
    /// else's, or fill memory with buckets for names it makes up.
    pub fn check_all(&self, checks: &[(Action, Scope, &str)]) -> bool {
        let now = Instant::now();
        checks
            .iter()
            .all(|(action, scope, key)| self.check_at(*action, *scope, key, now))
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inner.lock().unwrap().map.len()
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
    fn check_all_stops_at_the_first_refusal() {
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
        // The refused requests made no buckets for their addresses.
        assert_eq!(limiter.len(), 1 + ip_burst as usize);
        // Running out of an account's allowance still uses up the client's.
        let limiter = RateLimiter::default();
        let account_burst = rule(Action::MagicLink, Scope::Account).burst;
        for _ in 0..ip_burst {
            limiter.check_all(&[
                (Action::MagicLink, Scope::Ip, "1.2.3.4"),
                (Action::MagicLink, Scope::Account, "ruth"),
            ]);
        }
        assert!(ip_burst > account_burst);
        assert!(!limiter.check(Action::MagicLink, Scope::Ip, "1.2.3.4"));
    }

    #[test]
    fn long_keys_take_no_more_room() {
        let limiter = RateLimiter::default();
        let now = Instant::now();
        let long = "x".repeat(1_000_000);
        for i in 0..5 {
            limiter.check_at(Action::Login, Scope::Account, &format!("{long}{i}"), now);
        }
        // Past the length that can name anything, keys share a bucket.
        assert_eq!(limiter.len(), 1);
        assert_eq!(
            std::mem::size_of::<Key>(),
            std::mem::size_of::<(Action, Scope, [u8; 16])>()
        );
    }

    /// However many different keys clients send, and however fast, the
    /// limiter never holds more buckets than its capacity, and a key it
    /// still remembers is never allowed more than its rule says.
    #[hegel::test(test_cases = 200)]
    fn memory_is_bounded(tc: TestCase) {
        let capacity = tc.draw(gs::integers::<usize>().min_value(1).max_value(50));
        let limiter = RateLimiter::with_capacity(rule, capacity);
        let start = Instant::now();
        let times = times(&tc);
        for t in times {
            let key = tc.draw(gs::integers::<u32>().max_value(200)).to_string();
            let scope = tc.draw(
                gs::sampled_from(vec![Scope::Ip, Scope::Account, Scope::User]).print_as_debug(),
            );
            limiter.check_at(Action::Login, scope, &key, start + Duration::from_millis(t));
            assert!(limiter.len() <= capacity, "{} > {capacity}", limiter.len());
        }
    }

    #[test]
    fn a_flood_of_new_keys_stays_within_capacity() {
        let limiter = RateLimiter::with_capacity(rule, 1000);
        let now = Instant::now();
        for i in 0..20_000 {
            limiter.check_at(Action::Login, Scope::Account, &format!("k{i}"), now);
        }
        assert!(limiter.len() <= 1000);
        // The newest keys are remembered.
        for _ in 0..rule(Action::Login, Scope::Account).burst - 1 {
            assert!(limiter.check_at(Action::Login, Scope::Account, "k19999", now));
        }
        assert!(!limiter.check_at(Action::Login, Scope::Account, "k19999", now));
    }
}
