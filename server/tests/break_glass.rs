//! Break glass, first step (DESIGN.md §1 principle 3, §3.1, §6, §8,
//! milestone 15): staff read only what their own memberships let them,
//! like any member; they still run every space, acting on what they can't
//! read without being shown it; and anything more they read only under a
//! `staff_access` grant, which records every page it shows in the
//! append-only `staff_access_views`.

mod common;

use axum::http::StatusCode;
use common::{Forum, Response};
use parlour::permissions::{Grant, ThreadRestriction};
use parlour::staff_access::{self, NewAccess, Scope};
use parlour::store::{self, NewConversation};
use sqlx::PgPool;

/// Ruth and Jonah are members, Kit is in the Organisers group, Sam is a
/// moderator and Ada an admin, both in no group.
///
/// - General: members read and post. "Lunch plans" (open to every
///   member) and "Quiet word", restricted to Ruth and Jonah.
/// - Circle: only the Organisers read it. "Circle secret".
/// - Staffroom: read by staff, which names Sam and Ada by their role.
///   "Staff matters".
/// - Ruth and Jonah's conversation, which Jonah has reported.
///
/// Every title and post is a word of its own, so that a page showing it
/// can be found.
struct Club {
    f: Forum,
    ruth: i64,
    sam: i64,
    ada: i64,
    organisers: i64,
    general: i64,
    circle: i64,
    circle_two: i64,
    lunch: i64,
    quiet: i64,
    quiet_post: i64,
    secret: i64,
    secret_post: i64,
    staff_thread: i64,
    dm: i64,
    dm_post: i64,
    ruth_cookie: String,
    jonah_cookie: String,
    sam_cookie: String,
    ada_cookie: String,
}

async fn club(pool: PgPool) -> Club {
    let f = Forum::new(pool);
    f.no_new_members().await;
    let ruth = f.member("ruth", "user", None).await;
    let jonah = f.member("jonah", "user", None).await;
    let kit = f.member("kit", "user", None).await;
    let sam = f.member("sam", "moderator", None).await;
    let ada = f.member("ada", "admin", None).await;
    let organisers = f.group("organisers", "Organisers").await;
    f.join_group(organisers, kit).await;
    let general = f.space("general", "members", None, 0).await;
    let circle = f.space("circle", "members", None, 1).await;
    f.restrict(circle, &[organisers], Some(&[organisers])).await;
    let circle_two = f.space("circle-two", "members", None, 2).await;
    f.restrict(circle_two, &[organisers], Some(&[organisers]))
        .await;
    let staffroom = f.space("staffroom", "staff", None, 3).await;
    let lunch = f
        .thread(general, ruth, "Lunchplanstitle", "Lunchplansbody findme")
        .await
        .thread_id;
    let quiet = f
        .thread(general, ruth, "Quietwordtitle", "Quietwordbody findme")
        .await;
    assert!(
        store::set_thread_restriction(
            &f.db,
            quiet.thread_id,
            &ThreadRestriction {
                read: Some(Grant::listed([], [ruth, jonah])),
                reply: None,
            },
        )
        .await
        .unwrap()
    );
    let secret = f
        .thread(circle, kit, "Circlesecrettitle", "Circlesecretbody findme")
        .await;
    let staff_thread = f
        .thread(
            staffroom,
            ada,
            "Staffmatterstitle",
            "Staffmattersbody findme",
        )
        .await
        .thread_id;
    let dm = store::create_conversation(
        &f.db,
        &NewConversation {
            author_id: ruth,
            title: "Dmtitle".into(),
            recipient_ids: vec![jonah],
            body_md: "Dmbody findme".into(),
            created_at: None,
        },
    )
    .await
    .unwrap();
    let jonah_cookie = f.session(jonah).await;
    let response = f
        .submit(
            &format!("/p/{}/report", dm.first_post_id),
            &[("reason", "harassment"), ("details", "")],
            Some(&jonah_cookie),
        )
        .await;
    assert!(response.body.contains("Thank you"), "{}", response.body);
    Club {
        ruth_cookie: f.session(ruth).await,
        jonah_cookie,
        sam_cookie: f.session(sam).await,
        ada_cookie: f.session(ada).await,
        f,
        ruth,
        sam,
        ada,
        organisers,
        general,
        circle,
        circle_two,
        lunch,
        quiet: quiet.thread_id,
        quiet_post: quiet.first_post_id,
        secret: secret.thread_id,
        secret_post: secret.first_post_id,
        staff_thread,
        dm: dm.thread_id,
        dm_post: dm.first_post_id,
    }
}

