//! The anonymous page cache (DESIGN.md §5).
//!
//! Anonymous `GET` requests are answered from memory. Each entry is tagged
//! with what it was rendered from (`thread:N`, `category:N`, `user:N` and
//! `site`), and after each write commits the app clears the matching tags.
//!
//! Rendering happens outside the lock, so a write can commit and clear a tag
//! while a page that depends on it is still being rendered from the old
//! data. To stop that stale page being stored, a render takes a [`Ticket`]
//! before it reads anything, and [`PageCache::insert`] refuses the page if
//! any of its tags was cleared after the ticket was issued.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use axum::body::Bytes;

/// What a cached page depends on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Tag {
    /// Site-wide data: the site name, the list of categories and sections,
    /// user names. Every page carries this tag.
    Site,
    Category(i64),
    Thread(i64),
    /// Per-user data shown next to posts, such as post counts.
    User(i64),
    /// The membership list: the invite tree, and who each member invited.
    Members,
    /// An invite's registration page.
    Invite(i64),
    /// The public moderation log.
    ModLog,
}

impl fmt::Display for Tag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Tag::Site => write!(f, "site"),
            Tag::Category(id) => write!(f, "category:{id}"),
            Tag::Thread(id) => write!(f, "thread:{id}"),
            Tag::User(id) => write!(f, "user:{id}"),
            Tag::Members => write!(f, "members"),
            Tag::Invite(id) => write!(f, "invite:{id}"),
            Tag::ModLog => write!(f, "modlog"),
        }
    }
}

/// A rendered page as stored in the cache.
#[derive(Debug, PartialEq, Eq)]
pub struct CachedPage {
    pub content_type: String,
    pub body: Bytes,
    pub etag: String,
}

impl CachedPage {
    pub fn new(content_type: String, body: Bytes) -> CachedPage {
        let etag = etag_for(&body);
        CachedPage {
            content_type,
            body,
            etag,
        }
    }
}

/// A strong ETag computed from the page's bytes. The page cache holds the
/// rendered page anyway, so hashing it is cheap and is always exactly right,
/// however many things the page depends on.
pub fn etag_for(body: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    // The first 128 bits of SHA-256: a cryptographic hash, so nobody can
    // write a post that makes a page's new version share its old ETag.
    let digest = Sha256::digest(body);
    let mut etag = String::with_capacity(34);
    etag.push('"');
    for byte in &digest[..16] {
        etag.push_str(&format!("{byte:02x}"));
    }
    etag.push('"');
    etag
}

/// Proof that a render started at a particular moment; see the module docs.
#[derive(Clone, Copy, Debug)]
pub struct Ticket {
    epoch: u64,
}

/// How many cleared tags to remember before forgetting them all at once.
const MAX_REMEMBERED_INVALIDATIONS: usize = 100_000;
/// Rough per-entry bookkeeping cost, counted against the byte budget.
const ENTRY_OVERHEAD: usize = 256;

pub struct PageCache {
    max_bytes: usize,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    entries: HashMap<String, Entry>,
    by_tag: HashMap<Tag, HashSet<String>>,
    /// Least recently used first.
    lru: BTreeMap<u64, String>,
    clock: u64,
    bytes: usize,
    /// Incremented by every invalidation.
    epoch: u64,
    /// The epoch at which each tag was last cleared.
    cleared_at: HashMap<Tag, u64>,
    /// Tickets older than this are refused outright, because the
    /// invalidations they would need checking against have been forgotten.
    floor: u64,
}

struct Entry {
    page: Arc<CachedPage>,
    tags: Vec<Tag>,
    used: u64,
    size: usize,
    /// When the page stops being right by itself, with nothing to clear
    /// it, such as an invite's page when the invite expires.
    expires: Option<Instant>,
}

impl PageCache {
    /// A cache that holds at most about `max_bytes` of pages.
    pub fn new(max_bytes: usize) -> PageCache {
        PageCache {
            max_bytes,
            inner: Mutex::new(Inner::default()),
        }
    }

    /// A cache that never stores anything.
    pub fn disabled() -> PageCache {
        PageCache::new(0)
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // A panic while holding the lock can't leave the maps inconsistent
        // in a way that matters more than losing the cache, so carry on.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn get(&self, key: &str) -> Option<Arc<CachedPage>> {
        self.get_at(key, Instant::now())
    }

    pub fn get_at(&self, key: &str, now: Instant) -> Option<Arc<CachedPage>> {
        let mut inner = self.lock();
        let inner = &mut *inner;
        inner.clock += 1;
        if inner
            .entries
            .get(key)?
            .expires
            .is_some_and(|expires| expires <= now)
        {
            inner.remove(key);
            return None;
        }
        let entry = inner.entries.get_mut(key)?;
        inner.lru.remove(&entry.used);
        entry.used = inner.clock;
        inner.lru.insert(entry.used, key.to_owned());
        Some(entry.page.clone())
    }

    /// Call before reading anything that the page will be rendered from.
    pub fn ticket(&self) -> Ticket {
        Ticket {
            epoch: self.lock().epoch,
        }
    }

