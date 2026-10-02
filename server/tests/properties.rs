//! Property-based tests over random forum histories (DESIGN.md §6, §12).
//!
//! A state machine creates categories, threads, posts and quote-forks, then
//! edits (inside and after the grace period, so that some edits keep
//! history), reacts to, deletes, hides, moves and renames them, changes
//! category levels and read groups, moves the logged-in member in and out
//! of groups, and reports each change as a write handler would.
//! Throughout:
//!
//! - every page served through the anonymous cache is identical to the same
//!   page rendered fresh, so invalidation never leaves a stale page behind;
//! - no page contains text from anything its viewer may not read, judged
//!   by a reference permission function written separately here, and
//!   every post the viewer may read can be found on the page its number
//!   says, with its earlier versions on its history page. A deleted or
//!   hidden post's history is shown to nobody, and text replaced within the
//!   grace period is gone for good. Quote-fork embeds and backlinks are
//!   covered too: a source post's text, or a fork's title, on a page its
//!   viewer may not see it on is a leak. This is checked for anonymous
//!   visitors, a logged-in member and a moderator; while the logged-in viewers browse, anonymous pages must
//!   still show only public content, so nothing leaks through the cache.

mod common;

use std::collections::{BTreeSet, HashMap};

use axum::http::StatusCode;
use common::{Forum, Response};
use hegel::generators::{self as gs, Generator};
use hegel::{HealthCheck, TestCase};
use parlour::AppState;
use parlour::accounts::Role;
use parlour::cache::PageCache;
use parlour::changes::Change;
use parlour::content::{
    self, Deps, Embed, POSTS_PER_PAGE, PostBody, THREADS_PER_PAGE, page_count, page_of_seq,
};
use parlour::db::Db;
use parlour::groups::{self, GroupFields};
use parlour::pagination::page_href;
use parlour::sessions;
use parlour::store::{self, CategorySettings, NewCategory, NewPost, NewThread, NewUser};
use parlour::users::{self, RoleChange};
use parlour::viewer::Viewer;
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
    /// The groups that may read it, as indexes into `World::groups`.
    read_groups: BTreeSet<usize>,
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
    /// Group ids, and the member viewer's id and groups (indexes into
    /// `groups`).
    groups: Vec<i64>,
    member_id: i64,
    member_groups: BTreeSet<usize>,
}