/// The words only a grant may show staff: everything in the Circle, the
/// quiet word and the conversation.
const CLOSED: [&str; 6] = [
    "Circlesecrettitle",
    "Circlesecretbody",
    "Quietwordtitle",
    "Quietwordbody",
    "Dmtitle",
    "Dmbody",
];

async fn count(f: &Forum, sql: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(sql.to_owned()))
        .fetch_one(&f.db)
        .await
        .unwrap()
}

async fn slug(f: &Forum, thread: i64) -> String {
    sqlx::query_scalar("SELECT slug FROM threads WHERE id = $1")
        .bind(thread)
        .fetch_one(&f.db)
        .await
        .unwrap()
}

async fn thread_path(f: &Forum, thread: i64) -> String {
    format!("/t/{thread}/{}", slug(f, thread).await)
}

#[track_caller]
fn assert_status(response: &Response, status: StatusCode) {
    assert_eq!(response.status, status, "{}", response.body);
}

/// Submits a form and expects to be sent on with a 303.
async fn ok(f: &Forum, path: &str, fields: &[(&str, &str)], cookie: &str) -> Response {
    let response = f.submit(path, fields, Some(cookie)).await;
    assert_status(&response, StatusCode::SEE_OTHER);
    response
}

fn shows_none(path: &str, body: &str, words: &[&str]) {
    for word in words {
        assert!(!body.contains(word), "{path} shows {word}:\n{body}");
    }
}

/// Every view recorded so far, as (grant, path).
async fn views(f: &Forum) -> Vec<(i64, String)> {
    sqlx::query_as("SELECT access_id, path FROM staff_access_views ORDER BY viewed_at, access_id")
        .fetch_all(&f.db)
        .await
        .unwrap()
}

#[sqlx::test]
async fn staff_read_what_their_memberships_let_them(pool: PgPool) {
    let c = club(pool).await;
    let f = &c.f;
    let secret = thread_path(f, c.secret).await;
    let quiet = thread_path(f, c.quiet).await;
    let lunch = thread_path(f, c.lunch).await;
    let staff_thread = thread_path(f, c.staff_thread).await;
    for cookie in [&c.sam_cookie, &c.ada_cookie] {
        // What any member reads, and what a grant to staff lets them read.
        for path in [&lunch, &staff_thread] {
            f.page_as(path, cookie).await;
        }
        f.page_as("/c/staffroom", cookie).await;
        // Not a space whose grant leaves them out, a thread whose
        // restriction does, or a conversation they aren't in, reported or
        // not.
        for path in [
            secret.clone(),
            format!("/t/{}", c.secret),
            quiet.clone(),
            format!("/t/{}", c.quiet),
            "/c/circle".to_owned(),
            format!("/messages/{}", c.dm),
            format!("/p/{}", c.secret_post),
            format!("/p/{}", c.dm_post),
            format!("/mod/p/{}", c.secret_post),
            format!("/mod/p/{}", c.dm_post),
        ] {
            let response = f.get_as(&path, Some(cookie)).await;
            assert_status(&response, StatusCode::NOT_FOUND);
            shows_none(&path, &response.body, &CLOSED);
        }
        // Nor anywhere else.
        for path in [
            "/",
            "/latest",
            "/spaces",
            "/c/general",
            "/search?q=findme",
            "/mod/queue",
            "/mod/log",
            "/u/ruth",
            "/u/kit",
        ] {
            let body = f.page_as(path, cookie).await;
            shows_none(path, &body, &CLOSED);
        }
    }
    // The queue still says a message was reported, and by whom, but not
    // what it says.
    let queue = f.page_as("/mod/queue", &c.sam_cookie).await;
    assert!(queue.contains("jonah"), "{queue}");
    // The people the grants name read as before.
    let page = f.page_as(&quiet, &c.ruth_cookie).await;
    assert!(page.contains("Quietwordbody"), "{page}");
    let page = f
        .page_as(&format!("/messages/{}", c.dm), &c.jonah_cookie)
        .await;
    assert!(page.contains("Dmbody"), "{page}");
    // A moderator put in the Organisers reads the Circle like anyone in it.
    f.join_group(c.organisers, c.sam).await;
    let page = f.page_as(&secret, &c.sam_cookie).await;
    assert!(page.contains("Circlesecretbody"), "{page}");
    // And nothing of this was a read under a grant.
    assert_eq!(count(f, "SELECT count(*) FROM staff_access_views").await, 0);
}

