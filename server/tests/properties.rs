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
//!   Logged-in viewers are also shown the writing pages, which display the
//!   text being quoted, forked, edited or deleted: new threads, replies
//!   quoting posts in their own and other threads, forks, edits and
//!   deletions, along with the moderation and island pages about each post.
//!   There are many of those per post, so each check visits them for a
//!   sample of posts, threads and messages that Hegel draws afresh. The member writes posts of their own, so that leaving a
//!   group can strand them in a category they no longer read.
//! - Conversations (DMs) come and go among the members, the member viewer
//!   and the moderator viewer, who join, leave and are added back: a
//!   conversation's title and messages are seen by its current
//!   participants and nobody else (staff included), never through the
//!   anonymous cache, the category lists, the notifications page or any
//!   page about a post. Every new post is put through the notification
//!   fan-out, so the notifications page is checked too.
//! - Notification email is sent as members are away, and every email holds
//!   only text its recipient may read now; the notification badge, both
//!   counted directly and as the live badge stream sends it, counts exactly
//!   the unread notifications about posts the viewer may read.

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
use parlour::content::uploads::UploadAccess;
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
use parlour::writing;
use sqlx::PgPool;
use tokio::runtime::Handle;

/// Counts the (member, category) pairs whose maintained post count, as
/// thread pages and profiles show it, differs from counting the posts
/// afresh: live posts in live threads in that category.
const STALE_POST_COUNTS: &str = "
    WITH actual AS (
        SELECT p.author_id AS user_id, t.category_id, count(*) AS posts
        FROM posts p JOIN threads t ON t.id = p.thread_id
        WHERE t.category_id IS NOT NULL AND t.deleted_at IS NULL
          AND p.deleted_at IS NULL AND p.hidden_at IS NULL
        GROUP BY 1, 2
    ), kept AS (
        SELECT user_id, category_id, posts FROM user_category_post_counts WHERE posts <> 0
    )
    SELECT count(*) FROM actual FULL JOIN kept USING (user_id, category_id)
    WHERE actual.posts IS DISTINCT FROM kept.posts";

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

struct ConversationModel {
    id: i64,
    title_token: String,
    /// The current participants, by user id.
    participants: BTreeSet<i64>,
    last_seq: i64,
    /// Whether one of its messages was reported, opening it to staff.
    reported: bool,
}

struct MessageModel {
    id: i64,
    conversation: usize,
    seq: i64,
    body_token: String,
    deleted: bool,
}

struct ThreadModel {
    id: i64,
    category: usize,
    title_token: String,
    deleted: bool,
    last_seq: i64,
    /// Titles a moderator renamed it from, which the moderation log names
    /// to those who can read the thread.
    old_titles: Vec<String>,
    /// The thread it was merged into, by moving all its posts there.
    merged_into: Option<usize>,
}

/// A new member's post, held for a moderator.
struct HeldModel {
    id: i64,
    author: i64,
    title_token: String,
    body_token: String,
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

/// How many posts, threads and messages each check samples for the
/// logged-in item pages (see `World::sample`).
const SAMPLE_SIZE: usize = 4;

/// The posts, threads and messages (as indexes into `World`'s lists) whose
/// logged-in item pages a check visits.
struct Sample {
    posts: Vec<usize>,
    threads: Vec<usize>,
    messages: Vec<usize>,
}

/// The pages one viewer has fetched during a check, so that each is
/// rendered once however many checks look at it.
struct Pages<'a> {
    world: &'a World,
    who: Who,
    seen: std::cell::RefCell<HashMap<String, std::rc::Rc<Response>>>,
}

impl<'a> Pages<'a> {
    fn new(world: &'a World, who: Who) -> Pages<'a> {
        Pages {
            world,
            who,
            seen: Default::default(),
        }
    }

    fn get(&self, path: &str) -> std::rc::Rc<Response> {
        if let Some(response) = self.seen.borrow().get(path) {
            return response.clone();
        }
        let response = std::rc::Rc::new(self.world.get_as(self.who, path));
        self.seen
            .borrow_mut()
            .insert(path.to_owned(), response.clone());
        response
    }
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
    staff_id: i64,
    conversations: Vec<ConversationModel>,
    messages: Vec<MessageModel>,
    /// Who each email address belongs to.
    addresses: HashMap<String, i64>,
    /// Whether the member viewer is silenced or suspended now.
    member_silenced: bool,
    member_suspended: bool,
    held: Vec<HeldModel>,
    /// How many of the mailer's sent emails have been checked.
    emails_checked: usize,
}

impl World {
    fn new(rt: Handle, db: PgPool, mode: Mode) -> World {
        rt.block_on(async {
            sqlx::raw_sql(
                "TRUNCATE settings, jobs, posts, threads, categories, sections, groups, users RESTART IDENTITY CASCADE",
            )
            .execute(&db)
            .await
            .unwrap();
        });
        let mut world = World {
            // Without rate limits, so that searching every page is never
            // refused.
            cached: Forum {
                state: AppState::new(db.clone()).with_mailer(parlour::mail::Mailer::capture()),
                db: db.clone(),
            }
            .without_rate_limits(),
            fresh: Forum {
                state: AppState::with_cache(db.clone(), PageCache::disabled()),
                db,
            }
            .without_rate_limits(),
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
            staff_id: 0,
            conversations: Vec::new(),
            messages: Vec::new(),
            addresses: HashMap::new(),
            emails_checked: 0,
            member_silenced: false,
            member_suspended: false,
            held: Vec::new(),
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
            world.addresses.insert(format!("{name}@example.com"), id);
        }
        // Viewers who log in. The member writes too, so that the pages for
        // editing and deleting their own posts are checked as well; the
        // moderator writes nothing.
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
            world.addresses.insert(format!("{name}@example.com"), id);
            let cookie = world.block(world.cached.session(id));
            if role == "user" {
                world.member_cookie = cookie;
                world.member_id = id;
                world.users.push(id);
            } else {
                world.staff_cookie = cookie;
                world.staff_id = id;
            }
        }
        for n in 0..2 {
            let id = world.block(world.cached.group(&format!("g{n}"), &format!("Group {n}")));
            world.groups.push(id);
        }
        for level in ["public", "public", "members", "groups"] {
            world.add_category(level);
        }
        // Everyone can be emailed, as soon as a notification is made.
        world.exec("UPDATE users SET email_verified_at = now()".to_owned());
        world
            .block(parlour::settings::set(
                world.db(),
                "notifications",
                &parlour::settings::NotificationSettings {
                    email_delay_minutes: 0,
                },
            ))
            .unwrap();
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
        self.notify(id);
    }

    /// Runs the notification fan-out for a new post, as its job would.
    fn notify(&self, post_id: i64) {
        self.block(parlour::notify::post_created(&self.cached.state, post_id))
            .unwrap();
    }

    /// Everyone who can be in a conversation: the members and both viewers.
    fn dm_people(&self) -> Vec<i64> {
        let mut people = self.users.clone();
        people.push(self.staff_id);
        people
    }

    /// The access layer's view of a member, as they are now.
    fn viewer_of(&self, user: i64) -> Viewer {
        Viewer::User(
            self.block(sessions::user_ctx(self.fresh.state.db(), user))
                .unwrap()
                .unwrap(),
        )
    }

    fn viewer_id(&self, who: Who) -> Option<i64> {
        match who {
            Who::Anonymous => None,
            Who::Member => Some(self.member_id),
            Who::Staff => Some(self.staff_id),
        }
    }

    /// DESIGN.md §3.7: a conversation is read by its current participants
    /// and nobody else.
    fn conversation_readable(&self, conversation: usize, who: Who) -> bool {
        self.viewer_id(who)
            .is_some_and(|id| self.conversation_readable_by(conversation, id))
    }

    fn add_message(&mut self, conversation: usize, author: i64, mention: bool) {
        let token = self.token();
        let body = if mention {
            format!("Message {token} for @viewer and @watcher")
        } else {
            format!("Message {token}")
        };
        let id = self
            .block(store::create_post(
                self.db(),
                &NewPost {
                    thread_id: self.conversations[conversation].id,
                    author_id: author,
                    body_md: body,
                    reply_to_post_id: None,
                    created_at: None,
                },
            ))
            .unwrap();
        self.conversations[conversation].last_seq += 1;
        self.messages.push(MessageModel {
            id,
            conversation,
            seq: self.conversations[conversation].last_seq,
            body_token: token,
            deleted: false,
        });
        self.changed(Change::PostCreated { post_id: id });
        self.notify(id);
    }

    // -- The reference permission function, independent of the app's SQL.