    /// Stores a page rendered after `ticket` was issued. Returns whether it
    /// was stored: it isn't if one of its tags has been cleared since, or if
    /// it doesn't fit.
    pub fn insert(&self, ticket: Ticket, key: String, page: CachedPage, tags: &[Tag]) -> bool {
        self.insert_until(ticket, key, page, tags, None)
    }

    /// Like [`PageCache::insert`], for a page that must not be served after
    /// `expires`.
    pub fn insert_until(
        &self,
        ticket: Ticket,
        key: String,
        page: CachedPage,
        tags: &[Tag],
        expires: Option<Instant>,
    ) -> bool {
        let size = key.len() + page.body.len() + page.content_type.len() + ENTRY_OVERHEAD;
        if size > self.max_bytes {
            return false;
        }
        let mut inner = self.lock();
        if ticket.epoch < inner.floor
            || tags
                .iter()
                .any(|tag| inner.cleared_at.get(tag).is_some_and(|&e| e > ticket.epoch))
        {
            return false;
        }
        inner.remove(&key);
        while inner.bytes + size > self.max_bytes {
            let (_, oldest) = inner.lru.pop_first().expect("bytes > 0 implies entries");
            inner.remove(&oldest);
        }
        let mut tags = tags.to_vec();
        tags.sort();
        tags.dedup();
        for tag in &tags {
            inner.by_tag.entry(*tag).or_default().insert(key.clone());
        }
        inner.clock += 1;
        let used = inner.clock;
        inner.lru.insert(used, key.clone());
        inner.bytes += size;
        inner.entries.insert(
            key,
            Entry {
                page: Arc::new(page),
                tags,
                used,
                size,
                expires,
            },
        );
        true
    }

    /// Removes every page carrying any of these tags.
    pub fn invalidate(&self, tags: &[Tag]) {
        let mut inner = self.lock();
        inner.epoch += 1;
        let epoch = inner.epoch;
        if inner.cleared_at.len() + tags.len() > MAX_REMEMBERED_INVALIDATIONS {
            inner.cleared_at.clear();
            inner.floor = epoch;
        }
        for tag in tags {
            inner.cleared_at.insert(*tag, epoch);
            if let Some(keys) = inner.by_tag.remove(tag) {
                for key in keys {
                    inner.remove(&key);
                }
            }
        }
    }

