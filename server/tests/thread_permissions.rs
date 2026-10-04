//! Per-thread permissions (DESIGN.md §3.4, milestone 14): a thread may
//! carry its own `read` and `reply` restriction, a set of groups, lists and
//! people. Its effective audience is the intersection of its space's grant
//! and its restriction, so it is only ever narrower than its space; its
//! stored visibility is that effective visibility, and every read through
//! the access layer checks the thread's own names as well as the space's.

mod common;

use axum::http::StatusCode;
use common::Forum;
use parlour::changes::Change;
use parlour::content::{self, uploads::UploadAccess};
use parlour::permissions::{Grant, ThreadRestriction};
use parlour::store;
use parlour::viewer::Viewer;
use parlour::writing::{self, Refused};
use sqlx::PgPool;

struct Club {
    f: Forum,
    ruth: i64,
    jonah: i64,
    maya: i64,
    kit: i64,
    /// Ruth's Friends list, which Jonah is on.
    friends: i64,
    ruth_cookie: String,
    jonah_cookie: String,
    maya_cookie: String,
    staff_cookie: String,
}

/// Ruth, Jonah, Maya and Kit are members, and Sam a moderator. Jonah is on
/// Ruth's Friends list.
async fn club(pool: PgPool) -> Club {
    let f = Forum::new(pool);
    f.no_new_members().await;
    let ruth = f.member("ruth", "user", None).await;
    let jonah = f.member("jonah", "user", None).await;
    let maya = f.member("maya", "user", None).await;
    let kit = f.member("kit", "user", None).await;
    let sam = f.member("sam", "moderator", None).await;
    let friends = f
        .id(&format!(
            "SELECT id FROM groups WHERE owner_user_id = {ruth} AND name = 'Friends'"
        ))
        .await;
    f.join_group(friends, jonah).await;
    Club {
        ruth_cookie: f.session(ruth).await,
        jonah_cookie: f.session(jonah).await,
        maya_cookie: f.session(maya).await,
        staff_cookie: f.session(sam).await,
        f,
        ruth,
        jonah,
        maya,
        kit,
        friends,
    }
}

impl Club {
    async fn viewer(&self, user: i64) -> Viewer {
        Viewer::User(
            parlour::sessions::user_ctx(self.f.state.db(), user)
                .await
                .unwrap()
                .unwrap(),
        )
    }

    /// Gives a thread its own restriction directly, as the store does, and
    /// clears the cached pages as the handler would.
    async fn restrict(&self, thread: i64, read: Option<Grant>, reply: Option<Grant>) {
        assert!(
            store::set_thread_restriction(&self.f.db, thread, &ThreadRestriction { read, reply },)
                .await
                .unwrap()
        );
        self.f
            .changed(Change::ThreadChanged {
                thread_id: thread,
                previous_space_id: None,
            })
            .await;
    }

    async fn visibility(&self, thread: i64) -> String {
        sqlx::query_scalar("SELECT visibility FROM threads WHERE id = $1")
            .bind(thread)
            .fetch_one(&self.f.db)
            .await
            .unwrap()
    }

    async fn status(&self, path: &str, cookie: Option<&str>) -> StatusCode {
        self.f.get_as(path, cookie).await.status
    }
}

fn thread_path(id: i64, slug: &str) -> String {
    format!("/t/{id}/{slug}")
}