#[sqlx::test]
async fn staff_run_spaces_and_threads_they_cant_read(pool: PgPool) {
    let c = club(pool).await;
    let f = &c.f;
    let cookie = &c.sam_cookie;
    // The space's settings: its name and grants, never its threads.
    let settings = f.page_as("/c/circle/settings", cookie).await;
    assert!(settings.contains("Circle"), "{settings}");
    shows_none("/c/circle/settings", &settings, &CLOSED);
    // The admins' list of spaces has every space.
    let spaces = f.page_as("/admin/spaces", &c.ada_cookie).await;
    assert!(spaces.contains("/c/circle/settings"), "{spaces}");
    // A thread's moderation page: what it is and where, not what it says.
    let mod_path = format!("/mod/t/{}", c.secret);
    let page = f.page_as(&mod_path, cookie).await;
    assert!(page.contains("You can’t read this thread"), "{page}");
    assert!(page.contains("Thread № "), "{page}");
    assert!(page.contains("Circle"), "{page}");
    shows_none(&mod_path, &page, &CLOSED);
    // Lock, pin, rename.
    for (action, title) in [("lock", ""), ("pin", ""), ("rename", "Renamedtitle")] {
        ok(
            f,
            &mod_path,
            &[("action", action), ("title", title), ("reason", "Tidying")],
            cookie,
        )
        .await;
        let page = f.page_as(&mod_path, cookie).await;
        shows_none(&mod_path, &page, &CLOSED);
        assert!(!page.contains("Renamedtitle"), "{page}");
    }
    let (locked, pinned, title): (bool, bool, String) =
        sqlx::query_as("SELECT locked, pinned, title FROM threads WHERE id = $1")
            .bind(c.secret)
            .fetch_one(&f.db)
            .await
            .unwrap();
    assert!(locked && pinned);
    assert_eq!(title, "Renamedtitle");
    // The log says what they did, not what it was called.
    let log = f.page_as("/mod/log", cookie).await;
    shows_none("/mod/log", &log, &CLOSED);
    assert!(!log.contains("Renamedtitle"), "{log}");
    // Moving it into a space they read would let them read it: refused.
    let response = f
        .submit(
            &mod_path,
            &[("action", "move"), ("space", "general"), ("reason", "Tidy")],
            Some(cookie),
        )
        .await;
    assert_status(&response, StatusCode::BAD_REQUEST);
    assert!(
        response.body.contains("moving it there would let you"),
        "{}",
        response.body
    );
    // Moving it to another space they can't read is how a space is folded
    // into another.
    ok(
        f,
        &mod_path,
        &[
            ("action", "move"),
            ("space", "circle-two"),
            ("reason", "Merging"),
        ],
        cookie,
    )
    .await;
    assert_eq!(
        count(
            f,
            &format!("SELECT space_id FROM threads WHERE id = {}", c.secret)
        )
        .await,
        c.circle_two
    );
    // Deleting it, and restoring it from a page that still shows nothing
    // of it.
    ok(
        f,
        &mod_path,
        &[("action", "delete"), ("reason", "Spam")],
        cookie,
    )
    .await;
    let page = f.page_as(&mod_path, cookie).await;
    assert!(page.contains("Restore the thread"), "{page}");
    shows_none(&mod_path, &page, &CLOSED);
    assert!(!page.contains("Renamedtitle"), "{page}");
    ok(
        f,
        &mod_path,
        &[("action", "restore"), ("reason", "Not spam")],
        cookie,
    )
    .await;
    assert_eq!(
        count(
            f,
            &format!(
                "SELECT count(*) FROM threads WHERE id = {} AND deleted_at IS NULL",
                c.secret
            )
        )
        .await,
        1
    );
    // A restricted thread in a space they read, likewise.
    let quiet_mod = format!("/mod/t/{}", c.quiet);
    let page = f.page_as(&quiet_mod, cookie).await;
    shows_none(&quiet_mod, &page, &CLOSED);
    ok(
        f,
        &quiet_mod,
        &[("action", "lock"), ("reason", "Heated")],
        cookie,
    )
    .await;
    // Archiving the space they can't read.
    ok(f, "/c/circle/archive", &[], cookie).await;
    assert_eq!(
        count(
            f,
            &format!(
                "SELECT count(*) FROM spaces WHERE id = {} AND archived_at IS NOT NULL",
                c.circle
            )
        )
        .await,
        1
    );
    // Changing its read grant so that it would let them read it would be
    // breaking glass by another way: refused.
    let response = f
        .submit(
            "/c/circle/settings",
            &[
                ("name", "Circle"),
                ("slug", "circle"),
                ("description", ""),
                ("read_level", "members"),
                ("reply_level", "listed"),
                ("start_level", "listed"),
                ("administer_level", "listed"),
                ("section", ""),
            ],
            Some(cookie),
        )
        .await;
    assert_status(&response, StatusCode::BAD_REQUEST);
    assert!(
        response.body.contains("would let you read it"),
        "{}",
        response.body
    );
    // Nothing they did was a read under a grant.
    assert_eq!(count(f, "SELECT count(*) FROM staff_access_views").await, 0);
}

