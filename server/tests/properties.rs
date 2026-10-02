//! Property-based tests over random forum histories (DESIGN.md §6, §12).
//!
//! A state machine creates categories, threads, posts and quote-forks, then
//! edits (inside and after the grace period, so that some edits keep
//! history), reacts to, deletes, hides, moves and renames them, changes
//! category levels, and reports each change as a write handler would.
//! Throughout:
//!
//! - every page served through the anonymous cache is identical to the same
//!   page rendered fresh, so invalidation never leaves a stale page behind;
//! - no page contains text from anything its viewer may not read, judged
//!   by a reference permission function written separately here, and
//!   every post the viewer may read can be found on the page its number
//!   says, with its earlier versions on its history page. A deleted or
//!   hidden post's history is shown to nobody, and text replaced within the
//!   grace period is gone for good. This is checked for anonymous visitors, a logged-in member and a
//!   moderator; while the logged-in viewers browse, anonymous pages must
//!   still show only public content, so nothing leaks through the cache.

mod common;

use std::collections::BTreeSet;

use axum::http::StatusCode;
use common::{Forum, Response};
use hegel::generators::{self as gs};
use hegel::{HealthCheck, TestCase};
use parlour::cache::PageCache;
use parlour::changes::Change;
use parlour::content::{POSTS_PER_PAGE, THREADS_PER_PAGE, page_count, page_of_seq};
use parlour::pagination::page_href;
use parlour::store::{self, NewCategory, NewPost, NewThread, NewUser};
use parlour::AppState;
use sqlx::PgPool;
use tokio::runtime::Handle;

const LEVELS: [&str; 3] = ["public", "members", "groups"];

/// Who is looking.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Who {
    Anonymous,
    Member,
    Staff,
}

/// What a run checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// Cached pages match freshly rendered ones.
    Coherence,
    /// Pages show exactly what this viewer may see.
    Visibility(Who),
}

struct CategoryModel {
    id: i64,
    slug: String,
    read_level: &'static str,
    /// The token in the category's current name.
    token: String,
}

struct ThreadModel {
    id: i64,
    category: usize,
    title_token: String,
    deleted: bool,
    last_seq: i64,
}

struct PostModel {
    id: i64,
    thread: usize,
    seq: i64,
    author: i64,
    body_token: String,
    deleted: bool,
    hidden: bool,
    /// Tokens of earlier versions kept in the post's history.
    history: Vec<String>,
    /// Whether the grace period for edits is over (or an edit was kept).
    settled: bool,
}

/// The forum, served twice: through the page cache and without it.
struct World {
    rt: Handle,
    cached: Forum,
    fresh: Forum,
    users: Vec<i64>,
    categories: Vec<CategoryModel>,
    threads: Vec<ThreadModel>,
    posts: Vec<PostModel>,
    /// Every token ever used, including ones edited or renamed away.
    all_tokens: Vec<String>,
    next_token: u32,
    mode: Mode,
    /// Session cookies for the logged-in viewers.
    member_cookie: String,
    staff_cookie: String,
}