#[sqlx::test]
async fn a_restricted_thread_is_read_only_by_those_it_names(pool: PgPool) {
    let club = club(pool).await;
    let f = &club.f;
    let general = f.space("general", "public", None, 0).await;
    let secret = f
        .thread(general, club.ruth, "Secret plans", "Only for my friends.")
        .await
        .thread_id;
    f.thread(general, club.maya, "Open thread", "Anyone may read this.")
        .await;
    let path = thread_path(secret, "secret-plans");
    // Visitors' pages are cached before the restriction arrives.
    assert_eq!(club.status(&path, None).await, StatusCode::OK);
    assert!(f.page("/c/general").await.contains("Secret plans"));
    assert!(f.page("/latest").await.contains("Secret plans"));
    assert_eq!(club.visibility(secret).await, "public");

    club.restrict(secret, Some(Grant::listed([club.friends], [])), None)
        .await;
    // A thread with its own read restriction is restricted, even in a
    // public space.
    assert_eq!(club.visibility(secret).await, "restricted");
    assert_eq!(club.status(&path, None).await, StatusCode::NOT_FOUND);
    for page in ["/c/general", "/latest", "/", "/search?q=plans"] {
        let body = f.page(page).await;
        assert!(!body.contains("Secret plans"), "{page}: {body}");
        assert!(!body.contains("my friends"), "{page}: {body}");
    }
    assert!(f.page("/c/general").await.contains("Open thread"));
    // Ruth, whose list it names, and Jonah, who is on it, read it.
    for cookie in [&club.ruth_cookie, &club.jonah_cookie] {
        assert_eq!(club.status(&path, Some(cookie)).await, StatusCode::OK);
        assert!(
            f.page_as("/c/general", cookie)
                .await
                .contains("Secret plans")
        );
        assert!(
            f.page_as("/search?q=plans", cookie)
                .await
                .contains("my friends")
        );
    }
    // Maya, who isn't, doesn't.
    assert_eq!(
        club.status(&path, Some(&club.maya_cookie)).await,
        StatusCode::NOT_FOUND
    );
    for page in ["/c/general", "/latest", "/search?q=plans"] {
        let body = f.page_as(page, &club.maya_cookie).await;
        assert!(!body.contains("Secret plans"), "{page}: {body}");
    }
    // Staff still read every thread until milestone 15.
    assert_eq!(
        club.status(&path, Some(&club.staff_cookie)).await,
        StatusCode::OK
    );

    // Widening it back to the space's audience makes it public again.
    club.restrict(secret, None, None).await;
    assert_eq!(club.visibility(secret).await, "public");
    assert_eq!(club.status(&path, None).await, StatusCode::OK);
    assert!(f.page("/c/general").await.contains("Secret plans"));
}

#[sqlx::test]
async fn a_thread_is_never_wider_than_its_space(pool: PgPool) {
    let club = club(pool).await;
    let f = &club.f;
    let group = f.group("pals", "Pals").await;
    f.join_group(group, club.jonah).await;
    let lounge = f.space("lounge", "listed", None, 0).await;
    f.restrict(lounge, &[group], None).await;
    let thread = f
        .thread(lounge, club.jonah, "Inside", "Between us.")
        .await
        .thread_id;
    let plain = f
        .thread(lounge, club.jonah, "Plain", "For the lounge.")
        .await
        .thread_id;
    // The thread names Maya, who can't read the lounge: she still can't.
    club.restrict(
        thread,
        Some(Grant::listed([], [club.jonah, club.maya])),
        None,
    )
    .await;
    let path = thread_path(thread, "inside");
    assert_eq!(
        club.status(&path, Some(&club.jonah_cookie)).await,
        StatusCode::OK
    );
    assert_eq!(
        club.status(&path, Some(&club.maya_cookie)).await,
        StatusCode::NOT_FOUND
    );
    // Opening the lounge to every member lets her in, and nobody else the
    // thread doesn't name; the thread stays restricted while the other
    // thread follows its space.
    f.set_grant(lounge, "read", Grant::members()).await;
    assert_eq!(club.visibility(thread).await, "restricted");
    assert_eq!(club.visibility(plain).await, "members");
    assert_eq!(
        club.status(&path, Some(&club.maya_cookie)).await,
        StatusCode::OK
    );
    assert_eq!(
        club.status(&path, Some(&club.ruth_cookie)).await,
        StatusCode::NOT_FOUND
    );
    // Moving it to a public space keeps its restriction.
    let general = f.space("general", "public", None, 1).await;
    store::move_thread(&f.db, thread, general).await.unwrap();
    f.changed(Change::ThreadChanged {
        thread_id: thread,
        previous_space_id: Some(lounge),
    })
    .await;
    assert_eq!(club.visibility(thread).await, "restricted");
    assert_eq!(club.status(&path, None).await, StatusCode::NOT_FOUND);
    assert_eq!(
        club.status(&path, Some(&club.ruth_cookie)).await,
        StatusCode::NOT_FOUND
    );
    // And moving it into a smaller space narrows it.
    let small = f.space("small", "listed", None, 2).await;
    f.restrict(small, &[group], None).await;
    store::move_thread(&f.db, thread, small).await.unwrap();
    f.changed(Change::ThreadChanged {
        thread_id: thread,
        previous_space_id: Some(general),
    })
    .await;
    assert_eq!(
        club.status(&path, Some(&club.maya_cookie)).await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        club.status(&path, Some(&club.jonah_cookie)).await,
        StatusCode::OK
    );
}