#[sqlx::test]
async fn the_access_log_is_append_only(pool: PgPool) {
    let c = club(pool).await;
    let f = &c.f;
    let access = staff_access::open(
        &f.db,
        &NewAccess {
            actor_id: c.sam,
            scope: Scope::Thread(c.quiet),
            report_id: None,
            reason: "Checking a complaint",
        },
    )
    .await
    .unwrap();
    f.page_as(&thread_path(f, c.quiet).await, &c.sam_cookie)
        .await;
    assert_eq!(staff_access::views(&f.db, access).await.unwrap().len(), 1);
    for sql in [
        "UPDATE staff_access SET expires_at = now() + interval '1 year'",
        "UPDATE staff_access SET reason = 'Something else'",
        "DELETE FROM staff_access",
        "TRUNCATE staff_access CASCADE",
        "UPDATE staff_access_views SET path = '/'",
        "DELETE FROM staff_access_views",
        "TRUNCATE staff_access_views",
        // Taking out what a row points at would take the row with it.
        "DELETE FROM users WHERE username = 'sam'",
    ] {
        let result = sqlx::raw_sql(sqlx::AssertSqlSafe(sql)).execute(&f.db).await;
        assert!(result.is_err(), "{sql} was allowed");
    }
    assert_eq!(count(f, "SELECT count(*) FROM staff_access").await, 1);
    assert_eq!(count(f, "SELECT count(*) FROM staff_access_views").await, 1);
    // The application's role holds no right to change or remove either.
    assert_eq!(
        count(
            f,
            "SELECT count(*) FROM pg_class c, aclexplode(c.relacl) a
             WHERE c.relname IN ('staff_access', 'staff_access_views')
               AND a.privilege_type IN ('UPDATE', 'DELETE', 'TRUNCATE')"
        )
        .await,
        0
    );
    assert_eq!(
        count(
            f,
            "SELECT count(*) FROM pg_class c, aclexplode(c.relacl) a
             WHERE c.relname IN ('staff_access', 'staff_access_views')
               AND a.grantee = (SELECT oid FROM pg_roles WHERE rolname = current_user)
               AND a.privilege_type IN ('SELECT', 'INSERT')"
        )
        .await,
        4
    );
    // An empty space someone broke glass on keeps its record: it can be
    // archived but not deleted.
    let empty = f.space("empty", "members", None, 9).await;
    f.restrict(empty, &[c.organisers], None).await;
    staff_access::open(
        &f.db,
        &NewAccess {
            actor_id: c.ada,
            scope: Scope::Space(empty),
            report_id: None,
            reason: "Looking",
        },
    )
    .await
    .unwrap();
    let response = f.submit("/c/empty/delete", &[], Some(&c.ada_cookie)).await;
    assert_status(&response, StatusCode::BAD_REQUEST);
    assert!(response.body.contains("broke glass"), "{}", response.body);
}

