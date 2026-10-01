//! Property-based tests over random forum histories (DESIGN.md §6, §12).
//!
//! A state machine creates categories, threads, posts and quote-forks, then
//! edits, deletes, hides, moves and renames them, changes category levels,
//! and reports each change as a write handler would. Throughout:
//!
//! - every page served through the anonymous cache is identical to the same
//!   page rendered fresh, so invalidation never leaves a stale page behind;
//! - no anonymous page contains text from anything that isn't public,
//!   judged by a reference permission function written separately here, and
//!   every public post can be found on the page its number says.

mod common;

use std::collections::BTreeSet;

use axum::http::StatusCode;
use common::{Forum, Response};
use hegel::TestCase;
use hegel::generators::{self as gs};
use parlour::cache::PageCache;
use parlour::changes::Change;
use parlour::content::{POSTS_PER_PAGE, THREADS_PER_PAGE, page_count, page_of_seq};
use parlour::pagination::page_href;
use parlour::store::{self, NewCategory, NewPost, NewThread, NewUser};
use parlour::{AppState, markdown};
use sqlx::PgPool;
use tokio::runtime::Handle;

const LEVELS: [&str; 3] = ["public", "members", "groups"];

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
    body_token: String,
    deleted: bool,
    hidden: bool,
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
    /// Check cache coherence (true) or anonymous visibility (false).
    coherence: bool,
}

impl World {
    fn new(rt: Handle, db: PgPool, coherence: bool) -> World {
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
            coherence,
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
            body_token: token,
            deleted: false,
            hidden: false,
        });
        self.changed(Change::PostCreated { post_id: id });
    }

    // -- The reference permission function, independent of the app's SQL.

    fn category_public(&self, category: usize) -> bool {
        self.categories[category].read_level == "public"
    }

    fn thread_public(&self, thread: usize) -> bool {
        let t = &self.threads[thread];
        !t.deleted && self.category_public(t.category)
    }

    fn post_public(&self, post: usize) -> bool {
        let p = &self.posts[post];
        !p.deleted && !p.hidden && self.thread_public(p.thread)
    }

    /// Tokens an anonymous visitor may see somewhere.
    fn visible_tokens(&self) -> BTreeSet<&str> {
        let mut tokens = BTreeSet::new();
        for (i, c) in self.categories.iter().enumerate() {
            if self.category_public(i) {
                tokens.insert(c.token.as_str());
            }
        }
        for (i, t) in self.threads.iter().enumerate() {
            if self.thread_public(i) {
                tokens.insert(t.title_token.as_str());
            }
        }
        for (i, p) in self.posts.iter().enumerate() {
            if self.post_public(i) {
                tokens.insert(p.body_token.as_str());
            }
        }
        tokens
    }

    fn paths(&self) -> Vec<String> {
        let mut paths = vec!["/".to_owned()];
        let visible_threads = (0..self.threads.len())
            .filter(|&t| self.thread_public(t))
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
        }
        paths
    }

    fn slug_of(&self, t: &ThreadModel) -> String {
        store::slugify(&format!("Thread {}", t.title_token))
    }

    fn get(&self, forum: &Forum, path: &str) -> Response {
        self.block(forum.get(path))
    }

    fn check(&self) {
        if self.coherence {
            self.check_coherence();
        } else {
            self.check_visibility();
        }
    }

    fn check_coherence(&self) {
        for path in self.paths() {
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

    fn check_visibility(&self) {
        let visible = self.visible_tokens();
        let forbidden: Vec<&String> = self
            .all_tokens
            .iter()
            .filter(|t| !visible.contains(t.as_str()))
            .collect();
        for path in self.paths() {
            let response = self.get(&self.cached, &path);
            assert!(
                !response
                    .headers
                    .contains_key(axum::http::header::SET_COOKIE)
            );
            for token in &forbidden {
                assert!(
                    !response.body.contains(token.as_str()),
                    "{path} shows {token}:\n{}",
                    response.body
                );
            }
        }
        // Visible things can be found where they should be, and invisible
        // things are not found at all.
        for (i, t) in self.threads.iter().enumerate() {
            let response = self.get(&self.cached, &format!("/t/{}/{}", t.id, self.slug_of(t)));
            if self.thread_public(i) {
                assert_eq!(response.status, StatusCode::OK);
                assert!(response.body.contains(&t.title_token));
            } else {
                assert_eq!(response.status, StatusCode::NOT_FOUND);
            }
        }
        for (i, p) in self.posts.iter().enumerate() {
            let permalink = self.get(&self.cached, &format!("/p/{}", p.id));
            if !self.thread_public(p.thread) {
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
            if self.post_public(i) {
                let body = self
                    .get(&self.cached, expected.split('#').next().unwrap())
                    .body;
                assert!(
                    body.contains(&p.body_token),
                    "post {} missing from {expected}",
                    p.id
                );
            }
        }
        for (i, c) in self.categories.iter().enumerate() {
            let response = self.get(&self.cached, &format!("/c/{}", c.slug));
            let expected = if self.category_public(i) {
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
            body_token,
            deleted: false,
            hidden: false,
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
        let token = self.token();
        let body = format!("Edited {token}");
        sqlx::query!(
            "UPDATE posts SET body_md = $2, body_html = $3, edited_at = now() WHERE id = $1",
            self.posts[post].id,
            body,
            markdown::render(&body)
        )
        .execute(self.db())
        .await_blocking(self);
        self.posts[post].body_token = token;
        self.changed(Change::PostEdited {
            post_id: self.posts[post].id,
        });
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
        for path in self.paths() {
            self.get(&self.cached, &path);
        }
    }

    /// Checked at the start and end of each run and sampled in between.
    #[invariant]
    fn pages_are_right(&self, _: TestCase) {
        self.check();
    }
}

/// Lets a rule await a query from synchronous state-machine code.
trait AwaitBlocking {
    type Output;
    fn await_blocking(self, world: &World) -> Self::Output;
}

impl<F: Future<Output = Result<T, sqlx::Error>>, T> AwaitBlocking for F {
    type Output = T;
    fn await_blocking(self, world: &World) -> T {
        world.block(self).unwrap()
    }
}

#[hegel::standalone_function(test_cases = 100)]
fn run_machine(tc: TestCase, rt: &Handle, db: &PgPool, coherence: bool) {
    let world = World::new(rt.clone(), db.clone(), coherence);
    hegel::stateful::machine(world).steps(25).run(tc);
}

async fn run_in_blocking_thread(pool: PgPool, coherence: bool) {
    let rt = Handle::current();
    tokio::task::spawn_blocking(move || run_machine(&rt, &pool, coherence))
        .await
        .unwrap();
}

/// Cached pages always match freshly rendered ones.
#[sqlx::test]
async fn cache_is_coherent(pool: PgPool) {
    run_in_blocking_thread(pool, true).await;
}

/// Anonymous pages show exactly what the reference function allows.
#[sqlx::test]
async fn anonymous_pages_show_only_public_content(pool: PgPool) {
    run_in_blocking_thread(pool, false).await;
}