#[sqlx::test]
async fn repliers_must_be_named_readers(pool: PgPool) {
    let club = club(pool).await;
    let f = &club.f;
    let general = f.space("general", "members", None, 0).await;
    let thread = f
        .thread(general, club.ruth, "Plans", "Who is coming?")
        .await
        .thread_id;
    // Jonah and Maya read it; Jonah and Kit are named to reply, but Kit
    // can't read it, so he can't reply either.
    club.restrict(
        thread,
        Some(Grant::listed([], [club.ruth, club.jonah, club.maya])),
        Some(Grant::listed([], [club.ruth, club.jonah, club.kit])),
    )
    .await;
    let reply = |cookie: String| {
        let f = &club.f;
        async move {
            f.submit(
                &format!("/t/{thread}/reply"),
                &[("body", "Count me in.")],
                Some(&cookie),
            )
            .await
        }
    };
    let response = reply(club.jonah_cookie.clone()).await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    let response = reply(club.maya_cookie.clone()).await;
    assert_eq!(response.status, StatusCode::FORBIDDEN);
    assert!(
        response.body.contains("You can’t reply in this thread."),
        "{}",
        response.body
    );
    // Her page has no reply box, but says why.
    let page = f
        .page_as(&thread_path(thread, "plans"), &club.maya_cookie)
        .await;
    assert!(page.contains("You can’t reply in this thread."), "{page}");
    assert!(!page.contains(&format!("action=\"/t/{thread}/reply\"")));
    let kit = f.session(club.kit).await;
    assert_eq!(reply(kit).await.status, StatusCode::NOT_FOUND);
    // The same rule decides who may reply in the content module.
    let target = content::thread_target(f.state.db(), &club.viewer(club.jonah).await, thread)
        .await
        .unwrap()
        .unwrap();
    assert!(writing::may_reply_to(
        &club.viewer(club.jonah).await,
        &target
    ));
    assert!(!writing::may_reply_to(
        &club.viewer(club.maya).await,
        &target
    ));
}

#[sqlx::test]
async fn posts_in_restricted_threads_are_quoted_only_somewhere_as_private(pool: PgPool) {
    let club = club(pool).await;
    let f = &club.f;
    let pals = f.group("pals", "Pals").await;
    let others = f.group("others", "Others").await;
    for g in [pals, others] {
        f.join_group(g, club.jonah).await;
    }
    let general = f.space("general", "members", None, 0).await;
    let same = f.space("same", "listed", None, 1).await;
    f.restrict(same, &[pals], None).await;
    let wider = f.space("wider", "listed", None, 2).await;
    f.restrict(wider, &[pals, others], None).await;
    let thread = f
        .thread(general, club.jonah, "Pals only", "A secret.")
        .await;
    club.restrict(thread.thread_id, Some(Grant::listed([pals], [])), None)
        .await;
    let viewer = club.viewer(club.jonah).await;
    let source = content::post_target(f.state.db(), &viewer, thread.first_post_id)
        .await
        .unwrap()
        .unwrap();
    let destinations: Vec<String> = writing::fork_destinations(&f.state, &viewer, &source)
        .await
        .unwrap()
        .into_iter()
        .map(|c| c.space.slug)
        .collect();
    // Not the members space it is in, nor a space Others can read too:
    // only somewhere that names no more than the thread does.
    assert_eq!(destinations, vec!["same".to_owned()]);
}

#[sqlx::test]
async fn only_those_who_can_read_a_thread_hear_about_it(pool: PgPool) {
    let club = club(pool).await;
    let f = &club.f;
    let general = f.space("general", "members", None, 0).await;
    let thread = f
        .thread(general, club.ruth, "Plans", "Who is coming?")
        .await
        .thread_id;
    parlour::social::set_subscribed(&f.db, club.maya, thread, true)
        .await
        .unwrap();
    club.restrict(thread, Some(Grant::listed([club.friends], [])), None)
        .await;
    let post = f.post(thread, club.jonah, "Me, and @maya and @kit?").await;
    parlour::notify::post_created(&f.state, post).await.unwrap();
    let told: Vec<(i64, String)> = sqlx::query_as(
        "SELECT user_id, kind FROM notifications WHERE post_id = $1 ORDER BY user_id",
    )
    .bind(post)
    .fetch_all(&f.db)
    .await
    .unwrap();
    // Ruth, subscribed as its author, is told; Maya (subscribed) and Kit
    // (mentioned) can't read it, so they aren't.
    assert_eq!(told, vec![(club.ruth, "subscription".to_owned())]);
}