#[sqlx::test]
async fn pages_shown_under_a_grant_are_recorded(pool: PgPool) {
    let c = club(pool).await;
    let f = &c.f;
    let quiet = thread_path(f, c.quiet).await;
    let secret = thread_path(f, c.secret).await;
    let open = |scope, actor_id| {
        let db = f.db.clone();
        async move {
            staff_access::open(
                &db,
                &NewAccess {
                    actor_id,
                    scope,
                    report_id: None,
                    reason: "A complaint",
                },
            )
            .await
            .unwrap()
        }
    };
    let on_quiet = open(Scope::Thread(c.quiet), c.sam).await;
    // The grant is loaded with the session: the thread's page opens, and
    // shows it, and the view is recorded against the page asked for.
    let page = f.page_as(&quiet, &c.sam_cookie).await;
    assert!(page.contains("Quietwordbody"), "{page}");
    assert_eq!(views(f).await, vec![(on_quiet, quiet.clone())]);
    // It says so, and is for reading: nothing to subscribe or react with.
    assert!(page.contains("because you broke glass"), "{page}");
    assert!(!page.contains("/subscribe\""), "{page}");
    assert!(!page.contains("reaction-form"), "{page}");
    // Every page of it, every time.
    let page_one = format!("{quiet}?page=1");
    let response = f.get_as(&page_one, Some(&c.sam_cookie)).await;
    assert_status(&response, StatusCode::PERMANENT_REDIRECT);
    f.page_as(&quiet, &c.sam_cookie).await;
    assert_eq!(
        views(f).await,
        vec![(on_quiet, quiet.clone()), (on_quiet, quiet.clone())]
    );
    // Its short address leads there, showing nothing itself.
    let response = f
        .get_as(&format!("/t/{}", c.quiet), Some(&c.sam_cookie))
        .await;
    assert_status(&response, StatusCode::PERMANENT_REDIRECT);
    assert_eq!(response.location(), quiet);
    // Only that thread: not the Circle, not the conversation, and nowhere
    // but its own page.
    for path in [
        secret.clone(),
        "/c/circle".to_owned(),
        format!("/messages/{}", c.dm),
    ] {
        assert_status(
            &f.get_as(&path, Some(&c.sam_cookie)).await,
            StatusCode::NOT_FOUND,
        );
    }
    for path in ["/latest", "/c/general", "/search?q=findme"] {
        let body = f.page_as(path, &c.sam_cookie).await;
        shows_none(path, &body, &CLOSED);
    }
    assert_eq!(views(f).await.len(), 2);
    // Nobody else reads under Sam's grant: not Ada, nor Ruth's own pages
    // record anything.
    assert_status(
        &f.get_as(&quiet, Some(&c.ada_cookie)).await,
        StatusCode::NOT_FOUND,
    );
    f.page_as(&quiet, &c.ruth_cookie).await;
    assert_eq!(views(f).await.len(), 2);

    // A grant on the Circle opens the space's page and every thread in
    // it, restricted ones included, each recorded.
    let tucked = f
        .thread(c.circle, c.ruth, "Tuckedtitle", "Tuckedbody findme")
        .await;
    assert!(
        store::set_thread_restriction(
            &f.db,
            tucked.thread_id,
            &ThreadRestriction {
                read: Some(Grant::listed([], [c.ruth])),
                reply: None,
            },
        )
        .await
        .unwrap()
    );
    let on_circle = open(Scope::Space(c.circle), c.sam).await;
    let space = f.page_as("/c/circle", &c.sam_cookie).await;
    assert!(space.contains("Circlesecrettitle"), "{space}");
    assert!(space.contains("because you broke glass"), "{space}");
    assert!(!space.contains("/c/circle/follow"), "{space}");
    assert!(space.contains("Tuckedtitle"), "{space}");
    let tucked_path = thread_path(f, tucked.thread_id).await;
    for path in [&secret, &tucked_path] {
        let page = f.page_as(path, &c.sam_cookie).await;
        assert!(page.contains("body"), "{page}");
    }
    assert_eq!(
        views(f).await[2..],
        [
            (on_circle, "/c/circle".to_owned()),
            (on_circle, secret.clone()),
            (on_circle, tucked_path.clone()),
        ]
    );
    // They can read it, but a grant opens it to read, not to post in.
    let page = f.page_as(&secret, &c.sam_cookie).await;
    assert!(!page.contains("name=\"body\""), "{page}");
    let response = f
        .submit(
            &format!("/t/{}/reply", c.secret),
            &[("body", "Me too")],
            Some(&c.sam_cookie),
        )
        .await;
    assert!(
        response.status == StatusCode::NOT_FOUND || response.status == StatusCode::FORBIDDEN,
        "{response:?}"
    );

    // A grant on a conversation opens it, read-only, with a notice.
    let on_dm = open(Scope::Thread(c.dm), c.sam).await;
    let dm = format!("/messages/{}", c.dm);
    let page = f.page_as(&dm, &c.sam_cookie).await;
    assert!(page.contains("Dmbody"), "{page}");
    assert!(page.contains("You aren’t in this conversation"), "{page}");
    assert_eq!(views(f).await.last().unwrap(), &(on_dm, dm.clone()));

    // A grant that has run out opens nothing, and records nothing.
    f.exec(&format!(
        "ALTER TABLE staff_access DISABLE TRIGGER staff_access_append_only;
         UPDATE staff_access SET created_at = now() - interval '4 days',
                                 expires_at = now() - interval '1 day' WHERE id = {on_quiet};
         ALTER TABLE staff_access ENABLE TRIGGER staff_access_append_only;"
    ))
    .await;
    let before = views(f).await.len();
    assert_status(
        &f.get_as(&quiet, Some(&c.sam_cookie)).await,
        StatusCode::NOT_FOUND,
    );
    assert_eq!(views(f).await.len(), before);

    // Nor does one held by someone who is no longer staff.
    f.exec(&format!(
        "UPDATE users SET role = 'user' WHERE id = {}",
        c.sam
    ))
    .await;
    for path in [&secret, &dm] {
        assert_status(
            &f.get_as(path, Some(&c.sam_cookie)).await,
            StatusCode::NOT_FOUND,
        );
    }
    assert_eq!(views(f).await.len(), before);
    let _ = c.general;
}

