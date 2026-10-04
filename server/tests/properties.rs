//! Property-based tests over random forum histories (DESIGN.md §6, §12).
//!
//! A state machine creates spaces, threads, posts and quote-forks, then
//! edits (inside and after the grace period, so that some edits keep
//! history), reacts to, deletes, hides, moves and renames them, changes
//! space levels and read groups, moves the logged-in member in and out
//! of groups and on and off a list of another member's (DESIGN.md §3.3),
//! and reports each change as a write handler would. A list lets in its
//! owner as well as the people on it, and its name is a token that only
//! the people on it may see: anyone else is told only whose choice it is.
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
//!   group can strand them in a space they no longer read.
//! - Conversations (DMs) come and go among the members, the member viewer
//!   and the moderator viewer, who join, leave and are added back: a
//!   conversation's title and messages are seen by its current
//!   participants and nobody else (staff included), never through the
//!   anonymous cache, the space lists, the notifications page or any
//!   page about a post. Every new post is put through the notification
//!   fan-out, so the notifications page is checked too.
//! - Notification email is sent as members are away, and every email holds
//!   only text its recipient may read now; the notification badge, both
//!   counted directly and as the live badge stream sends it, counts exactly
//!   the unread notifications about posts the viewer may read.
//! - Who is notified of each new post is exactly what DESIGN.md §9 says,
//!   judged by a model of thread subscriptions: in a conversation those
//!   subscribed to it, then those mentioned in it, subscribed or not;
//!   elsewhere those mentioned in it, then those subscribed to its thread
//!   (authors to the threads they start, everyone in a conversation to it,
//!   anyone to what they subscribe to with the toggle on its page, which
//!   the viewers use), who may read it and didn't write it. Replying subscribes nobody. The toggle on each
//!   thread and conversation page shows the viewer's subscription.

mod common;

use std::collections::{BTreeMap, BTreeSet, HashMap};

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
    self, Deps, Embed, KindFilter, LatestScope, POSTS_PER_PAGE, PostBody, SpaceSort,
    THREADS_PER_PAGE, page_count, page_of_seq,
};
use parlour::db::Db;
use parlour::groups::{self, GroupFields};
use parlour::layouts::{self, MuteOutcome};
use parlour::lists;
use parlour::pagination::page_href;
use parlour::permissions::{Grant, Level, ThreadRestriction};
use parlour::sessions;
use parlour::store::{self, NewPost, NewSpace, NewThread, NewUser, SpaceSettings};
use parlour::users::{self, RoleChange};
use parlour::viewer::Viewer;
use parlour::writing::{self, Refused};
use sqlx::PgPool;
use tokio::runtime::Handle;

/// Counts the (member, space, restricted thread) rows whose maintained
/// post count, as thread pages and profiles show it, differs from counting
/// the posts afresh: live posts in live threads in that space, those in a
/// thread with its own read restriction counted for that thread.
const STALE_POST_COUNTS: &str = "
    WITH actual AS (
        SELECT p.author_id AS user_id, t.space_id,
               coalesce(CASE WHEN t.read_user_ids IS NOT NULL THEN t.id END, 0) AS thread_id,
               count(*) AS posts
        FROM posts p JOIN threads t ON t.id = p.thread_id
        WHERE t.space_id IS NOT NULL AND t.deleted_at IS NULL
          AND p.deleted_at IS NULL AND p.hidden_at IS NULL
        GROUP BY 1, 2, 3
    ), kept AS (
        SELECT user_id, space_id, coalesce(thread_id, 0) AS thread_id, posts
        FROM user_space_post_counts WHERE posts <> 0
    )
    SELECT count(*) FROM actual FULL JOIN kept USING (user_id, space_id, thread_id)
    WHERE actual.posts IS DISTINCT FROM kept.posts";

const LEVELS: [&str; 3] = ["public", "members", "listed"];

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

struct SpaceModel {
    id: i64,
    slug: String,
    read_level: &'static str,
    /// The groups that may read it, as indexes into `World::groups`.
    read_groups: BTreeSet<usize>,
    /// The token in the space's current name.
    token: String,
    /// Whether it is a member's feed (DESIGN.md §3.2): Ruth's, which is
    /// out of sight until it has a thread.
    feed: bool,
    /// For a feed, its owner, who holds all of its permissions: a grant
    /// that names people names them too.
    owner: Option<i64>,
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
    /// Whether it mentions both logged-in viewers.
    mentions: bool,
    deleted: bool,
}

struct ThreadModel {
    id: i64,
    space: usize,
    title_token: String,
    deleted: bool,
    last_seq: i64,
    /// Titles a moderator renamed it from, which the moderation log names
    /// to those who can read the thread.
    old_titles: Vec<String>,
    /// The thread it was merged into, by moving all its posts there.
    merged_into: Option<usize>,
    /// Its own read restriction (DESIGN.md §3.4), if it has one: the
    /// groups (indexes into `World::groups`) and the members (by id) it
    /// names.
    read: Option<(BTreeSet<usize>, BTreeSet<i64>)>,
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
    /// Whether it mentions both logged-in viewers.
    mentions: bool,
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
    spaces: Vec<SpaceModel>,
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
    /// `groups`). Some of the groups are lists (DESIGN.md §3.3): for each
    /// group, its owner if it is one, and the name of the list, a token
    /// that only its owner and the people on it may see.
    groups: Vec<i64>,
    list_owners: Vec<Option<i64>>,
    list_tokens: Vec<Option<String>>,
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
    /// The default layout and its one section, where new spaces go.
    default_layout: i64,
    default_section: i64,
    /// Who is subscribed to which thread or conversation, as (user id,
    /// thread id).
    subscriptions: BTreeSet<(i64, i64)>,
}