#[sqlx::test]
async fn images_and_post_counts_follow_the_thread(pool: PgPool) {
    let club = club(pool).await;
    let f = &club.f;
    let general = f.space("general", "public", None, 0).await;
    let sha = format!("{:064x}", 7);
    f.exec(&format!(
        "INSERT INTO uploads (sha256, ext, mime, bytes, width, height)
         VALUES ('{sha}', 'png', 'image/png', 1, 1, 1)"
    ))
    .await;
    let thread = f
        .thread(
            general,
            club.ruth,
            "Photos",
            &format!("Look: ![](/uploads/{sha}.png)"),
        )
        .await
        .thread_id;
    let db = f.state.db();
    assert_eq!(
        content::uploads::upload_access(db, &Viewer::Anonymous, &sha, "png")
            .await
            .unwrap(),
        Some(UploadAccess::Public)
    );
    club.restrict(thread, Some(Grant::listed([club.friends], [])), None)
        .await;
    assert_eq!(
        content::uploads::upload_access(db, &Viewer::Anonymous, &sha, "png")
            .await
            .unwrap(),
        None
    );
    let jonah = club.viewer(club.jonah).await;
    let maya = club.viewer(club.maya).await;
    assert_eq!(
        content::uploads::upload_access(db, &jonah, &sha, "png")
            .await
            .unwrap(),
        Some(UploadAccess::Private)
    );
    assert_eq!(
        content::uploads::upload_access(db, &maya, &sha, "png")
            .await
            .unwrap(),
        None
    );
    // Ruth's post counts only for those who can read it.
    f.thread(general, club.ruth, "Open", "Hello.").await;
    for (viewer, expected) in [(&Viewer::Anonymous, 1), (&maya, 1), (&jonah, 2)] {
        assert_eq!(
            content::visible_post_count(db, viewer, club.ruth)
                .await
                .unwrap(),
            expected
        );
    }
    // And lifting the restriction counts it for everyone again.
    club.restrict(thread, None, None).await;
    assert_eq!(
        content::visible_post_count(db, &Viewer::Anonymous, club.ruth)
            .await
            .unwrap(),
        2
    );
}