impl World {
    fn new(rt: Handle, db: PgPool, mode: Mode) -> World {
        rt.block_on(async {
            sqlx::raw_sql(
                "TRUNCATE settings, posts, threads, categories, sections, users RESTART IDENTITY CASCADE",
            )
            .execute(&db)
            .await
            .unwrap();
        });
        let mut world = World {
            cached: Forum {
                state: AppState::new(db.clone()),
                db: db.clone(),
            },
            fresh: Forum {
                state: AppState::with_cache(db.clone(), PageCache::disabled()),
                db,
            },
            rt,
            users: Vec::new(),
            categories: Vec::new(),
            threads: Vec::new(),
            posts: Vec::new(),
            all_tokens: Vec::new(),
            next_token: 0,
            mode,
            member_cookie: String::new(),
            staff_cookie: String::new(),
        };
        for name in ["ruth", "jonah", "maya"] {
            let id = world
                .block(store::create_user(
                    &world.cached.db,
                    &NewUser {
                        username: name.into(),
                        display_name: common::capitalise(name),
                        email: format!("{name}@example.com"),
                        ..NewUser::default()
                    },
                ))
                .unwrap();
            world.users.push(id);
        }
        // Viewers who log in. They write nothing, so they don't appear on
        // any page.
        for (name, role) in [("viewer", "user"), ("watcher", "moderator")] {
            let id = world
                .block(store::create_user(
                    &world.cached.db,
                    &NewUser {
                        username: name.into(),
                        display_name: common::capitalise(name),
                        email: format!("{name}@example.com"),
                        role: Some(role.into()),
                        ..NewUser::default()
                    },
                ))
                .unwrap();
            let cookie = world.block(world.cached.session(id));
            if role == "user" {
                world.member_cookie = cookie;
            } else {
                world.staff_cookie = cookie;
            }
        }
        for level in ["public", "public", "members", "groups"] {
            world.add_category(level);
        }
        world
    }

    fn block<F: Future>(&self, f: F) -> F::Output {
        self.rt.block_on(f)
    }

    fn token(&mut self) -> String {
        self.next_token += 1;
        let token = format!("tok{:05}x", self.next_token);
        self.all_tokens.push(token.clone());
        token
    }

    fn db(&self) -> &PgPool {
        &self.cached.db
    }

    fn changed(&self, change: Change) {
        self.block(self.cached.changed(change));
    }

    fn exec(&self, sql: String) {
        self.block(self.cached.exec(&sql));
    }

    fn add_category(&mut self, level: &'static str) {
        let token = self.token();
        let slug = format!("c{}", self.categories.len());
        let id = self
            .block(store::create_category(
                self.db(),
                &NewCategory {
                    section_id: None,
                    slug: slug.clone(),
                    name: format!("Category {token}"),
                    description: String::new(),
                    position: self.categories.len() as i32,
                    read_level: level.into(),
                    write_level: "members".into(),
                },
            ))
            .unwrap();
        self.changed(Change::CategoryChanged { category_id: id });
        self.categories.push(CategoryModel {
            id,
            slug,
            read_level: level,
            token,
        });
    }

    fn add_post(&mut self, thread: usize, author: i64, reply_to: Option<usize>) {
        let token = self.token();
        let id = self
            .block(store::create_post(
                self.db(),
                &NewPost {
                    thread_id: self.threads[thread].id,
                    author_id: author,
                    body_md: format!("Post {token}"),
                    reply_to_post_id: reply_to.map(|p| self.posts[p].id),
                    created_at: None,
                },
            ))
            .unwrap();
        self.threads[thread].last_seq += 1;
        self.posts.push(PostModel {
            id,
            thread,
            seq: self.threads[thread].last_seq,
            author,
            body_token: token,
            deleted: false,
            hidden: false,
            history: Vec::new(),
            settled: false,
        });
        self.changed(Change::PostCreated { post_id: id });
    }

    // -- The reference permission function, independent of the app's SQL.

    /// DESIGN.md §3.1: anyone reads public categories, members also read
    /// members-only ones, and staff read everything. (Group membership
    /// arrives in M5; until then only staff read group categories.)
    fn category_readable(&self, category: usize, who: Who) -> bool {
        matches!(
            (self.categories[category].read_level, who),
            ("public", _) | ("members", Who::Member | Who::Staff) | (_, Who::Staff)
        )
    }

    fn thread_readable(&self, thread: usize, who: Who) -> bool {
        let t = &self.threads[thread];
        !t.deleted && self.category_readable(t.category, who)
    }

    fn post_readable(&self, post: usize, who: Who) -> bool {
        let p = &self.posts[post];
        !p.deleted && !p.hidden && self.thread_readable(p.thread, who)
    }