impl World {
    fn new(rt: Handle, db: PgPool, mode: Mode) -> World {
        rt.block_on(async {
            sqlx::raw_sql(
                "TRUNCATE settings, posts, threads, categories, sections, groups, users RESTART IDENTITY CASCADE",
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
            groups: Vec::new(),
            member_id: 0,
            member_groups: BTreeSet::new(),
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
                world.member_id = id;
            } else {
                world.staff_cookie = cookie;
            }
        }
        for n in 0..2 {
            let id = world.block(world.cached.group(&format!("g{n}"), &format!("Group {n}")));
            world.groups.push(id);
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
            read_groups: BTreeSet::new(),
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
    /// members-only ones, members of a category's read groups read it, and
    /// staff read everything.
    fn category_readable(&self, category: usize, who: Who) -> bool {
        let c = &self.categories[category];
        match (c.read_level, who) {
            ("public", _) | ("members", Who::Member | Who::Staff) | (_, Who::Staff) => true,
            ("groups", Who::Member) => !self.member_groups.is_disjoint(&c.read_groups),
            _ => false,
        }
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
                assert!(
                    history
                        .body
                        .contains(&format!("{} version", p.history.len() + 1))
                );
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

    /// The member viewer joins or leaves a group.
    #[rule]
    fn toggle_membership(&mut self, tc: TestCase) {
        let g = tc.draw(gs::integers::<usize>().max_value(self.groups.len() - 1));
        let db = self.cached.state.db();
        if self.member_groups.remove(&g) {
            assert!(
                self.block(groups::remove_member(db, self.groups[g], self.member_id))
                    .unwrap()
            );
        } else {
            self.block(self.cached.join_group(self.groups[g], self.member_id));
            self.member_groups.insert(g);
        }
    }

    /// An admin changes which groups may read a category, as the category
    /// form does.
    #[rule]
    fn set_read_groups(&mut self, tc: TestCase) {
        let c = tc.draw(gs::integers::<usize>().max_value(self.categories.len() - 1));
        let read_groups: BTreeSet<usize> = (0..self.groups.len())
            .filter(|_| tc.draw(gs::booleans()))
            .collect();
        let category = &self.categories[c];
        let settings = CategorySettings {
            section_id: None,
            slug: category.slug.clone(),
            name: format!("Category {}", category.token),
            description: String::new(),
            position: c as i32,
            read_level: category.read_level.into(),
            write_level: "members".into(),
            read_groups: read_groups.iter().map(|&g| self.groups[g]).collect(),
            write_groups: Vec::new(),
        }
        .clean()
        .unwrap();
        self.block(store::update_category(
            self.cached.state.db(),
            category.id,
            &settings,
        ))
        .unwrap()
        .unwrap();
        let id = category.id;
        self.categories[c].read_groups = if settings.read_level == "groups" {
            read_groups
        } else {
            BTreeSet::new()
        };
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

// ---------------------------------------------------------------------------
// The access layer itself (DESIGN.md §6)
//
// A second state machine builds random forums out of groups, memberships,
// roles, categories with read levels and read groups, threads, posts,
// quote-forks, moves and deletions, and asks the `content` module directly
// what each viewer gets. Every answer is checked, in both directions,
// against `Access::reads_category` and friends below, which are written from
// DESIGN.md §3.1 and know nothing of the SQL.

struct UserModel {
    id: i64,
    staff: bool,
    groups: BTreeSet<usize>,
}

struct AccessCategory {
    id: i64,
    slug: String,
    level: &'static str,
    read_groups: BTreeSet<usize>,
}

struct AccessThread {
    id: i64,
    category: usize,
    deleted: bool,
    /// The post this thread quote-forks.
    fork_of: Option<usize>,
}

struct AccessPost {
    id: i64,
    thread: usize,
    author: usize,
    deleted: bool,
    hidden: bool,
}

struct Access {
    rt: Handle,
    pool: PgPool,
    db: Db,
    users: Vec<UserModel>,
    groups: Vec<i64>,
    categories: Vec<AccessCategory>,
    threads: Vec<AccessThread>,
    posts: Vec<AccessPost>,
}

/// The members who never change role; there is always an admin among them.
const FIXED_USERS: usize = 1;

impl Access {
    fn new(rt: Handle, pool: PgPool) -> Access {
        rt.block_on(async {
            sqlx::raw_sql(
                "TRUNCATE settings, posts, threads, categories, sections, groups, users RESTART IDENTITY CASCADE",
            )
            .execute(&pool)
            .await
            .unwrap();
        });
        let mut world = Access {
            db: Db::new(pool.clone()),
            pool,
            rt,
            users: Vec::new(),
            groups: Vec::new(),
            categories: Vec::new(),
            threads: Vec::new(),
            posts: Vec::new(),
        };
        for (name, role) in [
            ("boss", "admin"),
            ("ruth", "user"),
            ("jonah", "user"),
            ("maya", "user"),
            ("kit", "moderator"),
        ] {
            let id = world
                .block(store::create_user(
                    &world.pool,
                    &NewUser {
                        username: name.into(),
                        display_name: common::capitalise(name),
                        email: format!("{name}@example.com"),
                        role: Some(role.into()),
                        ..NewUser::default()
                    },
                ))
                .unwrap();
            world.users.push(UserModel {
                id,
                staff: role != "user",
                groups: BTreeSet::new(),
            });
        }
        for _ in 0..2 {
            world.add_group();
        }
        for level in ["public", "members", "groups"] {
            world.add_category(level);
        }
        world
    }

    fn block<F: Future>(&self, f: F) -> F::Output {
        self.rt.block_on(f)
    }

    fn exec(&self, sql: String) {
        self.block(sqlx::raw_sql(sqlx::AssertSqlSafe(sql)).execute(&self.pool))
            .unwrap();
    }

    fn add_group(&mut self) {
        let n = self.groups.len();
        let id = self
            .block(groups::create(
                &self.db,
                &GroupFields {
                    slug: format!("g{n}"),
                    name: format!("Group {n}"),
                    description: String::new(),
                },
            ))
            .unwrap()
            .unwrap();
        self.groups.push(id);
    }

    fn add_category(&mut self, level: &'static str) {
        let n = self.categories.len();
        let slug = format!("c{n}");
        let id = self
            .block(store::create_category(
                &self.pool,
                &NewCategory {
                    section_id: None,
                    slug: slug.clone(),
                    name: format!("Category {n}"),
                    description: String::new(),
                    position: n as i32,
                    read_level: level.into(),
                    write_level: "members".into(),
                },
            ))
            .unwrap();
        self.categories.push(AccessCategory {
            id,
            slug,
            level,
            read_groups: BTreeSet::new(),
        });
    }

    fn add_post(&mut self, thread: usize, author: usize) {
        let id = self
            .block(store::create_post(
                &self.pool,
                &NewPost {
                    thread_id: self.threads[thread].id,
                    author_id: self.users[author].id,
                    body_md: "Text".into(),
                    reply_to_post_id: None,
                    created_at: None,
                },
            ))
            .unwrap();
        self.posts.push(AccessPost {
            id,
            thread,
            author,
            deleted: false,
            hidden: false,
        });
    }

    // -- The reference permission function, from DESIGN.md §3.1.

    /// `who` is an index into `users`, or `None` for an anonymous visitor.
    fn reads_category(&self, who: Option<usize>, category: usize) -> bool {
        let c = &self.categories[category];
        match (c.level, who) {
            ("public", _) => true,
            (_, None) => false,
            ("members", Some(_)) => true,
            (_, Some(u)) => {
                let user = &self.users[u];
                user.staff || !user.groups.is_disjoint(&c.read_groups)
            }
        }
    }

    fn reads_thread(&self, who: Option<usize>, thread: usize) -> bool {
        let t = &self.threads[thread];
        !t.deleted && self.reads_category(who, t.category)
    }

    fn post_live(&self, post: usize) -> bool {
        !self.posts[post].deleted && !self.posts[post].hidden
    }

    /// Whether the viewer may see the post's text.
    fn reads_post(&self, who: Option<usize>, post: usize) -> bool {
        self.post_live(post) && self.reads_thread(who, self.posts[post].thread)
    }

    fn viewer(&self, who: Option<usize>) -> Viewer {
        match who {
            None => Viewer::Anonymous,
            Some(u) => Viewer::User(
                self.block(sessions::user_ctx(&self.db, self.users[u].id))
                    .unwrap()
                    .expect("the member exists"),
            ),
        }
    }

    fn ids<T>(items: &[T], keep: impl Fn(usize) -> bool, id: impl Fn(&T) -> i64) -> BTreeSet<i64> {
        (0..items.len())
            .filter(|&i| keep(i))
            .map(|i| id(&items[i]))
            .collect()
    }

    fn check_viewer(&self, who: Option<usize>) {
        let viewer = self.viewer(who);
        let db = &self.db;
        let mut deps = Deps::new();
        let readable_categories =
            Self::ids(&self.categories, |c| self.reads_category(who, c), |c| c.id);
        let readable_threads = Self::ids(&self.threads, |t| self.reads_thread(who, t), |t| t.id);
        let thread_index: HashMap<i64, usize> = self
            .threads
            .iter()
            .enumerate()
            .map(|(i, t)| (t.id, i))
            .collect();

        let targets: BTreeSet<i64> = self
            .block(content::readable_categories(db, &viewer))
            .unwrap()
            .iter()
            .map(|c| c.category.id)
            .collect();
        assert_eq!(targets, readable_categories, "{who:?}: readable categories");

        let sections = self
            .block(content::index_sections(db, &viewer, &mut deps))
            .unwrap();
        let mut listed = BTreeSet::new();
        for summary in sections.iter().flat_map(|s| &s.categories) {
            listed.insert(summary.category.id);
            let c = self
                .categories
                .iter()
                .position(|c| c.id == summary.category.id)
                .unwrap();
            let threads_here = (0..self.threads.len())
                .filter(|&t| self.threads[t].category == c && self.reads_thread(who, t))
                .count() as i64;
            assert_eq!(
                summary.thread_count, threads_here,
                "{who:?}: threads in {c}"
            );
            if let Some(last) = &summary.last {
                let t = thread_index[&last.thread.id];
                assert!(self.reads_thread(who, t) && self.threads[t].category == c);
            }
        }
        assert_eq!(listed, readable_categories, "{who:?}: index");

        let latest = self
            .block(content::latest(db, &viewer, 1, 10_000, &mut deps))
            .unwrap();
        let rows: BTreeSet<i64> = latest.threads.iter().map(|r| r.thread.id).collect();
        assert_eq!(rows, readable_threads, "{who:?}: latest");
        assert_eq!(latest.thread_count, readable_threads.len() as i64);
        for row in &latest.threads {
            let t = thread_index[&row.thread.id];
            let source = self.threads[t].fork_of.map(|p| self.posts[p].thread);
            let expected = source
                .filter(|&s| self.reads_thread(who, s))
                .map(|s| self.threads[s].id);
            assert_eq!(row.forked_from.as_ref().map(|f| f.id), expected);
        }

        for (c, category) in self.categories.iter().enumerate() {
            let page = self
                .block(content::category_page(
                    db,
                    &viewer,
                    &category.slug,
                    1,
                    &mut deps,
                ))
                .unwrap();
            match page {
                None => assert!(!self.reads_category(who, c), "{who:?}: category {c}"),
                Some(page) => {
                    assert!(self.reads_category(who, c), "{who:?}: category {c}");
                    let shown: BTreeSet<i64> = page.threads.iter().map(|r| r.thread.id).collect();
                    let expected = Self::ids(
                        &self.threads,
                        |t| self.threads[t].category == c && self.reads_thread(who, t),
                        |t| t.id,
                    );
                    assert_eq!(shown, expected, "{who:?}: threads in category {c}");
                }
            }
        }

        for (t, thread) in self.threads.iter().enumerate() {
            let visible = self.reads_thread(who, t);
            let page = self
                .block(content::thread_page(db, &viewer, thread.id, 1, &mut deps))
                .unwrap();
            assert_eq!(page.is_some(), visible, "{who:?}: thread {t}");
            for found in [
                self.block(content::thread_slug(db, &viewer, thread.id))
                    .unwrap()
                    .is_some(),
                self.block(content::thread_target(db, &viewer, thread.id))
                    .unwrap()
                    .is_some(),
                self.block(content::first_unread(db, &viewer, thread.id))
                    .unwrap()
                    .is_some(),
            ] {
                assert_eq!(found, visible, "{who:?}: thread {t}");
            }
            let Some(page) = page else { continue };
            match (thread.fork_of, &page.embed) {
                (None, None) => {}
                (Some(p), Some(Embed::Available(embedded))) => {
                    assert!(self.reads_post(who, p), "{who:?}: embed in {t}");
                    assert_eq!(embedded.post_id, self.posts[p].id);
                }
                (Some(p), Some(Embed::Unavailable)) => {
                    assert!(!self.reads_post(who, p), "{who:?}: embed in {t}")
                }
                _ => panic!("{who:?}: thread {t} has the wrong embed"),
            }
            for shown in &page.posts {
                let p = self.posts.iter().position(|p| p.id == shown.id).unwrap();
                assert_eq!(self.posts[p].thread, t);
                assert_eq!(
                    matches!(shown.body, PostBody::Live { .. }),
                    self.post_live(p),
                    "{who:?}: post {p}"
                );
                let backlinks: BTreeSet<i64> = shown.forks.iter().map(|f| f.thread.id).collect();
                let expected = if self.post_live(p) {
                    Self::ids(
                        &self.threads,
                        |f| self.threads[f].fork_of == Some(p) && self.reads_thread(who, f),
                        |f| f.id,
                    )
                } else {
                    BTreeSet::new()
                };
                assert_eq!(backlinks, expected, "{who:?}: backlinks of post {p}");
            }
        }

        for (p, post) in self.posts.iter().enumerate() {
            let in_thread = self.reads_thread(who, post.thread);
            let location = self
                .block(content::post_location(db, &viewer, post.id))
                .unwrap();
            assert_eq!(location.is_some(), in_thread, "{who:?}: post {p}");
            let target = self
                .block(content::post_target(db, &viewer, post.id))
                .unwrap();
            assert_eq!(target.is_some(), in_thread, "{who:?}: post {p}");
            if let Some(target) = target {
                assert_eq!(
                    target.body.is_some(),
                    self.post_live(p),
                    "{who:?}: post {p}"
                );
            }
            let readable = self.reads_post(who, p);
            let history = self
                .block(content::post_history(db, &viewer, post.id, &mut deps))
                .unwrap();
            assert_eq!(history.is_some(), readable, "{who:?}: history of {p}");
            let reactions = self
                .block(content::post_reactions(db, &viewer, post.id, &mut deps))
                .unwrap();
            assert_eq!(reactions.is_some(), readable, "{who:?}: reactions to {p}");
        }

        for (u, user) in self.users.iter().enumerate() {
            let count = self
                .block(content::visible_post_count(db, &viewer, user.id))
                .unwrap();
            let expected = (0..self.posts.len())
                .filter(|&p| self.posts[p].author == u && self.reads_post(who, p))
                .count() as i64;
            assert_eq!(count, expected, "{who:?}: posts by {u}");
        }
    }
}

#[hegel::state_machine]
impl Access {
    #[rule]
    fn new_group(&mut self, tc: TestCase) {
        tc.assume(self.groups.len() < 4);
        self.add_group();
    }

    #[rule]
    fn toggle_membership(&mut self, tc: TestCase) {
        let u = tc.draw(gs::integers::<usize>().max_value(self.users.len() - 1));
        let g = tc.draw(gs::integers::<usize>().max_value(self.groups.len() - 1));
        let (user_id, group_id) = (self.users[u].id, self.groups[g]);
        if self.users[u].groups.remove(&g) {
            assert!(
                self.block(groups::remove_member(&self.db, group_id, user_id))
                    .unwrap()
            );
        } else {
            assert!(
                self.block(groups::add_member(&self.db, group_id, user_id, None))
                    .unwrap()
            );
            self.users[u].groups.insert(g);
        }
    }

    #[rule]
    fn change_role(&mut self, tc: TestCase) {
        let u = tc.draw(
            gs::integers::<usize>()
                .min_value(FIXED_USERS)
                .max_value(self.users.len() - 1),
        );
        let role = tc.draw(
            gs::sampled_from(vec![Role::User, Role::Moderator, Role::Admin]).print_as_debug(),
        );
        let changed = self
            .block(users::change_role(&self.db, self.users[u].id, role))
            .unwrap();
        assert_eq!(changed, RoleChange::Done);
        self.users[u].staff = role.is_staff();
    }

    #[rule]
    fn new_category(&mut self, tc: TestCase) {
        tc.assume(self.categories.len() < 5);
        let level = tc.draw(gs::sampled_from(LEVELS.to_vec()));
        self.add_category(level);
    }

    /// What the admin's category form does: a new read level and read
    /// groups.
    #[rule]
    fn set_access(&mut self, tc: TestCase) {
        let c = tc.draw(gs::integers::<usize>().max_value(self.categories.len() - 1));
        let level = tc.draw(gs::sampled_from(LEVELS.to_vec()));
        let read_groups: BTreeSet<usize> = (0..self.groups.len())
            .filter(|_| tc.draw(gs::booleans()))
            .collect();
        let category = &self.categories[c];
        let settings = CategorySettings {
            section_id: None,
            slug: category.slug.clone(),
            name: format!("Category {c}"),
            description: String::new(),
            position: c as i32,
            read_level: level.into(),
            write_level: "members".into(),
            read_groups: read_groups.iter().map(|&g| self.groups[g]).collect(),
            write_groups: Vec::new(),
        };
        self.block(store::update_category(&self.db, category.id, &settings))
            .unwrap()
            .unwrap();
        let category = &mut self.categories[c];
        category.level = level;
        category.read_groups = read_groups;
    }

    #[rule]
    fn new_thread(&mut self, tc: TestCase) {
        tc.assume(self.threads.len() < 12);
        let category = tc.draw(gs::integers::<usize>().max_value(self.categories.len() - 1));
        let author = tc.draw(gs::integers::<usize>().max_value(self.users.len() - 1));
        let fork_of = if self.posts.is_empty() || !tc.draw(gs::booleans()) {
            None
        } else {
            Some(tc.draw(gs::integers::<usize>().max_value(self.posts.len() - 1)))
        };
        let created = self
            .block(store::create_thread(
                &self.pool,
                &NewThread {
                    category_id: self.categories[category].id,
                    title: "A thread".into(),
                    author_id: self.users[author].id,
                    body_md: "Opening".into(),
                    forked_from_post_id: fork_of.map(|p| self.posts[p].id),
                    pinned: false,
                    locked: false,
                    created_at: None,
                },
            ))
            .unwrap();
        self.threads.push(AccessThread {
            id: created.thread_id,
            category,
            deleted: false,
            fork_of,
        });
        self.posts.push(AccessPost {
            id: created.first_post_id,
            thread: self.threads.len() - 1,
            author,
            deleted: false,
            hidden: false,
        });
    }

    #[rule]
    fn reply(&mut self, tc: TestCase) {
        tc.assume(!self.threads.is_empty() && self.posts.len() < 40);
        let thread = tc.draw(gs::integers::<usize>().max_value(self.threads.len() - 1));
        let author = tc.draw(gs::integers::<usize>().max_value(self.users.len() - 1));
        self.add_post(thread, author);
    }

    #[rule]
    fn move_thread(&mut self, tc: TestCase) {
        tc.assume(!self.threads.is_empty());
        let t = tc.draw(gs::integers::<usize>().max_value(self.threads.len() - 1));
        let c = tc.draw(gs::integers::<usize>().max_value(self.categories.len() - 1));
        let previous = self
            .block(store::move_thread(
                &self.db,
                self.threads[t].id,
                self.categories[c].id,
            ))
            .unwrap();
        assert_eq!(previous, Some(self.categories[self.threads[t].category].id));
        self.threads[t].category = c;
    }

    #[rule]
    fn toggle_thread_deleted(&mut self, tc: TestCase) {
        tc.assume(!self.threads.is_empty());
        let t = tc.draw(gs::integers::<usize>().max_value(self.threads.len() - 1));
        let thread = &mut self.threads[t];
        thread.deleted = !thread.deleted;
        let sql = format!(
            "UPDATE threads SET deleted_at = {} WHERE id = {}",
            if thread.deleted { "now()" } else { "NULL" },
            thread.id
        );
        self.exec(sql);
    }

    #[rule]
    fn toggle_post_removal(&mut self, tc: TestCase) {
        tc.assume(!self.posts.is_empty());
        let p = tc.draw(gs::integers::<usize>().max_value(self.posts.len() - 1));
        let hide = tc.draw(gs::booleans());
        let post = &mut self.posts[p];
        if hide {
            post.hidden = !post.hidden;
        } else {
            post.deleted = !post.deleted;
        }
        let sql = format!(
            "UPDATE posts SET deleted_at = {}, hidden_at = {} WHERE id = {}",
            if post.deleted { "now()" } else { "NULL" },
            if post.hidden { "now()" } else { "NULL" },
            post.id
        );
        self.exec(sql);
    }

    /// Everything each viewer can get from the content module. It makes
    /// hundreds of queries, so it is sampled: it always runs on the first
    /// and last states, and now and then in between.
    #[invariant]
    fn every_viewer_sees_exactly_what_they_may(&self, _: TestCase) {
        self.check_viewer(None);
        for u in 0..self.users.len() {
            self.check_viewer(Some(u));
        }
    }

    /// The denormalised `threads.visibility` is always the category's read
    /// level, however threads move and levels change.
    #[invariant(always_run)]
    fn visibility_copies_are_current(&self, _: TestCase) {
        let stale: i64 = self
            .block(
                sqlx::query_scalar(
                    "SELECT count(*) FROM threads t JOIN categories c ON c.id = t.category_id
                     WHERE t.visibility <> c.read_level",
                )
                .fetch_one(&self.pool),
            )
            .unwrap();
        assert_eq!(stale, 0);
        for t in &self.threads {
            let level: String = self
                .block(
                    sqlx::query_scalar("SELECT visibility FROM threads WHERE id = $1")
                        .bind(t.id)
                        .fetch_one(&self.pool),
                )
                .unwrap();
            assert_eq!(level, self.categories[t.category].level);
        }
    }
}

#[hegel::standalone_function(test_cases = 100, suppress_health_check = [HealthCheck::TooSlow])]
fn run_access(tc: TestCase, rt: &Handle, pool: &PgPool) {
    let world = Access::new(rt.clone(), pool.clone());
    hegel::stateful::machine(world).steps(30).run(tc);
}

/// Every content function gives each viewer exactly what the reference
/// permission function allows: no more, and no less.
#[sqlx::test]
async fn the_content_module_shows_each_viewer_exactly_what_they_may_see(pool: PgPool) {
    let rt = Handle::current();
    tokio::task::spawn_blocking(move || run_access(&rt, &pool))
        .await
        .unwrap();
}