    pub fn len(&self) -> usize {
        self.lock().entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The bytes counted against the budget.
    pub fn bytes(&self) -> usize {
        self.lock().bytes
    }
}

impl Inner {
    fn remove(&mut self, key: &str) {
        let Some(entry) = self.entries.remove(key) else {
            return;
        };
        self.lru.remove(&entry.used);
        self.bytes -= entry.size;
        for tag in &entry.tags {
            if let Some(keys) = self.by_tag.get_mut(tag) {
                keys.remove(key);
                if keys.is_empty() {
                    self.by_tag.remove(tag);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hegel::TestCase;
    use hegel::generators::{self as gs, Generator, PrintableGenerator};

    fn tags() -> impl PrintableGenerator<Vec<Tag>> {
        gs::vecs(gs::sampled_from(vec![
            Tag::Site,
            Tag::Category(1),
            Tag::Category(2),
            Tag::Thread(1),
            Tag::Thread(2),
            Tag::Thread(3),
            Tag::User(1),
            Tag::Members,
            Tag::Invite(1),
        ]))
        .max_size(4)
        .print_as_debug()
    }

    fn keys() -> impl PrintableGenerator<String> {
        gs::sampled_from(vec![
            "/",
            "/latest",
            "/c/a",
            "/t/1/x",
            "/t/2/y",
            "/t/3/z?page=2",
        ])
        .map(|k| k.to_owned())
        .print_as_debug()
    }

    fn page(tc: &TestCase) -> CachedPage {
        let body = tc.draw(gs::text().max_size(300));
        CachedPage::new("text/html".into(), Bytes::from(body))
    }

    /// The cache against a model that never evicts. The cache may forget
    /// entries to stay in budget, but anything it returns must be exactly
    /// what the model holds: never a page whose tags have been cleared.
    struct CacheModel {
        cache: PageCache,
        model: HashMap<String, (String, Vec<Tag>)>,
        /// Tickets not yet used, with the tags cleared since each was issued.
        tickets: Vec<(Ticket, HashSet<Tag>)>,
        unbounded: bool,
    }

    #[hegel::state_machine]
    impl CacheModel {
        #[rule]
        fn take_ticket(&mut self, _: TestCase) {
            self.tickets.push((self.cache.ticket(), HashSet::new()));
        }

        #[rule]
        fn insert(&mut self, tc: TestCase) {
            tc.assume(!self.tickets.is_empty());
            let index = tc.draw(gs::integers::<usize>().max_value(self.tickets.len() - 1));
            let (ticket, cleared) = self.tickets.remove(index);
            let key = tc.draw(keys());
            let tags = tc.draw(tags());
            let page = page(&tc);
            let body = String::from_utf8(page.body.to_vec()).unwrap();
            let stored = self.cache.insert(ticket, key.clone(), page, &tags);
            let stale = tags.iter().any(|t| cleared.contains(t));
            if stale {
                assert!(
                    !stored,
                    "stored a page rendered before {cleared:?} were cleared"
                );
            }
            if self.unbounded {
                assert_eq!(stored, !stale);
            }
            if stored {
                self.model.insert(key, (body, tags));
            }
        }

        #[rule]
        fn invalidate(&mut self, tc: TestCase) {
            let tags = tc.draw(tags());
            self.cache.invalidate(&tags);
            self.model
                .retain(|_, (_, entry_tags)| !entry_tags.iter().any(|t| tags.contains(t)));
            for (_, cleared) in &mut self.tickets {
                cleared.extend(tags.iter().copied());
            }
        }

        #[rule]
        fn get(&mut self, tc: TestCase) {
            let key = tc.draw(keys());
            let got = self.cache.get(&key);
            match (&got, self.model.get(&key)) {
                (Some(page), Some((body, _))) => {
                    assert_eq!(page.body, body.as_bytes());
                    assert_eq!(page.etag, etag_for(body.as_bytes()));
                }
                (Some(page), None) => {
                    panic!("cache returned {page:?} for {key} but should be empty")
                }
                (None, Some(_)) => {
                    assert!(!self.unbounded, "unbounded cache lost {key}");
                    self.model.remove(&key);
                }
                (None, None) => {}
            }
        }

        #[invariant]
        fn within_budget(&self, _: TestCase) {
            assert!(self.cache.bytes() <= self.cache.max_bytes);
            assert!(self.cache.len() <= self.model.len());
            if self.unbounded {
                assert_eq!(self.cache.len(), self.model.len());
            }
        }
    }

    #[hegel::test(test_cases = 300)]
    fn unbounded_cache_matches_model(tc: TestCase) {
        hegel::stateful::machine(CacheModel {
            cache: PageCache::new(usize::MAX),
            model: HashMap::new(),
            tickets: Vec::new(),
            unbounded: true,
        })
        .run(tc);
    }

    #[hegel::test(test_cases = 300)]
    fn bounded_cache_never_returns_stale_pages(tc: TestCase) {
        let max_bytes = tc.draw(gs::integers::<usize>().max_value(2000));
        hegel::stateful::machine(CacheModel {
            cache: PageCache::new(max_bytes),
            model: HashMap::new(),
            tickets: Vec::new(),
            unbounded: false,
        })
        .run(tc);
    }

    #[test]
    fn forgetting_old_invalidations_refuses_old_tickets() {
        let cache = PageCache::new(usize::MAX);
        let ticket = cache.ticket();
        for i in 0..=(MAX_REMEMBERED_INVALIDATIONS as i64) {
            cache.invalidate(&[Tag::Thread(i)]);
        }
        let page = CachedPage::new("text/html".into(), Bytes::from_static(b"x"));
        assert!(!cache.insert(ticket, "/".into(), page, &[Tag::Site]));
        let page = CachedPage::new("text/html".into(), Bytes::from_static(b"x"));
        assert!(cache.insert(cache.ticket(), "/".into(), page, &[Tag::Site]));
    }

    #[test]
    fn pages_are_not_served_after_they_expire() {
        let cache = PageCache::new(1 << 20);
        let now = Instant::now();
        let later = now + std::time::Duration::from_secs(10);
        let page = || CachedPage::new("text/html".into(), Bytes::from_static(b"hello"));
        assert!(cache.insert_until(
            cache.ticket(),
            "/a".into(),
            page(),
            &[Tag::Site],
            Some(later)
        ));
        assert!(cache.insert(cache.ticket(), "/b".into(), page(), &[Tag::Site]));
        assert!(cache.get_at("/a", now).is_some());
        assert!(cache.get_at("/a", later).is_none());
        // It is gone, and its room is given back.
        assert_eq!(cache.len(), 1);
        assert!(cache.get_at("/a", now).is_none());
        assert!(
            cache
                .get_at("/b", later + later.duration_since(now))
                .is_some()
        );
    }

    #[test]
    fn disabled_cache_stores_nothing() {
        let cache = PageCache::disabled();
        let page = CachedPage::new("text/html".into(), Bytes::from_static(b"x"));
        assert!(!cache.insert(cache.ticket(), "/".into(), page, &[Tag::Site]));
        assert!(cache.get("/").is_none());
    }

    #[test]
    fn etag_is_quoted_and_depends_on_content() {
        let a = etag_for(b"hello");
        assert!(a.starts_with('"') && a.ends_with('"'));
        assert_eq!(a, etag_for(b"hello"));
        assert_ne!(a, etag_for(b"hellp"));
        // A cryptographic hash, so a poster can't craft a page whose ETag
        // matches an earlier version's and leave readers on the old one:
        // the first 128 bits of SHA-256.
        assert_eq!(a, "\"2cf24dba5fb0a30e26e83b2ac5b9e29e\"");
    }
}