    /// Tokens this viewer may see somewhere.
    fn visible_tokens(&self, who: Who) -> BTreeSet<&str> {
        let mut tokens = BTreeSet::new();
        for (i, c) in self.categories.iter().enumerate() {
            if self.category_readable(i, who) {
                tokens.insert(c.token.as_str());
            }
        }
        for (i, t) in self.threads.iter().enumerate() {
            if self.thread_readable(i, who) {
                tokens.insert(t.title_token.as_str());
            }
        }
        for (i, p) in self.posts.iter().enumerate() {
            if self.post_readable(i, who) {
                tokens.insert(p.body_token.as_str());
                tokens.extend(p.history.iter().map(String::as_str));
            }
        }
        tokens
    }

    fn cookie(&self, who: Who) -> Option<&str> {
        match who {
            Who::Anonymous => None,
            Who::Member => Some(&self.member_cookie),
            Who::Staff => Some(&self.staff_cookie),
        }
    }

    fn paths(&self, who: Who) -> Vec<String> {
        let mut paths = vec!["/".to_owned(), "/users/tree".to_owned()];
        for name in ["ruth", "jonah", "maya"] {
            paths.push(format!("/u/{name}"));
        }
        let visible_threads = (0..self.threads.len())
            .filter(|&t| self.thread_readable(t, who))
            .count() as i64;
        for page in 1..=page_count(visible_threads, THREADS_PER_PAGE) + 1 {
            paths.push(page_href("/latest", page));
        }
        for (i, c) in self.categories.iter().enumerate() {
            let threads = self
                .threads
                .iter()
                .filter(|t| t.category == i && !t.deleted)
                .count() as i64;
            for page in 1..=page_count(threads, THREADS_PER_PAGE) + 1 {
                paths.push(page_href(&format!("/c/{}", c.slug), page));
            }
        }
        for t in &self.threads {
            paths.push(format!("/t/{}", t.id));
            paths.push(format!("/t/{}/unread", t.id));
            for page in 1..=page_count(t.last_seq, POSTS_PER_PAGE) + 1 {
                paths.push(page_href(&format!("/t/{}/{}", t.id, self.slug_of(t)), page));
            }
        }
        for p in &self.posts {
            paths.push(format!("/p/{}", p.id));
            paths.push(format!("/p/{}/revisions", p.id));
            paths.push(format!("/p/{}/revisions?v=1", p.id));
            paths.push(format!("/p/{}/reactions", p.id));
        }
        paths
    }

    fn slug_of(&self, t: &ThreadModel) -> String {
        store::slugify(&format!("Thread {}", t.title_token))
    }

    fn get(&self, forum: &Forum, path: &str) -> Response {
        self.block(forum.get(path))
    }

    fn get_as(&self, who: Who, path: &str) -> Response {
        self.block(self.cached.get_as(path, self.cookie(who)))
    }

    fn check(&self) {
        match self.mode {
            Mode::Coherence => self.check_coherence(),
            Mode::Visibility(who) => {
                self.check_visibility(who);
                if who != Who::Anonymous {
                    // Logged-in browsing never leaks into what the anonymous
                    // cache serves.
                    self.check_visibility(Who::Anonymous);
                }
            }
        }
    }

    fn check_coherence(&self) {
        for path in self.paths(Who::Anonymous) {
            let cached = self.get(&self.cached, &path);
            let fresh = self.get(&self.fresh, &path);
            assert_eq!(cached.status, fresh.status, "{path}");
            assert_eq!(
                cached.header(axum::http::header::LOCATION),
                fresh.header(axum::http::header::LOCATION),
                "{path}"
            );
            assert!(
                cached.body == fresh.body,
                "cached {path} is stale:\n{}\n---- fresh:\n{}",
                cached.body,
                fresh.body
            );
        }
    }