impl World {
    fn new(rt: Handle, db: PgPool, mode: Mode) -> World {
        rt.block_on(async {
            sqlx::raw_sql(
                "TRUNCATE settings, jobs, posts, threads, spaces, layouts, groups, users RESTART IDENTITY CASCADE",
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
            spaces: Vec::new(),
            threads: Vec::new(),
            posts: Vec::new(),
            all_tokens: Vec::new(),
            next_token: 0,
            mode,
            member_cookie: String::new(),
            staff_cookie: String::new(),
            groups: Vec::new(),
            list_owners: Vec::new(),
            list_tokens: Vec::new(),
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
            default_layout: 0,
            default_section: 0,
            subscriptions: BTreeSet::new(),
        };
        world.default_layout = world.block(layouts::default_id(world.db())).unwrap();
        world.default_section = world
            .block(layouts::add_section(
                world.db(),
                world.default_layout,
                "Spaces",
            ))
            .unwrap();
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
            world.list_owners.push(None);
            world.list_tokens.push(None);
        }
        // Two lists: Ruth's Friends, renamed to a token so that any page
        // showing its name to someone not on it is caught, and the member
        // viewer's own Friends.
        let ruth = world.users[0];
        let ruths = world
            .block(lists::friends_id(world.cached.state.db(), ruth))
            .unwrap()
            .expect("every member has a Friends list");
        let token = world.token();
        let name = format!("List {token}");
        assert_eq!(
            world
                .block(lists::rename(world.cached.state.db(), ruth, ruths, &name))
                .unwrap(),
            lists::Change::Done
        );
        world.groups.push(ruths);
        world.list_owners.push(Some(ruth));
        world.list_tokens.push(Some(token));
        let own = world
            .block(lists::friends_id(world.cached.state.db(), world.member_id))
            .unwrap()
            .expect("every member has a Friends list");
        world.groups.push(own);
        world.list_owners.push(Some(world.member_id));
        world.list_tokens.push(None);
        // A second list of Ruth's, so that a grant can name two lists of
        // one owner's, which nobody who isn't on them should be able to
        // count.
        let token = world.token();
        let second = world
            .block(lists::create(
                world.cached.state.db(),
                ruth,
                &format!("List {token}"),
            ))
            .unwrap()
            .unwrap();
        world.groups.push(second);
        world.list_owners.push(Some(ruth));
        world.list_tokens.push(Some(token));
        for level in ["public", "public", "members", "listed", "listed"] {
            world.add_space(level);
        }
        // From the start, one listed space names a group and both of
        // Ruth's lists, and the other the member viewer's own list, so that
        // what a list lets through and how it is named are checked from the
        // first step.
        world.name_read_groups(3, BTreeSet::from([0, 2, 4]));
        world.name_read_groups(4, BTreeSet::from([3]));
        // Ruth's feed (DESIGN.md §3.2), which every member has: open to
        // visitors, on the default front page, and named with a token, so
        // that any page showing it before its first thread is caught.
        world.add_feed(ruth);
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

    fn add_space(&mut self, level: &'static str) {
        let token = self.token();
        let slug = format!("c{}", self.spaces.len());
        let id = self
            .block(store::create_space(
                self.db(),
                &NewSpace::with_levels(&slug, &format!("Space {token}"), "", level, "members"),
            ))
            .unwrap();
        assert!(
            self.block(layouts::follow(
                self.db(),
                self.default_layout,
                self.default_section,
                id
            ))
            .unwrap()
        );
        self.changed(Change::SpaceChanged { space_id: id });
        self.spaces.push(SpaceModel {
            id,
            slug,
            read_level: level,
            read_groups: BTreeSet::new(),
            token,
            feed: false,
            owner: None,
        });
    }

    /// Starts a thread in space `space` by `author`, as the form does.
    fn start_thread(&mut self, tc: &TestCase, space: usize, author: i64) {
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
                    space_id: self.spaces[space].id,
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
            space,
            title_token,
            deleted: false,
            last_seq: 1,
            old_titles: Vec::new(),
            merged_into: None,
            read: None,
        });
        // Starting a thread subscribes its author to it.
        self.subscriptions.insert((author, created.thread_id));
        self.posts.push(PostModel {
            id: created.first_post_id,
            thread: self.threads.len() - 1,
            seq: 1,
            author,
            body_token,
            mentions: false,
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

    /// Opens member `owner`'s feed to visitors, names it with a token and
    /// puts it on the default front page, as its owner and an admin can.
    fn add_feed(&mut self, owner: i64) {
        let token = self.token();
        let (id, slug): (i64, String) = self
            .block(
                sqlx::query_as(
                    "SELECT id, slug FROM spaces WHERE kind = 'feed' AND owner_user_id = $1",
                )
                .bind(owner)
                .fetch_one(self.db()),
            )
            .unwrap();
        self.exec(format!(
            "UPDATE spaces SET name = 'Space {token}' WHERE id = {id}"
        ));
        self.block(store::set_grant(self.db(), id, "read", &Grant::public()))
            .unwrap();
        assert!(
            self.block(layouts::follow(
                self.db(),
                self.default_layout,
                self.default_section,
                id
            ))
            .unwrap()
        );
        self.changed(Change::SpaceChanged { space_id: id });
        self.spaces.push(SpaceModel {
            id,
            slug,
            read_level: "public",
            read_groups: BTreeSet::new(),
            token,
            feed: true,
            owner: Some(owner),
        });
    }

    /// Gives thread `t` this read restriction, or none, as the handler
    /// would.
    fn set_read_restriction(&mut self, t: usize, read: Option<(BTreeSet<usize>, BTreeSet<i64>)>) {
        let restriction = ThreadRestriction {
            read: read.as_ref().map(|(groups, users)| {
                Grant::listed(
                    groups.iter().map(|&g| self.groups[g]),
                    users.iter().copied(),
                )
            }),
            reply: None,
        };
        let id = self.threads[t].id;
        assert!(
            self.block(store::set_thread_restriction(self.db(), id, &restriction))
                .unwrap()
        );
        self.threads[t].read = read;
        self.name_feed_owner(t);
        self.changed(Change::ThreadChanged {
            thread_id: id,
            previous_space_id: None,
        });
    }

    /// The database names a feed's owner in any restriction of a thread
    /// in their feed (DESIGN.md §2, §3.2), however it came there.
    fn name_feed_owner(&mut self, t: usize) {
        if let Some(owner) = self.spaces[self.threads[t].space].owner
            && let Some((_, users)) = &mut self.threads[t].read
        {
            users.insert(owner);
        }
    }

    /// Whether space `c` is out of sight in `who`'s lists of spaces
    /// (DESIGN.md §3.2): a feed with no thread they can read.
    fn out_of_sight(&self, c: usize, who: Who) -> bool {
        self.spaces[c].feed
            && !(0..self.threads.len())
                .any(|t| self.threads[t].space == c && self.thread_readable(t, who))
    }

    /// Has space `c`'s read grant name these groups and lists (indexes
    /// into `groups`), keeping its level: they matter only to a listed
    /// space.
    fn name_read_groups(&mut self, c: usize, read_groups: BTreeSet<usize>) {
        let space = &self.spaces[c];
        let group_ids: Vec<i64> = read_groups.iter().map(|&g| self.groups[g]).collect();
        let settings = SpaceSettings {
            slug: space.slug.clone(),
            name: format!("Space {}", space.token),
            description: String::new(),
            read: Grant::from_parts(space.read_level, &group_ids, &[]),
            reply: Grant::members(),
            start: Grant::members(),
            // A feed's owner always runs it; the store names them.
            administer: if space.feed {
                Grant::listed([], [])
            } else {
                Grant::staff()
            },
        }
        .clean()
        .unwrap();
        self.block(store::update_space(
            self.cached.state.db(),
            space.id,
            &settings,
        ))
        .unwrap()
        .unwrap();
        let id = space.id;
        self.spaces[c].read_groups = if settings.read.level == Level::Listed {
            read_groups
        } else {
            BTreeSet::new()
        };
        self.changed(Change::SpaceChanged { space_id: id });
    }

    fn add_post(&mut self, thread: usize, author: i64, reply_to: Option<usize>, mentions: bool) {
        let token = self.token();
        let id = self
            .block(store::create_post(
                self.db(),
                &NewPost {
                    thread_id: self.threads[thread].id,
                    author_id: author,
                    body_md: if mentions {
                        format!("Post {token} for @viewer and @watcher")
                    } else {
                        format!("Post {token}")
                    },
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
            mentions,
            deleted: false,
            hidden: false,
            history: Vec::new(),
            settled: false,
        });
        self.changed(Change::PostCreated { post_id: id });
        self.notify(id);
    }

    /// Runs the notification fan-out for a new post, as its job would, and
    /// checks that exactly the right people were told, in the right way.
    fn notify(&self, post_id: i64) {
        self.block(parlour::notify::post_created(&self.cached.state, post_id))
            .unwrap();
        let made: Vec<(i64, String)> = self
            .block(
                sqlx::query_as(
                    "SELECT user_id, kind FROM notifications WHERE post_id = $1 ORDER BY user_id",
                )
                .bind(post_id)
                .fetch_all(self.db()),
            )
            .unwrap();
        let expected: Vec<(i64, String)> = self
            .expected_notified(post_id)
            .into_iter()
            .map(|(user, kind)| (user, kind.to_owned()))
            .collect();
        assert_eq!(made, expected, "notifications about post {post_id}");
    }

    /// DESIGN.md §9: who should be told about a new post, and how. In a
    /// conversation, someone in it and subscribed to it hears of it as a
    /// message. Otherwise someone mentioned hears of it as a mention,
    /// subscribed or not (staff included, in a reported conversation they
    /// can read), and someone subscribed to its thread hears of it as a
    /// new post there. Nobody hears of their own posts or of what they
    /// can't read. (The model has no blocks.)
    fn expected_notified(&self, post_id: i64) -> BTreeMap<i64, &'static str> {
        let mut told = BTreeMap::new();
        if let Some(p) = self.posts.iter().find(|p| p.id == post_id) {
            let t = &self.threads[p.thread];
            if t.deleted || p.deleted || p.hidden {
                return told;
            }
            for user in self.dm_people() {
                if user == p.author || !self.thread_readable_by(p.thread, user) {
                    continue;
                }
                if p.mentions && (user == self.member_id || user == self.staff_id) {
                    told.insert(user, "mention");
                } else if self.subscriptions.contains(&(user, t.id)) {
                    told.insert(user, "subscription");
                }
            }
        } else {
            let m = self
                .messages
                .iter()
                .find(|m| m.id == post_id)
                .expect("a post the test made");
            let c = &self.conversations[m.conversation];
            let author: i64 = self
                .block(
                    sqlx::query_scalar("SELECT author_id FROM posts WHERE id = $1")
                        .bind(post_id)
                        .fetch_one(self.db()),
                )
                .unwrap();
            // Anyone who can read it, which is the people in it and,
            // once one of its messages is reported, staff.
            for user in self.dm_people() {
                if user == author
                    || m.deleted
                    || !self.conversation_readable_by(m.conversation, user)
                {
                    continue;
                }
                if c.participants.contains(&user) && self.subscriptions.contains(&(user, c.id)) {
                    told.insert(user, "message");
                } else if m.mentions && (user == self.member_id || user == self.staff_id) {
                    told.insert(user, "mention");
                }
            }
        }
        told
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
            mentions: mention,
            deleted: false,
        });
        self.changed(Change::PostCreated { post_id: id });
        self.notify(id);
    }

    // -- The reference permission function, independent of the app's SQL.

    /// DESIGN.md §3.1: anyone reads public spaces, members also read
    /// members-only ones, members of a space's read groups read it, and
    /// staff read everything.
    fn space_readable(&self, space: usize, who: Who) -> bool {
        let c = &self.spaces[space];
        // A suspended member reads what visitors read.
        let who = if who == Who::Member && self.member_suspended {
            Who::Anonymous
        } else {
            who
        };
        match (c.read_level, who) {
            ("public", _) | ("members", Who::Member | Who::Staff) | (_, Who::Staff) => true,
            ("listed", Who::Member) => {
                self.lets_in(&c.read_groups, self.member_id) || c.owner == Some(self.member_id)
            }
            _ => false,
        }
    }

    /// Whether naming these groups lets member `user` in: they are on one
    /// of them (only the member viewer joins any), or it is a list of
    /// theirs (DESIGN.md §3.3).
    fn lets_in(&self, groups: &BTreeSet<usize>, user: i64) -> bool {
        groups.iter().any(|&g| {
            self.list_owners[g] == Some(user)
                || (user == self.member_id && self.member_groups.contains(&g))
        })
    }

    /// Whether this viewer knows that group `g` exists and what it is
    /// called: a site group, or a list they own or are on.
    fn knows_group(&self, who: Who, g: usize) -> bool {
        match self.list_owners[g] {
            None => true,
            Some(owner) => {
                self.viewer_id(who) == Some(owner)
                    || (who == Who::Member && self.member_groups.contains(&g))
            }
        }
    }

    /// DESIGN.md §3.4: a thread with its own read restriction is read by
    /// those who read its space and whom it names; staff read every
    /// thread for now.
    fn thread_readable(&self, thread: usize, who: Who) -> bool {
        !self.threads[thread].deleted && self.readable_if_live(thread, who)
    }

    /// Whether the viewer would read a thread were it live: what a merged
    /// thread's address needs to lead them on to the thread it was merged
    /// into.
    fn readable_if_live(&self, thread: usize, who: Who) -> bool {
        let t = &self.threads[thread];
        self.space_readable(t.space, who)
            && match (&t.read, who) {
                (None, _) | (_, Who::Staff) => true,
                (Some(_), Who::Anonymous) => false,
                (Some(names), Who::Member) => {
                    !self.member_suspended && self.named(names, self.member_id)
                }
            }
    }

    /// Whether a thread's restriction names member `user`.
    fn named(&self, (groups, users): &(BTreeSet<usize>, BTreeSet<i64>), user: i64) -> bool {
        users.contains(&user) || self.lets_in(groups, user)
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

    /// DESIGN.md §3.3: the author of a post in a thread they can no longer
    /// read still sees its history, and may edit it, while it is live and
    /// its thread isn't deleted, unless they are suspended (a suspended
    /// member reads only what visitors read).
    fn own_post_kept(&self, post: usize, who: Who) -> bool {
        let p = &self.posts[post];
        let suspended = who == Who::Member && self.member_suspended;
        self.viewer_id(who) == Some(p.author)
            && !suspended
            && !p.deleted
            && !p.hidden
            && !self.threads[p.thread].deleted
            && !self.thread_readable(p.thread, who)
    }

    /// The tokens a page may show beyond what the viewer may see anywhere:
    /// on the history and edit pages of their own posts in threads they
    /// can no longer read, those posts' text.
    fn own_post_tokens(&self, path: &str, who: Who) -> BTreeSet<&str> {
        let mut tokens = BTreeSet::new();
        for (i, p) in self.posts.iter().enumerate() {
            let history = format!("/p/{}/revisions", p.id);
            let edit = format!("/p/{}/edit", p.id);
            let on_it = path == history
                || path
                    .strip_prefix(history.as_str())
                    .is_some_and(|rest| rest.starts_with('?'))
                || path == edit;
            if on_it && self.own_post_kept(i, who) {
                tokens.insert(p.body_token.as_str());
                tokens.extend(p.history.iter().map(String::as_str));
            }
        }
        tokens
    }

    /// The viewer a member is, for the reference permission function:
    /// the logged-in viewers as themselves, and the other members (who
    /// are in no groups, though Ruth owns a list) as a member viewer in no
    /// groups would be.
    fn space_readable_by(&self, space: usize, user: i64) -> bool {
        if user == self.staff_id {
            return true;
        }
        let c = &self.spaces[space];
        if user == self.member_id && self.member_suspended {
            return c.read_level == "public";
        }
        match c.read_level {
            "public" | "members" => true,
            _ => self.lets_in(&c.read_groups, user) || c.owner == Some(user),
        }
    }

    /// Whether member `user` reads a thread now: its space, and, if it
    /// has its own read restriction, it names them (staff read every
    /// thread for now; a suspended member is named by nothing).
    fn thread_readable_by(&self, thread: usize, user: i64) -> bool {
        let t = &self.threads[thread];
        !t.deleted
            && self.space_readable_by(t.space, user)
            && t.read.as_ref().is_none_or(|names| {
                user == self.staff_id
                    || (!(user == self.member_id && self.member_suspended)
                        && self.named(names, user))
            })
    }

    /// Tokens the member `user` may read now.
    fn tokens_for_user(&self, user: i64) -> BTreeSet<&str> {
        let mut tokens = BTreeSet::new();
        for (i, t) in self.threads.iter().enumerate() {
            if self.thread_readable_by(i, user) {
                tokens.insert(t.title_token.as_str());
            }
        }
        for p in &self.posts {
            if !p.deleted && !p.hidden && self.thread_readable_by(p.thread, user) {
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
    /// moved, spaces closed, groups or conversations left.
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
        for (i, c) in self.spaces.iter().enumerate() {
            if self.space_readable(i, who) {
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
        // A list's name, to the people on it (its owner is never a viewer).
        for (g, token) in self.list_tokens.iter().enumerate() {
            if let Some(token) = token
                && who == Who::Member
                && self.member_groups.contains(&g)
            {
                tokens.insert(token.as_str());
            }
        }
        tokens
    }

    /// The subscription toggle on a thread or conversation page: for a
    /// logged-in viewer who `may` use it, it offers the opposite of what
    /// they have; otherwise it isn't there.
    fn check_toggle(&self, who: Who, thread_id: i64, may: bool, body: &str) {
        let subscribe = format!("action=\"/t/{thread_id}/subscribe\"");
        let unsubscribe = format!("action=\"/t/{thread_id}/unsubscribe\"");
        let subscribed = self
            .viewer_id(who)
            .is_some_and(|id| self.subscriptions.contains(&(id, thread_id)));
        let shown = who != Who::Anonymous && may;
        assert_eq!(
            body.contains(&subscribe),
            shown && !subscribed,
            "{who:?}'s subscribe toggle for {thread_id}"
        );
        assert_eq!(
            body.contains(&unsubscribe),
            shown && subscribed,
            "{who:?}'s unsubscribe toggle for {thread_id}"
        );
    }

    fn cookie(&self, who: Who) -> Option<&str> {
        match who {
            Who::Anonymous => None,
            Who::Member => Some(&self.member_cookie),
            Who::Staff => Some(&self.staff_cookie),
        }
    }

    fn paths(&self, who: Who) -> Vec<String> {
        let mut paths = vec![
            "/".to_owned(),
            "/users/tree".to_owned(),
            "/spaces".to_owned(),
            "/spaces?kind=feed".to_owned(),
        ];
        for name in ["ruth", "jonah", "maya", "viewer"] {
            paths.push(format!("/u/{name}"));
        }
        let visible_threads = (0..self.threads.len())
            .filter(|&t| self.thread_readable(t, who))
            .count() as i64;
        for page in 1..=page_count(visible_threads, THREADS_PER_PAGE) + 1 {
            paths.push(page_href("/latest", page));
        }
        // The front page's tabs: the viewer's sections (the default
        // layout's for a visitor), and for a member the front page itself.
        // Each has at most as many pages as All; the page past the end is
        // visited too.
        let layout = match who {
            Who::Anonymous => self.default_layout,
            Who::Member => self.layout_of(self.member_id),
            Who::Staff => self.layout_of(self.staff_id),
        };
        let sections = self
            .block(layouts::read(self.db(), layout))
            .unwrap()
            .expect("the layout exists")
            .sections;
        for page in 1..=page_count(visible_threads, THREADS_PER_PAGE) + 1 {
            if who != Who::Anonymous {
                paths.push(page_href("/latest/front", page));
            }
            for section in &sections {
                paths.push(page_href(&format!("/latest/s/{}", section.id), page));
            }
        }
        paths.push("/latest/front".to_owned());
        for (i, c) in self.spaces.iter().enumerate() {
            let threads = self
                .threads
                .iter()
                .filter(|t| t.space == i && !t.deleted)
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
            for c in &self.spaces {
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
                // The layout editors.
                "/settings/layout",
                "/admin/layout",
                // Lists, and the pages that name them to those who may
                // see them.
                "/settings/lists",
            ] {
                paths.push(path.to_owned());
            }
            for g in &self.groups {
                paths.push(format!("/settings/lists/{g}"));
            }
            for c in &self.spaces {
                paths.push(format!("/c/{}/settings", c.slug));
            }
        }
        // Who can take part in each space: only for members, by the names
        // they may know.
        for c in &self.spaces {
            paths.push(format!("/c/{}/audience", c.slug));
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

    /// A member's own layout.
    fn layout_of(&self, user_id: i64) -> i64 {
        self.block(layouts::member_id(self.db(), user_id))
            .unwrap()
            .expect("every member has a layout")
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

    /// A post's history page shows its text and every version of it.
    fn check_history(&self, pages: &Pages, post: usize) {
        let p = &self.posts[post];
        let history = pages.get(&format!("/p/{}/revisions", p.id));
        assert_eq!(history.status, StatusCode::OK, "post {}", p.id);
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
            let kept = self.own_post_tokens(path, who);
            for token in forbidden.iter().filter(|t| !kept.contains(t.as_str())) {
                assert!(
                    !response.body.contains(token.as_str()),
                    "{path} shows {token} to {who:?}:\n{}",
                    response.body
                );
            }
        }
        // A feed with no threads is in no list of spaces (DESIGN.md §3.2);
        // its owner, Ruth, is never the viewer.
        for (c, space) in self.spaces.iter().enumerate() {
            for path in ["/", "/spaces", "/spaces?kind=feed"] {
                let response = pages.get(path);
                let listed = response
                    .body
                    .contains(&format!("href=\"/c/{}\"", space.slug));
                if self.out_of_sight(c, who) {
                    assert!(
                        !response.body.contains(&space.token) && !listed,
                        "{path} shows the empty feed {c} to {who:?}:\n{}",
                        response.body
                    );
                } else if path != "/" && self.space_readable(c, who) {
                    assert_eq!(
                        listed,
                        space.feed || path == "/spaces",
                        "{who:?} {path} {c}"
                    );
                }
            }
        }
        // Visible things can be found where they should be, and invisible
        // things are not found at all.
        for (i, t) in self.threads.iter().enumerate() {
            let response = pages.get(&format!("/t/{}/{}", t.id, self.slug_of(t)));
            if self.thread_readable(i, who) {
                assert_eq!(response.status, StatusCode::OK);
                assert!(response.body.contains(&t.title_token));
                self.check_toggle(who, t.id, true, &response.body);
            } else if let Some(into) = t.merged_into.filter(|&into| {
                t.deleted && self.readable_if_live(i, who) && self.thread_readable(into, who)
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
            if self.own_post_kept(i, who) {
                // Their own post, in a thread they can no longer read: its
                // address leads to its history, which is all there is.
                assert_eq!(permalink.status, StatusCode::TEMPORARY_REDIRECT);
                assert_eq!(permalink.location(), format!("/p/{}/revisions", p.id));
                self.check_history(&pages, i);
                continue;
            }
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
                self.check_history(&pages, i);
            } else {
                // Deleted and hidden posts take their history with them.
                assert_eq!(history.status, StatusCode::NOT_FOUND, "post {}", p.id);
            }
        }
        for (i, c) in self.spaces.iter().enumerate() {
            let response = pages.get(&format!("/c/{}", c.slug));
            let expected = if self.space_readable(i, who) {
                StatusCode::OK
            } else {
                StatusCode::NOT_FOUND
            };
            assert_eq!(response.status, expected);
            // Its audience page names a list to the people on it, and
            // visitors are asked to log in.
            let audience = pages.get(&format!("/c/{}/audience", c.slug));
            let expected = match who {
                Who::Anonymous => StatusCode::SEE_OTHER,
                _ => expected,
            };
            assert_eq!(audience.status, expected, "{who:?} audience of space {i}");
            if expected == StatusCode::OK {
                for &g in &c.read_groups {
                    if let Some(token) = &self.list_tokens[g] {
                        assert_eq!(
                            audience.body.contains(token.as_str()),
                            who == Who::Member && self.member_groups.contains(&g),
                            "{who:?} audience of space {i}, list {g}"
                        );
                    }
                }
            }
        }
        // A space's settings form, for those who run it, offers the lists
        // its read grant names that they don't know as one choice per
        // owner, which gives neither their ids nor how many there are.
        for c in &self.spaces {
            let settings = pages.get(&format!("/c/{}/settings", c.slug));
            if settings.status != StatusCode::OK {
                continue;
            }
            let mut strangers = BTreeSet::new();
            for &g in &c.read_groups {
                if let Some(owner) = self.list_owners[g]
                    && !self.knows_group(who, g)
                {
                    strangers.insert(owner);
                }
            }
            for (g, owner) in self.list_owners.iter().enumerate() {
                let Some(owner) = owner else {
                    continue;
                };
                for verb in ["read", "reply", "start", "administer"] {
                    let value = format!("name=\"{verb}_group\" value=\"{}\"", self.groups[g]);
                    if !self.knows_group(who, g) {
                        assert!(
                            !settings.body.contains(&value),
                            "{who:?} settings of {} give list {g}'s id",
                            c.slug
                        );
                    }
                    let chosen = format!("name=\"{verb}_group\" value=\"owner:{owner}\"");
                    let expected = usize::from(verb == "read" && strangers.contains(owner));
                    assert_eq!(
                        settings.body.matches(&chosen).count(),
                        expected,
                        "{who:?} settings of {}: {verb}, lists of {owner}",
                        c.slug
                    );
                }
            }
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
                // Staff reading a reported conversation they aren't in
                // have nothing to subscribe to.
                let participant = self
                    .viewer_id(who)
                    .is_some_and(|id| c.participants.contains(&id));
                self.check_toggle(who, c.id, participant, &response.body);
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
        let space = tc.draw(gs::integers::<usize>().max_value(self.spaces.len() - 1));
        let author = tc.draw(gs::sampled_from(self.users.clone()));
        self.start_thread(&tc, space, author);
    }

    /// Ruth's feed gets its first thread visitors can read, or loses the
    /// ones it has (DESIGN.md §3.2): either moves it into or out of
    /// visitors' lists of spaces. A thread comes into their sight by being
    /// started or by losing its own read restriction (§3.4), and goes by
    /// being deleted or restricted to Ruth, so that a feed with threads
    /// visitors can't read is covered too. Cached pages are checked at
    /// once, before anything else clears them.
    #[rule]
    fn feed_comes_or_goes(&mut self, tc: TestCase) {
        let feed = self.spaces.iter().position(|c| c.feed).unwrap();
        let ruth = self.users[0];
        if self.out_of_sight(feed, Who::Anonymous) {
            let restricted: Vec<usize> = (0..self.threads.len())
                .filter(|&t| {
                    let thread = &self.threads[t];
                    thread.space == feed && !thread.deleted && thread.read.is_some()
                })
                .collect();
            if !restricted.is_empty() && tc.draw(gs::booleans()) {
                let t = tc.draw(gs::sampled_from(restricted));
                self.set_read_restriction(t, None);
            } else {
                self.start_thread(&tc, feed, ruth);
            }
        } else {
            for t in 0..self.threads.len() {
                if self.threads[t].space == feed && self.thread_readable(t, Who::Anonymous) {
                    if tc.draw(gs::booleans()) {
                        self.set_read_restriction(
                            t,
                            Some((BTreeSet::new(), BTreeSet::from([ruth]))),
                        );
                    } else {
                        self.threads[t].deleted = true;
                        let id = self.threads[t].id;
                        self.exec(format!(
                            "UPDATE threads SET deleted_at = now() WHERE id = {id}"
                        ));
                        self.changed(Change::ThreadChanged {
                            thread_id: id,
                            previous_space_id: None,
                        });
                    }
                }
            }
        }
        if self.mode == Mode::Coherence {
            self.check_coherence();
        }
    }

    /// Gives a thread its own read restriction, naming random groups,
    /// lists and members, or takes it away (DESIGN.md §3.4). A thread in
    /// Ruth's feed may take the feed into or out of visitors' sight, so the
    /// cached pages are then checked at once.
    #[rule]
    fn restrict_thread(&mut self, tc: TestCase) {
        tc.assume(!self.threads.is_empty());
        let t = tc.draw(gs::integers::<usize>().max_value(self.threads.len() - 1));
        let read = tc.draw(gs::booleans()).then(|| {
            let groups: BTreeSet<usize> = (0..self.groups.len())
                .filter(|_| tc.draw(gs::booleans()))
                .collect();
            let users: BTreeSet<i64> = self
                .dm_people()
                .into_iter()
                .filter(|_| tc.draw(gs::booleans()))
                .collect();
            (groups, users)
        });
        self.set_read_restriction(t, read);
        if self.mode == Mode::Coherence && self.spaces[self.threads[t].space].feed {
            self.check_coherence();
        }
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
        let mentions = tc.draw(gs::booleans());
        // Replying subscribes nobody: the model's subscriptions stay as
        // they are.
        self.add_post(thread, author, reply_to, mentions);
    }

    /// Enough posts to spill onto further pages.
    #[rule]
    fn many_replies(&mut self, tc: TestCase) {
        tc.assume(!self.threads.is_empty());
        let thread = tc.draw(gs::integers::<usize>().max_value(self.threads.len() - 1));
        let count = tc.draw(gs::integers::<usize>().min_value(1).max_value(60));
        for i in 0..count {
            self.add_post(thread, self.users[i % self.users.len()], None, false);
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
        let space = tc.draw(gs::integers::<usize>().max_value(self.spaces.len() - 1));
        let previous = self.threads[thread].space;
        self.threads[thread].space = space;
        self.name_feed_owner(thread);
        self.exec(format!(
            "UPDATE threads SET space_id = {} WHERE id = {}",
            self.spaces[space].id, self.threads[thread].id
        ));
        self.changed(Change::ThreadChanged {
            thread_id: self.threads[thread].id,
            previous_space_id: Some(self.spaces[previous].id),
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
            previous_space_id: None,
        });
    }

    #[rule]
    fn change_space(&mut self, tc: TestCase) {
        let space = tc.draw(gs::integers::<usize>().max_value(self.spaces.len() - 1));
        let id = self.spaces[space].id;
        if tc.draw(gs::booleans()) {
            let level = tc.draw(gs::sampled_from(LEVELS.to_vec()));
            self.spaces[space].read_level = level;
            // As the settings form does: a new level keeps the groups
            // named only while it is `listed`.
            let groups: Vec<i64> = if level == "listed" {
                self.spaces[space]
                    .read_groups
                    .iter()
                    .map(|&g| self.groups[g])
                    .collect()
            } else {
                self.spaces[space].read_groups.clear();
                Vec::new()
            };
            // A feed's owner is named wherever people are (DESIGN.md
            // §3.2), as the store does.
            let users: Vec<i64> = self.spaces[space].owner.into_iter().collect();
            self.block(store::set_grant(
                self.db(),
                id,
                "read",
                &Grant::from_parts(level, &groups, &users),
            ))
            .unwrap();
        } else {
            let token = self.token();
            self.exec(format!(
                "UPDATE spaces SET name = 'Space {token}' WHERE id = {id}"
            ));
            self.spaces[space].token = token;
        }
        self.changed(Change::SpaceChanged { space_id: id });
    }

    /// The member viewer joins or leaves a group, or a list of Ruth's (as
    /// she would put them on it, or take them off).
    #[rule]
    fn toggle_membership(&mut self, tc: TestCase) {
        let g = tc.draw(gs::integers::<usize>().max_value(self.groups.len() - 1));
        // Nobody is on their own list.
        tc.assume(self.list_owners[g] != Some(self.member_id));
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

    /// An admin changes which groups and lists may read a space, as the
    /// space form does.
    #[rule]
    fn set_read_groups(&mut self, tc: TestCase) {
        let c = tc.draw(gs::integers::<usize>().max_value(self.spaces.len() - 1));
        let read_groups: BTreeSet<usize> = (0..self.groups.len())
            .filter(|_| tc.draw(gs::booleans()))
            .collect();
        self.name_read_groups(c, read_groups);
    }

    #[rule]
    fn add_space_rule(&mut self, tc: TestCase) {
        tc.assume(self.spaces.len() < 7);
        let level = tc.draw(gs::sampled_from(LEVELS.to_vec()));
        self.add_space(level);
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

    /// An admin marks a space essential, or not; the front page shows
    /// essential spaces first, so this changes cached pages.
    #[rule]
    fn toggle_essential(&mut self, tc: TestCase) {
        let c = tc.draw(gs::integers::<usize>().max_value(self.spaces.len() - 1));
        let id = self.spaces[c].id;
        let essential: bool = self
            .block(
                sqlx::query_scalar("SELECT essential FROM spaces WHERE id = $1")
                    .bind(id)
                    .fetch_one(self.db()),
            )
            .unwrap();
        assert!(
            self.block(layouts::set_essential(self.db(), id, !essential))
                .unwrap()
        );
        self.changed(Change::SpaceChanged { space_id: id });
    }

    /// An admin takes a space off the default front page, or puts it back.
    #[rule]
    fn toggle_on_front_page(&mut self, tc: TestCase) {
        let c = tc.draw(gs::integers::<usize>().max_value(self.spaces.len() - 1));
        let id = self.spaces[c].id;
        let placed = self
            .block(layouts::section_of(self.db(), self.default_layout, id))
            .unwrap();
        let done = match placed {
            Some(_) => self.block(layouts::unfollow(self.db(), self.default_layout, id)),
            None => self.block(layouts::follow(
                self.db(),
                self.default_layout,
                self.default_section,
                id,
            )),
        };
        assert!(done.unwrap());
        self.changed(Change::SiteChanged);
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
        // Everyone in a conversation is subscribed to it.
        for &user in &participants {
            self.subscriptions.insert((user, created.thread_id));
        }
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
            mentions: mention,
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
        // Leaving a conversation unsubscribes you from it; being added
        // (back) subscribes you.
        if self.conversations[c].participants.remove(&user) {
            assert!(
                self.block(store::leave_conversation(self.db(), id, user))
                    .unwrap()
            );
            self.subscriptions.remove(&(user, id));
        } else {
            let adder = *self.dm_people().first().unwrap();
            assert!(
                self.block(store::add_participant(self.db(), id, user, adder))
                    .unwrap()
            );
            self.conversations[c].participants.insert(user);
            self.subscriptions.insert((user, id));
        }
    }

    /// Someone subscribes to a thread or conversation, or unsubscribes.
    /// The logged-in viewers use the toggle on its page, which is there for
    /// threads they can read and conversations they are in; everyone else
    /// is subscribed directly, wherever it is (being subscribed to
    /// something you can't read tells you nothing about it).
    #[rule]
    fn toggle_subscription(&mut self, tc: TestCase) {
        let in_conversation =
            !self.conversations.is_empty() && (self.threads.is_empty() || tc.draw(gs::booleans()));
        tc.assume(in_conversation || !self.threads.is_empty());
        let user = tc.draw(gs::sampled_from(self.dm_people()));
        let (thread_id, page_has_toggle) = if in_conversation {
            let c = tc.draw(gs::integers::<usize>().max_value(self.conversations.len() - 1));
            let conversation = &self.conversations[c];
            (
                conversation.id,
                conversation.participants.contains(&user) && self.conversation_readable_by(c, user),
            )
        } else {
            let t = tc.draw(gs::integers::<usize>().max_value(self.threads.len() - 1));
            let thread = &self.threads[t];
            (thread.id, self.thread_readable_by(t, user))
        };
        let on = !self.subscriptions.contains(&(user, thread_id));
        let who = if user == self.member_id {
            Some(Who::Member)
        } else if user == self.staff_id {
            Some(Who::Staff)
        } else {
            None
        };
        match who {
            Some(who) => {
                let action = if on { "subscribe" } else { "unsubscribe" };
                let response = self.block(self.cached.submit(
                    &format!("/t/{thread_id}/{action}"),
                    &[],
                    self.cookie(who),
                ));
                if !page_has_toggle {
                    assert_eq!(response.status, StatusCode::NOT_FOUND, "{who:?} {action}");
                    return;
                }
                assert_eq!(response.status, StatusCode::SEE_OTHER, "{who:?} {action}");
            }
            None => {
                self.block(parlour::social::set_subscribed(
                    self.db(),
                    user,
                    thread_id,
                    on,
                ))
                .unwrap();
            }
        }
        if on {
            self.subscriptions.insert((user, thread_id));
        } else {
            self.subscriptions.remove(&(user, thread_id));
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
        let (destination, title_token, space) = if new_thread {
            let space = tc.draw(gs::integers::<usize>().max_value(self.spaces.len() - 1));
            let token = self.token();
            (
                Destination::New {
                    space: self.spaces[space].slug.clone(),
                    title: format!("Thread {token}"),
                },
                Some(token),
                space,
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
                    space,
                    title_token,
                    deleted: false,
                    last_seq: 0,
                    old_titles: Vec::new(),
                    merged_into: None,
                    read: None,
                });
                self.threads.len() - 1
            }
            None => space,
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
        let space = tc.draw(gs::integers::<usize>().max_value(self.spaces.len() - 1));
        let (title_token, body_token) = (self.token(), self.token());
        let id = self
            .block(parlour::mod_records::hold(
                self.db(),
                author,
                &parlour::mod_records::HeldPlace::Thread {
                    space_id: self.spaces[space].id,
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
                let (thread_id, space_id): (i64, i64) = self
                    .block(
                        sqlx::query_as(
                            "SELECT t.id, t.space_id FROM posts p JOIN threads t ON t.id = p.thread_id
                             WHERE p.id = $1",
                        )
                        .bind(post_id)
                        .fetch_one(self.db()),
                    )
                    .unwrap();
                let held = &self.held[h];
                let space = self.spaces.iter().position(|c| c.id == space_id).unwrap();
                let (title_token, body_token, author) = (
                    held.title_token.clone(),
                    held.body_token.clone(),
                    held.author,
                );
                self.threads.push(ThreadModel {
                    id: thread_id,
                    space,
                    title_token,
                    deleted: false,
                    last_seq: 1,
                    old_titles: Vec::new(),
                    merged_into: None,
                    read: None,
                });
                // An approved thread is its author's, who is subscribed.
                self.subscriptions.insert((author, thread_id));
                self.posts.push(PostModel {
                    id: post_id,
                    thread: self.threads.len() - 1,
                    seq: 1,
                    author,
                    body_token,
                    mentions: false,
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

/// Staff see every space, and their browsing never reaches anonymous
/// visitors either.
#[sqlx::test]
async fn staff_see_every_space(pool: PgPool) {
    run_in_blocking_thread(pool, Mode::Visibility(Who::Staff)).await;
}

// ---------------------------------------------------------------------------
// The access layer itself (DESIGN.md §6)
//
// A second state machine builds random forums out of groups, members'
// lists (which let in their owners too), memberships, roles, spaces with
// read and write levels and groups, threads, posts,
// quote-forks, moves and deletions, and asks the `content` module directly
// what each viewer gets, where they may post, and where they may quote
// each post they can read. Every answer is checked, in both directions,
// against `Access::reads_space` and friends below, which are written from
// DESIGN.md §3.1 and know nothing of the SQL.

struct UserModel {
    id: i64,
    staff: bool,
    groups: BTreeSet<usize>,
    /// Silenced: reads as before, but may not post.
    silenced: bool,
    /// Suspended: reads only what visitors read, and isn't staff.
    suspended: bool,
    /// Their layout, and the spaces in its sections.
    layout: i64,
    sections: BTreeSet<usize>,
    /// Whether they have edited the layout; until then it follows the
    /// default.
    edited: bool,
    /// What their Feeds section holds while it follows the default, which
    /// copies of the default keep: their own feed, until they mute it
    /// (DESIGN.md §3.5).
    kept: BTreeSet<usize>,
    muted: BTreeSet<usize>,
}

impl UserModel {
    /// Staff, as far as reading goes: a suspended moderator is a visitor.
    fn acts_as_staff(&self) -> bool {
        self.staff && !self.suspended
    }
}

struct AccessSpace {
    id: i64,
    slug: String,
    level: &'static str,
    read_groups: BTreeSet<usize>,
    /// The people the read grant names, with a `listed` level: a feed's
    /// owner, whom the store names there (DESIGN.md §3.2).
    read_users: BTreeSet<usize>,
    write_level: &'static str,
    write_groups: BTreeSet<usize>,
    /// The people the write grant names, with a `listed` level.
    write_users: BTreeSet<usize>,
    /// The reply grant, which is the write grant but for a feed as it is
    /// made, where members reply and only its owner starts threads.
    reply_level: &'static str,
    reply_groups: BTreeSet<usize>,
    reply_users: BTreeSet<usize>,
    essential: bool,
    /// Whether it is in the default layout's one section.
    on_default: bool,
    /// For a feed (DESIGN.md §3.2), the member whose it is.
    owner: Option<usize>,
}

struct AccessThread {
    id: i64,
    space: usize,
    author: usize,
    deleted: bool,
    /// The post this thread quote-forks.
    fork_of: Option<usize>,
    /// Its own read restriction (DESIGN.md §3.4), if it has one: the
    /// groups and the people it names.
    read: Option<Names>,
    /// Its own reply restriction, if it has one.
    reply: Option<Names>,
}

/// The groups (indexes into `Access::groups`) and members (indexes into
/// `Access::users`) a thread's restriction names.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Names {
    groups: BTreeSet<usize>,
    users: BTreeSet<usize>,
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
    /// For each group, the user who owns it if it is a list (DESIGN.md
    /// §3.3), who is let in wherever it is named.
    group_owners: Vec<Option<usize>>,
    spaces: Vec<AccessSpace>,
    threads: Vec<AccessThread>,
    posts: Vec<AccessPost>,
    /// For each post, the threads it has been moved out of.
    moved_from: Vec<BTreeSet<usize>>,
    conversations: Vec<AccessConversation>,
    messages: Vec<AccessMessage>,
    images: u64,
    /// The images in posts held for approval.
    held_images: Vec<String>,
    /// The default layout and its one section.
    default_layout: i64,
    default_section: i64,
}

/// The members who never change role; there is always an admin among them.
const FIXED_USERS: usize = 1;

impl Access {
    fn new(rt: Handle, pool: PgPool) -> Access {
        rt.block_on(async {
            sqlx::raw_sql(
                "TRUNCATE settings, jobs, posts, threads, spaces, layouts, groups, users RESTART IDENTITY CASCADE",
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
            group_owners: Vec::new(),
            spaces: Vec::new(),
            threads: Vec::new(),
            posts: Vec::new(),
            moved_from: Vec::new(),
            conversations: Vec::new(),
            messages: Vec::new(),
            images: 0,
            held_images: Vec::new(),
            default_layout: 0,
            default_section: 0,
        };
        world.default_layout = world.block(layouts::default_id(&world.db)).unwrap();
        world.default_section = world
            .block(layouts::add_section(
                &world.db,
                world.default_layout,
                "Spaces",
            ))
            .unwrap();
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
            let layout = world
                .block(layouts::member_id(&world.db, id))
                .unwrap()
                .expect("every new member has a layout");
            world.users.push(UserModel {
                id,
                staff: role != "user",
                groups: BTreeSet::new(),
                silenced: false,
                suspended: false,
                layout,
                sections: BTreeSet::new(),
                edited: false,
                kept: BTreeSet::new(),
                muted: BTreeSet::new(),
            });
        }
        // Every member has a feed from the moment their account exists:
        // members read and reply, they alone start threads and run it, and
        // it waits in their own Feeds section (DESIGN.md §3.2).
        for u in 0..world.users.len() {
            let (id, slug): (i64, String) = world
                .block(
                    sqlx::query_as(
                        "SELECT id, slug FROM spaces WHERE kind = 'feed' AND owner_user_id = $1",
                    )
                    .bind(world.users[u].id)
                    .fetch_one(&world.pool),
                )
                .unwrap();
            let c = world.spaces.len();
            world.spaces.push(AccessSpace {
                id,
                slug,
                level: "members",
                read_groups: BTreeSet::new(),
                read_users: BTreeSet::new(),
                write_level: "listed",
                write_groups: BTreeSet::new(),
                write_users: BTreeSet::from([u]),
                reply_level: "members",
                reply_groups: BTreeSet::new(),
                reply_users: BTreeSet::new(),
                essential: false,
                on_default: false,
                owner: Some(u),
            });
            world.users[u].kept.insert(c);
            world.users[u].sections.insert(c);
        }
        for _ in 0..2 {
            world.add_group();
        }
        // The Friends lists of a member and of the moderator, which every
        // account has.
        for u in [1, 4] {
            let id = world
                .block(lists::friends_id(&world.db, world.users[u].id))
                .unwrap()
                .expect("every member has a Friends list");
            world.groups.push(id);
            world.group_owners.push(Some(u));
        }
        for level in ["public", "members", "listed", "listed"] {
            world.add_space(level);
        }
        world
    }

    /// Whether naming these groups lets user `u` in: they are on one, or
    /// it is a list of theirs.
    fn lets_in(&self, groups: &BTreeSet<usize>, u: usize) -> bool {
        !self.users[u].groups.is_disjoint(groups)
            || groups.iter().any(|&g| self.group_owners[g] == Some(u))
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
        self.group_owners.push(None);
    }

    /// A member makes a list.
    fn add_list(&mut self, owner: usize) {
        let n = self.groups.len();
        let id = self
            .block(lists::create(
                &self.db,
                self.users[owner].id,
                &format!("List {n}"),
            ))
            .unwrap()
            .unwrap();
        self.groups.push(id);
        self.group_owners.push(Some(owner));
    }

    fn add_space(&mut self, level: &'static str) {
        let n = self.spaces.len();
        let slug = format!("c{n}");
        let id = self
            .block(store::create_space(
                &self.pool,
                &NewSpace::with_levels(&slug, &format!("Space {n}"), "", level, "members"),
            ))
            .unwrap();
        assert!(
            self.block(layouts::follow(
                &self.db,
                self.default_layout,
                self.default_section,
                id
            ))
            .unwrap()
        );
        self.spaces.push(AccessSpace {
            id,
            slug,
            level,
            read_groups: BTreeSet::new(),
            read_users: BTreeSet::new(),
            write_level: "members",
            write_groups: BTreeSet::new(),
            write_users: BTreeSet::new(),
            reply_level: "members",
            reply_groups: BTreeSet::new(),
            reply_users: BTreeSet::new(),
            essential: false,
            on_default: true,
            owner: None,
        });
        self.default_changed();
    }

    /// The spaces on the default front page.
    fn default_set(&self) -> BTreeSet<usize> {
        (0..self.spaces.len())
            .filter(|&c| self.spaces[c].on_default)
            .collect()
    }

    /// What a change to the default layout does: every member who hasn't
    /// edited their copy gets the default again, less what they have
    /// muted, keeping what their Feeds section holds.
    fn default_changed(&mut self) {
        let default = self.default_set();
        for user in &mut self.users {
            if !user.edited {
                user.sections = default
                    .difference(&user.muted)
                    .chain(&user.kept)
                    .copied()
                    .collect();
            }
        }
    }

    /// Whether lists of spaces show space `c` to `who` (DESIGN.md §3.2):
    /// a topic space always, a feed once it has a thread they can read,
    /// and a member's own feed always.
    fn in_view(&self, who: Option<usize>, c: usize) -> bool {
        self.listed_for(who, c) || (who.is_some() && self.spaces[c].owner == who)
    }

    /// Whether lists of spaces that make no exception for a feed's owner,
    /// such as the space index, show space `c` to `who`: a topic space, or
    /// a feed with a thread they can read.
    fn listed_for(&self, who: Option<usize>, c: usize) -> bool {
        self.spaces[c].owner.is_none()
            || (0..self.threads.len())
                .any(|t| self.threads[t].space == c && self.reads_thread(who, t))
    }

    /// Whether a thread's restriction names member `u`: they are named
    /// themselves, or are on a group or list it names, or own such a list.
    fn names(&self, names: &Names, u: usize) -> bool {
        names.users.contains(&u) || self.lets_in(&names.groups, u)
    }

    /// The spaces the viewer's front page sections hold: their own
    /// layout's, or the default's for a visitor.
    fn following(&self, who: Option<usize>) -> BTreeSet<usize> {
        match who {
            None => self.default_set(),
            Some(u) => self.users[u].sections.clone(),
        }
    }

    fn muted(&self, who: Option<usize>, space: usize) -> bool {
        who.is_some_and(|u| self.users[u].muted.contains(&space))
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

    /// Random names for a thread's restriction, or none.
    fn draw_names(&self, tc: &TestCase) -> Option<Names> {
        if !tc.draw(gs::booleans()) {
            return None;
        }
        Some(Names {
            groups: (0..self.groups.len())
                .filter(|_| tc.draw(gs::booleans()))
                .collect(),
            users: (0..self.users.len())
                .filter(|_| tc.draw(gs::booleans()))
                .collect(),
        })
    }

    /// A thread's restriction as the store takes it.
    fn restriction(&self, read: &Option<Names>, reply: &Option<Names>) -> ThreadRestriction {
        let grant = |names: &Option<Names>| {
            names.as_ref().map(|names| {
                Grant::listed(
                    names.groups.iter().map(|&g| self.groups[g]),
                    names.users.iter().map(|&u| self.users[u].id),
                )
            })
        };
        ThreadRestriction {
            read: grant(read),
            reply: grant(reply),
        }
    }

    // -- The reference permission function, from DESIGN.md §3.1.

    /// `who` is an index into `users`, or `None` for an anonymous visitor.
    /// A suspended member reads what a visitor reads (DESIGN.md §3.1:
    /// members-only means members who are not suspended).
    /// Staff read every space for now (DESIGN.md §13, milestone 15).
    fn reads_space(&self, who: Option<usize>, space: usize) -> bool {
        let c = &self.spaces[space];
        let who = who.filter(|&u| !self.users[u].suspended);
        match (c.level, who) {
            ("public", _) => true,
            (_, None) => false,
            ("members", Some(_)) => true,
            ("staff", Some(u)) => self.users[u].staff,
            (_, Some(u)) => {
                self.users[u].staff || self.lets_in(&c.read_groups, u) || c.read_users.contains(&u)
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

    /// DESIGN.md §3.4: a thread's readers are its space's readers, and,
    /// if it has its own read restriction, only those it names (staff
    /// read everything for now; a suspended member is named by nothing).
    fn reads_thread(&self, who: Option<usize>, thread: usize) -> bool {
        let t = &self.threads[thread];
        !t.deleted
            && self.reads_space(who, t.space)
            && match &t.read {
                None => true,
                Some(names) => who.is_some_and(|u| {
                    self.users[u].acts_as_staff()
                        || (!self.users[u].suspended && self.names(names, u))
                }),
            }
    }

    /// Whether the viewer may reply in a thread: they may post in its
    /// space as far as replying goes, they can read the thread (repliers
    /// must be readers), and its own reply restriction, if any, names them;
    /// staff are no exception to that (DESIGN.md §3.1, §3.4).
    fn replies_in(&self, who: Option<usize>, thread: usize) -> bool {
        let Some(u) = who else { return false };
        let t = &self.threads[thread];
        let (c, user) = (&self.spaces[t.space], &self.users[u]);
        !user.silenced
            && !user.suspended
            && self.reads_thread(who, thread)
            && match c.reply_level {
                "members" => true,
                "listed" => self.lets_in(&c.reply_groups, u) || c.reply_users.contains(&u),
                _ => user.staff,
            }
            && t.reply.as_ref().is_none_or(|names| self.names(names, u))
    }

    fn post_live(&self, post: usize) -> bool {
        !self.posts[post].deleted && !self.posts[post].hidden
    }

    /// DESIGN.md §3.3: the author of a live post in a thread they can no
    /// longer read keeps its history and its edit form, unless the thread
    /// was deleted or they are suspended.
    fn keeps_own_post(&self, who: Option<usize>, post: usize) -> bool {
        let p = &self.posts[post];
        who.is_some_and(|u| u == p.author && !self.users[u].suspended)
            && self.post_live(post)
            && !self.threads[p.thread].deleted
            && !self.reads_thread(who, p.thread)
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
            // Conversations never have space pages.
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
        let found = self.search(viewer, "psst or message", None);
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

    /// Whether the viewer may start threads in a space: they read it,
    /// and they are staff, or it lets every member post, or they are in
    /// one of its write groups.
    fn posts_in_space(&self, who: Option<usize>, space: usize) -> bool {
        let Some(u) = who else { return false };
        let (c, user) = (&self.spaces[space], &self.users[u]);
        !user.silenced
            && !user.suspended
            && self.reads_space(who, space)
            && match c.write_level {
                "members" => true,
                "listed" => self.lets_in(&c.write_groups, u) || c.write_users.contains(&u),
                _ => user.staff,
            }
    }

    /// The "at most as visible" table in DESIGN.md §3.1: may content from
    /// a thread be quoted into space `dest`? The thread's audience is its
    /// space's, narrowed by its own read restriction if it has one, and the
    /// table holds for each: the destination must be at most as visible as
    /// the space, and name no more than the restriction does.
    fn may_quote_into(&self, thread: usize, dest: usize) -> bool {
        let t = &self.threads[thread];
        let (s, d) = (&self.spaces[t.space], &self.spaces[dest]);
        let from_space = match (s.level, d.level) {
            ("public", _) => true,
            ("members", "members" | "staff" | "listed") => true,
            ("staff", "staff") => true,
            ("listed", "listed") => {
                d.read_groups.is_subset(&s.read_groups) && d.read_users.is_subset(&s.read_users)
            }
            _ => false,
        };
        let from_restriction = t.read.as_ref().is_none_or(|names| {
            d.level == "listed"
                && d.read_groups.is_subset(&names.groups)
                && d.read_users.is_subset(&names.users)
        });
        from_space && from_restriction
    }

    /// Where the viewer may start new threads, and where they may quote
    /// each post they can read.
    fn check_writing(&self, who: Option<usize>, viewer: &Viewer) {
        let db = &self.db;
        let readable = self.block(content::readable_spaces(db, viewer)).unwrap();
        let may_post: BTreeSet<i64> = readable
            .iter()
            .filter(|c| writing::may_start_in(viewer, c))
            .map(|c| c.space.id)
            .collect();
        let expected = Self::ids(&self.spaces, |c| self.posts_in_space(who, c), |c| c.id);
        assert_eq!(may_post, expected, "{who:?}: spaces to post in");
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
                .map(|c| c.space.id)
                .collect();
            let expected = Self::ids(
                &self.spaces,
                |c| self.posts_in_space(who, c) && self.may_quote_into(post.thread, c),
                |c| c.id,
            );
            assert_eq!(
                destinations, expected,
                "{who:?}: where post {p} can be quoted"
            );
        }
    }

    /// Gives space `c` random read and write grants, as an administrator
    /// can with the settings form. Replying and starting threads share one
    /// grant here.
    fn set_space_access(&mut self, tc: &TestCase, c: usize) {
        let level = tc.draw(gs::sampled_from(vec![
            "public", "members", "staff", "listed",
        ]));
        let read_groups: BTreeSet<usize> = (0..self.groups.len())
            .filter(|_| tc.draw(gs::booleans()))
            .collect();
        let write_level = tc.draw(gs::sampled_from(vec!["members", "listed", "staff"]));
        let write_groups: BTreeSet<usize> = (0..self.groups.len())
            .filter(|_| tc.draw(gs::booleans()))
            .collect();
        let space = &self.spaces[c];
        let to_ids = |groups: &BTreeSet<usize>| -> Vec<i64> {
            groups.iter().map(|&g| self.groups[g]).collect()
        };
        // A feed's owner holds all of its permissions (DESIGN.md §3.2):
        // the form doesn't name them, and the store names them in every
        // grant that lists people.
        let owner_where = |grant_level: &str| -> BTreeSet<usize> {
            match (space.owner, grant_level) {
                (Some(owner), "listed") => BTreeSet::from([owner]),
                _ => BTreeSet::new(),
            }
        };
        let read_users = owner_where(level);
        let write_users = owner_where(write_level);
        let write = Grant::from_parts(write_level, &to_ids(&write_groups), &[]);
        let settings = SpaceSettings {
            slug: space.slug.clone(),
            name: format!("Space {c}"),
            description: String::new(),
            read: Grant::from_parts(level, &to_ids(&read_groups), &[]),
            reply: write.clone(),
            start: write,
            administer: if space.owner.is_some() {
                Grant::listed([], [])
            } else {
                Grant::staff()
            },
        }
        .clean()
        .unwrap();
        let saved = self
            .block(store::update_space(&self.db, space.id, &settings))
            .unwrap();
        // Nor may any of a feed's grants be left to staff alone.
        if space.owner.is_some() && (level == "staff" || write_level == "staff") {
            assert_eq!(saved, Err(store::FEED_OWNER_TAKES_PART.to_owned()));
            return;
        }
        saved.unwrap();
        let space = &mut self.spaces[c];
        space.level = level;
        space.read_users = read_users;
        space.read_groups = if level == "listed" {
            read_groups
        } else {
            BTreeSet::new()
        };
        space.write_level = write_level;
        space.write_groups = if write_level == "listed" {
            write_groups
        } else {
            BTreeSet::new()
        };
        space.write_users = write_users;
        space.reply_level = space.write_level;
        space.reply_groups = space.write_groups.clone();
        space.reply_users = space.write_users.clone();
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
        let readable_spaces = Self::ids(&self.spaces, |c| self.reads_space(who, c), |c| c.id);
        let thread_index: HashMap<i64, usize> = self
            .threads
            .iter()
            .enumerate()
            .map(|(i, t)| (t.id, i))
            .collect();

        let targets: BTreeSet<i64> = self
            .block(content::readable_spaces(db, &viewer))
            .unwrap()
            .iter()
            .map(|c| c.space.id)
            .collect();
        assert_eq!(targets, readable_spaces, "{who:?}: readable spaces");

        // The front page (DESIGN.md §3.5): the essential spaces the viewer
        // can read, then the spaces in their layout's sections that they
        // can read and haven't muted, each shown once with counts of what
        // they can read in it.
        let front = self
            .block(content::front_page(db, &viewer, &mut deps))
            .unwrap();
        let following = self.following(who);
        let mut shown = BTreeSet::new();
        for summary in front
            .essential
            .iter()
            .chain(front.sections.iter().flat_map(|s| &s.spaces))
        {
            assert!(shown.insert(summary.space.id), "{who:?}: shown twice");
            let c = self
                .spaces
                .iter()
                .position(|c| c.id == summary.space.id)
                .unwrap();
            assert_eq!(summary.essential, self.spaces[c].essential);
            let threads_here = (0..self.threads.len())
                .filter(|&t| self.threads[t].space == c && self.reads_thread(who, t))
                .count() as i64;
            assert_eq!(
                summary.thread_count, threads_here,
                "{who:?}: threads in {c}"
            );
            if let Some(last) = &summary.last {
                let t = thread_index[&last.thread.id];
                assert!(self.reads_thread(who, t) && self.threads[t].space == c);
            }
        }
        let essential: BTreeSet<i64> = front.essential.iter().map(|s| s.space.id).collect();
        let expected = Self::ids(
            &self.spaces,
            |c| {
                self.spaces[c].essential
                    && self.reads_space(who, c)
                    && !self.muted(who, c)
                    && self.in_view(who, c)
            },
            |c| c.id,
        );
        assert_eq!(essential, expected, "{who:?}: essential spaces");
        let in_sections: BTreeSet<i64> = front
            .sections
            .iter()
            .flat_map(|s| &s.spaces)
            .map(|s| s.space.id)
            .collect();
        let expected = Self::ids(
            &self.spaces,
            |c| {
                !self.spaces[c].essential
                    && following.contains(&c)
                    && self.reads_space(who, c)
                    && !self.muted(who, c)
                    && self.in_view(who, c)
            },
            |c| c.id,
        );
        assert_eq!(in_sections, expected, "{who:?}: front page sections");
        assert!(
            front.sections.iter().all(|s| !s.spaces.is_empty()),
            "{who:?}: an empty section is shown"
        );
        // Whether there is anything to read, their own feed waiting for its
        // first thread aside.
        assert_eq!(
            front.any_spaces,
            (0..self.spaces.len()).any(|c| self.reads_space(who, c) && self.listed_for(who, c)),
            "{who:?}: any spaces"
        );

        // The layout editor (DESIGN.md §3.5): the viewer's layout (the
        // default for a visitor, as the admin's editor shows it) with the
        // spaces in its sections that they can read, in the layout's
        // order; the readable spaces in no section, less their mutes, to
        // add; and the readable spaces they have muted.
        let arranged_layout = match who {
            None => self.default_layout,
            Some(u) => self.users[u].layout,
        };
        let arrangement = self
            .block(content::arrangement(db, &viewer, arranged_layout))
            .unwrap()
            .expect("the layout exists");
        let stored = self
            .block(layouts::read(db, arranged_layout))
            .unwrap()
            .expect("the layout exists");
        assert_eq!(
            arrangement
                .sections
                .iter()
                .map(|s| (s.id, s.spaces.iter().map(|c| c.id).collect::<Vec<_>>()))
                .collect::<Vec<_>>(),
            stored
                .sections
                .iter()
                .map(|s| (
                    s.id,
                    s.spaces
                        .iter()
                        .map(|c| c.id)
                        .filter(|id| readable_spaces.contains(id))
                        .collect::<Vec<_>>()
                ))
                .collect::<Vec<_>>(),
            "{who:?}: the editor's sections"
        );
        let arranged: BTreeSet<i64> = arrangement
            .sections
            .iter()
            .flat_map(|s| &s.spaces)
            .map(|c| c.id)
            .collect();
        let expected = Self::ids(
            &self.spaces,
            |c| following.contains(&c) && self.reads_space(who, c),
            |c| c.id,
        );
        assert_eq!(arranged, expected, "{who:?}: the editor's spaces");
        let available: BTreeSet<i64> = arrangement.available.iter().map(|c| c.id).collect();
        // A feed without threads is offered only to its owner, for their
        // own layout (a visitor's is the default).
        let expected = Self::ids(
            &self.spaces,
            |c| {
                !following.contains(&c)
                    && self.reads_space(who, c)
                    && !self.muted(who, c)
                    && self.in_view(who, c)
            },
            |c| c.id,
        );
        assert_eq!(available, expected, "{who:?}: spaces to add");
        let muted: BTreeSet<i64> = arrangement.muted.iter().map(|c| c.id).collect();
        let expected = Self::ids(
            &self.spaces,
            |c| self.muted(who, c) && self.reads_space(who, c),
            |c| c.id,
        );
        assert_eq!(muted, expected, "{who:?}: muted spaces in the editor");

        // The /latest tabs (DESIGN.md §3.5). All: every thread the viewer
        // can read, less those in spaces they have muted. Front page: those
        // in the essential spaces and the spaces in their sections. Each
        // section: those in its spaces. The sections are the viewer's own
        // layout's (the default's for a visitor), and together hold exactly
        // the spaces the model says the viewer follows.
        let unmuted_threads = Self::ids(
            &self.threads,
            |t| self.reads_thread(who, t) && !self.muted(who, self.threads[t].space),
            |t| t.id,
        );
        let latest = self
            .block(content::latest(
                db,
                &viewer,
                LatestScope::All,
                1,
                10_000,
                &mut deps,
            ))
            .unwrap();
        let rows: BTreeSet<i64> = latest.threads.iter().map(|r| r.thread.id).collect();
        assert_eq!(rows, unmuted_threads, "{who:?}: latest");
        assert_eq!(latest.thread_count, unmuted_threads.len() as i64);
        for row in &latest.threads {
            let t = thread_index[&row.thread.id];
            let source = self.threads[t].fork_of.map(|p| self.posts[p].thread);
            let expected = source
                .filter(|&s| self.reads_thread(who, s))
                .map(|s| self.threads[s].id);
            assert_eq!(row.forked_from.as_ref().map(|f| f.id), expected);
        }
        let front = self
            .block(content::latest(
                db,
                &viewer,
                LatestScope::Front,
                1,
                10_000,
                &mut deps,
            ))
            .unwrap();
        let rows: BTreeSet<i64> = front.threads.iter().map(|r| r.thread.id).collect();
        let expected = Self::ids(
            &self.threads,
            |t| {
                let c = self.threads[t].space;
                self.reads_thread(who, t)
                    && !self.muted(who, c)
                    && (self.spaces[c].essential || following.contains(&c))
            },
            |t| t.id,
        );
        assert_eq!(rows, expected, "{who:?}: front page tab");
        assert_eq!(front.thread_count, expected.len() as i64);
        let layout_id = match who {
            None => self.default_layout,
            Some(u) => self.users[u].layout,
        };
        let layout = self
            .block(layouts::read(db, layout_id))
            .unwrap()
            .expect("the layout exists");
        let tabs = self
            .block(content::layout_sections(db, &viewer, &mut deps))
            .unwrap();
        let any_muted = who.is_some_and(|u| !self.users[u].muted.is_empty());
        assert_eq!(tabs.has_mutes, any_muted, "{who:?}: has mutes");
        let tabs = tabs.sections;
        assert_eq!(
            tabs.iter().map(|s| s.id).collect::<Vec<_>>(),
            layout.sections.iter().map(|s| s.id).collect::<Vec<_>>(),
            "{who:?}: section tabs"
        );
        let mut in_sections = BTreeSet::new();
        for (section, tab) in layout.sections.iter().zip(&tabs) {
            let spaces: BTreeSet<usize> = section
                .spaces
                .iter()
                .map(|c| self.spaces.iter().position(|s| s.id == c.id).unwrap())
                .collect();
            for &c in &spaces {
                assert!(in_sections.insert(c), "{who:?}: space {c} in two sections");
            }
            let visible = spaces
                .iter()
                .filter(|&&c| {
                    self.reads_space(who, c) && !self.muted(who, c) && self.in_view(who, c)
                })
                .count() as i64;
            assert_eq!(
                tab.spaces, visible,
                "{who:?}: spaces on tab {}",
                section.name
            );
            let page = self
                .block(content::latest(
                    db,
                    &viewer,
                    LatestScope::Section(section.id),
                    1,
                    10_000,
                    &mut deps,
                ))
                .unwrap();
            let rows: BTreeSet<i64> = page.threads.iter().map(|r| r.thread.id).collect();
            let expected = Self::ids(
                &self.threads,
                |t| {
                    let c = self.threads[t].space;
                    self.reads_thread(who, t) && !self.muted(who, c) && spaces.contains(&c)
                },
                |t| t.id,
            );
            assert_eq!(rows, expected, "{who:?}: tab {}", section.name);
            assert_eq!(page.thread_count, expected.len() as i64);
        }
        assert_eq!(in_sections, following, "{who:?}: the layout's spaces");
        // Nobody else's sections are theirs to list.
        for other in self
            .users
            .iter()
            .map(|u| u.layout)
            .chain([self.default_layout])
        {
            if other == layout_id {
                continue;
            }
            for section in self
                .block(layouts::read(db, other))
                .unwrap()
                .expect("the layout exists")
                .sections
            {
                let page = self
                    .block(content::latest(
                        db,
                        &viewer,
                        LatestScope::Section(section.id),
                        1,
                        10_000,
                        &mut deps,
                    ))
                    .unwrap();
                assert!(page.threads.is_empty(), "{who:?}: someone else's section");
                assert_eq!(page.thread_count, 0);
            }
        }

        // The space index (DESIGN.md §3.5): the spaces of the kinds asked
        // for that the viewer can read and hasn't muted, feeds only once
        // they have a thread, their owners' included (§3.2).
        for kinds in [KindFilter::Both, KindFilter::Topic, KindFilter::Feed] {
            let index: BTreeSet<i64> = self
                .block(content::space_index(
                    db,
                    &viewer,
                    SpaceSort::Created,
                    kinds,
                    false,
                    false,
                    &mut deps,
                ))
                .unwrap()
                .iter()
                .map(|listing| {
                    let c = self
                        .spaces
                        .iter()
                        .position(|c| c.id == listing.space.id)
                        .unwrap();
                    assert_eq!(listing.feed, self.spaces[c].owner.is_some());
                    listing.space.id
                })
                .collect();
            let expected = Self::ids(
                &self.spaces,
                |c| {
                    let feed = self.spaces[c].owner.is_some();
                    self.reads_space(who, c)
                        && !self.muted(who, c)
                        && self.listed_for(who, c)
                        && match kinds {
                            KindFilter::Both => true,
                            KindFilter::Topic => !feed,
                            KindFilter::Feed => feed,
                        }
                },
                |c| c.id,
            );
            assert_eq!(index, expected, "{who:?}: the space index of {kinds:?}");
        }

        for (c, space) in self.spaces.iter().enumerate() {
            let page = self
                .block(content::space_page(db, &viewer, &space.slug, 1, &mut deps))
                .unwrap();
            match page {
                None => assert!(!self.reads_space(who, c), "{who:?}: space {c}"),
                Some(page) => {
                    assert!(self.reads_space(who, c), "{who:?}: space {c}");
                    let shown: BTreeSet<i64> = page.threads.iter().map(|r| r.thread.id).collect();
                    let expected = Self::ids(
                        &self.threads,
                        |t| self.threads[t].space == c && self.reads_thread(who, t),
                        |t| t.id,
                    );
                    assert_eq!(shown, expected, "{who:?}: threads in space {c}");
                }
            }
        }

        for (t, thread) in self.threads.iter().enumerate() {
            let visible = self.reads_thread(who, t);
            let page = self
                .block(content::thread_page(db, &viewer, thread.id, 1, &mut deps))
                .unwrap();
            assert_eq!(page.is_some(), visible, "{who:?}: thread {t}");
            assert_eq!(
                self.block(content::thread_slug(db, &viewer, thread.id))
                    .unwrap()
                    .is_some(),
                visible,
                "{who:?}: thread {t}"
            );
            let target = self
                .block(content::thread_target(db, &viewer, thread.id))
                .unwrap();
            assert_eq!(target.is_some(), visible, "{who:?}: thread {t}");
            // Who may reply (DESIGN.md §3.4): readers whom the space and
            // the thread's own reply restriction let reply.
            if let Some(target) = &target {
                assert_eq!(
                    writing::may_reply_to(&viewer, target),
                    self.replies_in(who, t),
                    "{who:?}: replying in thread {t}"
                );
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
            let kept = self.keeps_own_post(who, p);
            let own = self
                .block(content::post_target_or_own(db, &viewer, post.id))
                .unwrap();
            assert_eq!(own.is_some(), in_thread || kept, "{who:?}: own post {p}");
            if let Some(own) = own {
                assert_eq!(
                    own.thread.readable(),
                    in_thread,
                    "{who:?}: post {p}'s thread"
                );
                if !in_thread {
                    assert!(own.thread.thread.title.is_empty(), "{who:?}: post {p}");
                }
            }
            let history = self
                .block(content::post_history(db, &viewer, post.id, &mut deps))
                .unwrap();
            assert_eq!(
                history.is_some(),
                (in_thread && self.shown_post(who, p)) || kept,
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
        // are, less those in spaces they have muted: every post's text is
        // "Text", or "Opening" for a thread's first post. A search within
        // one space finds its posts whether or not it is muted.
        let found = self.search(&viewer, "text or opening", None);
        let expected = Self::ids(
            &self.posts,
            |p| {
                let t = self.posts[p].thread;
                self.reads_thread(who, t)
                    && self.shown_post(who, p)
                    && !self.muted(who, self.threads[t].space)
            },
            |p| p.id,
        );
        assert_eq!(found, expected, "{who:?}: search");
        for (c, space) in self.spaces.iter().enumerate() {
            let found = self.search(&viewer, "text or opening", Some(&space.slug));
            let expected = Self::ids(
                &self.posts,
                |p| {
                    let t = self.posts[p].thread;
                    self.threads[t].space == c
                        && self.reads_thread(who, t)
                        && self.shown_post(who, p)
                },
                |p| p.id,
            );
            assert_eq!(found, expected, "{who:?}: search in space {c}");
        }

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

        self.check_audience(who, &viewer);
        if let Some(u) = who {
            self.check_grant_choices(u);
        }
        self.check_writing(who, &viewer);
        self.check_conversations(who, &viewer);
    }

    /// A space's audience page (DESIGN.md §3.3): for those who can read
    /// the space, every group and list its grants name, where a list is
    /// named, with who is on it, only to its owner and the people on it,
    /// and to anyone else is "people X has chosen", once per owner; a site
    /// group's members are listed only to the people in it.
    fn check_audience(&self, who: Option<usize>, viewer: &Viewer) {
        const NAMES: [&str; 5] = ["Boss", "Ruth", "Jonah", "Maya", "Kit"];
        let nobody = BTreeSet::new();
        for (c, space) in self.spaces.iter().enumerate() {
            let audience = self
                .block(content::space_audience(&self.db, viewer, &space.slug))
                .unwrap();
            assert_eq!(
                audience.is_some(),
                self.reads_space(who, c),
                "{who:?}: audience of space {c}"
            );
            let Some(audience) = audience else {
                continue;
            };
            for grant in &audience.grants {
                let named = match grant.verb {
                    "read" => &space.read_groups,
                    "reply" | "start" => &space.write_groups,
                    _ => &nobody,
                };
                let user_ids = |members: &BTreeSet<usize>| -> BTreeSet<i64> {
                    members.iter().map(|&u| self.users[u].id).collect()
                };
                let on = |g: usize| -> BTreeSet<usize> {
                    (0..self.users.len())
                        .filter(|&u| self.users[u].groups.contains(&g))
                        .collect()
                };
                let mut expected: Vec<(bool, Option<BTreeSet<i64>>, Option<String>)> = Vec::new();
                let mut strangers = BTreeSet::new();
                for &g in named {
                    let knows = who.is_some_and(|u| {
                        self.users[u].groups.contains(&g) || self.group_owners[g] == Some(u)
                    });
                    match self.group_owners[g] {
                        None => expected.push((false, knows.then(|| user_ids(&on(g))), None)),
                        Some(_) if knows => expected.push((true, Some(user_ids(&on(g))), None)),
                        Some(owner) => {
                            if strangers.insert(owner) {
                                expected.push((
                                    true,
                                    None,
                                    Some(format!("people {} has chosen", NAMES[owner])),
                                ));
                            }
                        }
                    }
                }
                let mut found: Vec<(bool, Option<BTreeSet<i64>>, Option<String>)> = grant
                    .groups
                    .iter()
                    .map(|group| {
                        let members = group
                            .members
                            .as_ref()
                            .map(|m| m.iter().map(|u| u.id).collect());
                        let stranger =
                            (group.list && members.is_none()).then(|| group.label.clone());
                        (group.list, members, stranger)
                    })
                    .collect();
                expected.sort();
                found.sort();
                assert_eq!(
                    found, expected,
                    "{who:?}: {} grant of space {c}",
                    grant.verb
                );
                // A list's own name reaches only those who know it.
                for group in &grant.groups {
                    if group.list && group.members.is_none() {
                        assert!(group.label.starts_with("people "), "{}", group.label);
                    }
                }
            }
        }
    }

    /// Whether member `u` knows group `g` exists and what it is called: a
    /// site group, or a list they own or are on (DESIGN.md §3.3).
    fn knows_group(&self, u: usize, g: usize) -> bool {
        match self.group_owners[g] {
            None => true,
            Some(owner) => owner == u || self.users[u].groups.contains(&g),
        }
    }

    /// What a grant form offers member `u` for a grant naming `named`
    /// (DESIGN.md §3.3): every group they know, by its id, and the lists
    /// the grant names that they don't know as one choice per owner, which
    /// gives neither the lists' ids nor how many there are. Ticking what
    /// the grant names gives it back unchanged.
    fn check_grant_choices(&self, u: usize) {
        let user_id = self.users[u].id;
        let mut grants: Vec<&BTreeSet<usize>> = Vec::new();
        for space in &self.spaces {
            grants.push(&space.read_groups);
            grants.push(&space.write_groups);
        }
        for named in grants {
            let ids: Vec<i64> = named.iter().map(|&g| self.groups[g]).collect();
            let choices = self
                .block(groups::choosable(&self.db, Some(user_id), &ids))
                .unwrap();
            let mut expected: BTreeMap<String, BTreeSet<i64>> = BTreeMap::new();
            for g in 0..self.groups.len() {
                let id = self.groups[g];
                if self.knows_group(u, g) {
                    expected.insert(id.to_string(), BTreeSet::from([id]));
                } else if named.contains(&g) {
                    let owner = self.group_owners[g].expect("site groups are known");
                    expected
                        .entry(format!("owner:{}", self.users[owner].id))
                        .or_default()
                        .insert(id);
                }
            }
            let mut found: BTreeMap<String, BTreeSet<i64>> = choices
                .iter()
                .map(|c| (c.value.clone(), c.ids.iter().copied().collect()))
                .collect();
            assert_eq!(found.len(), choices.len(), "{u}: a value offered twice");
            // Every member has a Friends list; the model leaves out those
            // it doesn't use, which are offered only to their owners.
            found.retain(|_, ids| {
                ids.iter().any(|id| self.groups.contains(id)) || {
                    let owners: Vec<Option<i64>> = self
                        .block(
                            sqlx::query_scalar(
                                "SELECT owner_user_id FROM groups WHERE id = ANY($1)",
                            )
                            .bind(ids.iter().copied().collect::<Vec<i64>>())
                            .fetch_all(&self.pool),
                        )
                        .unwrap();
                    assert!(
                        owners.iter().all(|&owner| owner == Some(user_id)),
                        "{u}: offered {ids:?}"
                    );
                    false
                }
            });
            assert_eq!(found, expected, "{u}: choices for {named:?}");
            for choice in &choices {
                if choice.value.starts_with("owner:") {
                    assert!(choice.label.starts_with("People "), "{}", choice.label);
                }
            }
            let ticked: Vec<String> = choices
                .iter()
                .filter(|c| c.ids.iter().any(|id| ids.contains(id)))
                .map(|c| c.value.clone())
                .collect();
            let mut sorted = ids.clone();
            sorted.sort_unstable();
            assert_eq!(groups::chosen(&choices, &ticked), Some(sorted));
            // A list they don't know can't be ticked by its id.
            for (g, &id) in self.groups.iter().enumerate() {
                if !self.knows_group(u, g) {
                    assert_eq!(groups::chosen(&choices, &[id.to_string()]), None);
                }
            }
        }
    }

    /// The ids of every post a search finds for the viewer, everywhere or
    /// in one space.
    fn search(&self, viewer: &Viewer, words: &str, space: Option<&str>) -> BTreeSet<i64> {
        let query = content::search::SearchQuery {
            text: words.into(),
            space: space.map(str::to_owned),
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
        tc.assume(self.groups.len() < 6);
        self.add_group();
    }

    #[rule]
    fn new_list(&mut self, tc: TestCase) {
        tc.assume(self.groups.len() < 6);
        let owner = tc.draw(gs::integers::<usize>().max_value(self.users.len() - 1));
        self.add_list(owner);
    }

    #[rule]
    fn toggle_membership(&mut self, tc: TestCase) {
        let u = tc.draw(gs::integers::<usize>().max_value(self.users.len() - 1));
        let g = tc.draw(gs::integers::<usize>().max_value(self.groups.len() - 1));
        // Nobody is on their own list.
        tc.assume(self.group_owners[g] != Some(u));
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
        assert!(matches!(changed, RoleChange::Done { .. }), "{changed:?}");
        self.users[u].staff = role.is_staff();
    }

    #[rule]
    fn new_space(&mut self, tc: TestCase) {
        tc.assume(self.spaces.iter().filter(|c| c.owner.is_none()).count() < 5);
        let level = tc.draw(gs::sampled_from(LEVELS.to_vec()));
        self.add_space(level);
    }

    /// A member who runs a space names in its read grant some of what the
    /// form offers them: groups, their own lists and the lists they are
    /// on, and whatever lists of others the grant names already (DESIGN.md
    /// §3.3).
    #[rule]
    fn name_in_read_grant(&mut self, tc: TestCase) {
        let u = tc.draw(gs::integers::<usize>().max_value(self.users.len() - 1));
        let c = tc.draw(gs::integers::<usize>().max_value(self.spaces.len() - 1));
        let named: Vec<i64> = self.spaces[c]
            .read_groups
            .iter()
            .map(|&g| self.groups[g])
            .collect();
        let choices = self
            .block(groups::choosable(&self.db, Some(self.users[u].id), &named))
            .unwrap();
        // The Friends lists the model leaves out are never named.
        let ticked: Vec<String> = choices
            .iter()
            .filter(|choice| choice.ids.iter().all(|id| self.groups.contains(id)))
            .filter(|_| tc.draw(gs::booleans()))
            .map(|choice| choice.value.clone())
            .collect();
        let ids = groups::chosen(&choices, &ticked).expect("every value was offered");
        let read_groups: BTreeSet<usize> = ids
            .iter()
            .map(|id| self.groups.iter().position(|g| g == id).unwrap())
            .collect();
        for &g in &read_groups {
            // Nobody names a list they don't know unless it was named.
            assert!(self.knows_group(u, g) || self.spaces[c].read_groups.contains(&g));
        }
        let id = self.spaces[c].id;
        // A feed's owner is named too, as the store names them.
        let read_users: BTreeSet<usize> = self.spaces[c].owner.into_iter().collect();
        let user_ids: Vec<i64> = read_users.iter().map(|&u| self.users[u].id).collect();
        self.block(store::set_grant(
            &self.pool,
            id,
            "read",
            &Grant::from_parts("listed", &ids, &user_ids),
        ))
        .unwrap();
        self.spaces[c].level = "listed";
        self.spaces[c].read_groups = read_groups;
        self.spaces[c].read_users = read_users;
    }

    /// What the admin's space form does: new read and write levels and
    /// groups.
    #[rule]
    fn set_access(&mut self, tc: TestCase) {
        let c = tc.draw(gs::integers::<usize>().max_value(self.spaces.len() - 1));
        self.set_space_access(&tc, c);
    }

    /// An admin takes a space off the default front page, or puts it
    /// back; members who follow the default follow this too.
    #[rule]
    fn toggle_on_front_page(&mut self, tc: TestCase) {
        let c = tc.draw(gs::integers::<usize>().max_value(self.spaces.len() - 1));
        let id = self.spaces[c].id;
        let done = if self.spaces[c].on_default {
            self.block(layouts::unfollow(&self.db, self.default_layout, id))
        } else {
            self.block(layouts::follow(
                &self.db,
                self.default_layout,
                self.default_section,
                id,
            ))
        };
        assert!(done.unwrap());
        self.spaces[c].on_default = !self.spaces[c].on_default;
        self.default_changed();
    }

    /// An admin marks a space essential, which unmutes it for everyone,
    /// or not.
    #[rule]
    fn toggle_essential(&mut self, tc: TestCase) {
        let c = tc.draw(gs::integers::<usize>().max_value(self.spaces.len() - 1));
        let essential = !self.spaces[c].essential;
        assert!(
            self.block(layouts::set_essential(
                &self.db,
                self.spaces[c].id,
                essential
            ))
            .unwrap()
        );
        self.spaces[c].essential = essential;
        if essential {
            for user in &mut self.users {
                user.muted.remove(&c);
            }
        }
    }

    /// A member mutes a space (unless it is essential), or unmutes it.
    /// Muting takes it out of their sections; unmuting doesn't put it
    /// back.
    #[rule]
    fn toggle_mute(&mut self, tc: TestCase) {
        let u = tc.draw(gs::integers::<usize>().max_value(self.users.len() - 1));
        let c = tc.draw(gs::integers::<usize>().max_value(self.spaces.len() - 1));
        let (user_id, space_id) = (self.users[u].id, self.spaces[c].id);
        if self.users[u].muted.contains(&c) {
            assert!(
                self.block(layouts::unmute(&self.db, user_id, space_id))
                    .unwrap()
            );
            self.users[u].muted.remove(&c);
        } else {
            let outcome = self
                .block(layouts::mute(&self.db, user_id, space_id))
                .unwrap();
            if self.spaces[c].essential {
                assert_eq!(outcome, MuteOutcome::Essential);
            } else {
                assert_eq!(outcome, MuteOutcome::Muted);
                self.users[u].muted.insert(c);
                self.users[u].sections.remove(&c);
                self.users[u].kept.remove(&c);
            }
        }
    }

    /// A member follows a space (into the first section of their layout,
    /// which unmutes it) or unfollows it. Either makes the layout theirs:
    /// the default no longer reaches it.
    #[rule]
    fn toggle_follow(&mut self, tc: TestCase) {
        let u = tc.draw(gs::integers::<usize>().max_value(self.users.len() - 1));
        let c = tc.draw(gs::integers::<usize>().max_value(self.spaces.len() - 1));
        let (layout, space_id) = (self.users[u].layout, self.spaces[c].id);
        if self.users[u].sections.contains(&c) {
            assert!(
                self.block(layouts::unfollow(&self.db, layout, space_id))
                    .unwrap()
            );
            self.users[u].sections.remove(&c);
            self.users[u].kept.remove(&c);
        } else {
            let section = self
                .block(layouts::read(&self.db, layout))
                .unwrap()
                .expect("the member's layout exists")
                .sections[0]
                .id;
            assert!(
                self.block(layouts::follow(&self.db, layout, section, space_id))
                    .unwrap()
            );
            self.users[u].sections.insert(c);
            self.users[u].muted.remove(&c);
        }
        self.users[u].edited = true;
    }

    /// A member arranges their front page in the editor: a space goes to
    /// the top of a new section of their own. Like following, this unmutes
    /// it and makes the layout theirs.
    #[rule]
    fn arrange(&mut self, tc: TestCase) {
        let u = tc.draw(gs::integers::<usize>().max_value(self.users.len() - 1));
        let c = tc.draw(gs::integers::<usize>().max_value(self.spaces.len() - 1));
        let (layout, space_id) = (self.users[u].layout, self.spaces[c].id);
        let section = self
            .block(layouts::add_section(&self.db, layout, "Arranged"))
            .unwrap();
        assert_eq!(
            self.block(layouts::move_space(&self.db, layout, space_id, section, 0))
                .unwrap(),
            layouts::Step::Changed
        );
        let sections: Vec<i64> = vec![section];
        assert_ne!(
            self.block(layouts::reorder_sections(&self.db, layout, &sections))
                .unwrap(),
            layouts::Step::NotFound
        );
        self.users[u].sections.insert(c);
        self.users[u].muted.remove(&c);
        self.users[u].edited = true;
    }

    /// An admin applies the default front page again to everyone who
    /// follows it ("apply to members who haven't edited theirs").
    #[rule]
    fn reapply_default(&mut self, _: TestCase) {
        self.block(layouts::default_changed(&self.db)).unwrap();
        self.default_changed();
    }

    #[rule]
    fn new_thread(&mut self, tc: TestCase) {
        tc.assume(self.threads.len() < 12);
        let space = tc.draw(gs::integers::<usize>().max_value(self.spaces.len() - 1));
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
                    space_id: self.spaces[space].id,
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
            space,
            author,
            deleted: false,
            fork_of,
            read: None,
            reply: None,
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
        let c = tc.draw(gs::integers::<usize>().max_value(self.spaces.len() - 1));
        let previous = self
            .block(store::move_thread(
                &self.db,
                self.threads[t].id,
                self.spaces[c].id,
            ))
            .unwrap();
        assert_eq!(previous, Some(self.spaces[self.threads[t].space].id));
        self.threads[t].space = c;
        self.name_feed_owner(t);
    }

    /// A feed's owner holds every permission on it (DESIGN.md §2, §3.2),
    /// so the database names them in every part of a restriction of a
    /// thread in their feed, however it came there; a thread moved out
    /// keeps the names it had.
    fn name_feed_owner(&mut self, t: usize) {
        let Some(owner) = self.spaces[self.threads[t].space].owner else {
            return;
        };
        let thread = &mut self.threads[t];
        for names in [&mut thread.read, &mut thread.reply].into_iter().flatten() {
            names.users.insert(owner);
        }
    }

    /// Gives a thread its own read and reply restrictions, naming random
    /// groups, lists and members, or takes them away (DESIGN.md §3.4).
    #[rule]
    fn restrict_thread(&mut self, tc: TestCase) {
        tc.assume(!self.threads.is_empty());
        let t = tc.draw(gs::integers::<usize>().max_value(self.threads.len() - 1));
        let read = self.draw_names(&tc);
        let reply = self.draw_names(&tc);
        let restriction = self.restriction(&read, &reply);
        assert!(
            self.block(store::set_thread_restriction(
                &self.pool,
                self.threads[t].id,
                &restriction
            ))
            .unwrap()
        );
        self.threads[t].read = read;
        self.threads[t].reply = reply;
        self.name_feed_owner(t);
    }

    /// A member tries to change a thread's restriction as the writing
    /// layer lets them: its author while they can read it, and those who
    /// run its space, whether or not they can; the author is always named.
    #[rule]
    fn restrict_thread_as(&mut self, tc: TestCase) {
        tc.assume(!self.threads.is_empty());
        let t = tc.draw(gs::integers::<usize>().max_value(self.threads.len() - 1));
        let u = tc.draw(gs::integers::<usize>().max_value(self.users.len() - 1));
        let mut read = self.draw_names(&tc);
        let mut reply = self.draw_names(&tc);
        // Each part may name the groups and lists `u` knows, and keep
        // those it names already (DESIGN.md §3.3; decisions #52, #67).
        let nameable = |names: &Option<Names>, now: &Option<Names>| {
            names.as_ref().is_none_or(|names| {
                names.groups.iter().all(|&g| {
                    self.knows_group(u, g)
                        || now.as_ref().is_some_and(|now| now.groups.contains(&g))
                })
            })
        };
        if tc.draw(gs::booleans()) {
            // Mostly, only what they may name.
            for (names, now) in [
                (&mut read, &self.threads[t].read),
                (&mut reply, &self.threads[t].reply),
            ] {
                if let Some(names) = names {
                    names.groups.retain(|&g| {
                        self.knows_group(u, g)
                            || now.as_ref().is_some_and(|now| now.groups.contains(&g))
                    });
                }
            }
        }
        let lawful =
            nameable(&read, &self.threads[t].read) && nameable(&reply, &self.threads[t].reply);
        let restriction = self.restriction(&read, &reply);
        let viewer = self.viewer(Some(u));
        let outcome = self
            .block(writing::restrict_thread(
                &self.state,
                &viewer,
                self.threads[t].id,
                &restriction,
            ))
            .unwrap();
        let thread = &self.threads[t];
        let user = &self.users[u];
        let space = &self.spaces[thread.space];
        let reads = self.reads_thread(Some(u), t);
        let runs = !user.suspended
            && self.reads_space(Some(u), thread.space)
            && (user.staff || space.owner == Some(u));
        let author = thread.author == u && reads;
        if thread.deleted || !self.reads_space(Some(u), thread.space) {
            assert_eq!(outcome, Err(Refused::NotFound), "{u} restricting {t}");
            return;
        }
        if !author && !runs {
            assert_eq!(
                matches!(outcome, Err(Refused::NotAllowed(_))),
                reads,
                "{u} restricting {t}: {outcome:?}"
            );
            assert!(outcome.is_err());
            return;
        }
        if user.suspended {
            assert!(
                matches!(outcome, Err(Refused::NotAllowed(_))),
                "{outcome:?}"
            );
            return;
        }
        if !lawful {
            assert!(
                matches!(outcome, Err(Refused::Invalid(_))),
                "{u} naming what they don't know in {t}: {outcome:?}"
            );
            return;
        }
        assert_eq!(outcome, Ok(()), "{u} restricting {t}");
        let author = thread.author;
        let with_author = |names: Option<Names>| {
            names.map(|mut names| {
                names.users.insert(author);
                names
            })
        };
        self.threads[t].read = with_author(read);
        self.threads[t].reply = with_author(reply);
        self.name_feed_owner(t);
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
    /// A feed's owner holds all of its permissions (DESIGN.md §2, §3.2),
    /// whatever its administrators have saved: every grant lets them
    /// through, they can always open it and its settings, and they can
    /// read and reply in every thread in it, whoever restricted it.
    #[invariant]
    fn feeds_are_run_by_their_owners(&self, _: TestCase) {
        for (space, c) in self.spaces.iter().enumerate() {
            let Some(owner) = c.owner else { continue };
            let grants: Vec<(String, String, Vec<i64>)> = self
                .block(
                    sqlx::query_as(
                        "SELECT verb, level, user_ids FROM space_grants WHERE space_id = $1",
                    )
                    .bind(c.id)
                    .fetch_all(&self.pool),
                )
                .unwrap();
            assert_eq!(grants.len(), 4);
            for (verb, level, user_ids) in grants {
                assert!(
                    level == "members"
                        || (level == "public" && verb == "read")
                        || (level == "listed" && user_ids.contains(&self.users[owner].id)),
                    "feed {}: {verb} is {level} {user_ids:?}",
                    c.slug
                );
            }
            // A suspended member reads only what visitors read.
            if self.users[owner].suspended {
                continue;
            }
            let viewer = self.viewer(Some(owner));
            let target = self
                .block(content::space_target(&self.db, &viewer, &c.slug))
                .unwrap()
                .unwrap_or_else(|| panic!("the owner of feed {} can't read it", c.slug));
            let rules = &target.rules;
            assert!(rules.may_administer(&viewer), "feed {}", c.slug);
            if !self.users[owner].silenced {
                assert!(
                    rules.may_start(&viewer) && rules.may_reply(&viewer, false),
                    "feed {}",
                    c.slug
                );
            }
            // And every thread in it, whatever its own restriction says
            // (DESIGN.md §3.4): it names the owner.
            for (t, thread) in self.threads.iter().enumerate() {
                if thread.space != space || thread.deleted {
                    continue;
                }
                let target = self
                    .block(content::thread_target(&self.db, &viewer, thread.id))
                    .unwrap()
                    .filter(|target| target.readable())
                    .unwrap_or_else(|| {
                        panic!("the owner of feed {} can't read thread {t}", c.slug)
                    });
                if !self.users[owner].silenced {
                    assert!(
                        target.restriction.may_reply(rules, &viewer, false),
                        "the owner of feed {} can't reply in thread {t}",
                        c.slug
                    );
                }
            }
        }
    }

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

    /// The denormalised `threads.space_visibility` is always the space's
    /// visibility, and `threads.visibility` the thread's effective one:
    /// restricted while it has its own read restriction, its space's
    /// otherwise (DESIGN.md §3.1, §3.4), however threads move and read
    /// grants and restrictions change.
    #[invariant(always_run)]
    fn visibility_copies_are_current(&self, _: TestCase) {
        let stale: i64 = self
            .block(
                sqlx::query_scalar(
                    "SELECT count(*) FROM threads t JOIN spaces c ON c.id = t.space_id
                     WHERE t.space_visibility <> c.visibility
                        OR t.visibility <> CASE WHEN t.read_user_ids IS NULL THEN c.visibility
                                                ELSE 'restricted' END",
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
            let expected = match (self.spaces[t.space].level, &t.read) {
                (_, Some(_)) => "restricted",
                ("public", None) => "public",
                ("members", None) => "members",
                _ => "restricted",
            };
            assert_eq!(level, expected);
        }
    }
}

#[hegel::standalone_function(suppress_health_check = [HealthCheck::TooSlow])]
fn run_access(tc: TestCase, rt: &Handle, pool: &PgPool) {
    let mut world = Access::new(rt.clone(), pool.clone());
    // Start from random settings, so that every run has a mix of levels
    // and groups to read, post and quote across.
    for c in 0..world.spaces.len() {
        world.set_space_access(&tc, c);
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