#[sqlx::test]
async fn the_author_and_the_spaces_administrators_restrict_a_thread(pool: PgPool) {
    let club = club(pool).await;
    let f = &club.f;
    let general = f.space("general", "members", None, 0).await;
    f.set_grant(general, "administer", Grant::listed([], [club.kit]))
        .await;
    let thread = f
        .thread(general, club.ruth, "Plans", "Who is coming?")
        .await
        .thread_id;
    let friends_only = ThreadRestriction {
        read: Some(Grant::listed([club.friends], [])),
        reply: None,
    };
    // Maya can read it, but it isn't hers to restrict.
    let refused = writing::restrict_thread(
        &f.state,
        &club.viewer(club.maya).await,
        thread,
        &friends_only,
    )
    .await
    .unwrap();
    assert!(
        matches!(refused, Err(Refused::NotAllowed(_))),
        "{refused:?}"
    );
    // Ruth, its author, may; and she is always among those it names, as
    // the owner of a list is.
    writing::restrict_thread(
        &f.state,
        &club.viewer(club.ruth).await,
        thread,
        &ThreadRestriction {
            read: Some(Grant::listed([], [club.jonah])),
            reply: Some(Grant::listed([], [])),
        },
    )
    .await
    .unwrap()
    .unwrap();
    let (read_users, reply_users): (Vec<i64>, Vec<i64>) =
        sqlx::query_as("SELECT read_user_ids, reply_user_ids FROM threads WHERE id = $1")
            .bind(thread)
            .fetch_one(&f.db)
            .await
            .unwrap();
    assert_eq!(read_users, vec![club.ruth, club.jonah]);
    assert_eq!(reply_users, vec![club.ruth]);
    // Kit runs the space, so he may change it, even back to the space's
    // audience, though he can't read it now.
    writing::restrict_thread(
        &f.state,
        &club.viewer(club.kit).await,
        thread,
        &ThreadRestriction::default(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(club.visibility(thread).await, "members");
    // A restriction is a list of names: it can't let in "every member".
    let refused = writing::restrict_thread(
        &f.state,
        &club.viewer(club.ruth).await,
        thread,
        &ThreadRestriction {
            read: Some(Grant::members()),
            reply: None,
        },
    )
    .await
    .unwrap();
    assert!(matches!(refused, Err(Refused::Invalid(_))), "{refused:?}");
    // A moderator who doesn't run the space may too, naming what they
    // may name (not Ruth's list, which they aren't on), and it is logged.
    let sam = f.id("SELECT id FROM users WHERE username = 'sam'").await;
    let organisers = f.group("organisers", "Organisers").await;
    let organisers_only = ThreadRestriction {
        read: Some(Grant::listed([organisers], [])),
        reply: None,
    };
    writing::restrict_thread(&f.state, &club.viewer(sam).await, thread, &organisers_only)
        .await
        .unwrap()
        .unwrap();
    let logged: i64 = f
        .id(&format!(
            "SELECT count(*) FROM mod_actions WHERE action = 'restrict_thread' AND target_id = {thread}"
        ))
        .await;
    assert_eq!(logged, 1);
    let log = f.page_as("/mod/log", &club.staff_cookie).await;
    assert!(log.contains("changed who can read or reply in"), "{log}");
}

#[sqlx::test]
async fn a_list_named_by_a_thread_cant_be_deleted(pool: PgPool) {
    let club = club(pool).await;
    let f = &club.f;
    let general = f.space("general", "members", None, 0).await;
    let thread = f
        .thread(general, club.ruth, "Plans", "Who is coming?")
        .await
        .thread_id;
    club.restrict(thread, Some(Grant::listed([club.friends], [])), None)
        .await;
    let response = f
        .submit(
            &format!("/settings/lists/{}/delete", club.friends),
            &[],
            Some(&club.ruth_cookie),
        )
        .await;
    assert!(
        response.body.contains("while 1 thread names it"),
        "{}",
        response.body
    );
    let left: i64 = f
        .id(&format!(
            "SELECT count(*) FROM groups WHERE id = {}",
            club.friends
        ))
        .await;
    assert_eq!(left, 1);
}

#[sqlx::test]
async fn a_feed_nobody_else_can_read_a_thread_in_stays_out_of_sight(pool: PgPool) {
    let club = club(pool).await;
    let f = &club.f;
    let feed = f
        .id(&format!(
            "SELECT id FROM spaces WHERE kind = 'feed' AND owner_user_id = {}",
            club.ruth
        ))
        .await;
    f.set_grant(feed, "read", Grant::public()).await;
    let response = f
        .submit(
            "/c/ruth/new",
            &[("title", "Just friends"), ("body", "Nobody else.")],
            Some(&club.ruth_cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    let thread = f
        .id("SELECT id FROM threads WHERE title = 'Just friends'")
        .await;
    club.restrict(thread, Some(Grant::listed([club.friends], [])), None)
        .await;
    // Visitors and Maya see no feed: it has no thread they can read.
    assert!(!f.page("/spaces").await.contains("href=\"/c/ruth\""));
    assert!(
        !f.page_as("/spaces", &club.maya_cookie)
            .await
            .contains("href=\"/c/ruth\"")
    );
    // Jonah, who can read the thread, does.
    assert!(
        f.page_as("/spaces", &club.jonah_cookie)
            .await
            .contains("href=\"/c/ruth\"")
    );
    // Opening the thread to the feed's audience brings it into sight,
    // cached pages included.
    club.restrict(thread, None, None).await;
    assert!(f.page("/spaces").await.contains("href=\"/c/ruth\""));
    // And restricting it again takes it away.
    club.restrict(thread, Some(Grant::listed([club.friends], [])), None)
        .await;
    assert!(!f.page("/spaces").await.contains("href=\"/c/ruth\""));
}

#[sqlx::test]
async fn the_moderation_log_keeps_restricted_threads_private(pool: PgPool) {
    let club = club(pool).await;
    let f = &club.f;
    let general = f.space("general", "public", None, 0).await;
    let lock = |thread: i64| {
        let staff = club.staff_cookie.clone();
        async move {
            let response = f
                .submit(
                    &format!("/mod/t/{thread}"),
                    &[("action", "lock"), ("reason", "Calm down")],
                    Some(&staff),
                )
                .await;
            assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
            sqlx::query_scalar::<_, bool>(
                "SELECT public FROM mod_actions WHERE action = 'lock_thread' AND target_id = $1",
            )
            .bind(thread)
            .fetch_one(&f.db)
            .await
            .unwrap()
        }
    };
    // Locking a restricted thread in a public space isn't for the public
    // log.
    let quiet = f
        .thread(general, club.ruth, "Quiet word", "Between us.")
        .await
        .thread_id;
    club.restrict(quiet, Some(Grant::listed([club.friends], [])), None)
        .await;
    assert!(!lock(quiet).await);
    // An entry made while a thread was public names it only to those who
    // can read it once it is restricted.
    let loud = f
        .thread(general, club.ruth, "Loud word", "For everyone, then.")
        .await
        .thread_id;
    assert!(lock(loud).await);
    assert!(f.page("/moderation").await.contains("Loud word"));
    club.restrict(loud, Some(Grant::listed([club.friends], [])), None)
        .await;
    for cookie in [None, Some(club.maya_cookie.as_str())] {
        let log = f.get_as("/moderation", cookie).await.body;
        assert!(
            !log.contains("Loud word") && !log.contains("Quiet word"),
            "{log}"
        );
    }
    let log = f.get_as("/moderation", Some(&club.jonah_cookie)).await.body;
    assert!(log.contains("Loud word"), "{log}");
}

/// The id of a member's feed.
async fn feed_of(f: &Forum, owner: i64) -> i64 {
    f.id(&format!(
        "SELECT id FROM spaces WHERE kind = 'feed' AND owner_user_id = {owner}"
    ))
    .await
}

#[sqlx::test]
async fn a_feeds_owner_is_named_in_every_restriction_there(pool: PgPool) {
    let club = club(pool).await;
    let f = &club.f;
    // Ruth opens her feed to every member's threads: a guestbook.
    let feed = feed_of(f, club.ruth).await;
    f.set_grant(feed, "start", Grant::members()).await;
    let thread = f
        .thread(feed, club.maya, "Hello Ruth", "Signing your book.")
        .await
        .thread_id;
    // Maya narrows it to herself, but it is in Ruth's feed, and the owner
    // holds every permission on it (DESIGN.md §2, §3.2).
    writing::restrict_thread(
        &f.state,
        &club.viewer(club.maya).await,
        thread,
        &ThreadRestriction {
            read: Some(Grant::listed([], [club.maya])),
            reply: Some(Grant::listed([], [club.maya])),
        },
    )
    .await
    .unwrap()
    .unwrap();
    let ruth = club.viewer(club.ruth).await;
    let target = content::thread_target(f.state.db(), &ruth, thread)
        .await
        .unwrap()
        .expect("Ruth reads every thread in her feed");
    assert!(target.readable());
    let space = target.space().unwrap();
    assert!(target.restriction.may_reply(&space.rules, &ruth, false));
    // Kit isn't named, and reads nothing.
    assert!(
        content::thread_target(f.state.db(), &club.viewer(club.kit).await, thread)
            .await
            .unwrap()
            .is_none_or(|t| !t.readable())
    );
    // The store can't leave her out either.
    store::set_thread_restriction(
        f.state.db(),
        thread,
        &ThreadRestriction {
            read: Some(Grant::listed([], [club.kit])),
            reply: Some(Grant::listed([], [])),
        },
    )
    .await
    .unwrap();
    let (read_users, reply_users): (Vec<i64>, Vec<i64>) =
        sqlx::query_as("SELECT read_user_ids, reply_user_ids FROM threads WHERE id = $1")
            .bind(thread)
            .fetch_one(&f.db)
            .await
            .unwrap();
    assert!(read_users.contains(&club.ruth), "{read_users:?}");
    assert!(reply_users.contains(&club.ruth), "{reply_users:?}");
    // Nor does a restricted thread moved into her feed.
    let general = f.space("general", "members", None, 0).await;
    let moved = f
        .thread(general, club.maya, "Moving in", "Soon to be Ruth's.")
        .await
        .thread_id;
    club.restrict(moved, Some(Grant::listed([], [club.maya])), None)
        .await;
    store::move_thread(f.state.db(), moved, feed)
        .await
        .unwrap()
        .unwrap();
    let read_users: Vec<i64> =
        sqlx::query_scalar("SELECT read_user_ids FROM threads WHERE id = $1")
            .bind(moved)
            .fetch_one(&f.db)
            .await
            .unwrap();
    assert!(read_users.contains(&club.ruth), "{read_users:?}");
}

#[sqlx::test]
async fn a_deleted_thread_doesnt_hold_on_to_a_list_or_group(pool: PgPool) {
    let club = club(pool).await;
    let f = &club.f;
    let general = f.space("general", "members", None, 0).await;
    let organisers = f.group("organisers", "Organisers").await;
    let thread = f
        .thread(general, club.ruth, "Plans", "Who is coming?")
        .await
        .thread_id;
    club.restrict(
        thread,
        Some(Grant::listed([club.friends, organisers], [])),
        Some(Grant::listed([organisers], [])),
    )
    .await;
    f.exec(&format!(
        "UPDATE threads SET deleted_at = now() WHERE id = {thread}"
    ))
    .await;
    // Nobody can change a deleted thread's restriction, so it doesn't
    // keep the list from being deleted.
    let page = f
        .page_as(
            &format!("/settings/lists/{}", club.friends),
            &club.ruth_cookie,
        )
        .await;
    assert!(!page.contains("names it"), "{page}");
    assert_eq!(
        parlour::lists::threads_naming(f.state.db(), club.friends)
            .await
            .unwrap(),
        0
    );
    let response = f
        .submit(
            &format!("/settings/lists/{}/delete", club.friends),
            &[],
            Some(&club.ruth_cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    assert_eq!(
        parlour::groups::delete(f.state.db(), organisers)
            .await
            .unwrap(),
        Ok(true)
    );
    // The names go from the deleted thread, which, restored, is still
    // restricted, to the names left: only ever narrower.
    let (read_groups, reply_groups, read_users): (Vec<i64>, Vec<i64>, Vec<i64>) = sqlx::query_as(
        "SELECT read_group_ids, reply_group_ids, read_user_ids FROM threads WHERE id = $1",
    )
    .bind(thread)
    .fetch_one(&f.db)
    .await
    .unwrap();
    assert!(read_groups.is_empty() && reply_groups.is_empty());
    assert!(read_users.is_empty());
    f.exec(&format!(
        "UPDATE threads SET deleted_at = NULL WHERE id = {thread}"
    ))
    .await;
    assert_eq!(club.visibility(thread).await, "restricted");
    // A live thread still holds on to what it names.
    let live = f
        .thread(general, club.ruth, "More plans", "Still coming?")
        .await
        .thread_id;
    let book_club = parlour::lists::create(f.state.db(), club.ruth, "Book club")
        .await
        .unwrap()
        .unwrap();
    club.restrict(live, Some(Grant::listed([book_club], [])), None)
        .await;
    assert_eq!(
        parlour::lists::delete(f.state.db(), club.ruth, book_club)
            .await
            .unwrap(),
        parlour::lists::Change::Refused(parlour::lists::named_in(0, 1))
    );
}

#[sqlx::test]
async fn a_restriction_names_only_groups_and_people_the_editor_may(pool: PgPool) {
    let club = club(pool).await;
    let f = &club.f;
    let general = f.space("general", "members", None, 0).await;
    f.set_grant(general, "administer", Grant::listed([], [club.kit]))
        .await;
    let inner = parlour::lists::create(f.state.db(), club.maya, "Inner circle")
        .await
        .unwrap()
        .unwrap();
    let organisers = f.group("organisers", "Organisers").await;
    let thread = f
        .thread(general, club.ruth, "Plans", "Who is coming?")
        .await
        .thread_id;
    let ruth = club.viewer(club.ruth).await;
    let read = |groups: Vec<i64>, users: Vec<i64>| ThreadRestriction {
        read: Some(Grant::listed(groups, users)),
        reply: None,
    };
    // Maya's list is hidden from Ruth; a group that doesn't exist, and
    // someone who doesn't, can't be named either.
    let deleted = f.member("gone", "user", None).await;
    f.exec(&format!(
        "UPDATE users SET deleted_at = now() WHERE id = {deleted}"
    ))
    .await;
    for restriction in [
        read(vec![inner], vec![]),
        read(vec![999_999], vec![]),
        read(vec![], vec![999_999]),
        read(vec![], vec![deleted]),
        ThreadRestriction {
            read: None,
            reply: Some(Grant::listed([inner], [])),
        },
    ] {
        let refused = writing::restrict_thread(&f.state, &ruth, thread, &restriction)
            .await
            .unwrap();
        assert!(
            matches!(refused, Err(Refused::Invalid(_))),
            "{restriction:?}: {refused:?}"
        );
    }
    let named: Option<Vec<i64>> =
        sqlx::query_scalar("SELECT read_group_ids FROM threads WHERE id = $1")
            .bind(thread)
            .fetch_one(&f.db)
            .await
            .unwrap();
    assert_eq!(named, None);
    // Her own list, a site group and live members are hers to name.
    writing::restrict_thread(
        &f.state,
        &ruth,
        thread,
        &read(vec![club.friends, organisers], vec![club.maya]),
    )
    .await
    .unwrap()
    .unwrap();
    // Jonah is on Ruth's Friends, so he may name it too.
    let jonahs = f
        .thread(general, club.jonah, "Jonah's", "Mine.")
        .await
        .thread_id;
    writing::restrict_thread(
        &f.state,
        &club.viewer(club.jonah).await,
        jonahs,
        &read(vec![club.friends], vec![]),
    )
    .await
    .unwrap()
    .unwrap();
    // Kit runs the space and doesn't know Ruth's Friends, but may keep it
    // where it is named already, while changing the rest (naming for
    // replying no more than for reading: repliers must be readers).
    let kit = club.viewer(club.kit).await;
    writing::restrict_thread(
        &f.state,
        &kit,
        thread,
        &ThreadRestriction {
            read: Some(Grant::listed([club.friends, organisers], [club.maya])),
            reply: Some(Grant::listed([organisers], [])),
        },
    )
    .await
    .unwrap()
    .unwrap();
    // But not put it somewhere it isn't.
    let refused = writing::restrict_thread(
        &f.state,
        &kit,
        thread,
        &ThreadRestriction {
            read: Some(Grant::listed([club.friends, organisers], [])),
            reply: Some(Grant::listed([club.friends], [])),
        },
    )
    .await
    .unwrap();
    assert!(matches!(refused, Err(Refused::Invalid(_))), "{refused:?}");
    // And the database refuses a group that doesn't exist.
    assert!(
        store::set_thread_restriction(f.state.db(), thread, &read(vec![999_999], vec![]))
            .await
            .is_err()
    );
}

#[sqlx::test]
async fn a_busy_feed_visitors_cant_read_clears_no_cached_front_page(pool: PgPool) {
    let club = club(pool).await;
    let f = &club.f;
    // Ruth's feed is read by members, as feeds are made.
    let feed = feed_of(f, club.ruth).await;
    let mut posts = Vec::new();
    for title in ["One", "Two", "Three"] {
        posts.push(
            f.thread(feed, club.ruth, title, "In my feed.")
                .await
                .first_post_id,
        );
    }
    let thread = f.id("SELECT id FROM threads WHERE title = 'Three'").await;
    for change in [
        Change::PostCreated {
            post_id: *posts.last().unwrap(),
        },
        Change::ThreadChanged {
            thread_id: thread,
            previous_space_id: None,
        },
    ] {
        let tags = parlour::changes::tags_for(f.state.db(), change)
            .await
            .unwrap();
        assert!(
            !tags.contains(&parlour::cache::Tag::Site),
            "{change:?}: {tags:?}"
        );
    }
    // Opened to visitors, its first public thread does clear it.
    f.set_grant(feed, "read", Grant::public()).await;
    let tags = parlour::changes::tags_for(
        f.state.db(),
        Change::ThreadChanged {
            thread_id: thread,
            previous_space_id: None,
        },
    )
    .await
    .unwrap();
    assert!(!tags.contains(&parlour::cache::Tag::Site), "{tags:?}");
    f.exec("UPDATE threads SET deleted_at = now() WHERE title IN ('One', 'Two')")
        .await;
    let tags = parlour::changes::tags_for(
        f.state.db(),
        Change::ThreadChanged {
            thread_id: thread,
            previous_space_id: None,
        },
    )
    .await
    .unwrap();
    assert!(tags.contains(&parlour::cache::Tag::Site), "{tags:?}");
}