    fn check_visibility(&self, who: Who) {
        let visible = self.visible_tokens(who);
        let forbidden: Vec<&String> = self
            .all_tokens
            .iter()
            .filter(|t| !visible.contains(t.as_str()))
            .collect();
        for path in self.paths(who) {
            let response = self.get_as(who, &path);
            assert!(
                !response
                    .headers
                    .contains_key(axum::http::header::SET_COOKIE),
                "{who:?} {path}"
            );
            if who != Who::Anonymous {
                assert_eq!(
                    response.header(axum::http::header::CACHE_CONTROL),
                    Some(parlour::http_cache::PRIVATE_CACHE_CONTROL),
                    "{path}"
                );
            }
            for token in &forbidden {
                assert!(
                    !response.body.contains(token.as_str()),
                    "{path} shows {token} to {who:?}:\n{}",
                    response.body
                );
            }
        }
        // Visible things can be found where they should be, and invisible
        // things are not found at all.
        for (i, t) in self.threads.iter().enumerate() {
            let response = self.get_as(who, &format!("/t/{}/{}", t.id, self.slug_of(t)));
            if self.thread_readable(i, who) {
                assert_eq!(response.status, StatusCode::OK);
                assert!(response.body.contains(&t.title_token));
            } else {
                assert_eq!(response.status, StatusCode::NOT_FOUND);
            }
        }
        for (i, p) in self.posts.iter().enumerate() {
            let permalink = self.get_as(who, &format!("/p/{}", p.id));
            if !self.thread_readable(p.thread, who) {
                assert_eq!(permalink.status, StatusCode::NOT_FOUND);
                continue;
            }
            let t = &self.threads[p.thread];
            let page = page_of_seq(p.seq);
            let expected = if page == 1 {
                format!("/t/{}/{}#p{}", t.id, self.slug_of(t), p.id)
            } else {
                format!("/t/{}/{}?page={page}#p{}", t.id, self.slug_of(t), p.id)
            };
            assert_eq!(permalink.location(), expected);
            let history = self.get_as(who, &format!("/p/{}/revisions", p.id));
            if self.post_readable(i, who) {
                let body = self.get_as(who, expected.split('#').next().unwrap()).body;
                assert!(
                    body.contains(&p.body_token),
                    "post {} missing from {expected}",
                    p.id
                );
                assert_eq!(history.status, StatusCode::OK);
                assert!(history.body.contains(&p.body_token));
                assert!(history.body.contains(&format!(
                    "{} version",
                    p.history.len() + 1
                )));
                for (n, token) in p.history.iter().enumerate() {
                    let version = self.get_as(who, &format!("/p/{}/revisions?v={}", p.id, n + 1));
                    assert!(
                        version.body.contains(token),
                        "version {} of post {} lacks {token}",
                        n + 1,
                        p.id
                    );
                }
            } else {
                // Deleted and hidden posts take their history with them.
                assert_eq!(history.status, StatusCode::NOT_FOUND, "post {}", p.id);
            }
        }
        for (i, c) in self.categories.iter().enumerate() {
            let response = self.get_as(who, &format!("/c/{}", c.slug));
            let expected = if self.category_readable(i, who) {
                StatusCode::OK
            } else {
                StatusCode::NOT_FOUND
            };
            assert_eq!(response.status, expected);
        }
    }
}

