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
/// - An image, shown both in Lunch plans and in Circle secret.
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
    kit: i64,
    /// The image's name, as `/mod/uploads/{image}` has it.
    image: String,
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
    let sha = "ab".repeat(32);
    f.exec(&format!(
        "INSERT INTO uploads (sha256, ext, mime, bytes, width, height)
         VALUES ('{sha}', 'png', 'image/png', 1, 1, 1)"
    ))
    .await;
    let image = format!("{sha}.png");
    f.post(lunch, ruth, &format!("Pictured ![](/uploads/{image})"))
        .await;
    f.post(
        secret.thread_id,
        kit,
        &format!("Pictured ![](/uploads/{image})"),
    )
    .await;
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
        kit,
        image,
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

/// Asks for an image as the browser holding `cookie`, saying it has it
/// already (the images these tests make have no files): "not modified" if
/// they may fetch it, "not found" if not.
async fn image_status(f: &Forum, path: &str, cookie: &str) -> StatusCode {
    use axum::body::Body;
    use axum::http::{Request, header};
    let sha = path
        .trim_start_matches("/uploads/")
        .split('.')
        .next()
        .unwrap();
    f.request(
        Request::get(path)
            .header(header::COOKIE, cookie)
            .header(header::IF_NONE_MATCH, format!("\"{sha}\""))
            .body(Body::empty())
            .unwrap(),
    )
    .await
    .status
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
        // An image's page names the threads they read that show it, and
        // only counts the others.
        let path = format!("/mod/uploads/{}", c.image);
        let body = f.page_as(&path, cookie).await;
        shows_none(&path, &body, &CLOSED);
        assert!(body.contains("Lunchplanstitle"), "{body}");
        assert!(body.contains("1 post in a thread you can’t read"), "{body}");
        let secret_image_post = f
            .id(&format!(
                "SELECT max(id) FROM posts WHERE thread_id = {}",
                c.secret
            ))
            .await;
        assert!(
            !body.contains(&format!("/mod/p/{secret_image_post}\"")),
            "{body}"
        );
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
    let page = f
        .page_as(&format!("/mod/uploads/{}", c.image), &c.sam_cookie)
        .await;
    assert!(page.contains("Circlesecrettitle"), "{page}");
    assert!(!page.contains("can’t read"), "{page}");
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
    // Moving it into a space they read would let them read it: that is
    // breaking glass, so they are asked first, and nothing moves yet.
    let response = f
        .submit(
            &mod_path,
            &[("action", "move"), ("space", "general"), ("reason", "Tidy")],
            Some(cookie),
        )
        .await;
    assert_status(&response, StatusCode::OK);
    assert!(
        response.body.contains("moving it there would let you"),
        "{}",
        response.body
    );
    assert!(
        response.body.contains("name=\"glass_reason\""),
        "{}",
        response.body
    );
    shows_none(&mod_path, &response.body, &CLOSED);
    assert_eq!(
        count(
            f,
            &format!("SELECT space_id FROM threads WHERE id = {}", c.secret)
        )
        .await,
        c.circle
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
    // Changing its read grant so that it would let them read it is
    // breaking glass by another way: they are asked first.
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
    assert_status(&response, StatusCode::OK);
    assert!(
        response
            .body
            .contains("saving these settings would let you"),
        "{}",
        response.body
    );
    shows_none("/c/circle/settings", &response.body, &CLOSED);
    assert_eq!(
        count(
            f,
            &format!(
                "SELECT count(*) FROM space_grants WHERE space_id = {} AND verb = 'read' AND level = 'members'",
                c.circle
            )
        )
        .await,
        0
    );
    // Nothing they did was a read under a grant, or opened one.
    assert_eq!(count(f, "SELECT count(*) FROM staff_access_views").await, 0);
    assert_eq!(count(f, "SELECT count(*) FROM staff_access").await, 0);
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
            by_change: false,
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
            by_change: false,
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
                    by_change: false,
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
    // Its short address leads there, and since the address it gives says
    // what the thread is called, that is recorded too.
    let short = format!("/t/{}", c.quiet);
    let response = f.get_as(&short, Some(&c.sam_cookie)).await;
    assert_status(&response, StatusCode::PERMANENT_REDIRECT);
    assert_eq!(response.location(), quiet);
    assert_eq!(views(f).await.last().unwrap(), &(on_quiet, short));
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
    assert_eq!(views(f).await.len(), 3);
    // Nobody else reads under Sam's grant: not Ada, nor Ruth's own pages
    // record anything.
    assert_status(
        &f.get_as(&quiet, Some(&c.ada_cookie)).await,
        StatusCode::NOT_FOUND,
    );
    f.page_as(&quiet, &c.ruth_cookie).await;
    assert_eq!(views(f).await.len(), 3);

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
        views(f).await[3..],
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
                by_change: false,
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
        format!("/mod/uploads/{}", c.image),
        "/settings/access".to_owned(),
        "/admin/access".to_owned(),
        "/mod/break-glass?space=circle".to_owned(),
        "/mod/break-glass?space=circle-two".to_owned(),
        format!("/mod/break-glass?thread={}", c.quiet),
        format!("/mod/break-glass?thread={}", c.dm),
        format!("/mod/break-glass?thread={}", c.secret),
    ];
    paths.push("/search?q=findme".to_owned());
    let report = f.id("SELECT id FROM reports WHERE kind = 'post'").await;
    paths.push(format!("/mod/break-glass?report={report}"));
    for thread in [c.lunch, c.quiet, c.secret, c.staff_thread] {
        let path = thread_path(f, thread).await;
        paths.push(path.clone());
        paths.push(format!("{path}?page=2"));
        paths.push(format!("/t/{thread}"));
        paths.push(format!("/t/{thread}/wrong-slug"));
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
        // A redirect shows what its address says: a thread's slug is made
        // from its title.
        let location = response
            .header(axum::http::header::LOCATION)
            .unwrap_or_default()
            .to_lowercase();
        let shown: Vec<&str> = CLOSED
            .iter()
            .chain(&["Mentionbody", "Tuckedtitle"])
            .copied()
            .filter(|word| response.body.contains(word) || location.contains(&word.to_lowercase()))
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

/// Changing who may read something so that it would let a moderator read
/// what they can't is breaking glass by another way (DESIGN.md §3.1, §8),
/// which shows the confirmation page and changes nothing until they
/// confirm: for a thread's restriction as for a space's read grant, even
/// once they have named themselves among those who run the space.
#[sqlx::test]
async fn staff_cant_let_themselves_read_by_changing_who_may(pool: PgPool) {
    let c = club(pool).await;
    let f = &c.f;
    let cookie = &c.sam_cookie;
    let quiet = thread_path(f, c.quiet).await;
    let restriction = |f: &Forum| {
        let db = f.db.clone();
        let quiet = c.quiet;
        async move {
            sqlx::query_as::<_, (Option<Vec<i64>>, Option<Vec<i64>>)>(
                "SELECT read_group_ids, read_user_ids FROM threads WHERE id = $1",
            )
            .bind(quiet)
            .fetch_one(&db)
            .await
            .unwrap()
        }
    };
    let before = restriction(f).await;
    // The thread's audience form: lifting the restriction, or naming
    // themselves in it.
    let audience = format!("/t/{}/audience", c.quiet);
    for fields in [
        vec![("read", "space"), ("reply", "space")],
        vec![
            ("read", "only"),
            ("read_users", "jonah, sam"),
            ("reply", "space"),
        ],
        vec![("read", "only"), ("read_users", "sam"), ("reply", "space")],
    ] {
        let response = f.submit(&audience, &fields, Some(cookie)).await;
        assert_status(&response, StatusCode::OK);
        assert!(
            response
                .body
                .contains("You can’t read this thread, and saving this would let you."),
            "{fields:?}: {}",
            response.body
        );
        shows_none(&audience, &response.body, &CLOSED);
        assert_eq!(restriction(f).await, before);
    }
    assert_status(&f.get_as(&quiet, Some(cookie)).await, StatusCode::NOT_FOUND);
    // Narrowing it, which still leaves them out, is theirs to do.
    ok(
        f,
        &audience,
        &[
            ("read", "only"),
            ("read_users", "jonah"),
            ("reply", "space"),
        ],
        cookie,
    )
    .await;
    let (groups, mut users) = restriction(f).await;
    users.as_mut().unwrap().sort();
    let jonah = f.id("SELECT id FROM users WHERE username = 'jonah'").await;
    assert_eq!((groups, users), (Some(vec![]), Some(vec![c.ruth, jonah])));

    // A space's read grant: naming themselves among those who run it
    // first doesn't make it theirs to open. They would be the only member
    // the administer grant names who can't read the space.
    let settings = |read_level: &'static str, read_users: &'static str| {
        let organisers = c.organisers.to_string();
        move || {
            vec![
                ("name".to_owned(), "Circle".to_owned()),
                ("slug".to_owned(), "circle".to_owned()),
                ("description".to_owned(), String::new()),
                ("read_level".to_owned(), read_level.to_owned()),
                ("read_group".to_owned(), organisers.clone()),
                ("read_users".to_owned(), read_users.to_owned()),
                ("reply_level".to_owned(), "listed".to_owned()),
                ("reply_group".to_owned(), organisers.clone()),
                ("start_level".to_owned(), "listed".to_owned()),
                ("start_group".to_owned(), organisers.clone()),
                ("administer_level".to_owned(), "listed".to_owned()),
                ("administer_users".to_owned(), "sam".to_owned()),
                ("section".to_owned(), String::new()),
            ]
        }
    };
    let save = |fields: Vec<(String, String)>| async move {
        let fields: Vec<(&str, &str)> = fields
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        f.submit("/c/circle/settings", &fields, Some(cookie)).await
    };
    // Naming themselves to run it leaves them unable to read it: allowed.
    let response = save(settings("listed", "")()).await;
    assert_status(&response, StatusCode::SEE_OTHER);
    let administer: Vec<i64> = sqlx::query_scalar(
        "SELECT user_ids FROM space_grants WHERE space_id = $1 AND verb = 'administer'",
    )
    .bind(c.circle)
    .fetch_one(&f.db)
    .await
    .unwrap();
    assert_eq!(administer, vec![c.sam]);
    // Then letting members, or themselves, read it: breaking glass all the
    // same, which they are asked about first.
    for fields in [settings("members", "")(), settings("listed", "sam")()] {
        let response = save(fields).await;
        assert_status(&response, StatusCode::OK);
        assert!(
            response
                .body
                .contains("saving these settings would let you"),
            "{}",
            response.body
        );
    }
    assert_status(
        &f.get_as("/c/circle", Some(cookie)).await,
        StatusCode::NOT_FOUND,
    );

    // Even running a space in their own right, a member of staff who opens
    // a thread in it they can't read is breaking glass (DESIGN.md §3.1:
    // any staff action that would let them read what they can't), however
    // they came to be named there.
    f.set_grant(c.general, "administer", Grant::listed([], [c.sam]))
        .await;
    let response = f
        .submit(
            &audience,
            &[("read", "space"), ("reply", "space")],
            Some(cookie),
        )
        .await;
    assert_status(&response, StatusCode::OK);
    assert!(
        response
            .body
            .contains("You can’t read this thread, and saving this would let you."),
        "{}",
        response.body
    );
    assert_status(&f.get_as(&quiet, Some(cookie)).await, StatusCode::NOT_FOUND);
}

/// A moderator who names themselves among those who run a space they read,
/// from its settings, and then lifts the restriction of a thread in it
/// that leaves them out, breaks glass doing it, and everyone who could
/// read the thread is told (DESIGN.md §3.1, §8).
#[sqlx::test]
async fn naming_themselves_to_run_a_space_opens_none_of_its_threads(pool: PgPool) {
    let c = club(pool).await;
    let f = &c.f;
    let cookie = &c.sam_cookie;
    ok(
        f,
        "/c/general/settings",
        &[
            ("name", "General"),
            ("slug", "general"),
            ("description", ""),
            ("read_level", "members"),
            ("reply_level", "members"),
            ("start_level", "members"),
            ("administer_level", "listed"),
            ("administer_users", "sam"),
            ("section", ""),
        ],
        cookie,
    )
    .await;
    let audience = format!("/t/{}/audience", c.quiet);
    let response = f
        .submit(
            &audience,
            &[("read", "space"), ("reply", "space")],
            Some(cookie),
        )
        .await;
    assert_status(&response, StatusCode::OK);
    assert!(response.body.contains("Break glass"), "{}", response.body);
    let quiet = thread_path(f, c.quiet).await;
    assert_status(&f.get_as(&quiet, Some(cookie)).await, StatusCode::NOT_FOUND);
    // Confirmed, it is a grant, and Ruth and Jonah are told.
    ok(
        f,
        &audience,
        &[
            ("read", "space"),
            ("reply", "space"),
            ("glass_reason", "Settling a dispute"),
        ],
        cookie,
    )
    .await;
    let jonah = f.id("SELECT id FROM users WHERE username = 'jonah'").await;
    let mut expected = vec![c.ruth, jonah];
    expected.sort();
    assert_eq!(told(f).await, expected);
    assert_eq!(
        count(
            f,
            &format!(
                "SELECT count(*) FROM staff_access WHERE thread_id = {} AND by_change",
                c.quiet
            )
        )
        .await,
        1
    );
}

/// Moving a thread a moderator can't read somewhere some of its readers
/// couldn't follow is breaking glass on it, telling them, as a first step
/// to reading it: the moderator could make that space theirs next
/// (DESIGN.md §3.1, §8). So is moving it into their own feed, where the
/// thread's restriction would name them. Moving it where everyone who
/// reads it still can is not.
#[sqlx::test]
async fn moving_a_thread_from_its_readers_breaks_glass(pool: PgPool) {
    let c = club(pool).await;
    let f = &c.f;
    let cookie = &c.sam_cookie;
    // A space nobody reads yet.
    let empty = f.unlisted_space("empty", "listed").await;
    let mod_path = format!("/mod/t/{}", c.secret);
    let fields = [("action", "move"), ("space", "empty"), ("reason", "Tidy")];
    let page = f.submit(&mod_path, &fields, Some(cookie)).await;
    assert_status(&page, StatusCode::OK);
    assert!(
        page.body
            .contains("moving it there would take it away from some of the people who read it now"),
        "{}",
        page.body
    );
    assert!(page.body.contains("72 hours"), "{}", page.body);
    assert!(page.body.contains("(1 person)"), "{}", page.body);
    shows_none(&mod_path, &page.body, &CLOSED);
    assert_eq!(
        count(
            f,
            &format!("SELECT space_id FROM threads WHERE id = {}", c.secret)
        )
        .await,
        c.circle
    );
    let mut confirmed = fields.to_vec();
    confirmed.push(("glass_reason", "Quarantine"));
    ok(f, &mod_path, &confirmed, cookie).await;
    assert_eq!(
        count(
            f,
            &format!("SELECT space_id FROM threads WHERE id = {}", c.secret)
        )
        .await,
        empty
    );
    // Kit, who read it in the Circle, was told; the grant is one to read it
    // for a while, the move not having made the moderator a reader.
    assert_eq!(told(f).await, vec![c.kit]);
    assert_eq!(
        count(
            f,
            &format!(
                "SELECT count(*) FROM staff_access WHERE thread_id = {} AND NOT by_change",
                c.secret
            )
        )
        .await,
        1
    );

    // Into the moderator's own feed, a restricted thread would name them.
    let quiet_move = format!("/mod/t/{}", c.quiet);
    let page = f
        .submit(
            &quiet_move,
            &[("action", "move"), ("space", "sam"), ("reason", "Mine")],
            Some(cookie),
        )
        .await;
    assert_status(&page, StatusCode::OK);
    assert!(
        page.body
            .contains("You can’t read this thread, and moving it there would let you."),
        "{}",
        page.body
    );
    assert_eq!(
        count(
            f,
            &format!("SELECT space_id FROM threads WHERE id = {}", c.quiet)
        )
        .await,
        c.general
    );

    // Between two spaces the same people read, nothing is asked.
    let other = f
        .thread(c.circle, c.kit, "Othertitle", "Otherbody")
        .await
        .thread_id;
    let before = count(f, "SELECT count(*) FROM staff_access").await;
    ok(
        f,
        &format!("/mod/t/{other}"),
        &[
            ("action", "move"),
            ("space", "circle-two"),
            ("reason", "Tidy"),
        ],
        cookie,
    )
    .await;
    assert_eq!(count(f, "SELECT count(*) FROM staff_access").await, before);
}

/// A post held for approval is shown to a moderator only where they could
/// read it once it is up: a new thread in a space they can't read, or one
/// whose restriction leaves them out, or a reply in a thread they can't
/// read, is shown by who wrote it and where, and can still be approved or
/// rejected, as a report on what they can't read is (DESIGN.md §1
/// principle 3, §8). Its author always sees their own.
#[sqlx::test]
async fn held_posts_are_shown_only_where_staff_read(pool: PgPool) {
    use parlour::mod_records::{self, HeldPlace};
    let c = club(pool).await;
    let f = &c.f;
    let hold = |author: i64, place: HeldPlace, body: &'static str| {
        let db = f.db.clone();
        async move { mod_records::hold(&db, author, &place, body).await.unwrap() }
    };
    let image = format!("/uploads/{}", c.image);
    let in_secret = hold(
        c.kit,
        HeldPlace::Reply {
            thread_id: c.secret,
            reply_to_post_id: None,
        },
        "Heldsecretbody",
    )
    .await;
    let in_circle = hold(
        c.kit,
        HeldPlace::Thread {
            space_id: c.circle,
            title: "Heldcircletitle".into(),
            forked_from_post_id: None,
            read: None,
        },
        "Heldcirclebody",
    )
    .await;
    let narrowed = hold(
        c.ruth,
        HeldPlace::Thread {
            space_id: c.general,
            title: "Heldquiettitle".into(),
            forked_from_post_id: None,
            read: Some(Grant::listed([], [c.ruth])),
        },
        "Heldquietbody",
    )
    .await;
    let in_lunch = hold(
        c.ruth,
        HeldPlace::Reply {
            thread_id: c.lunch,
            reply_to_post_id: None,
        },
        "Heldlunchbody",
    )
    .await;
    // The image only in a held post where the moderator can't read.
    let sha = "cd".repeat(32);
    f.exec(&format!(
        "INSERT INTO uploads (sha256, ext, mime, bytes, width, height)
         VALUES ('{sha}', 'png', 'image/png', 1, 1, 1);
         UPDATE held_posts SET body_md = body_md || ' ![](/uploads/{sha}.png)' WHERE id = {in_secret};"
    ))
    .await;
    let held_image = format!("/uploads/{sha}.png");
    let closed = [
        "Heldsecretbody",
        "Heldcircletitle",
        "Heldcirclebody",
        "Heldquiettitle",
        "Heldquietbody",
        &held_image,
    ];
    let cookie = &c.sam_cookie;
    let queue = f.page_as("/mod/queue", cookie).await;
    shows_none("/mod/queue", &queue, &closed);
    shows_none("/mod/queue", &queue, &CLOSED);
    assert!(queue.contains("Heldlunchbody"), "{queue}");
    assert!(
        queue.contains("You can’t read where it would go"),
        "{queue}"
    );
    for id in [in_secret, in_circle, narrowed] {
        let path = format!("/held/{id}");
        let page = f.page_as(&path, cookie).await;
        shows_none(&path, &page, &closed);
        shows_none(&path, &page, &CLOSED);
        assert!(page.contains("You can’t read where it would go"), "{page}");
    }
    let page = f.page_as(&format!("/held/{in_lunch}"), cookie).await;
    assert!(page.contains("Heldlunchbody"), "{page}");
    // Nor do they get its images.
    assert_eq!(
        image_status(f, &held_image, cookie).await,
        StatusCode::NOT_FOUND
    );
    let _ = image;
    // Its author sees all of their own.
    let kit_cookie = f.session(c.kit).await;
    let page = f.page_as(&format!("/held/{in_secret}"), &kit_cookie).await;
    assert!(page.contains("Heldsecretbody"), "{page}");
    assert!(page.contains(&held_image), "{page}");
    // And a moderator who reads there sees it, and its image.
    f.join_group(c.organisers, c.sam).await;
    let page = f.page_as(&format!("/held/{in_secret}"), cookie).await;
    assert!(page.contains("Heldsecretbody"), "{page}");
    assert_eq!(
        image_status(f, &held_image, cookie).await,
        StatusCode::NOT_MODIFIED
    );
    f.exec(&format!(
        "DELETE FROM group_members WHERE group_id = {} AND user_id = {}",
        c.organisers, c.sam
    ))
    .await;
    // Approving and rejecting need no reading.
    ok(
        f,
        &format!("/mod/held/{in_circle}/approve"),
        &[("back", "/mod/queue")],
        cookie,
    )
    .await;
    ok(
        f,
        &format!("/mod/held/{narrowed}/reject"),
        &[("reason", "Not here"), ("back", "/mod/queue")],
        cookie,
    )
    .await;
    let log = f.page_as("/mod/log", cookie).await;
    shows_none("/mod/log", &log, &closed);
    assert_eq!(count(f, "SELECT count(*) FROM staff_access_views").await, 0);
}

/// The report form tells the reporter of something private what the
/// moderators will see, and what happens if they open it (DESIGN.md §8).
#[sqlx::test]
async fn reporting_private_content_says_what_staff_will_see(pool: PgPool) {
    let c = club(pool).await;
    let f = &c.f;
    let lunch_post = f
        .id(&format!(
            "SELECT min(id) FROM posts WHERE thread_id = {}",
            c.lunch
        ))
        .await;
    let form = f
        .page_as(&format!("/p/{}/report", c.quiet_post), &c.ruth_cookie)
        .await;
    assert!(
        form.contains("If they need to open the thread to act on it, everyone who can read it will be told a moderator looked, and can read the reason the moderator gave in the access log, but not who reported it."),
        "{form}"
    );
    let form = f
        .page_as(&format!("/p/{}/report", c.dm_post), &c.ruth_cookie)
        .await;
    assert!(
        form.contains("If they need to open the conversation to act on it, everyone in it will be told a moderator looked, and can read the reason the moderator gave in the access log, but not who reported it."),
        "{form}"
    );
    assert!(!form.contains("but not the conversation itself"), "{form}");
    let form = f
        .page_as(&format!("/p/{lunch_post}/report"), &c.ruth_cookie)
        .await;
    assert!(!form.contains("a moderator looked"), "{form}");
}

/// Today, as the notices say it.
fn today() -> String {
    chrono::Utc::now().format("%-d %b %Y").to_string()
}

/// Who has been told about grants, in order of who.
async fn told(f: &Forum) -> Vec<i64> {
    sqlx::query_scalar(
        "SELECT user_id FROM notifications WHERE kind = 'staff_access' ORDER BY user_id, id",
    )
    .fetch_all(&f.db)
    .await
    .unwrap()
}

/// Opening reported content a moderator can't read is breaking glass
/// (DESIGN.md §3.9, §8): the queue offers to open it, the confirmation page
/// says what and who will be told and asks why, and confirming opens a
/// grant for 72 hours that tells everyone in the conversation at once,
/// shows on its page for good, and goes in the log.
#[sqlx::test]
async fn opening_a_report_breaks_glass_and_everyone_in_it_is_told(pool: PgPool) {
    let c = club(pool).await;
    let f = &c.f;
    // Notification emails wait an hour here, but not these.
    parlour::settings::set(
        &f.db,
        "notifications",
        &parlour::settings::NotificationSettings {
            email_delay_minutes: 60,
        },
    )
    .await
    .unwrap();
    let cookie = &c.sam_cookie;
    let jonah = f.id("SELECT id FROM users WHERE username = 'jonah'").await;
    let report = f.id("SELECT id FROM reports WHERE kind = 'post'").await;
    let report_field = report.to_string();
    let open = format!("/mod/break-glass?report={report}");
    let dm = format!("/messages/{}", c.dm);
    // The queue offers to open it, and says what that means.
    let queue = f.page_as("/mod/queue", cookie).await;
    assert!(queue.contains(&format!("href=\"{open}\"")), "{queue}");
    assert!(queue.contains("that is breaking glass"), "{queue}");
    shows_none("/mod/queue", &queue, &CLOSED);
    // Nobody but staff gets there.
    assert_status(
        &f.get_as(&open, Some(&c.ruth_cookie)).await,
        StatusCode::NOT_FOUND,
    );
    // The confirmation page: what it opens, without what it says, who
    // will be told, for how long, and a reason to give.
    let page = f.page_as(&open, cookie).await;
    shows_none(&open, &page, &CLOSED);
    assert!(page.contains("a conversation between 2 people"), "{page}");
    assert!(
        page.contains("breaking glass on this conversation"),
        "{page}"
    );
    assert!(
        page.contains("Everyone else in it (2 people) will be told at once"),
        "{page}"
    );
    assert!(
        page.contains("The notice won’t say why or who reported it, but they can read your reason in the access log."),
        "{page}"
    );
    assert!(!page.contains("won’t be told why"), "{page}");
    assert!(page.contains("72 hours"), "{page}");
    assert!(page.contains("name=\"glass_reason\""), "{page}");
    // Without a reason, nothing happens.
    let response = f
        .submit(
            "/mod/break-glass",
            &[("report", &report_field), ("glass_reason", "  ")],
            Some(cookie),
        )
        .await;
    assert_status(&response, StatusCode::BAD_REQUEST);
    assert!(
        response.body.contains("Say why you need to read it"),
        "{}",
        response.body
    );
    shows_none("/mod/break-glass", &response.body, &CLOSED);
    assert_eq!(count(f, "SELECT count(*) FROM staff_access").await, 0);
    // With one, it opens the conversation, for 72 hours.
    let response = ok(
        f,
        "/mod/break-glass",
        &[
            ("report", &report_field),
            ("glass_reason", "Harassment reported from the queue"),
        ],
        cookie,
    )
    .await;
    assert_eq!(response.location(), dm);
    let (scope, thread, from_report, hours): (String, Option<i64>, Option<i64>, f64) =
        sqlx::query_as(
            "SELECT scope, thread_id, report_id,
                    (extract(epoch FROM expires_at - created_at) / 3600)::float8
             FROM staff_access",
        )
        .fetch_one(&f.db)
        .await
        .unwrap();
    assert_eq!(
        (scope.as_str(), thread, from_report, hours),
        ("thread", Some(c.dm), Some(report), 72.0)
    );
    let page = f.page_as(&dm, cookie).await;
    assert!(page.contains("Dmbody"), "{page}");
    // Asking again, while it runs, leads straight there.
    let response = f.get_as(&open, Some(cookie)).await;
    assert_status(&response, StatusCode::SEE_OTHER);
    assert_eq!(response.location(), dm);
    assert_eq!(count(f, "SELECT count(*) FROM staff_access").await, 1);

    // Everyone in it was told, and nobody else.
    let mut expected = vec![c.ruth, jonah];
    expected.sort();
    assert_eq!(told(f).await, expected);
    let sentence = format!("A moderator (Sam) viewed this conversation on {}.", today());
    for who in [&c.ruth_cookie, &c.jonah_cookie] {
        let notes = f.page_as("/notifications", who).await;
        assert!(
            notes.contains("A moderator (<strong>Sam</strong>) viewed"),
            "{notes}"
        );
        assert!(!notes.contains("from the queue"), "{notes}");
        // The conversation's page says so, to everyone in it.
        let page = f.page_as(&dm, who).await;
        assert!(page.contains(&sentence), "{page}");
        assert!(!page.contains("from the queue"), "{page}");
    }
    let badge: serde_json::Value =
        serde_json::from_str(&f.page_as("/api/notifications", &c.ruth_cookie).await).unwrap();
    assert!(
        badge["items"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("A moderator (Sam) viewed your conversation"),
        "{badge}"
    );
    // By email at once, though other notifications wait an hour, and
    // without why, or who reported it.
    for to in ["ruth@example.com", "jonah@example.com"] {
        let email = f.last_email_to(to).await;
        assert!(
            email
                .body
                .contains("A moderator (Sam) viewed your conversation"),
            "{}",
            email.body
        );
        assert!(!email.body.contains("from the queue"), "{}", email.body);
    }
    for email in f.emails().await {
        if email.body.contains("A moderator (Sam) viewed") {
            assert!(
                ["ruth@example.com", "jonah@example.com"].contains(&email.to.as_str()),
                "{}",
                email.to
            );
        }
    }
    // The notice leads to the conversation.
    let notice = f
        .id(&format!(
            "SELECT id FROM notifications WHERE kind = 'staff_access' AND user_id = {}",
            c.ruth
        ))
        .await;
    let response = f
        .get_as(&format!("/notifications/{notice}"), Some(&c.ruth_cookie))
        .await;
    assert_eq!(response.location(), dm);

    // The log: every admin sees it, with why; each person in it sees it
    // among what touched them; nobody else does.
    let log = f.page_as("/admin/access", &c.ada_cookie).await;
    assert!(log.contains("Harassment reported from the queue"), "{log}");
    assert!(log.contains("between 2 people"), "{log}");
    assert!(log.contains("after a report"), "{log}");
    shows_none("/admin/access", &log, &CLOSED);
    assert_status(
        &f.get_as("/admin/access", Some(cookie)).await,
        StatusCode::FORBIDDEN,
    );
    let mine = f.page_as("/settings/access", &c.ruth_cookie).await;
    assert!(mine.contains("Dmtitle"), "{mine}");
    assert!(
        mine.contains("Harassment reported from the queue"),
        "{mine}"
    );
    let kit_cookie = f.session(c.kit).await;
    let kits = f.page_as("/settings/access", &kit_cookie).await;
    assert!(kits.contains("No moderator has needed"), "{kits}");
    shows_none("/settings/access", &kits, &CLOSED);
    // The public log counts it, by month.
    let public = f.page("/moderation").await;
    assert!(
        public.contains("It has happened 1 time in all."),
        "{public}"
    );
    assert!(
        public.contains(&chrono::Utc::now().format("%B %Y").to_string()),
        "{public}"
    );

    // Under the grant, the moderator can act on the report, from the queue,
    // which still shows nothing of it.
    let queue = f.page_as("/mod/queue", cookie).await;
    assert!(queue.contains("You broke glass on it"), "{queue}");
    shows_none("/mod/queue", &queue, &CLOSED);
    ok(
        f,
        &format!("/mod/p/{}", c.dm_post),
        &[
            ("action", "hide"),
            ("reason", "Harassment"),
            ("back", "/mod/queue"),
        ],
        cookie,
    )
    .await;
    assert_eq!(
        count(
            f,
            &format!(
                "SELECT count(*) FROM posts WHERE id = {} AND hidden_at IS NOT NULL",
                c.dm_post
            )
        )
        .await,
        1
    );

    // Once it runs out, the conversation is closed to them again, and the
    // line stays on its page.
    f.exec(
        "ALTER TABLE staff_access DISABLE TRIGGER staff_access_append_only;
         UPDATE staff_access SET created_at = now() - interval '4 days',
                                 expires_at = now() - interval '1 day';
         ALTER TABLE staff_access ENABLE TRIGGER staff_access_append_only;",
    )
    .await;
    assert_status(&f.get_as(&dm, Some(cookie)).await, StatusCode::NOT_FOUND);
    let page = f.page_as(&dm, &c.ruth_cookie).await;
    assert!(
        page.contains("A moderator (Sam) viewed this conversation on"),
        "{page}"
    );
    // And opening the report again asks again.
    let page = f.page_as(&open, cookie).await;
    assert!(page.contains("name=\"glass_reason\""), "{page}");
}

/// A thread or space a member of staff can't read is opened from a staff
/// page (DESIGN.md §8): a thread's moderation page and a space's settings
/// lead to the confirmation page, and confirming tells everyone who can
/// read it, whose pages of it say so from then on.
#[sqlx::test]
async fn staff_pages_lead_to_breaking_glass(pool: PgPool) {
    let c = club(pool).await;
    let f = &c.f;
    let cookie = &c.sam_cookie;
    let kit_cookie = f.session(c.kit).await;
    let jonah = f.id("SELECT id FROM users WHERE username = 'jonah'").await;
    // A thread's moderation page.
    let page = f.page_as(&format!("/mod/t/{}", c.secret), cookie).await;
    let thread_glass = format!("/mod/break-glass?thread={}", c.secret);
    assert!(page.contains(&format!("href=\"{thread_glass}\"")), "{page}");
    let page = f.page_as(&thread_glass, cookie).await;
    shows_none(&thread_glass, &page, &CLOSED);
    assert!(
        page.contains(&format!("thread № {} in Circle, started by Kit", c.secret)),
        "{page}"
    );
    assert!(
        page.contains("Everyone who can read the thread (1 person) will be told"),
        "{page}"
    );
    // Something they read already needs no glass: straight there.
    for (path, to) in [
        (
            format!("/mod/break-glass?thread={}", c.lunch),
            format!("/t/{}", c.lunch),
        ),
        (
            "/mod/break-glass?space=general".to_owned(),
            "/c/general".to_owned(),
        ),
    ] {
        let response = f.get_as(&path, Some(cookie)).await;
        assert_status(&response, StatusCode::SEE_OTHER);
        assert_eq!(response.location(), to);
    }
    // Nothing that isn't there.
    for path in [
        "/mod/break-glass?thread=999999",
        "/mod/break-glass?space=nowhere",
        "/mod/break-glass",
    ] {
        assert_status(&f.get_as(path, Some(cookie)).await, StatusCode::NOT_FOUND);
    }
    // A space's settings.
    let settings = f.page_as("/c/circle/settings", cookie).await;
    assert!(
        settings.contains("href=\"/mod/break-glass?space=circle\""),
        "{settings}"
    );
    let page = f.page_as("/mod/break-glass?space=circle", cookie).await;
    shows_none("/mod/break-glass?space=circle", &page, &CLOSED);
    assert!(page.contains("the whole space Circle"), "{page}");
    assert!(page.contains("every thread in it included"), "{page}");
    let response = ok(
        f,
        "/mod/break-glass",
        &[
            ("space", "circle"),
            ("glass_reason", "A complaint about the Circle"),
        ],
        cookie,
    )
    .await;
    assert_eq!(response.location(), "/c/circle");
    let page = f.page_as("/c/circle", cookie).await;
    assert!(page.contains("Circlesecrettitle"), "{page}");
    // Kit, who reads it, was told; nobody else was.
    assert_eq!(told(f).await, vec![c.kit]);
    let space_line = format!("A moderator (Sam) viewed this space on {}.", today());
    let page = f.page_as("/c/circle", &kit_cookie).await;
    assert!(page.contains(&space_line), "{page}");
    let notes = f.page_as("/notifications", &kit_cookie).await;
    assert!(notes.contains("the space Circle"), "{notes}");
    let notice = f
        .id(&format!(
            "SELECT id FROM notifications WHERE kind = 'staff_access' AND user_id = {}",
            c.kit
        ))
        .await;
    // The badge counts it, until it is opened, which leads to the space.
    let badge: serde_json::Value =
        serde_json::from_str(&f.page_as("/api/notifications", &kit_cookie).await).unwrap();
    assert_eq!(badge["unread"], 1, "{badge}");
    let response = f
        .get_as(&format!("/notifications/{notice}"), Some(&kit_cookie))
        .await;
    assert_eq!(response.location(), "/c/circle");

    // An admin breaks glass on a thread whose restriction leaves them out:
    // the two it names are told, and its page says so to them.
    let quiet = format!("/mod/break-glass?thread={}", c.quiet);
    let page = f.page_as(&quiet, &c.ada_cookie).await;
    assert!(page.contains("(2 people)"), "{page}");
    ok(
        f,
        "/mod/break-glass",
        &[
            ("thread", &c.quiet.to_string()),
            ("glass_reason", "Checking a complaint"),
        ],
        &c.ada_cookie,
    )
    .await;
    let mut expected = vec![c.kit, c.ruth, jonah];
    expected.sort();
    assert_eq!(told(f).await, expected);
    let thread_line = format!("A moderator (Ada) viewed this thread on {}.", today());
    let page = f
        .page_as(&thread_path(f, c.quiet).await, &c.ruth_cookie)
        .await;
    assert!(page.contains(&thread_line), "{page}");
    // Not on other pages of the space.
    let page = f.page_as("/c/general", &c.ruth_cookie).await;
    assert!(!page.contains("A moderator ("), "{page}");
    let page = f
        .page_as(&thread_path(f, c.lunch).await, &c.ruth_cookie)
        .await;
    assert!(!page.contains("A moderator ("), "{page}");
    // The public log counts both.
    let public = f.page("/moderation").await;
    assert!(
        public.contains("It has happened 2 times in all."),
        "{public}"
    );
}

/// Changing who may read something so that it lets the member of staff
/// doing it read what they couldn't is breaking glass (DESIGN.md §8):
/// once they confirm it, saying why, the change is made and recorded as a
/// grant, and those who could read it before are told. That goes for a
/// space's read grant, a thread's restriction, moving a thread, and an
/// admin joining a group.
#[sqlx::test]
async fn confirmed_changes_that_let_staff_read_break_glass(pool: PgPool) {
    let c = club(pool).await;
    let f = &c.f;
    let cookie = &c.sam_cookie;
    let jonah = f.id("SELECT id FROM users WHERE username = 'jonah'").await;
    let grants = |f: &Forum| {
        let db = f.db.clone();
        async move {
            sqlx::query_as::<_, (String, Option<i64>, Option<i64>, String)>(
                "SELECT scope, thread_id, space_id, reason FROM staff_access ORDER BY id",
            )
            .fetch_all(&db)
            .await
            .unwrap()
        }
    };
    // A thread in the second circle, to move.
    let hidden = f
        .thread(c.circle_two, c.kit, "Hiddentitle", "Hiddenbody")
        .await
        .thread_id;

    // The thread's restriction, lifted.
    let audience = format!("/t/{}/audience", c.quiet);
    let fields = [("read", "space"), ("reply", "space")];
    let page = f.submit(&audience, &fields, Some(cookie)).await;
    assert_status(&page, StatusCode::OK);
    assert!(page.body.contains("(2 people)"), "{}", page.body);
    // The page sends the form back with a reason.
    assert!(
        page.body
            .contains("<input type=\"hidden\" name=\"read\" value=\"space\">"),
        "{}",
        page.body
    );
    let response = f
        .submit(
            &audience,
            &[
                ("read", "space"),
                ("reply", "space"),
                ("glass_reason", "Opening it up after a dispute"),
            ],
            Some(cookie),
        )
        .await;
    assert_status(&response, StatusCode::SEE_OTHER);
    let page = f.page_as(&thread_path(f, c.quiet).await, cookie).await;
    assert!(page.contains("Quietwordbody"), "{page}");
    assert_eq!(
        grants(f).await,
        vec![(
            "thread".to_owned(),
            Some(c.quiet),
            None,
            "Opening it up after a dispute".to_owned()
        )]
    );
    // Those who could read it before were told; not everyone who can now.
    let mut expected = vec![c.ruth, jonah];
    expected.sort();
    assert_eq!(told(f).await, expected);

    // The Circle's read grant, opened to members.
    let fields = [
        ("name", "Circle"),
        ("slug", "circle"),
        ("description", ""),
        ("read_level", "members"),
        ("reply_level", "listed"),
        ("reply_group", &c.organisers.to_string()),
        ("start_level", "listed"),
        ("start_group", &c.organisers.to_string()),
        ("administer_level", "listed"),
        ("section", ""),
    ];
    let page = f.submit("/c/circle/settings", &fields, Some(cookie)).await;
    assert_status(&page, StatusCode::OK);
    assert!(
        page.body
            .contains("You can’t read Circle, and saving these settings would let you."),
        "{}",
        page.body
    );
    // A reason that won't do asks again, keeping the form.
    let mut with_reason = fields.to_vec();
    with_reason.push(("glass_reason", ""));
    let page = f
        .submit("/c/circle/settings", &with_reason, Some(cookie))
        .await;
    assert_status(&page, StatusCode::BAD_REQUEST);
    assert!(
        page.body.contains("Say why you need to read it"),
        "{}",
        page.body
    );
    with_reason.pop();
    with_reason.push(("glass_reason", "The Organisers asked to open it"));
    ok(f, "/c/circle/settings", &with_reason, cookie).await;
    assert_eq!(
        grants(f).await[1],
        (
            "space".to_owned(),
            None,
            Some(c.circle),
            "The Organisers asked to open it".to_owned()
        )
    );
    let page = f.page_as("/c/circle", cookie).await;
    assert!(page.contains("Circlesecrettitle"), "{page}");
    // Kit, who could read it before, was told.
    let mut expected = vec![c.kit, c.ruth, jonah];
    expected.sort();
    assert_eq!(told(f).await, expected);

    // Moving a thread from the second circle into General.
    let mod_path = format!("/mod/t/{hidden}");
    let move_fields = [("action", "move"), ("space", "general"), ("reason", "Tidy")];
    let page = f.submit(&mod_path, &move_fields, Some(cookie)).await;
    assert_status(&page, StatusCode::OK);
    assert!(
        page.body.contains("Break glass and move it"),
        "{}",
        page.body
    );
    shows_none(&mod_path, &page.body, &["Hiddentitle", "Hiddenbody"]);
    let mut confirmed = move_fields.to_vec();
    confirmed.push(("glass_reason", "It belongs in General"));
    ok(f, &mod_path, &confirmed, cookie).await;
    assert_eq!(
        count(
            f,
            &format!("SELECT space_id FROM threads WHERE id = {hidden}")
        )
        .await,
        c.general
    );
    assert_eq!(
        grants(f).await[2],
        (
            "thread".to_owned(),
            Some(hidden),
            None,
            "It belongs in General".to_owned()
        )
    );
    assert_eq!(
        count(
            f,
            &format!(
                "SELECT count(*) FROM notifications WHERE kind = 'staff_access' AND thread_id = {hidden}"
            )
        )
        .await,
        1
    );

    // An admin joining the Organisers, who alone read the second circle.
    let join = format!("/admin/groups/{}/members", c.organisers);
    let page = f
        .submit(&join, &[("username", "ada")], Some(&c.ada_cookie))
        .await;
    assert_status(&page, StatusCode::OK);
    assert!(
        page.body
            .contains("Joining Organisers would let you read what you can’t now."),
        "{}",
        page.body
    );
    assert!(
        page.body.contains("the whole space Circle two"),
        "{}",
        page.body
    );
    assert_eq!(
        count(
            f,
            &format!(
                "SELECT count(*) FROM group_members WHERE group_id = {} AND user_id = {}",
                c.organisers, c.ada
            )
        )
        .await,
        0
    );
    ok(
        f,
        &join,
        &[
            ("username", "ada"),
            ("glass_reason", "Helping the Organisers"),
        ],
        &c.ada_cookie,
    )
    .await;
    assert_eq!(
        count(
            f,
            &format!(
                "SELECT count(*) FROM group_members WHERE group_id = {} AND user_id = {}",
                c.organisers, c.ada
            )
        )
        .await,
        1
    );
    assert_eq!(
        grants(f).await[3],
        (
            "space".to_owned(),
            None,
            Some(c.circle_two),
            "Helping the Organisers".to_owned()
        )
    );
    // Adding someone else asks nothing.
    let before = grants(f).await.len();
    ok(f, &join, &[("username", "ruth")], &c.ada_cookie).await;
    assert_eq!(grants(f).await.len(), before);
    // Each of these let them read by their memberships from then on, so
    // no page was read under the grants.
    assert_eq!(count(f, "SELECT count(*) FROM staff_access_views").await, 0);
}

/// Those told are those who can read it, as the access layer has it, with
/// a suspended member counted by the memberships they will read with
/// again: their conversation, or a thread they are in, was read whether or
/// not they could read it that week, and they are often the person a
/// report is about (DESIGN.md §8 step 4).
#[sqlx::test]
async fn the_readers_told_are_those_the_access_layer_lets_read(pool: PgPool) {
    let c = club(pool).await;
    let f = &c.f;
    let jonah = f.id("SELECT id FROM users WHERE username = 'jonah'").await;
    f.exec(&format!(
        "UPDATE users SET suspended_until = now() + interval '1 day' WHERE id = {jonah}"
    ))
    .await;
    let readers = |scope| {
        let db = f.db.clone();
        async move { staff_access::readers(&db, scope, c.ada).await.unwrap() }
    };
    // The thread only Ruth and Jonah read, and their conversation: both,
    // Jonah though suspended.
    let mut both = vec![c.ruth, jonah];
    both.sort();
    assert_eq!(readers(Scope::Thread(c.quiet)).await, both);
    assert_eq!(readers(Scope::Thread(c.dm)).await, both);
    // A public space: everyone but the one opening it, Jonah included.
    let open = f.space("open", "public", None, 8).await;
    let everyone: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM users WHERE id <> $1 AND deleted_at IS NULL ORDER BY id",
    )
    .bind(c.ada)
    .fetch_all(&f.db)
    .await
    .unwrap();
    assert!(everyone.contains(&jonah));
    assert_eq!(readers(Scope::Space(open)).await, everyone);
    // A members-only one: Jonah too.
    assert_eq!(readers(Scope::Space(c.general)).await, everyone);
    // Breaking glass on the conversation tells him, and his log has it
    // once the suspension is over.
    let page = f
        .submit(
            "/mod/break-glass",
            &[
                ("thread", &c.dm.to_string()),
                ("glass_reason", "Reading the reported message"),
            ],
            Some(&c.sam_cookie),
        )
        .await;
    assert_status(&page, StatusCode::SEE_OTHER);
    assert_eq!(told(f).await, both);
    f.exec(&format!(
        "UPDATE users SET suspended_until = NULL WHERE id = {jonah}"
    ))
    .await;
    let log = f.page_as("/settings/access", &c.jonah_cookie).await;
    assert!(log.contains("Reading the reported message"), "{log}");
}

/// A change that breaks glass tells those who could read it before
/// (DESIGN.md §8), and so they are told even when the change shuts them
/// out: their notifications, their badge, their email and their own log
/// all say so, naming what they can no longer read without its title or
/// anything else they can't read now.
#[sqlx::test]
async fn readers_a_change_shuts_out_are_still_told(pool: PgPool) {
    let c = club(pool).await;
    let f = &c.f;
    let cookie = &c.sam_cookie;
    let jonah = f.id("SELECT id FROM users WHERE username = 'jonah'").await;
    let kit_cookie = f.session(c.kit).await;
    let today = today();

    // Sam narrows the quiet word to Ruth and himself, shutting Jonah out.
    let audience = format!("/t/{}/audience", c.quiet);
    ok(
        f,
        &audience,
        &[
            ("read", "only"),
            ("read_users", "sam"),
            ("reply", "space"),
            ("glass_reason", "Looking into a complaint"),
        ],
        cookie,
    )
    .await;
    let quiet = thread_path(f, c.quiet).await;
    assert_status(
        &f.get_as(&quiet, Some(&c.jonah_cookie)).await,
        StatusCode::NOT_FOUND,
    );
    let mut expected = vec![c.ruth, jonah];
    expected.sort();
    assert_eq!(told(f).await, expected);
    // Jonah is told, on the site, on the badge and by email, of a thread
    // he could read, not what it is called.
    let sentence =
        format!("A moderator (Sam) viewed a thread you could read in General on {today}.");
    let notes = f.page_as("/notifications", &c.jonah_cookie).await;
    assert!(
        notes.contains("A moderator (<strong>Sam</strong>) viewed"),
        "{notes}"
    );
    assert!(
        notes.contains("a thread you could read in General"),
        "{notes}"
    );
    shows_none("/notifications", &notes, &CLOSED);
    let badge: serde_json::Value =
        serde_json::from_str(&f.page_as("/api/notifications", &c.jonah_cookie).await).unwrap();
    assert_eq!(badge["unread"], 1, "{badge}");
    assert_eq!(badge["items"][0]["text"], sentence.as_str(), "{badge}");
    let email = f.last_email_to("jonah@example.com").await;
    assert!(email.body.contains(&sentence), "{}", email.body);
    shows_none("email", &email.body, &CLOSED);
    shows_none("email", &email.subject, &CLOSED);
    // The notice leads to his log, which has why, and names the thread by
    // its number.
    let notice = f
        .id(&format!(
            "SELECT id FROM notifications WHERE kind = 'staff_access' AND user_id = {jonah}"
        ))
        .await;
    let response = f
        .get_as(&format!("/notifications/{notice}"), Some(&c.jonah_cookie))
        .await;
    assert_eq!(response.location(), "/settings/access");
    let log = f.page_as("/settings/access", &c.jonah_cookie).await;
    assert!(log.contains("Looking into a complaint"), "{log}");
    assert!(
        log.contains(&format!("the thread № {} in General", c.quiet)),
        "{log}"
    );
    assert!(!log.contains("/mod/t/"), "{log}");
    shows_none("/settings/access", &log, &CLOSED);
    // Ruth, who still reads it, is told what it is.
    let notes = f.page_as("/notifications", &c.ruth_cookie).await;
    assert!(notes.contains("the thread “Quietwordtitle”"), "{notes}");

    // Sam lets only himself read the Circle, shutting Kit out: Kit is told
    // of a space he could read, and not which.
    let fields = [
        ("name", "Circle"),
        ("slug", "circle"),
        ("description", ""),
        ("read_level", "listed"),
        ("read_users", "sam"),
        ("reply_level", "listed"),
        ("reply_users", "sam"),
        ("start_level", "listed"),
        ("start_users", "sam"),
        ("administer_level", "listed"),
        ("section", ""),
        ("glass_reason", "Closing the Circle after a complaint"),
    ];
    ok(f, "/c/circle/settings", &fields, cookie).await;
    assert_status(
        &f.get_as("/c/circle", Some(&kit_cookie)).await,
        StatusCode::NOT_FOUND,
    );
    let notes = f.page_as("/notifications", &kit_cookie).await;
    assert!(notes.contains("a space you could read"), "{notes}");
    assert!(!notes.contains("the space Circle"), "{notes}");
    shows_none("/notifications", &notes, &CLOSED);
    let email = f.last_email_to("kit@example.com").await;
    assert!(
        email.body.contains(&format!(
            "A moderator (Sam) viewed a space you could read on {today}."
        )),
        "{}",
        email.body
    );
    assert!(!email.body.contains("space Circle"), "{}", email.body);
    let log = f.page_as("/settings/access", &kit_cookie).await;
    assert!(
        log.contains("Closing the Circle after a complaint"),
        "{log}"
    );
    assert!(log.contains("a space you could read"), "{log}");
    assert!(!log.contains("the space Circle"), "{log}");

    // Sam moves a thread out of the second circle into the staffroom: Kit
    // is told of a thread he could read, and not where it went.
    let hidden = f
        .thread(c.circle_two, c.kit, "Hiddentitle", "Hiddenbody")
        .await
        .thread_id;
    ok(
        f,
        &format!("/mod/t/{hidden}"),
        &[
            ("action", "move"),
            ("space", "staffroom"),
            ("reason", "Staff business"),
            ("glass_reason", "It belongs with the staff"),
        ],
        cookie,
    )
    .await;
    let notes = f.page_as("/notifications", &kit_cookie).await;
    assert!(notes.contains(">a thread you could read</a> on"), "{notes}");
    shows_none(
        "/notifications",
        &notes,
        &["Hiddentitle", "Hiddenbody", "Staffroom"],
    );
    let badge: serde_json::Value =
        serde_json::from_str(&f.page_as("/api/notifications", &kit_cookie).await).unwrap();
    assert_eq!(badge["unread"], 2, "{badge}");
    let email = f.last_email_to("kit@example.com").await;
    assert!(
        email.body.contains(&format!(
            "A moderator (Sam) viewed a thread you could read on {today}."
        )),
        "{}",
        email.body
    );
    shows_none("email", &email.body, &["Hiddentitle", "Staffroom"]);
}

/// Each member's log lists the times a moderator looked at what they
/// could read then, which they were told about (DESIGN.md §8): not what
/// came before they could read it, and still what they can't read any
/// more.
#[sqlx::test]
async fn each_members_log_is_what_they_were_told(pool: PgPool) {
    let c = club(pool).await;
    let f = &c.f;
    ok(
        f,
        "/mod/break-glass",
        &[
            ("space", "circle"),
            ("glass_reason", "A complaint about the Circle"),
        ],
        &c.sam_cookie,
    )
    .await;
    let kit_cookie = f.session(c.kit).await;
    let log = f.page_as("/settings/access", &kit_cookie).await;
    assert!(log.contains("A complaint about the Circle"), "{log}");
    // Ruth joins the Organisers afterwards: she reads the Circle now, but
    // wasn't told, so it isn't hers.
    f.join_group(c.organisers, c.ruth).await;
    assert!(
        f.page_as("/c/circle", &c.ruth_cookie)
            .await
            .contains("Circlesecrettitle")
    );
    let log = f.page_as("/settings/access", &c.ruth_cookie).await;
    assert!(log.contains("No moderator has needed"), "{log}");
    assert!(!log.contains("A complaint about the Circle"), "{log}");
    // Kit leaves them: it is still his, without the space's name.
    f.exec(&format!(
        "DELETE FROM group_members WHERE group_id = {} AND user_id = {}",
        c.organisers, c.kit
    ))
    .await;
    let log = f.page_as("/settings/access", &kit_cookie).await;
    assert!(log.contains("A complaint about the Circle"), "{log}");
    assert!(log.contains("a space you could read"), "{log}");
    shows_none("/settings/access", &log, &CLOSED);
}

/// Confirming again while a grant already covers it (the form sent twice,
/// or again from the browser's history) opens nothing more and tells
/// nobody again.
#[sqlx::test]
async fn confirming_twice_breaks_glass_once(pool: PgPool) {
    let c = club(pool).await;
    let f = &c.f;
    let thread = c.secret.to_string();
    let fields = [("thread", thread.as_str()), ("glass_reason", "A complaint")];
    for _ in 0..2 {
        let response = ok(f, "/mod/break-glass", &fields, &c.sam_cookie).await;
        assert_eq!(response.location(), format!("/t/{}", c.secret));
    }
    // A grant on its space covers it too.
    ok(
        f,
        "/mod/break-glass",
        &[("space", "circle"), ("glass_reason", "A complaint")],
        &c.sam_cookie,
    )
    .await;
    ok(f, "/mod/break-glass", &fields, &c.sam_cookie).await;
    assert_eq!(count(f, "SELECT count(*) FROM staff_access").await, 2);
    assert_eq!(told(f).await, vec![c.kit, c.kit]);
    // Two at once open one.
    let lunch_two = f
        .thread(c.circle_two, c.kit, "Othertitle", "Otherbody")
        .await
        .thread_id
        .to_string();
    let fields = [("thread", lunch_two.as_str()), ("glass_reason", "Again")];
    let (a, b) = tokio::join!(
        f.submit("/mod/break-glass", &fields, Some(&c.sam_cookie)),
        f.submit("/mod/break-glass", &fields, Some(&c.sam_cookie)),
    );
    assert_status(&a, StatusCode::SEE_OTHER);
    assert_status(&b, StatusCode::SEE_OTHER);
    assert_eq!(count(f, "SELECT count(*) FROM staff_access").await, 3);
    assert_eq!(told(f).await, vec![c.kit, c.kit, c.kit]);
}

/// A change that breaks glass and the grant it opens are made together:
/// if the grant can't be opened, or its readers told, the change isn't
/// made either, so that nobody reads by it unannounced (DESIGN.md §8).
#[sqlx::test]
async fn a_change_that_breaks_glass_needs_its_grant(pool: PgPool) {
    let c = club(pool).await;
    let f = &c.f;
    let cookie = &c.sam_cookie;
    let hidden = f
        .thread(c.circle_two, c.kit, "Hiddentitle", "Hiddenbody")
        .await
        .thread_id;
    // Telling anyone fails.
    f.exec(
        "CREATE FUNCTION refuse_notices() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN RAISE EXCEPTION 'no notices today'; END $$;
         CREATE TRIGGER refuse_notices BEFORE INSERT ON notifications
             FOR EACH ROW WHEN (NEW.kind = 'staff_access') EXECUTE FUNCTION refuse_notices();",
    )
    .await;
    let restriction = || async {
        sqlx::query_as::<_, (Option<Vec<i64>>, Option<Vec<i64>>)>(
            "SELECT read_group_ids, read_user_ids FROM threads WHERE id = $1",
        )
        .bind(c.quiet)
        .fetch_one(&f.db)
        .await
        .unwrap()
    };
    let before = restriction().await;
    let response = f
        .submit(
            &format!("/t/{}/audience", c.quiet),
            &[
                ("read", "space"),
                ("reply", "space"),
                ("glass_reason", "Opening it up"),
            ],
            Some(cookie),
        )
        .await;
    assert_status(&response, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(restriction().await, before);
    let response = f
        .submit(
            "/c/circle/settings",
            &[
                ("name", "Circle"),
                ("slug", "circle"),
                ("description", ""),
                ("read_level", "members"),
                ("reply_level", "listed"),
                ("reply_group", &c.organisers.to_string()),
                ("start_level", "listed"),
                ("start_group", &c.organisers.to_string()),
                ("administer_level", "listed"),
                ("section", ""),
                ("glass_reason", "Opening it up"),
            ],
            Some(cookie),
        )
        .await;
    assert_status(&response, StatusCode::INTERNAL_SERVER_ERROR);
    assert_status(
        &f.get_as("/c/circle", Some(cookie)).await,
        StatusCode::NOT_FOUND,
    );
    let response = f
        .submit(
            &format!("/mod/t/{hidden}"),
            &[
                ("action", "move"),
                ("space", "general"),
                ("reason", "Tidy"),
                ("glass_reason", "It belongs in General"),
            ],
            Some(cookie),
        )
        .await;
    assert_status(&response, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        count(
            f,
            &format!("SELECT space_id FROM threads WHERE id = {hidden}")
        )
        .await,
        c.circle_two
    );
    let response = f
        .submit(
            &format!("/admin/groups/{}/members", c.organisers),
            &[("username", "ada"), ("glass_reason", "Helping")],
            Some(&c.ada_cookie),
        )
        .await;
    assert_status(&response, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        count(
            f,
            &format!(
                "SELECT count(*) FROM group_members WHERE group_id = {} AND user_id = {}",
                c.organisers, c.ada
            )
        )
        .await,
        0
    );
    assert_eq!(count(f, "SELECT count(*) FROM staff_access").await, 0);
}

/// The confirmation page for a change says what the change does: the
/// member of staff reads by it for as long as it stands, not for 72
/// hours, and pages aren't recorded one by one.
#[sqlx::test]
async fn the_page_for_a_change_says_what_it_does(pool: PgPool) {
    let c = club(pool).await;
    let f = &c.f;
    let page = f
        .submit(
            &format!("/t/{}/audience", c.quiet),
            &[("read", "space"), ("reply", "space")],
            Some(&c.sam_cookie),
        )
        .await;
    assert_status(&page, StatusCode::OK);
    assert!(!page.body.contains("hours, until"), "{}", page.body);
    assert!(
        !page.body.contains("Every page of it you open is recorded"),
        "{}",
        page.body
    );
    assert!(
        page.body
            .contains("The pages you open aren’t recorded one by one"),
        "{}",
        page.body
    );
    assert!(
        page.body.contains("for as long as the change stands"),
        "{}",
        page.body
    );
    // Breaking glass to read something says how long and that each page
    // is recorded.
    let page = f
        .page_as(
            &format!("/mod/break-glass?thread={}", c.quiet),
            &c.sam_cookie,
        )
        .await;
    assert!(page.contains("72 hours"), "{page}");
    assert!(
        page.contains("Every page of it you open is recorded"),
        "{page}"
    );
}

/// Both access logs list every grant, a page at a time, newest first
/// (DESIGN.md §8: the log is visible to all admins and to each affected
/// person, and being append-only it keeps everything).
#[sqlx::test]
async fn the_access_logs_are_paged(pool: PgPool) {
    let c = club(pool).await;
    let f = &c.f;
    let mut tx = f.state.db().begin().await.unwrap();
    for n in 1..=120 {
        parlour::notify::open_staff_access(
            &mut tx,
            &NewAccess {
                actor_id: c.sam,
                scope: Scope::Thread(c.quiet),
                report_id: None,
                reason: &format!("Reason number {n:03}"),
                by_change: false,
            },
            &[c.ruth],
        )
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();
    for (path, cookie) in [
        ("/admin/access", &c.ada_cookie),
        ("/settings/access", &c.ruth_cookie),
    ] {
        let first = f.page_as(path, cookie).await;
        assert!(first.contains("Reason number 120"), "{first}");
        assert!(first.contains("Reason number 071"), "{first}");
        assert!(!first.contains("Reason number 070"), "{first}");
        assert!(
            first.contains(&format!("href=\"{path}?page=3\"")),
            "{first}"
        );
        let last = f.page_as(&format!("{path}?page=3"), cookie).await;
        assert!(last.contains("Reason number 001"), "{last}");
        assert!(last.contains("Reason number 020"), "{last}");
        assert!(!last.contains("Reason number 021"), "{last}");
        assert_status(
            &f.get_as(&format!("{path}?page=4"), Some(cookie)).await,
            StatusCode::NOT_FOUND,
        );
        let response = f.get_as(&format!("{path}?page=1"), Some(cookie)).await;
        assert!(response.status.is_redirection(), "{}", response.body);
        assert_eq!(response.location(), path);
    }
}

/// A grant opened by a change of who may read is shown in both logs as
/// lasting for as long as the change stands, without a closing time or a
/// count of pages, which aren't recorded for it (DESIGN.md §8).
#[sqlx::test]
async fn a_grant_by_a_change_lasts_while_the_change_stands(pool: PgPool) {
    let c = club(pool).await;
    let f = &c.f;
    ok(
        f,
        &format!("/t/{}/audience", c.quiet),
        &[
            ("read", "space"),
            ("reply", "space"),
            ("glass_reason", "Opening it up after a dispute"),
        ],
        &c.sam_cookie,
    )
    .await;
    for (path, cookie) in [
        ("/admin/access", &c.ada_cookie),
        ("/settings/access", &c.ruth_cookie),
    ] {
        let page = f.page_as(path, cookie).await;
        assert!(
            page.contains("reads it as one of its readers for as long as the change stands"),
            "{page}"
        );
        assert!(!page.contains("pages viewed"), "{page}");
        assert!(!page.contains("open until"), "{page}");
        assert!(!page.contains("closed"), "{page}");
    }
}

/// A grant opens what a moderator needs to see of what it opens beyond
/// its page (DESIGN.md §8): the images in it, each post's edit history, a
/// post's permalink and its moderation page, each recorded against the
/// address asked for. Without a grant, all of them stay closed.
#[sqlx::test]
async fn a_grant_opens_the_images_and_histories_in_what_it_opens(pool: PgPool) {
    let c = club(pool).await;
    let f = &c.f;
    // An image shown only in the Circle, and an edit to its post.
    let sha = "cd".repeat(32);
    f.exec(&format!(
        "INSERT INTO uploads (sha256, ext, mime, bytes, width, height)
         VALUES ('{sha}', 'png', 'image/png', 1, 1, 1)"
    ))
    .await;
    let image = format!("/uploads/{sha}.png");
    let post = f
        .post(c.secret, c.kit, &format!("Circlepicture ![]({image})"))
        .await;
    let kit_cookie = f.session(c.kit).await;
    ok(
        f,
        &format!("/p/{post}/edit"),
        &[("body", &format!("Circlepicture edited ![]({image})"))],
        &kit_cookie,
    )
    .await;
    f.exec(&format!(
        "UPDATE post_revisions SET created_at = created_at - interval '1 hour' WHERE post_id = {post};
         UPDATE posts SET created_at = created_at - interval '1 hour' WHERE id = {post}"
    ))
    .await;
    let paths = [
        format!("/p/{post}/revisions"),
        format!("/p/{post}"),
        format!("/mod/p/{post}"),
    ];
    assert_eq!(
        image_status(f, &image, &c.sam_cookie).await,
        StatusCode::NOT_FOUND
    );
    for path in &paths {
        assert_status(
            &f.get_as(path, Some(&c.sam_cookie)).await,
            StatusCode::NOT_FOUND,
        );
    }
    assert!(views(f).await.is_empty());
    ok(
        f,
        "/mod/break-glass",
        &[
            ("thread", &c.secret.to_string()),
            ("glass_reason", "A report about a picture"),
        ],
        &c.sam_cookie,
    )
    .await;
    let access = count(f, "SELECT max(id) FROM staff_access").await;
    assert_eq!(
        image_status(f, &image, &c.sam_cookie).await,
        StatusCode::NOT_MODIFIED
    );
    assert_eq!(views(f).await, vec![(access, image.clone())]);
    let history = f.page_as(&paths[0], &c.sam_cookie).await;
    assert!(history.contains("Circlepicture"), "{history}");
    assert!(history.contains("because you broke glass"), "{history}");
    let permalink = f.get_as(&paths[1], Some(&c.sam_cookie)).await;
    assert!(permalink.status.is_redirection(), "{}", permalink.body);
    assert!(
        permalink
            .location()
            .starts_with(&format!("/t/{}/", c.secret))
    );
    let moderate = f.page_as(&paths[2], &c.sam_cookie).await;
    assert!(moderate.contains("Circlepicture"), "{moderate}");
    let recorded: Vec<String> = views(f).await.into_iter().map(|(_, path)| path).collect();
    assert_eq!(
        recorded,
        vec![
            image.clone(),
            paths[0].clone(),
            paths[1].clone(),
            paths[2].clone()
        ]
    );
    // Ruth, who isn't in the Circle, still can't fetch the image.
    assert_eq!(
        image_status(f, &image, &c.ruth_cookie).await,
        StatusCode::NOT_FOUND
    );
}

/// A grant on a conversation opens its messages' permalinks as it does a
/// thread's posts' (DESIGN.md §8), recording the view.
#[sqlx::test]
async fn a_grant_on_a_conversation_opens_its_permalinks(pool: PgPool) {
    let c = club(pool).await;
    let f = &c.f;
    let path = format!("/p/{}", c.dm_post);
    assert_status(
        &f.get_as(&path, Some(&c.sam_cookie)).await,
        StatusCode::NOT_FOUND,
    );
    ok(
        f,
        "/mod/break-glass",
        &[
            ("thread", &c.dm.to_string()),
            ("glass_reason", "A report about a message"),
        ],
        &c.sam_cookie,
    )
    .await;
    let access = count(f, "SELECT max(id) FROM staff_access").await;
    let permalink = f.get_as(&path, Some(&c.sam_cookie)).await;
    assert_status(&permalink, StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(
        permalink.location(),
        format!("/messages/{}#p{}", c.dm, c.dm_post)
    );
    assert!(views(f).await.contains(&(access, path)));
}