    /// DESIGN.md §3.1: anyone reads public categories, members also read
    /// members-only ones, members of a category's read groups read it, and
    /// staff read everything.
    fn category_readable(&self, category: usize, who: Who) -> bool {
        let c = &self.categories[category];
        // A suspended member reads what visitors read.
        let who = if who == Who::Member && self.member_suspended {
            Who::Anonymous
        } else {
            who
        };
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

    /// Whether a post's text is shown to the viewer on pages: as
    /// [`World::post_readable`], but staff also see hidden posts (marked
    /// as hidden), with their history.
    fn post_shown(&self, post: usize, who: Who) -> bool {
        let p = &self.posts[post];
        !p.deleted && (!p.hidden || who == Who::Staff) && self.thread_readable(p.thread, who)
    }

    /// The viewer a member is, for the reference permission function:
    /// the logged-in viewers as themselves, and the other members (who
    /// are in no groups) as a member viewer in no groups would be.
    fn category_readable_by(&self, category: usize, user: i64) -> bool {
        if user == self.staff_id {
            return true;
        }
        let c = &self.categories[category];
        if user == self.member_id && self.member_suspended {
            return c.read_level == "public";
        }
        match c.read_level {
            "public" | "members" => true,
            _ => user == self.member_id && !self.member_groups.is_disjoint(&c.read_groups),
        }
    }

    /// Tokens the member `user` may read now.
    fn tokens_for_user(&self, user: i64) -> BTreeSet<&str> {
        let mut tokens = BTreeSet::new();
        for t in &self.threads {
            if !t.deleted && self.category_readable_by(t.category, user) {
                tokens.insert(t.title_token.as_str());
            }
        }
        for p in &self.posts {
            let t = &self.threads[p.thread];
            if !p.deleted && !p.hidden && !t.deleted && self.category_readable_by(t.category, user)
            {
                tokens.insert(p.body_token.as_str());
            }
        }
        for (i, c) in self.conversations.iter().enumerate() {
            if self.conversation_readable_by(i, user) {
                tokens.insert(c.title_token.as_str());
            }
        }
        for m in &self.messages {
            if !m.deleted && self.conversation_readable_by(m.conversation, user) {
                tokens.insert(m.body_token.as_str());
            }
        }
        tokens
    }

    /// Whether member `user` reads a conversation: they are in it, or they
    /// are staff and it was reported; a suspended member reads none.
    fn conversation_readable_by(&self, conversation: usize, user: i64) -> bool {
        let c = &self.conversations[conversation];
        if user == self.member_id && self.member_suspended {
            return false;
        }
        c.participants.contains(&user) || (c.reported && user == self.staff_id)
    }

    /// How many unread notifications the viewer should see on the badge:
    /// those about posts they may read now, by the reference function.
    fn expected_unread(&self, who: Who) -> i64 {
        let user = self.viewer_id(who).unwrap();
        let unread: Vec<Option<i64>> = self
            .block(
                sqlx::query_scalar(
                    "SELECT post_id FROM notifications WHERE user_id = $1 AND read_at IS NULL",
                )
                .bind(user)
                .fetch_all(self.db()),
            )
            .unwrap();
        unread
            .into_iter()
            .filter(|post_id| {
                let Some(post_id) = post_id else { return true };
                if let Some(i) = self.posts.iter().position(|p| p.id == *post_id) {
                    return self.post_readable(i, who);
                }
                let m = self
                    .messages
                    .iter()
                    .find(|m| m.id == *post_id)
                    .expect("a notification about a post the test made");
                !m.deleted && self.conversation_readable(m.conversation, who)
            })
            .count() as i64
    }

    /// The first counts the live badge stream sends the viewer.
    fn streamed_counts(&self, who: Who) -> String {
        use axum::body::Body;
        use http_body_util::BodyExt;
        use tower::ServiceExt;
        let request = axum::http::Request::get("/events/notifications")
            .header(axum::http::header::COOKIE, self.cookie(who).unwrap())
            .body(Body::empty())
            .unwrap();
        self.block(async {
            let response = parlour::app(self.fresh.state.clone())
                .oneshot(request)
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let mut body = response.into_body();
            let mut text = String::new();
            loop {
                if let Some(data) = text
                    .split("\n\n")
                    .next()
                    .filter(|_| text.contains("\n\n"))
                    .and_then(|event| event.lines().find_map(|l| l.strip_prefix("data: ")))
                {
                    return data.to_owned();
                }
                let frame = body.frame().await.expect("a counts event").unwrap();
                if let Ok(data) = frame.into_data() {
                    text.push_str(std::str::from_utf8(&data).unwrap());
                }
            }
        })
    }

    /// Runs the notification email jobs that are due, and checks that each
    /// email sent holds only what its recipient may read now. Every unread
    /// notification is emailed again, not only new ones, so that each is
    /// checked against how things are now: after posts were deleted or
    /// moved, categories closed, groups or conversations left.
    fn send_emails(&mut self) {
        self.exec("UPDATE notifications SET emailed_at = NULL WHERE read_at IS NULL".to_owned());
        let unread: Vec<i64> = self
            .block(
                sqlx::query_scalar(
                    "SELECT id FROM notifications WHERE read_at IS NULL ORDER BY id",
                )
                .fetch_all(self.db()),
            )
            .unwrap();
        for id in unread {
            self.block(parlour::notify::email_if_away(&self.cached.state, id))
                .unwrap();
        }
        self.block(parlour::jobs::run_due(&self.cached.state))
            .unwrap();
        let sent = self.cached.state.mailer().sent();
        for email in &sent[self.emails_checked..] {
            let user = self.addresses[&email.to];
            let allowed = self.tokens_for_user(user);
            for token in &self.all_tokens {
                if allowed.contains(token.as_str()) {
                    continue;
                }
                assert!(
                    !email.body.contains(token.as_str()) && !email.subject.contains(token.as_str()),
                    "an email to {} shows {token}:\n{}",
                    email.to,
                    email.body
                );
            }
        }
        self.emails_checked = sent.len();
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
            // Staff are told in the moderation log what deleted threads
            // were called.
            let staff_knows = who == Who::Staff;
            if self.thread_readable(i, who) || staff_knows {
                tokens.insert(t.title_token.as_str());
                tokens.extend(t.old_titles.iter().map(String::as_str));
            }
        }
        for (i, p) in self.posts.iter().enumerate() {
            if self.post_shown(i, who) {
                tokens.insert(p.body_token.as_str());
                tokens.extend(p.history.iter().map(String::as_str));
            }
        }
        for h in &self.held {
            if who == Who::Staff || (who == Who::Member && h.author == self.member_id) {
                tokens.insert(h.title_token.as_str());
                tokens.insert(h.body_token.as_str());
            }
        }
        for (i, c) in self.conversations.iter().enumerate() {
            if self.conversation_readable(i, who) {
                tokens.insert(c.title_token.as_str());
            }
        }
        for m in &self.messages {
            if !m.deleted && self.conversation_readable(m.conversation, who) {
                tokens.insert(m.body_token.as_str());
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
        for name in ["ruth", "jonah", "maya", "viewer"] {
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
        paths.push("/messages".to_owned());
        paths.push("/notifications".to_owned());
        for c in &self.conversations {
            paths.push(format!("/messages/{}", c.id));
            paths.push(format!("/messages/{}/unread", c.id));
            paths.push(format!("/t/{}", c.id));
            paths.push(format!("/t/{}/message", c.id));
            paths.push(format!("/t/{}/unread", c.id));
        }
        for m in &self.messages {
            paths.push(format!("/p/{}", m.id));
            paths.push(format!("/p/{}/revisions", m.id));
            paths.push(format!("/p/{}/reactions", m.id));
        }
        paths.push("/moderation".to_owned());
        // Search finds every kind of text there is, a page at a time.
        let texts = (self.posts.len() + self.messages.len()) as i64;
        for page in 1..=page_count(texts, 20).min(25) {
            let base = "/search?q=post+or+message+or+thread&sort=new";
            paths.push(if page == 1 {
                base.to_owned()
            } else {
                format!("{base}&page={page}")
            });
        }
        for h in &self.held {
            paths.push(format!("/held/{}", h.id));
        }
        if who != Who::Anonymous {
            // Writing pages show logged-in viewers the text they quote,
            // fork or edit. Visitors are only asked to log in. The ones for
            // particular posts and threads are in `item_paths`.
            for c in &self.categories {
                paths.push(format!("/c/{}/new", c.slug));
            }
            for c in &self.conversations {
                paths.push(format!("/t/{}/reply", c.id));
            }
            for path in [
                "/mod/queue",
                "/mod/log",
                "/mod/users/viewer",
                "/mod/users/ruth",
                // What the notifications island fetches.
                "/api/notifications",
            ] {
                paths.push(path.to_owned());
            }
        }
        paths
    }

    /// Draws which posts, threads and messages a check (or a warming step)
    /// visits the logged-in item pages of. There are about ten such pages
    /// per post, plus quoting each post into each other thread, so visiting
    /// all of them every time made the logged-in suites take many minutes
    /// as forums grew. Hegel draws a fresh sample each time, so over a run
    /// they are all visited, and a failure still shrinks to the pages
    /// involved.
    fn sample(&self, tc: &TestCase) -> Sample {
        fn pick(tc: &TestCase, n: usize) -> Vec<usize> {
            if n == 0 {
                return Vec::new();
            }
            tc.draw(gs::btree_sets(gs::integers::<usize>().max_value(n - 1)).max_size(SAMPLE_SIZE))
                .into_iter()
                .collect()
        }
        Sample {
            posts: pick(tc, self.posts.len()),
            threads: pick(tc, self.threads.len()),
            messages: pick(tc, self.messages.len()),
        }
    }

    /// The logged-in pages about particular posts, threads and messages:
    /// writing pages (which show the text being quoted, forked, edited or
    /// deleted), moderation pages and what the islands fetch, for the
    /// sampled ones.
    fn item_paths(&self, sample: &Sample) -> Vec<String> {
        let mut paths = Vec::new();
        for &t in &sample.threads {
            let t = &self.threads[t];
            paths.push(format!("/t/{}/reply", t.id));
            paths.push(format!("/mod/t/{}", t.id));
        }
        for &p in &sample.posts {
            let p = &self.posts[p];
            let thread = self.threads[p.thread].id;
            paths.push(format!("/p/{}/fork", p.id));
            paths.push(format!("/p/{}/edit", p.id));
            paths.push(format!("/p/{}/delete", p.id));
            paths.push(format!("/t/{thread}/reply?quote={}", p.id));
            paths.push(format!("/t/{thread}/reply?to={}", p.id));
            // Quoting a post into another thread is refused, but must not
            // show the post on the way.
            for &other in &sample.threads {
                let other = self.threads[other].id;
                if other != thread {
                    paths.push(format!("/t/{other}/reply?quote={}", p.id));
                }
            }
        }
        for &m in &sample.messages {
            let m = &self.messages[m];
            let conversation = self.conversations[m.conversation].id;
            for action in ["fork", "edit", "delete"] {
                paths.push(format!("/p/{}/{action}", m.id));
            }
            paths.push(format!("/t/{conversation}/reply?quote={}", m.id));
            // Quoting a message into a thread is refused, and must not
            // show the message on the way.
            for &t in &sample.threads {
                paths.push(format!("/t/{}/reply?quote={}", self.threads[t].id, m.id));
            }
        }
        for id in sample
            .posts
            .iter()
            .map(|&p| self.posts[p].id)
            .chain(sample.messages.iter().map(|&m| self.messages[m].id))
        {
            paths.push(format!("/mod/p/{id}"));
            paths.push(format!("/api/posts/{id}/source"));
            paths.push(format!("/api/posts/{id}/fork"));
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

    fn check(&self, tc: &TestCase) {
        match self.mode {
            Mode::Coherence => self.check_coherence(),
            Mode::Visibility(Who::Anonymous) => self.check_visibility(Who::Anonymous, &[]),
            Mode::Visibility(who) => {
                self.check_visibility(who, &self.item_paths(&self.sample(tc)));
                // Logged-in browsing never leaks into what the anonymous
                // cache serves.
                self.check_visibility(Who::Anonymous, &[]);
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

    /// Checks every page `who` can visit, plus `extra` ones.
    fn check_visibility(&self, who: Who, extra: &[String]) {
        // Nothing changes during a check, so each page is fetched once,
        // however many of the checks below look at it.
        let pages = Pages::new(self, who);
        let visible = self.visible_tokens(who);
        let forbidden: Vec<&String> = self
            .all_tokens
            .iter()
            .filter(|t| !visible.contains(t.as_str()))
            .collect();
        for path in self.paths(who).iter().chain(extra) {
            let response = pages.get(path);
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
            let response = pages.get(&format!("/t/{}/{}", t.id, self.slug_of(t)));
            if self.thread_readable(i, who) {
                assert_eq!(response.status, StatusCode::OK);
                assert!(response.body.contains(&t.title_token));
            } else if let Some(into) = t.merged_into.filter(|&into| {
                t.deleted
                    && self.category_readable(t.category, who)
                    && self.thread_readable(into, who)
            }) {
                // A merged thread leads to the one it was merged into.
                let into = &self.threads[into];
                assert_eq!(response.status, StatusCode::TEMPORARY_REDIRECT);
                assert_eq!(
                    response.location(),
                    format!("/t/{}/{}", into.id, self.slug_of(into))
                );
            } else {
                assert_eq!(response.status, StatusCode::NOT_FOUND, "{who:?} thread {i}");
            }
        }
        for (i, p) in self.posts.iter().enumerate() {
            let permalink = pages.get(&format!("/p/{}", p.id));
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
            let history = pages.get(&format!("/p/{}/revisions", p.id));
            if self.post_shown(i, who) {
                let page = pages.get(expected.split('#').next().unwrap());
                let body = &page.body;
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
                    let version = pages.get(&format!("/p/{}/revisions?v={}", p.id, n + 1));
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
            let response = pages.get(&format!("/c/{}", c.slug));
            let expected = if self.category_readable(i, who) {
                StatusCode::OK
            } else {
                StatusCode::NOT_FOUND
            };
            assert_eq!(response.status, expected);
        }
        for (i, c) in self.conversations.iter().enumerate() {
            let response = pages.get(&format!("/messages/{}", c.id));
            let expected = match who {
                Who::Anonymous => StatusCode::SEE_OTHER,
                _ if self.conversation_readable(i, who) => StatusCode::OK,
                _ => StatusCode::NOT_FOUND,
            };
            assert_eq!(response.status, expected, "{who:?} conversation {i}");
            if expected == StatusCode::OK {
                assert!(response.body.contains(&c.title_token));
            }
        }
        if who != Who::Anonymous {
            let viewer = Viewer::User(
                self.block(sessions::user_ctx(
                    self.fresh.state.db(),
                    self.viewer_id(who).unwrap(),
                ))
                .unwrap()
                .unwrap(),
            );
            let expected = self.expected_unread(who);
            let counted = self
                .block(content::notifications::unread_count(
                    self.fresh.state.db(),
                    &viewer,
                ))
                .unwrap();
            assert_eq!(counted, expected, "{who:?}'s unread notifications");
            let streamed = self.streamed_counts(who);
            assert!(
                streamed.starts_with(&format!("{{\"notifications\":{expected},")),
                "{who:?} was streamed {streamed}, expecting {expected}"
            );
        }
        for m in &self.messages {
            let permalink = pages.get(&format!("/p/{}", m.id));
            if !self.conversation_readable(m.conversation, who) {
                assert_eq!(permalink.status, StatusCode::NOT_FOUND);
                continue;
            }
            let c = &self.conversations[m.conversation];
            let page = page_of_seq(m.seq);
            let expected = if page == 1 {
                format!("/messages/{}#p{}", c.id, m.id)
            } else {
                format!("/messages/{}?page={page}#p{}", c.id, m.id)
            };
            assert_eq!(permalink.location(), expected);
            if !m.deleted {
                let page = pages.get(expected.split('#').next().unwrap());
                let body = &page.body;
                assert!(body.contains(&m.body_token), "message {} missing", m.id);
            }
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
            old_titles: Vec::new(),
            merged_into: None,
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
        self.notify(created.first_post_id);
    }

    #[rule]
    fn reply(&mut self, tc: TestCase) {
        tc.assume(!self.threads.is_empty());
        let thread = tc.draw(gs::integers::<usize>().max_value(self.threads.len() - 1));
        let author = tc.draw(gs::sampled_from(self.users.clone()));
        let in_thread: Vec<usize> = (0..self.posts.len())
            .filter(|&p| self.posts[p].thread == thread)
            .collect();
        // A thread whose posts were all moved away has none to reply to.
        let reply_to = if !in_thread.is_empty() && tc.draw(gs::booleans()) {
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

    #[rule]
    fn start_conversation(&mut self, tc: TestCase) {
        tc.assume(self.conversations.len() < 4);
        let people = self.dm_people();
        let author = tc.draw(gs::sampled_from(people.clone()));
        let mut others: BTreeSet<i64> = people
            .iter()
            .copied()
            .filter(|&u| u != author && tc.draw(gs::booleans()))
            .collect();
        if others.is_empty() {
            others.insert(*people.iter().find(|&&u| u != author).unwrap());
        }
        let title_token = self.token();
        let body_token = self.token();
        let mention = tc.draw(gs::booleans());
        let created = self
            .block(store::create_conversation(
                self.db(),
                &store::NewConversation {
                    author_id: author,
                    title: format!("Chat {title_token}"),
                    recipient_ids: others.iter().copied().collect(),
                    body_md: if mention {
                        format!("Message {body_token} for @viewer and @watcher")
                    } else {
                        format!("Message {body_token}")
                    },
                    created_at: None,
                },
            ))
            .unwrap();
        let mut participants = others;
        participants.insert(author);
        self.conversations.push(ConversationModel {
            id: created.thread_id,
            title_token,
            participants,
            last_seq: 1,
            reported: false,
        });
        self.messages.push(MessageModel {
            id: created.first_post_id,
            conversation: self.conversations.len() - 1,
            seq: 1,
            body_token,
            deleted: false,
        });
        self.changed(Change::PostCreated {
            post_id: created.first_post_id,
        });
        self.notify(created.first_post_id);
    }

    #[rule]
    fn send_message(&mut self, tc: TestCase) {
        tc.assume(!self.conversations.is_empty());
        let c = tc.draw(gs::integers::<usize>().max_value(self.conversations.len() - 1));
        let people: Vec<i64> = self.conversations[c].participants.iter().copied().collect();
        tc.assume(!people.is_empty());
        let author = tc.draw(gs::sampled_from(people));
        let mention = tc.draw(gs::booleans());
        self.add_message(c, author, mention);
    }

    /// Someone leaves a conversation, or is added (back) to one.
    #[rule]
    fn toggle_participant(&mut self, tc: TestCase) {
        tc.assume(!self.conversations.is_empty());
        let c = tc.draw(gs::integers::<usize>().max_value(self.conversations.len() - 1));
        let user = tc.draw(gs::sampled_from(self.dm_people()));
        let id = self.conversations[c].id;
        if self.conversations[c].participants.remove(&user) {
            assert!(
                self.block(store::leave_conversation(self.db(), id, user))
                    .unwrap()
            );
        } else {
            let adder = *self.dm_people().first().unwrap();
            assert!(
                self.block(store::add_participant(self.db(), id, user, adder))
                    .unwrap()
            );
            self.conversations[c].participants.insert(user);
        }
    }

    #[rule]
    fn delete_message(&mut self, tc: TestCase) {
        tc.assume(!self.messages.is_empty());
        let m = tc.draw(gs::integers::<usize>().max_value(self.messages.len() - 1));
        let message = &mut self.messages[m];
        message.deleted = !message.deleted;
        let (id, deleted) = (message.id, message.deleted);
        self.exec(format!(
            "UPDATE posts SET deleted_at = {} WHERE id = {id}",
            if deleted { "now()" } else { "NULL" }
        ));
        self.changed(Change::PostRemoved { post_id: id });
    }

    /// Someone in a conversation reports a message, which opens that
    /// conversation to staff.
    #[rule]
    fn report_message(&mut self, tc: TestCase) {
        tc.assume(!self.messages.is_empty());
        let m = tc.draw(gs::integers::<usize>().max_value(self.messages.len() - 1));
        let c = self.messages[m].conversation;
        let people: Vec<i64> = self.conversations[c].participants.iter().copied().collect();
        tc.assume(!people.is_empty());
        let reporter = tc.draw(gs::sampled_from(people));
        let viewer = self.viewer_of(reporter);
        let outcome = self
            .block(parlour::moderation::report_post(
                &self.cached.state,
                &viewer,
                self.messages[m].id,
                "harassment",
                "",
            ))
            .unwrap();
        if outcome.is_ok() {
            self.conversations[c].reported = true;
        }
    }

    /// A moderator silences or suspends the member viewer, or lifts it.
    #[rule]
    fn restrict_member(&mut self, tc: TestCase) {
        use parlour::mod_records::Restriction;
        let suspend = tc.draw(gs::booleans());
        let (restriction, on) = if suspend {
            (Restriction::Suspend, !self.member_suspended)
        } else {
            (Restriction::Silence, !self.member_silenced)
        };
        let staff = self.viewer_of(self.staff_id);
        let outcome = self
            .block(parlour::moderation::restrict_member(
                &self.cached.state,
                &staff,
                "viewer",
                restriction,
                on.then_some(3),
                "Testing",
                None,
            ))
            .unwrap();
        assert!(outcome.is_ok(), "{restriction:?} {on}: {outcome:?}");
        if suspend {
            self.member_suspended = on;
        } else {
            self.member_silenced = on;
        }
    }

    /// A moderator hides a post or shows it again, through the moderation
    /// action (which also logs it and tells its author).
    #[rule]
    fn moderator_hides(&mut self, tc: TestCase) {
        use parlour::moderation::PostAction;
        tc.assume(!self.posts.is_empty());
        let post = tc.draw(gs::integers::<usize>().max_value(self.posts.len() - 1));
        let p = &self.posts[post];
        tc.assume(!self.threads[p.thread].deleted);
        let action = if p.hidden {
            PostAction::Unhide
        } else {
            PostAction::Hide
        };
        let staff = self.viewer_of(self.staff_id);
        let outcome = self
            .block(parlour::moderation::moderate_post(
                &self.cached.state,
                &staff,
                p.id,
                action,
                "Testing",
                None,
            ))
            .unwrap();
        // A deleted post can't be hidden.
        assert_eq!(outcome.is_ok(), !p.deleted, "{action:?}");
        if outcome.is_ok() {
            self.posts[post].hidden = action == PostAction::Hide;
        }
    }

    /// A moderator renames a thread, which the log remembers.
    #[rule]
    fn moderator_renames(&mut self, tc: TestCase) {
        use parlour::moderation::ThreadAction;
        tc.assume(!self.threads.is_empty());
        let t = tc.draw(gs::integers::<usize>().max_value(self.threads.len() - 1));
        tc.assume(!self.threads[t].deleted);
        let token = self.token();
        let staff = self.viewer_of(self.staff_id);
        let outcome = self
            .block(parlour::moderation::moderate_thread(
                &self.cached.state,
                &staff,
                self.threads[t].id,
                ThreadAction::Rename(format!("Thread {token}")),
                "Clearer",
            ))
            .unwrap();
        assert!(outcome.is_ok(), "{outcome:?}");
        let thread = &mut self.threads[t];
        let old = std::mem::replace(&mut thread.title_token, token);
        thread.old_titles.push(old);
    }

    /// A moderator moves some of a thread's posts to the end of another
    /// thread or a new one (DESIGN.md §8). Moving them all merges the
    /// thread into the other.
    #[rule]
    fn moderator_moves_posts(&mut self, tc: TestCase) {
        use parlour::moderation::Destination;
        tc.assume(!self.threads.is_empty());
        let from = tc.draw(gs::integers::<usize>().max_value(self.threads.len() - 1));
        tc.assume(!self.threads[from].deleted);
        let in_thread: Vec<usize> = (0..self.posts.len())
            .filter(|&p| self.posts[p].thread == from)
            .collect();
        tc.assume(!in_thread.is_empty());
        let moving: Vec<usize> = in_thread
            .iter()
            .copied()
            .filter(|_| tc.draw(gs::booleans()))
            .collect();
        tc.assume(!moving.is_empty());
        let numbers: Vec<String> = moving
            .iter()
            .map(|&p| self.posts[p].seq.to_string())
            .collect();
        let others: Vec<usize> = (0..self.threads.len())
            .filter(|&t| t != from && !self.threads[t].deleted)
            .collect();
        let new_thread = others.is_empty() || tc.draw(gs::booleans());
        let (destination, title_token, category) = if new_thread {
            let category = tc.draw(gs::integers::<usize>().max_value(self.categories.len() - 1));
            let token = self.token();
            (
                Destination::New {
                    category: self.categories[category].slug.clone(),
                    title: format!("Thread {token}"),
                },
                Some(token),
                category,
            )
        } else {
            let to = tc.draw(gs::sampled_from(others));
            (
                Destination::Existing(self.threads[to].id.to_string()),
                None,
                to,
            )
        };
        let staff = self.viewer_of(self.staff_id);
        let to_id = self
            .block(parlour::moderation::move_posts(
                &self.cached.state,
                &staff,
                self.threads[from].id,
                &numbers.join(", "),
                &destination,
                "Tidying",
            ))
            .unwrap()
            .expect("moving posts works");
        let to = match title_token {
            Some(title_token) => {
                self.threads.push(ThreadModel {
                    id: to_id,
                    category,
                    title_token,
                    deleted: false,
                    last_seq: 0,
                    old_titles: Vec::new(),
                    merged_into: None,
                });
                self.threads.len() - 1
            }
            None => category,
        };
        assert_eq!(self.threads[to].id, to_id);
        for p in moving {
            self.threads[to].last_seq += 1;
            self.posts[p].thread = to;
            self.posts[p].seq = self.threads[to].last_seq;
        }
        if !self.posts.iter().any(|p| p.thread == from) {
            self.threads[from].deleted = true;
            self.threads[from].merged_into = Some(to);
        }
    }

    /// A new member's thread is held for a moderator.
    #[rule]
    fn hold_post(&mut self, tc: TestCase) {
        tc.assume(self.held.len() < 3);
        let author = tc.draw(gs::sampled_from(self.users.clone()));
        let category = tc.draw(gs::integers::<usize>().max_value(self.categories.len() - 1));
        let (title_token, body_token) = (self.token(), self.token());
        let id = self
            .block(parlour::mod_records::hold(
                self.db(),
                author,
                &parlour::mod_records::HeldPlace::Thread {
                    category_id: self.categories[category].id,
                    title: format!("Thread {title_token}"),
                    forked_from_post_id: None,
                },
                &format!("Held {body_token}"),
            ))
            .unwrap();
        self.held.push(HeldModel {
            id,
            author,
            title_token,
            body_token,
        });
    }

    /// A moderator approves a held post, which is posted if its author may
    /// still post there, or rejects it.
    #[rule]
    fn decide_held(&mut self, tc: TestCase) {
        tc.assume(!self.held.is_empty());
        let h = tc.draw(gs::integers::<usize>().max_value(self.held.len() - 1));
        let id = self.held[h].id;
        let staff = self.viewer_of(self.staff_id);
        if tc.draw(gs::booleans()) {
            let approved = self
                .block(parlour::moderation::approve(&self.cached.state, &staff, id))
                .unwrap();
            if let Ok(post_id) = approved {
                let (thread_id, category_id): (i64, i64) = self
                    .block(
                        sqlx::query_as(
                            "SELECT t.id, t.category_id FROM posts p JOIN threads t ON t.id = p.thread_id
                             WHERE p.id = $1",
                        )
                        .bind(post_id)
                        .fetch_one(self.db()),
                    )
                    .unwrap();
                let held = &self.held[h];
                let category = self
                    .categories
                    .iter()
                    .position(|c| c.id == category_id)
                    .unwrap();
                let (title_token, body_token, author) = (
                    held.title_token.clone(),
                    held.body_token.clone(),
                    held.author,
                );
                self.threads.push(ThreadModel {
                    id: thread_id,
                    category,
                    title_token,
                    deleted: false,
                    last_seq: 1,
                    old_titles: Vec::new(),
                    merged_into: None,
                });
                self.posts.push(PostModel {
                    id: post_id,
                    thread: self.threads.len() - 1,
                    seq: 1,
                    author,
                    body_token,
                    deleted: false,
                    hidden: false,
                    history: Vec::new(),
                    settled: false,
                });
                self.notify(post_id);
                return;
            }
        }
        // Rejected (or approved once already): either way it stays where
        // its author and staff can read it.
        let _ = self
            .block(parlour::moderation::reject(
                &self.cached.state,
                &staff,
                id,
                "No",
            ))
            .unwrap();
    }

    /// The notification email jobs run, as the job workers would once the
    /// delay is over.
    #[rule]
    fn send_notification_emails(&mut self, _: TestCase) {
        self.send_emails();
    }

    /// Requests every page through the cache, so that later changes have
    /// something to invalidate.
    #[rule]
    fn warm(&mut self, tc: TestCase) {
        for path in self.paths(Who::Anonymous) {
            self.get(&self.cached, &path);
        }
        if let Mode::Visibility(who) = self.mode
            && who != Who::Anonymous
        {
            for path in self.paths(who) {
                self.get_as(who, &path);
            }
            for path in self.item_paths(&self.sample(&tc)) {
                self.get_as(who, &path);
            }
        }
    }

    /// Checked at the start and end of each run and sampled in between.
    #[invariant]
    fn pages_are_right(&self, tc: TestCase) {
        self.check(&tc);
    }

    /// Every change keeps the maintained post counts exact.
    #[invariant(always_run)]
    fn post_counts_are_current(&self, _: TestCase) {
        let stale: i64 = self
            .block(sqlx::query_scalar(STALE_POST_COUNTS).fetch_one(self.db()))
            .unwrap();
        assert_eq!(stale, 0, "maintained post counts are stale");
    }
}

// Every step writes to Postgres and renders pages over HTTP, so a test
// case takes seconds, and more when the suites here run side by side.
// Hegel counts that as slow generation; it is expected, so the TooSlow
// health check is off. The number of test cases comes from hegel.toml:
// a quick run locally, a thorough one on CI (see the README).
#[hegel::standalone_function(suppress_health_check = [HealthCheck::TooSlow])]
fn run_machine(tc: TestCase, rt: &Handle, db: &PgPool, mode: Mode) {
    let world = World::new(rt.clone(), db.clone(), mode);
    hegel::stateful::machine(world).steps(25).run(tc);
}

async fn run_in_blocking_thread(pool: PgPool, mode: Mode) {
    let rt = Handle::current();
    tokio::task::spawn_blocking(move || run_machine(&rt, &pool, mode))
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
// roles, categories with read and write levels and groups, threads, posts,
// quote-forks, moves and deletions, and asks the `content` module directly
// what each viewer gets, where they may post, and where they may quote
// each post they can read. Every answer is checked, in both directions,
// against `Access::reads_category` and friends below, which are written from
// DESIGN.md §3.1 and know nothing of the SQL.

struct UserModel {
    id: i64,
    staff: bool,
    groups: BTreeSet<usize>,
    /// Silenced: reads as before, but may not post.
    silenced: bool,
    /// Suspended: reads only what visitors read, and isn't staff.
    suspended: bool,
}

impl UserModel {
    /// Staff, as far as reading goes: a suspended moderator is a visitor.
    fn acts_as_staff(&self) -> bool {
        self.staff && !self.suspended
    }
}

struct AccessCategory {
    id: i64,
    slug: String,
    level: &'static str,
    read_groups: BTreeSet<usize>,
    write_level: &'static str,
    write_groups: BTreeSet<usize>,
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
    /// The image the post shows, by hash.
    image: String,
    /// The images its earlier versions showed, and whether each version
    /// has been redacted.
    earlier_images: Vec<(String, bool)>,
    thread: usize,
    author: usize,
    deleted: bool,
    hidden: bool,
}

struct AccessConversation {
    id: i64,
    /// Current participants, as indexes into `users`.
    participants: BTreeSet<usize>,
    /// Whether one of its messages was reported, which opens it to staff.
    reported: bool,
}

struct AccessMessage {
    id: i64,
    image: String,
    conversation: usize,
    deleted: bool,
    /// Hidden by a moderator (after it was reported).
    hidden: bool,
}

struct Access {
    rt: Handle,
    pool: PgPool,
    db: Db,
    state: AppState,
    users: Vec<UserModel>,
    groups: Vec<i64>,
    categories: Vec<AccessCategory>,
    threads: Vec<AccessThread>,
    posts: Vec<AccessPost>,
    /// For each post, the threads it has been moved out of.
    moved_from: Vec<BTreeSet<usize>>,
    conversations: Vec<AccessConversation>,
    messages: Vec<AccessMessage>,
    images: u64,
    /// The images in posts held for approval.
    held_images: Vec<String>,
}

/// The members who never change role; there is always an admin among them.
const FIXED_USERS: usize = 1;

impl Access {
    fn new(rt: Handle, pool: PgPool) -> Access {
        rt.block_on(async {
            sqlx::raw_sql(
                "TRUNCATE settings, jobs, posts, threads, categories, sections, groups, users RESTART IDENTITY CASCADE",
            )
            .execute(&pool)
            .await
            .unwrap();
        });
        let mut world = Access {
            db: Db::new(pool.clone()),
            state: AppState::with_cache(pool.clone(), PageCache::disabled()),
            pool,
            rt,
            users: Vec::new(),
            groups: Vec::new(),
            categories: Vec::new(),
            threads: Vec::new(),
            posts: Vec::new(),
            moved_from: Vec::new(),
            conversations: Vec::new(),
            messages: Vec::new(),
            images: 0,
            held_images: Vec::new(),
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
                silenced: false,
                suspended: false,
            });
        }
        for _ in 0..2 {
            world.add_group();
        }
        for level in ["public", "members", "groups", "groups"] {
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
            write_level: "members",
            write_groups: BTreeSet::new(),
        });
    }

    /// A new uploaded image (with no uploader, who could always fetch
    /// it), and the Markdown that shows it.
    fn new_image(&mut self) -> (String, String) {
        self.images += 1;
        let sha = format!("{:064x}", self.images);
        self.exec(format!(
            "INSERT INTO uploads (sha256, ext, mime, bytes, width, height)
             VALUES ('{sha}', 'png', 'image/png', 1, 1, 1)"
        ));
        let markdown = format!(" ![](/uploads/{sha}.png)");
        (sha, markdown)
    }

    fn add_post(&mut self, thread: usize, author: usize) {
        let (image, markdown) = self.new_image();
        let id = self
            .block(store::create_post(
                &self.pool,
                &NewPost {
                    thread_id: self.threads[thread].id,
                    author_id: self.users[author].id,
                    body_md: format!("Text{markdown}"),
                    reply_to_post_id: None,
                    created_at: None,
                },
            ))
            .unwrap();
        self.posts.push(AccessPost {
            id,
            image,
            earlier_images: Vec::new(),
            thread,
            author,
            deleted: false,
            hidden: false,
        });
        self.moved_from.push(BTreeSet::new());
    }

    // -- The reference permission function, from DESIGN.md §3.1.

    /// `who` is an index into `users`, or `None` for an anonymous visitor.
    /// A suspended member reads what a visitor reads (DESIGN.md §3.1:
    /// members-only means members who are not suspended).
    fn reads_category(&self, who: Option<usize>, category: usize) -> bool {
        let c = &self.categories[category];
        let who = who.filter(|&u| !self.users[u].suspended);
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

    /// Whether the viewer is staff who may read hidden posts.
    fn sees_hidden(&self, who: Option<usize>) -> bool {
        who.is_some_and(|u| self.users[u].acts_as_staff())
    }

    /// Whether the viewer is shown the post's text where it is: live
    /// posts, and for staff hidden ones too.
    fn shown_post(&self, who: Option<usize>, post: usize) -> bool {
        let p = &self.posts[post];
        !p.deleted && (!p.hidden || self.sees_hidden(who))
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

    /// DESIGN.md §3.7: only a conversation's current participants read it,
    /// and staff once one of its messages has been reported; suspended
    /// members read none.
    fn reads_conversation(&self, who: Option<usize>, conversation: usize) -> bool {
        let c = &self.conversations[conversation];
        who.filter(|&u| !self.users[u].suspended)
            .is_some_and(|u| c.participants.contains(&u) || (c.reported && self.users[u].staff))
    }

    /// The viewer's own conversations, which their list shows.
    fn in_conversation(&self, who: Option<usize>, conversation: usize) -> bool {
        who.filter(|&u| !self.users[u].suspended)
            .is_some_and(|u| self.conversations[conversation].participants.contains(&u))
    }

    /// Checks everything the content module says about conversations.
    fn check_conversations(&self, who: Option<usize>, viewer: &Viewer) {
        let db = &self.db;
        let mut deps = Deps::new();
        let listed: BTreeSet<i64> = self
            .block(content::messages::conversations(db, viewer, 1))
            .unwrap()
            .rows
            .iter()
            .map(|r| r.id)
            .collect();
        let expected = Self::ids(
            &self.conversations,
            |c| self.in_conversation(who, c),
            |c| c.id,
        );
        assert_eq!(listed, expected, "{who:?}: conversations");
        for (c, conversation) in self.conversations.iter().enumerate() {
            let visible = self.reads_conversation(who, c);
            let page = self
                .block(content::messages::conversation_page(
                    db,
                    viewer,
                    conversation.id,
                    1,
                    &mut deps,
                ))
                .unwrap();
            assert_eq!(page.is_some(), visible, "{who:?}: conversation {c}");
            if let Some(page) = page {
                let people: BTreeSet<i64> = page
                    .conversation
                    .participants
                    .iter()
                    .map(|p| p.user.id)
                    .collect();
                let expected: BTreeSet<i64> = conversation
                    .participants
                    .iter()
                    .map(|&u| self.users[u].id)
                    .collect();
                assert_eq!(people, expected, "{who:?}: people in {c}");
            }
            for found in [
                self.block(content::thread_target(db, viewer, conversation.id))
                    .unwrap()
                    .is_some(),
                self.block(content::first_unread(db, viewer, conversation.id))
                    .unwrap()
                    .is_some(),
            ] {
                assert_eq!(found, visible, "{who:?}: conversation {c}");
            }
            // Conversations never have category pages.
            assert!(
                self.block(content::thread_slug(db, viewer, conversation.id))
                    .unwrap()
                    .is_none()
            );
            assert!(
                self.block(content::thread_page(
                    db,
                    viewer,
                    conversation.id,
                    1,
                    &mut deps
                ))
                .unwrap()
                .is_none()
            );
        }
        for (m, message) in self.messages.iter().enumerate() {
            let visible = self.reads_conversation(who, message.conversation);
            let live = visible && !message.deleted;
            let reactable = live && !message.hidden;
            let target = self
                .block(content::post_target(db, viewer, message.id))
                .unwrap();
            assert_eq!(target.is_some(), visible, "{who:?}: message {m}");
            let location = self
                .block(content::post_location(db, viewer, message.id))
                .unwrap();
            assert_eq!(location.is_some(), visible, "{who:?}: message {m}");
            assert!(location.is_none_or(|l| l.private));
            let history = self
                .block(content::post_history(db, viewer, message.id, &mut deps))
                .unwrap();
            let shown = live && (!message.hidden || self.sees_hidden(who));
            assert_eq!(history.is_some(), shown, "{who:?}: history of message {m}");
            let reactions = self
                .block(content::post_reactions(db, viewer, message.id, &mut deps))
                .unwrap();
            assert_eq!(
                reactions.is_some(),
                reactable,
                "{who:?}: reactions to message {m}"
            );
            // Messages can never be quoted into a thread.
            if let Some(target) = target {
                let destinations = self
                    .block(writing::fork_destinations(&self.state, viewer, &target))
                    .unwrap_or_else(|_| panic!("{who:?}: fork destinations of message {m} failed"));
                assert!(
                    destinations.is_empty(),
                    "{who:?}: message {m} can be forked"
                );
            }
        }
        // Messages are found by searching only by the people in the
        // conversation now: not by staff it was reported to, nor by
        // anyone suspended.
        let found = self.search(viewer, "psst or message");
        let expected = Self::ids(
            &self.messages,
            |m| {
                let message = &self.messages[m];
                self.in_conversation(who, message.conversation)
                    && !message.deleted
                    && (!message.hidden || self.sees_hidden(who))
            },
            |m| m.id,
        );
        assert_eq!(found, expected, "{who:?}: messages found by search");
        for (m, message) in self.messages.iter().enumerate() {
            let shown = self.reads_conversation(who, message.conversation)
                && !message.deleted
                && (!message.hidden || self.sees_hidden(who));
            let access = self
                .block(content::uploads::upload_access(
                    &self.db,
                    viewer,
                    &message.image,
                    "png",
                ))
                .unwrap();
            assert_eq!(
                access,
                shown.then_some(UploadAccess::Private),
                "{who:?}: image in message {m}"
            );
        }
    }

    /// Whether the viewer may start threads in a category: they read it,
    /// and they are staff, or it lets every member post, or they are in
    /// one of its write groups.
    fn posts_in_category(&self, who: Option<usize>, category: usize) -> bool {
        let Some(u) = who else { return false };
        let (c, user) = (&self.categories[category], &self.users[u]);
        !user.silenced
            && !user.suspended
            && self.reads_category(who, category)
            && match c.write_level {
                "members" => true,
                "groups" => user.staff || !user.groups.is_disjoint(&c.write_groups),
                _ => user.staff,
            }
    }

    /// The "at most as visible" table in DESIGN.md §3.1: may content from
    /// category `source` be quoted into category `dest`?
    fn may_quote_into(&self, source: usize, dest: usize) -> bool {
        let (s, d) = (&self.categories[source], &self.categories[dest]);
        match (s.level, d.level) {
            ("public", _) => true,
            ("members", "members" | "groups") => true,
            ("groups", "groups") => d.read_groups.is_subset(&s.read_groups),
            _ => false,
        }
    }

    /// Where the viewer may start new threads, and where they may quote
    /// each post they can read.
    fn check_writing(&self, who: Option<usize>, viewer: &Viewer) {
        let db = &self.db;
        let readable = self
            .block(content::readable_categories(db, viewer))
            .unwrap();
        let may_post: BTreeSet<i64> = readable
            .iter()
            .filter(|c| writing::may_post_in(viewer, c))
            .map(|c| c.category.id)
            .collect();
        let expected = Self::ids(
            &self.categories,
            |c| self.posts_in_category(who, c),
            |c| c.id,
        );
        assert_eq!(may_post, expected, "{who:?}: categories to post in");
        for (p, post) in self.posts.iter().enumerate() {
            if !self.reads_post(who, p) {
                continue;
            }
            let source = self
                .block(content::post_target(db, viewer, post.id))
                .unwrap()
                .expect("a readable post is found");
            let destinations: BTreeSet<i64> = self
                .block(writing::fork_destinations(&self.state, viewer, &source))
                .unwrap_or_else(|_| panic!("{who:?}: fork destinations of post {p} failed"))
                .iter()
                .map(|c| c.category.id)
                .collect();
            let from = self.threads[post.thread].category;
            let expected = Self::ids(
                &self.categories,
                |c| self.posts_in_category(who, c) && self.may_quote_into(from, c),
                |c| c.id,
            );
            assert_eq!(
                destinations, expected,
                "{who:?}: where post {p} can be quoted"
            );
        }
    }

    /// Gives category `c` random read and write levels and groups, as an
    /// admin can with the category form.
    fn set_category_access(&mut self, tc: &TestCase, c: usize) {
        let level = tc.draw(gs::sampled_from(LEVELS.to_vec()));
        let read_groups: BTreeSet<usize> = (0..self.groups.len())
            .filter(|_| tc.draw(gs::booleans()))
            .collect();
        let write_level = tc.draw(gs::sampled_from(vec!["members", "groups", "staff"]));
        let mut write_groups: BTreeSet<usize> = (0..self.groups.len())
            .filter(|_| tc.draw(gs::booleans()))
            .collect();
        if level == "groups" && write_level == "groups" {
            // The form refuses groups that may post but not read.
            write_groups.retain(|g| read_groups.contains(g));
        }
        let category = &self.categories[c];
        let to_ids = |groups: &BTreeSet<usize>| groups.iter().map(|&g| self.groups[g]).collect();
        let settings = CategorySettings {
            section_id: None,
            slug: category.slug.clone(),
            name: format!("Category {c}"),
            description: String::new(),
            position: c as i32,
            read_level: level.into(),
            write_level: write_level.into(),
            read_groups: to_ids(&read_groups),
            write_groups: to_ids(&write_groups),
        }
        .clean()
        .unwrap();
        self.block(store::update_category(&self.db, category.id, &settings))
            .unwrap()
            .unwrap();
        let category = &mut self.categories[c];
        category.level = level;
        category.read_groups = if level == "groups" {
            read_groups
        } else {
            BTreeSet::new()
        };
        category.write_level = write_level;
        category.write_groups = if write_level == "groups" {
            write_groups
        } else {
            BTreeSet::new()
        };
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
            ] {
                assert_eq!(found, visible, "{who:?}: thread {t}");
            }
            // A thread whose posts have all moved away has none to jump to.
            let has_posts = self.posts.iter().any(|p| p.thread == t);
            assert_eq!(
                self.block(content::first_unread(db, &viewer, thread.id))
                    .unwrap()
                    .is_some(),
                visible && has_posts,
                "{who:?}: first unread in thread {t}"
            );
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
                if let PostBody::Moved { to } = &shown.body {
                    // A post moved away leaves its number here, naming
                    // where it is now only to those who can read it there
                    // (which may be this thread again, further on, if it
                    // was moved back).
                    let now = self.posts[p].thread;
                    assert!(
                        self.moved_from[p].contains(&t),
                        "{who:?}: post {p} never left {t}"
                    );
                    assert_eq!(
                        to.as_ref().map(|r| r.id),
                        self.reads_thread(who, now).then(|| self.threads[now].id),
                        "{who:?}: where post {p} went"
                    );
                    continue;
                }
                assert_eq!(self.posts[p].thread, t);
                assert_eq!(
                    matches!(shown.body, PostBody::Live { .. }),
                    self.shown_post(who, p),
                    "{who:?}: post {p}"
                );
                if let PostBody::Live { hidden, .. } = &shown.body {
                    assert_eq!(*hidden, self.posts[p].hidden, "{who:?}: post {p} marked");
                }
                let backlinks: BTreeSet<i64> = shown.forks.iter().map(|f| f.thread.id).collect();
                let expected = if self.shown_post(who, p) {
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
                    self.shown_post(who, p),
                    "{who:?}: post {p}"
                );
                assert_eq!(target.is_live(), self.post_live(p), "{who:?}: post {p}");
            }
            let readable = self.reads_post(who, p);
            let history = self
                .block(content::post_history(db, &viewer, post.id, &mut deps))
                .unwrap();
            assert_eq!(
                history.is_some(),
                in_thread && self.shown_post(who, p),
                "{who:?}: history of {p}"
            );
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

        // Search finds exactly the posts the viewer is shown where they
        // are: every post's text is "Text", or "Opening" for a thread's
        // first post.
        let found = self.search(&viewer, "text or opening");
        let expected = Self::ids(
            &self.posts,
            |p| self.reads_thread(who, self.posts[p].thread) && self.shown_post(who, p),
            |p| p.id,
        );
        assert_eq!(found, expected, "{who:?}: search");

        // Each post's image may be fetched by those who are shown the
        // post, and by everyone (and so by shared caches) only if every
        // visitor is.
        for (p, post) in self.posts.iter().enumerate() {
            let expected = if self.reads_thread(None, post.thread) && self.post_live(p) {
                Some(UploadAccess::Public)
            } else if self.reads_thread(who, post.thread) && self.shown_post(who, p) {
                Some(UploadAccess::Private)
            } else {
                None
            };
            let access = self
                .block(content::uploads::upload_access(
                    db,
                    &viewer,
                    &post.image,
                    "png",
                ))
                .unwrap();
            assert_eq!(access, expected, "{who:?}: image in post {p}");
            // An earlier version's image goes with the post, as the edit
            // history does, unless the version was redacted.
            for (image, redacted) in &post.earlier_images {
                let access = self
                    .block(content::uploads::upload_access(db, &viewer, image, "png"))
                    .unwrap();
                let expected = if *redacted { None } else { expected };
                assert_eq!(access, expected, "{who:?}: earlier image in post {p}");
            }
        }

        // Images in posts held for approval are for staff, who review
        // them.
        for image in &self.held_images {
            let access = self
                .block(content::uploads::upload_access(db, &viewer, image, "png"))
                .unwrap();
            assert_eq!(
                access,
                self.sees_hidden(who).then_some(UploadAccess::Private),
                "{who:?}: image in a held post"
            );
        }

        self.check_writing(who, &viewer);
        self.check_conversations(who, &viewer);
    }

    /// The ids of every post a search finds for the viewer.
    fn search(&self, viewer: &Viewer, words: &str) -> BTreeSet<i64> {
        let query = content::search::SearchQuery {
            text: words.into(),
            ..Default::default()
        };
        match self
            .block(content::search::search(&self.db, viewer, &query, 0, 10_000))
            .unwrap()
        {
            content::search::SearchOutcome::Hits { hits, more } => {
                assert!(!more);
                hits.iter().map(|h| h.post_id).collect()
            }
            content::search::SearchOutcome::NoTerms => panic!("{words:?} has terms"),
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

    /// What the admin's category form does: new read and write levels and
    /// groups.
    #[rule]
    fn set_access(&mut self, tc: TestCase) {
        let c = tc.draw(gs::integers::<usize>().max_value(self.categories.len() - 1));
        self.set_category_access(&tc, c);
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
        let (image, markdown) = self.new_image();
        let created = self
            .block(store::create_thread(
                &self.pool,
                &NewThread {
                    category_id: self.categories[category].id,
                    title: "A thread".into(),
                    author_id: self.users[author].id,
                    body_md: format!("Opening{markdown}"),
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
            image,
            earlier_images: Vec::new(),
            thread: self.threads.len() - 1,
            author,
            deleted: false,
            hidden: false,
        });
        self.moved_from.push(BTreeSet::new());
    }

    /// Edits a post past its grace period, so the old version, and its
    /// image, stay in its history.
    #[rule]
    fn edit_post(&mut self, tc: TestCase) {
        tc.assume(!self.posts.is_empty());
        let p = tc.draw(gs::integers::<usize>().max_value(self.posts.len() - 1));
        let (image, markdown) = self.new_image();
        let post = &self.posts[p];
        self.exec(format!(
            "UPDATE posts SET created_at = created_at - interval '1 hour' WHERE id = {}",
            post.id
        ));
        let edited = self
            .block(store::edit_post(
                &self.pool,
                post.id,
                self.users[post.author].id,
                &format!("Text{markdown}"),
            ))
            .unwrap();
        assert!(matches!(edited, store::Edited::Revised { .. }));
        let post = &mut self.posts[p];
        let old = std::mem::replace(&mut post.image, image);
        post.earlier_images.push((old, false));
    }

    #[rule]
    fn redact_revision(&mut self, tc: TestCase) {
        let edited: Vec<usize> = (0..self.posts.len())
            .filter(|&p| !self.posts[p].earlier_images.is_empty())
            .collect();
        tc.assume(!edited.is_empty());
        let p = edited[tc.draw(gs::integers::<usize>().max_value(edited.len() - 1))];
        let n = self.posts[p].earlier_images.len();
        let rev = tc.draw(gs::integers::<usize>().max_value(n - 1));
        let done = self
            .block(parlour::mod_records::redact_revision(
                &self.pool,
                self.posts[p].id,
                rev as i32 + 1,
                self.users[0].id,
            ))
            .unwrap();
        let redacted = &mut self.posts[p].earlier_images[rev].1;
        assert_eq!(done, !*redacted);
        *redacted = true;
    }

    /// Holds a reply with an image for approval, in any thread.
    #[rule]
    fn hold_post(&mut self, tc: TestCase) {
        tc.assume(!self.threads.is_empty() && self.held_images.len() < 4);
        let thread = tc.draw(gs::integers::<usize>().max_value(self.threads.len() - 1));
        let author = tc.draw(gs::integers::<usize>().max_value(self.users.len() - 1));
        let (image, markdown) = self.new_image();
        self.block(parlour::mod_records::hold(
            &self.pool,
            self.users[author].id,
            &parlour::mod_records::HeldPlace::Reply {
                thread_id: self.threads[thread].id,
                reply_to_post_id: None,
            },
            &format!("Held{markdown}"),
        ))
        .unwrap();
        self.held_images.push(image);
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

    #[rule]
    fn start_conversation(&mut self, tc: TestCase) {
        tc.assume(self.conversations.len() < 4);
        let author = tc.draw(gs::integers::<usize>().max_value(self.users.len() - 1));
        let mut others: BTreeSet<usize> = (0..self.users.len())
            .filter(|&u| u != author && tc.draw(gs::booleans()))
            .collect();
        if others.is_empty() {
            others.insert((author + 1) % self.users.len());
        }
        let (image, markdown) = self.new_image();
        let created = self
            .block(store::create_conversation(
                &self.pool,
                &store::NewConversation {
                    author_id: self.users[author].id,
                    title: String::new(),
                    recipient_ids: others.iter().map(|&u| self.users[u].id).collect(),
                    body_md: format!("Psst{markdown}"),
                    created_at: None,
                },
            ))
            .unwrap();
        others.insert(author);
        self.conversations.push(AccessConversation {
            id: created.thread_id,
            participants: others,
            reported: false,
        });
        self.messages.push(AccessMessage {
            id: created.first_post_id,
            image,
            conversation: self.conversations.len() - 1,
            deleted: false,
            hidden: false,
        });
    }

    #[rule]
    fn send_message(&mut self, tc: TestCase) {
        tc.assume(!self.conversations.is_empty() && self.messages.len() < 20);
        let c = tc.draw(gs::integers::<usize>().max_value(self.conversations.len() - 1));
        let author = tc.draw(gs::integers::<usize>().max_value(self.users.len() - 1));
        let (image, markdown) = self.new_image();
        let id = self
            .block(store::create_post(
                &self.pool,
                &NewPost {
                    thread_id: self.conversations[c].id,
                    author_id: self.users[author].id,
                    body_md: format!("Message{markdown}"),
                    reply_to_post_id: None,
                    created_at: None,
                },
            ))
            .unwrap();
        self.messages.push(AccessMessage {
            id,
            image,
            conversation: c,
            deleted: false,
            hidden: false,
        });
    }

    #[rule]
    fn toggle_participant(&mut self, tc: TestCase) {
        tc.assume(!self.conversations.is_empty());
        let c = tc.draw(gs::integers::<usize>().max_value(self.conversations.len() - 1));
        let u = tc.draw(gs::integers::<usize>().max_value(self.users.len() - 1));
        let (thread, user) = (self.conversations[c].id, self.users[u].id);
        if self.conversations[c].participants.remove(&u) {
            assert!(
                self.block(store::leave_conversation(&self.db, thread, user))
                    .unwrap()
            );
        } else {
            assert!(
                self.block(store::add_participant(
                    &self.db,
                    thread,
                    user,
                    self.users[0].id
                ))
                .unwrap()
            );
            self.conversations[c].participants.insert(u);
        }
    }

    #[rule]
    fn toggle_message_deleted(&mut self, tc: TestCase) {
        tc.assume(!self.messages.is_empty());
        let m = tc.draw(gs::integers::<usize>().max_value(self.messages.len() - 1));
        let message = &mut self.messages[m];
        message.deleted = !message.deleted;
        let sql = format!(
            "UPDATE posts SET deleted_at = {} WHERE id = {}",
            if message.deleted { "now()" } else { "NULL" },
            message.id
        );
        self.exec(sql);
    }

    /// A moderator silences or suspends a member, or lifts it; the
    /// fixed admin stays as they are.
    #[rule]
    fn toggle_restriction(&mut self, tc: TestCase) {
        let u = tc.draw(
            gs::integers::<usize>()
                .min_value(FIXED_USERS)
                .max_value(self.users.len() - 1),
        );
        let suspend = tc.draw(gs::booleans());
        let user = &mut self.users[u];
        let (column, on) = if suspend {
            user.suspended = !user.suspended;
            ("suspended_until", user.suspended)
        } else {
            user.silenced = !user.silenced;
            ("silenced_until", user.silenced)
        };
        // Staff can't be restricted through the moderation pages, but the
        // access layer mustn't depend on that.
        let sql = format!(
            "UPDATE users SET {column} = {} WHERE id = {}",
            if on {
                "now() + interval '1 day'"
            } else {
                "now() - interval '1 second'"
            },
            user.id
        );
        self.exec(sql);
    }

    /// Someone reports a message, which opens its conversation to staff.
    #[rule]
    fn report_message(&mut self, tc: TestCase) {
        tc.assume(!self.messages.is_empty());
        let m = tc.draw(gs::integers::<usize>().max_value(self.messages.len() - 1));
        let (message, c) = (self.messages[m].id, self.messages[m].conversation);
        let reporter =
            self.users[tc.draw(gs::integers::<usize>().max_value(self.users.len() - 1))].id;
        let author: i64 = self
            .block(
                sqlx::query_scalar("SELECT author_id FROM posts WHERE id = $1")
                    .bind(message)
                    .fetch_one(&self.pool),
            )
            .unwrap();
        self.block(parlour::mod_records::insert_report(
            &self.pool,
            &parlour::mod_records::NewReport {
                reporter_id: Some(reporter),
                contact_email: None,
                about: parlour::mod_records::ReportAbout::Post {
                    post_id: message,
                    thread_id: self.conversations[c].id,
                    author_id: author,
                },
                reason: "harassment",
                details: "",
            },
        ))
        .unwrap();
        self.conversations[c].reported = true;
    }

    #[rule]
    fn toggle_message_hidden(&mut self, tc: TestCase) {
        tc.assume(!self.messages.is_empty());
        let m = tc.draw(gs::integers::<usize>().max_value(self.messages.len() - 1));
        let message = &mut self.messages[m];
        message.hidden = !message.hidden;
        let sql = format!(
            "UPDATE posts SET hidden_at = {} WHERE id = {}",
            if message.hidden { "now()" } else { "NULL" },
            message.id
        );
        self.exec(sql);
    }

    /// A moderator moves some of a thread's posts to the end of another.
    /// If none are left, the thread is merged into the other.
    #[rule]
    fn move_posts(&mut self, tc: TestCase) {
        tc.assume(self.threads.len() >= 2);
        let from = tc.draw(gs::integers::<usize>().max_value(self.threads.len() - 1));
        let to = tc.draw(gs::integers::<usize>().max_value(self.threads.len() - 1));
        tc.assume(from != to);
        let moving: Vec<usize> = (0..self.posts.len())
            .filter(|&p| self.posts[p].thread == from && tc.draw(gs::booleans()))
            .collect();
        tc.assume(!moving.is_empty());
        let ids: Vec<i64> = moving.iter().map(|&p| self.posts[p].id).collect();
        let moved = self
            .block(parlour::mod_records::move_posts(
                &self.pool,
                self.threads[from].id,
                &ids,
                self.threads[to].id,
                self.users[0].id,
            ))
            .unwrap();
        assert_eq!(moved.posts.len(), moving.len());
        for p in moving {
            self.posts[p].thread = to;
            self.moved_from[p].insert(from);
        }
        let left = self.posts.iter().any(|p| p.thread == from);
        assert_eq!(moved.emptied, !left);
        if moved.emptied {
            self.threads[from].deleted = true;
        }
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

    /// The maintained post counts stay exact however posts and threads
    /// move, and levels change.
    #[invariant(always_run)]
    fn post_counts_are_current(&self, _: TestCase) {
        let stale: i64 = self
            .block(sqlx::query_scalar(STALE_POST_COUNTS).fetch_one(&self.pool))
            .unwrap();
        assert_eq!(stale, 0, "maintained post counts are stale");
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

#[hegel::standalone_function(suppress_health_check = [HealthCheck::TooSlow])]
fn run_access(tc: TestCase, rt: &Handle, pool: &PgPool) {
    let mut world = Access::new(rt.clone(), pool.clone());
    // Start from random settings, so that every run has a mix of levels
    // and groups to read, post and quote across.
    for c in 0..world.categories.len() {
        world.set_category_access(&tc, c);
    }
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