#[hegel::state_machine]
impl World {
    #[rule]
    fn create_thread(&mut self, tc: TestCase) {
        let category = tc.draw(gs::integers::<usize>().max_value(self.categories.len() - 1));
        let author = tc.draw(gs::sampled_from(self.users.clone()));
        let fork_of = if self.posts.is_empty() || !tc.draw(gs::booleans()) {
            None
        } else {
            Some(tc.draw(gs::integers::<usize>().max_value(self.posts.len() - 1)))
        };
        let pinned = tc.draw(gs::booleans());
        let title_token = self.token();
        let body_token = self.token();
        let created = self
            .block(store::create_thread(
                self.db(),
                &NewThread {
                    category_id: self.categories[category].id,
                    title: format!("Thread {title_token}"),
                    author_id: author,
                    body_md: format!("Opening {body_token}"),
                    forked_from_post_id: fork_of.map(|p| self.posts[p].id),
                    pinned,
                    locked: false,
                    created_at: None,
                },
            ))
            .unwrap();
        self.threads.push(ThreadModel {
            id: created.thread_id,
            category,
            title_token,
            deleted: false,
            last_seq: 1,
        });
        self.posts.push(PostModel {
            id: created.first_post_id,
            thread: self.threads.len() - 1,
            seq: 1,
            author,
            body_token,
            deleted: false,
            hidden: false,
            history: Vec::new(),
            settled: false,
        });
        self.changed(Change::PostCreated {
            post_id: created.first_post_id,
        });
    }

    #[rule]
    fn reply(&mut self, tc: TestCase) {
        tc.assume(!self.threads.is_empty());
        let thread = tc.draw(gs::integers::<usize>().max_value(self.threads.len() - 1));
        let author = tc.draw(gs::sampled_from(self.users.clone()));
        let in_thread: Vec<usize> = (0..self.posts.len())
            .filter(|&p| self.posts[p].thread == thread)
            .collect();
        let reply_to = if tc.draw(gs::booleans()) {
            Some(tc.draw(gs::sampled_from(in_thread)))
        } else {
            None
        };
        self.add_post(thread, author, reply_to);
    }

    /// Enough posts to spill onto further pages.
    #[rule]
    fn many_replies(&mut self, tc: TestCase) {
        tc.assume(!self.threads.is_empty());
        let thread = tc.draw(gs::integers::<usize>().max_value(self.threads.len() - 1));
        let count = tc.draw(gs::integers::<usize>().min_value(1).max_value(60));
        for i in 0..count {
            self.add_post(thread, self.users[i % self.users.len()], None);
        }
    }

    #[rule]
    fn edit_post(&mut self, tc: TestCase) {
        tc.assume(!self.posts.is_empty());
        let post = tc.draw(gs::integers::<usize>().max_value(self.posts.len() - 1));
        // Either within the grace period after posting, or long after.
        if !self.posts[post].settled && tc.draw(gs::booleans()) {
            self.exec(format!(
                "UPDATE posts SET created_at = created_at - interval '1 hour' WHERE id = {}",
                self.posts[post].id
            ));
            self.posts[post].settled = true;
            // Only for this test: the post's time shows on pages, so they
            // must be cleared like after an edit.
            self.changed(Change::PostEdited {
                post_id: self.posts[post].id,
            });
        }
        let token = self.token();
        let body = format!("Edited {token}");
        let edited = self
            .block(store::edit_post(
                self.db(),
                self.posts[post].id,
                self.posts[post].author,
                &body,
            ))
            .unwrap();
        let p = &mut self.posts[post];
        let old = std::mem::replace(&mut p.body_token, token);
        if p.settled {
            assert!(matches!(edited, store::Edited::Revised { .. }));
            p.history.push(old);
        } else {
            // Replaced within the grace period: the old text is gone.
            assert_eq!(edited, store::Edited::Replaced);
        }
        self.changed(Change::PostEdited {
            post_id: self.posts[post].id,
        });
    }

    #[rule]
    fn react(&mut self, tc: TestCase) {
        tc.assume(!self.posts.is_empty());
        let post = tc.draw(gs::integers::<usize>().max_value(self.posts.len() - 1));
        let user = tc.draw(gs::sampled_from(self.users.clone()));
        let emoji = tc.draw(gs::sampled_from(vec!["👍", "❤️", "😂"]));
        let id = self.posts[post].id;
        let changed = if tc.draw(gs::booleans()) {
            self.block(store::add_reaction(self.db(), id, user, emoji))
        } else {
            self.block(store::remove_reaction(self.db(), id, user, emoji))
        }
        .unwrap();
        if changed {
            self.changed(Change::ReactionChanged { post_id: id });
        }
    }