/// The guarantee itself: with grants open on a thread, a space and a
/// conversation, every page a moderator can ask for that shows anything
/// only a grant lets them read recorded a view, against that very page.
#[sqlx::test]
async fn every_page_showing_granted_text_is_recorded(pool: PgPool) {
    let c = club(pool).await;
    let f = &c.f;
    for scope in [
        Scope::Thread(c.quiet),
        Scope::Space(c.circle),
        Scope::Thread(c.dm),
    ] {
        staff_access::open(
            &f.db,
            &NewAccess {
                actor_id: c.sam,
                scope,
                report_id: None,
                reason: "A complaint",
            },
        )
        .await
        .unwrap();
    }
    // Something in each place that mentions the moderator, so that their
    // notifications have something to show.
    for (thread, author) in [(c.quiet, c.ruth), (c.dm, c.ruth)] {
        f.post(thread, author, "Asking @sam Mentionbody findme")
            .await;
    }
    let mut paths = vec![
        "/".to_owned(),
        "/latest".to_owned(),
        "/latest/front".to_owned(),
        "/spaces".to_owned(),
        "/spaces?archived=1".to_owned(),
        "/c/general".to_owned(),
        "/c/circle".to_owned(),
        "/c/circle/audience".to_owned(),
        "/c/circle/settings".to_owned(),
        "/c/staffroom".to_owned(),
        "/notifications".to_owned(),
        "/messages".to_owned(),
        format!("/messages/{}", c.dm),
        format!("/messages/{}/unread", c.dm),
        "/mod/queue".to_owned(),
        "/mod/log".to_owned(),
        "/moderation".to_owned(),
        "/u/ruth".to_owned(),
        "/u/kit".to_owned(),
        "/settings/export".to_owned(),
    ];
    paths.push("/search?q=findme".to_owned());
    for thread in [c.lunch, c.quiet, c.secret, c.staff_thread] {
        let path = thread_path(f, thread).await;
        paths.push(path.clone());
        paths.push(format!("{path}?page=2"));
        paths.push(format!("/t/{thread}"));
        paths.push(format!("/t/{thread}/unread"));
        paths.push(format!("/t/{thread}/audience"));
        paths.push(format!("/t/{thread}/reply"));
        paths.push(format!("/mod/t/{thread}"));
    }
    for post in [c.quiet_post, c.secret_post, c.dm_post] {
        for path in [
            format!("/p/{post}"),
            format!("/p/{post}/revisions"),
            format!("/p/{post}/edit"),
            format!("/p/{post}/delete"),
            format!("/p/{post}/report"),
            format!("/p/{post}/fork"),
            format!("/mod/p/{post}"),
            format!("/api/posts/{post}/source"),
            format!("/api/posts/{post}/reactions"),
        ] {
            paths.push(path);
        }
    }
    let mut recorded = 0;
    for path in &paths {
        let before = views(f).await.len();
        let response = f.get_as(path, Some(&c.sam_cookie)).await;
        let after = views(f).await;
        let new: Vec<&String> = after[before..].iter().map(|(_, p)| p).collect();
        assert!(
            new.iter().all(|p| *p == path),
            "{path} recorded views of other pages: {new:?}"
        );
        let shown: Vec<&str> = CLOSED
            .iter()
            .chain(&["Mentionbody", "Tuckedtitle"])
            .copied()
            .filter(|word| response.body.contains(word))
            .collect();
        if !shown.is_empty() {
            assert!(
                !new.is_empty(),
                "{path} showed {shown:?} under a grant without recording it:\n{}",
                response.body
            );
            recorded += 1;
        }
    }
    // The pages that are for reading under a grant did: the thread, the
    // space, the conversation.
    assert!(recorded >= 4, "only {recorded} pages showed anything");
}