    #[rule]
    fn toggle_post_removal(&mut self, tc: TestCase) {
        tc.assume(!self.posts.is_empty());
        let post = tc.draw(gs::integers::<usize>().max_value(self.posts.len() - 1));
        let hide = tc.draw(gs::booleans());
        let p = &mut self.posts[post];
        if hide {
            p.hidden = !p.hidden;
        } else {
            p.deleted = !p.deleted;
        }
        let (id, deleted, hidden) = (p.id, p.deleted, p.hidden);
        self.exec(format!(
            "UPDATE posts SET deleted_at = {}, hidden_at = {} WHERE id = {id}",
            if deleted { "now()" } else { "NULL" },
            if hidden { "now()" } else { "NULL" },
        ));
        self.changed(Change::PostRemoved { post_id: id });
    }

    #[rule]
    fn move_thread(&mut self, tc: TestCase) {
        tc.assume(!self.threads.is_empty());
        let thread = tc.draw(gs::integers::<usize>().max_value(self.threads.len() - 1));
        let category = tc.draw(gs::integers::<usize>().max_value(self.categories.len() - 1));
        let previous = self.threads[thread].category;
        self.threads[thread].category = category;
        self.exec(format!(
            "UPDATE threads SET category_id = {} WHERE id = {}",
            self.categories[category].id, self.threads[thread].id
        ));
        self.changed(Change::ThreadChanged {
            thread_id: self.threads[thread].id,
            previous_category_id: Some(self.categories[previous].id),
        });
    }

    #[rule]
    fn change_thread(&mut self, tc: TestCase) {
        tc.assume(!self.threads.is_empty());
        let thread = tc.draw(gs::integers::<usize>().max_value(self.threads.len() - 1));
        let id = self.threads[thread].id;
        match tc.draw(gs::sampled_from(vec!["rename", "pin", "lock", "delete"])) {
            "rename" => {
                let token = self.token();
                let title = format!("Thread {token}");
                self.exec(format!(
                    "UPDATE threads SET title = '{title}', slug = '{}' WHERE id = {id}",
                    store::slugify(&title)
                ));
                self.threads[thread].title_token = token;
            }
            "pin" => self.exec(format!(
                "UPDATE threads SET pinned = NOT pinned WHERE id = {id}"
            )),
            "lock" => self.exec(format!(
                "UPDATE threads SET locked = NOT locked WHERE id = {id}"
            )),
            _ => {
                let t = &mut self.threads[thread];
                t.deleted = !t.deleted;
                let deleted = t.deleted;
                self.exec(format!(
                    "UPDATE threads SET deleted_at = {} WHERE id = {id}",
                    if deleted { "now()" } else { "NULL" }
                ));
            }
        }
        self.changed(Change::ThreadChanged {
            thread_id: id,
            previous_category_id: None,
        });
    }

    #[rule]
    fn change_category(&mut self, tc: TestCase) {
        let category = tc.draw(gs::integers::<usize>().max_value(self.categories.len() - 1));
        let id = self.categories[category].id;
        if tc.draw(gs::booleans()) {
            let level = tc.draw(gs::sampled_from(LEVELS.to_vec()));
            self.categories[category].read_level = level;
            self.exec(format!(
                "UPDATE categories SET read_level = '{level}' WHERE id = {id}"
            ));
        } else {
            let token = self.token();
            self.exec(format!(
                "UPDATE categories SET name = 'Category {token}' WHERE id = {id}"
            ));
            self.categories[category].token = token;
        }
        self.changed(Change::CategoryChanged { category_id: id });
    }

    #[rule]
    fn add_category_rule(&mut self, tc: TestCase) {
        tc.assume(self.categories.len() < 7);
        let level = tc.draw(gs::sampled_from(LEVELS.to_vec()));
        self.add_category(level);
    }

    #[rule]
    fn rename_user(&mut self, tc: TestCase) {
        let user = tc.draw(gs::sampled_from(self.users.clone()));
        let name = format!("User {}", self.next_token);
        self.next_token += 1;
        self.block(
            sqlx::query!(
                "UPDATE users SET display_name = $2 WHERE id = $1",
                user,
                name
            )
            .execute(self.db()),
        )
        .unwrap();
        self.changed(Change::UserChanged { user_id: user });
    }

    #[rule]
    fn rename_site(&mut self, _: TestCase) {
        let name = format!("Site {}", self.next_token);
        self.next_token += 1;
        self.exec(format!(
            "INSERT INTO settings (key, value) VALUES ('site_name', to_jsonb('{name}'::text))
             ON CONFLICT (key) DO UPDATE SET value = excluded.value"
        ));
        self.changed(Change::SiteChanged);
    }

    /// Requests every page through the cache, so that later changes have
    /// something to invalidate.
    #[rule]
    fn warm(&mut self, _: TestCase) {
        for path in self.paths(Who::Anonymous) {
            self.get(&self.cached, &path);
        }
        if let Mode::Visibility(who) = self.mode
            && who != Who::Anonymous
        {
            for path in self.paths(who) {
                self.get_as(who, &path);
            }
        }
    }

    /// Checked at the start and end of each run and sampled in between.
    #[invariant]
    fn pages_are_right(&self, _: TestCase) {
        self.check();
    }
}

// Every step writes to Postgres and renders pages over HTTP, so a test
// case takes seconds, and more when the four suites here run side by side.
// Hegel counts that as slow generation; it is expected, so the TooSlow
// health check is off. The number of test cases is unchanged.
#[hegel::standalone_function(test_cases = 100, suppress_health_check = [HealthCheck::TooSlow])]
fn run_machine(tc: TestCase, rt: &Handle, db: &PgPool, mode: Mode) {
    let world = World::new(rt.clone(), db.clone(), mode);
    hegel::stateful::machine(world).steps(25).run(tc);
}

#[hegel::standalone_function(test_cases = 50, suppress_health_check = [HealthCheck::TooSlow])]
fn run_machine_briefly(tc: TestCase, rt: &Handle, db: &PgPool, mode: Mode) {
    let world = World::new(rt.clone(), db.clone(), mode);
    hegel::stateful::machine(world).steps(25).run(tc);
}

async fn run_in_blocking_thread(pool: PgPool, mode: Mode) {
    let rt = Handle::current();
    tokio::task::spawn_blocking(move || match mode {
        Mode::Visibility(Who::Member | Who::Staff) => run_machine_briefly(&rt, &pool, mode),
        _ => run_machine(&rt, &pool, mode),
    })
    .await
    .unwrap();
}

/// Cached pages always match freshly rendered ones.
#[sqlx::test]
async fn cache_is_coherent(pool: PgPool) {
    run_in_blocking_thread(pool, Mode::Coherence).await;
}

/// Anonymous pages show exactly what the reference function allows.
#[sqlx::test]
async fn anonymous_pages_show_only_public_content(pool: PgPool) {
    run_in_blocking_thread(pool, Mode::Visibility(Who::Anonymous)).await;
}

/// A logged-in member sees public and members-only content and nothing
/// else, and their browsing never reaches anonymous visitors.
#[sqlx::test]
async fn members_see_only_what_members_may(pool: PgPool) {
    run_in_blocking_thread(pool, Mode::Visibility(Who::Member)).await;
}

/// Staff see every category, and their browsing never reaches anonymous
/// visitors either.
#[sqlx::test]
async fn staff_see_every_category(pool: PgPool) {
    run_in_blocking_thread(pool, Mode::Visibility(Who::Staff)).await;
}
