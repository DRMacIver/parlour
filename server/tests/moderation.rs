//! Moderation (milestone 7, DESIGN.md §7, §8): reports and the contact
//! form, the queue, moderators' actions and the logs, held posts and the
//! other limits on new members, silences and suspensions, staff notes and
//! addresses, and appeals, over HTTP as a browser without JavaScript would.

mod common;

use axum::http::{StatusCode, header};
use common::{Forum, Response, post_html};
use parlour::settings::{self, MessageSettings, ModerationSettings};
use parlour::store::{self, NewConversation};
use sqlx::PgPool;

/// A forum with a public space `games` and a members-only `lounge`;
/// members ruth, jonah and maya, who don't count as new; and staff: sam,
/// an admin, and mo, a moderator. Everyone has a session.
struct Setup {
    f: Forum,
    games: i64,
    lounge: i64,
    ruth: i64,
    ruth_cookie: String,
    jonah: i64,
    jonah_cookie: String,
    maya: i64,
    maya_cookie: String,
    sam_cookie: String,
    mo_cookie: String,
}

async fn setup(pool: PgPool) -> Setup {
    let f = Forum::new(pool).trusting_proxy();
    f.no_new_members().await;
    let games = f.space("games", "public", None, 0).await;
    let lounge = f.space("lounge", "members", None, 1).await;
    let ruth = f.member("ruth", "user", None).await;
    let jonah = f.member("jonah", "user", None).await;
    let maya = f.member("maya", "user", None).await;
    let sam = f.member("sam", "admin", None).await;
    let mo = f.member("mo", "moderator", None).await;
    Setup {
        ruth_cookie: f.session(ruth).await,
        jonah_cookie: f.session(jonah).await,
        maya_cookie: f.session(maya).await,
        sam_cookie: f.session(sam).await,
        mo_cookie: f.session(mo).await,
        f,
        games,
        lounge,
        ruth,
        jonah,
        maya,
    }
}

fn assert_status(response: &Response, status: StatusCode) {
    assert_eq!(response.status, status, "{}", response.body);
}

async fn count(f: &Forum, sql: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(sql.to_owned()))
        .fetch_one(&f.db)
        .await
        .unwrap()
}

async fn text(f: &Forum, sql: &str) -> String {
    sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(sql.to_owned()))
        .fetch_one(&f.db)
        .await
        .unwrap()
}

/// Submits a form and expects to be sent on with a 303.
async fn ok(f: &Forum, path: &str, fields: &[(&str, &str)], cookie: &str) -> Response {
    let response = f.submit(path, fields, Some(cookie)).await;
    assert_status(&response, StatusCode::SEE_OTHER);
    response
}

async fn report(f: &Forum, cookie: &str, post: i64, reason: &str, details: &str) -> Response {
    f.submit(
        &format!("/p/{post}/report"),
        &[("reason", reason), ("details", details)],
        Some(cookie),
    )
    .await
}

/// The id of the newest moderation action.
async fn last_action(f: &Forum) -> i64 {
    count(f, "SELECT max(id) FROM mod_actions").await
}

// ---------------------------------------------------------------------------
// Reports and the queue

#[sqlx::test]
async fn members_report_posts_and_staff_see_them_in_the_queue(pool: PgPool) {
    let s = setup(pool).await;
    let t =
        s.f.thread(
            s.games,
            s.jonah,
            "Cheap watches",
            "Buy replica watches here",
        )
        .await;
    let form =
        s.f.page_as(&format!("/p/{}/report", t.first_post_id), &s.ruth_cookie)
            .await;
    assert!(form.contains("Buy replica watches here"), "{form}");
    let response = report(
        &s.f,
        &s.ruth_cookie,
        t.first_post_id,
        "spam",
        "Obvious spam",
    )
    .await;
    assert_status(&response, StatusCode::OK);
    assert!(response.body.contains("Thank you"));
    // Reporting again while the first is open is refused.
    let again = report(&s.f, &s.ruth_cookie, t.first_post_id, "spam", "").await;
    assert_status(&again, StatusCode::FORBIDDEN);
    // A second member's report joins the same item.
    let response = report(&s.f, &s.maya_cookie, t.first_post_id, "spam", "").await;
    assert_status(&response, StatusCode::OK);
    // A reason must be one of those offered.
    let bad = report(&s.f, &s.maya_cookie, t.first_post_id, "bogus", "").await;
    assert_status(&bad, StatusCode::BAD_REQUEST);

    for cookie in [&s.sam_cookie, &s.mo_cookie] {
        let queue = s.f.page_as("/mod/queue", cookie).await;
        assert!(queue.contains("Buy replica watches here"), "{queue}");
        assert!(queue.contains("reported by ruth and maya"), "{queue}");
        assert!(queue.contains("Obvious spam"), "{queue}");
        assert!(queue.contains("1 item needs attention"), "{queue}");
    }
    // The badge in the header counts it, for staff only.
    let page = s.f.page_as("/", &s.mo_cookie).await;
    assert!(page.contains("href=\"/mod/queue\""), "{page}");
    let page = s.f.page_as("/", &s.ruth_cookie).await;
    assert!(!page.contains("href=\"/mod/queue\""));
    // Nobody else gets in: members are told there's nothing there, and
    // visitors are asked to log in, without a cookie.
    for path in ["/mod/queue", "/mod/log", "/mod/users", "/mod/users/jonah"] {
        let response = s.f.get_as(path, Some(&s.ruth_cookie)).await;
        assert_status(&response, StatusCode::NOT_FOUND);
        let response = s.f.get(path).await;
        assert_status(&response, StatusCode::SEE_OTHER);
        assert!(response.location().starts_with("/login?next="));
        assert!(response.header(header::SET_COOKIE).is_none());
    }
    // Members can't act either.
    let response =
        s.f.submit(
            &format!("/mod/p/{}", t.first_post_id),
            &[("action", "hide"), ("reason", "x")],
            Some(&s.ruth_cookie),
        )
        .await;
    assert_status(&response, StatusCode::NOT_FOUND);
    assert_eq!(
        count(
            &s.f,
            "SELECT count(*) FROM posts WHERE hidden_at IS NOT NULL"
        )
        .await,
        0
    );
}

/// The queue offers to suspend an invite branch only when there is one,
/// as the member's page does.
#[sqlx::test]
async fn the_queue_offers_a_branch_suspension_only_when_there_is_a_branch(pool: PgPool) {
    let s = setup(pool).await;
    let t = s.f.thread(s.games, s.jonah, "Watches", "Buy watches").await;
    report(&s.f, &s.ruth_cookie, t.first_post_id, "spam", "").await;
    s.f.submit(
        "/u/maya/report",
        &[("reason", "spam")],
        Some(&s.ruth_cookie),
    )
    .await;
    let queue = s.f.page_as("/mod/queue", &s.mo_cookie).await;
    assert!(queue.contains("/mod/users/jonah/suspend\""), "{queue}");
    assert!(queue.contains("/mod/users/maya/suspend\""), "{queue}");
    assert!(!queue.contains("suspend-branch"), "{queue}");

    s.f.invitee("felix", s.jonah).await;
    let gus = s.f.invitee("gus", s.maya).await;
    s.f.invitee("hal", gus).await;
    let queue = s.f.page_as("/mod/queue", &s.mo_cookie).await;
    assert!(queue.contains("/mod/users/jonah/suspend-branch"), "{queue}");
    assert!(queue.contains("/mod/users/maya/suspend-branch"), "{queue}");
    assert!(
        queue.contains("Suspend invite branch (2 accounts)"),
        "{queue}"
    );
    assert!(
        queue.contains("Suspend invite branch (3 accounts)"),
        "{queue}"
    );
}

#[sqlx::test]
async fn members_report_members(pool: PgPool) {
    let s = setup(pool).await;
    let page = s.f.page_as("/u/jonah", &s.ruth_cookie).await;
    assert!(page.contains("/u/jonah/report"), "{page}");
    let response =
        s.f.submit(
            "/u/jonah/report",
            &[("reason", "harassment"), ("details", "Keeps messaging me")],
            Some(&s.ruth_cookie),
        )
        .await;
    assert_status(&response, StatusCode::OK);
    let response =
        s.f.submit(
            "/u/ruth/report",
            &[("reason", "spam")],
            Some(&s.ruth_cookie),
        )
        .await;
    assert_status(&response, StatusCode::FORBIDDEN);
    let queue = s.f.page_as("/mod/queue", &s.mo_cookie).await;
    assert!(queue.contains("Report on a member"), "{queue}");
    assert!(queue.contains("Keeps messaging me"), "{queue}");
    // Dismissing settles it, and is logged for staff only.
    let id = count(&s.f, "SELECT id FROM reports").await;
    ok(
        &s.f,
        &format!("/mod/reports/{id}/dismiss"),
        &[("reason", "Nothing wrong")],
        &s.mo_cookie,
    )
    .await;
    let queue = s.f.page_as("/mod/queue", &s.mo_cookie).await;
    assert!(queue.contains("The queue is empty"), "{queue}");
    let log = s.f.page_as("/mod/log", &s.mo_cookie).await;
    assert!(log.contains("dismissed a report about"), "{log}");
    let public = s.f.page("/moderation").await;
    assert!(!public.contains("dismissed"), "{public}");
}

#[sqlx::test]
async fn a_reported_conversation_stays_closed_to_staff(pool: PgPool) {
    let s = setup(pool).await;
    let start = |cookie: String, to: &'static str, title: &'static str, body: &'static str| {
        let f = &s.f;
        async move {
            let response = f
                .submit(
                    "/messages/new",
                    &[("to", to), ("title", title), ("body", body)],
                    Some(&cookie),
                )
                .await;
            assert_status(&response, StatusCode::SEE_OTHER);
            let location = response.location().to_owned();
            location
                .trim_start_matches("/messages/")
                .split(|c: char| !c.is_ascii_digit())
                .next()
                .unwrap()
                .parse::<i64>()
                .unwrap()
        }
    };
    let reported = start(
        s.jonah_cookie.clone(),
        "ruth",
        "Games",
        "come on, one more game, why are you ignoring me",
    )
    .await;
    let other = start(
        s.jonah_cookie.clone(),
        "maya",
        "Other",
        "an entirely private aside",
    )
    .await;
    let post = count(
        &s.f,
        &format!("SELECT max(id) FROM posts WHERE thread_id = {reported}"),
    )
    .await;
    // Before the report, staff can't read it.
    let response =
        s.f.get_as(&format!("/messages/{reported}"), Some(&s.mo_cookie))
            .await;
    assert_status(&response, StatusCode::NOT_FOUND);
    // Only those in it can report its messages.
    let response = report(&s.f, &s.maya_cookie, post, "harassment", "").await;
    assert_status(&response, StatusCode::NOT_FOUND);
    let form =
        s.f.page_as(&format!("/p/{post}/report"), &s.ruth_cookie)
            .await;
    assert!(
        form.contains("everyone in it will be told a moderator looked"),
        "{form}"
    );
    let response = report(
        &s.f,
        &s.ruth_cookie,
        post,
        "harassment",
        "Asked him to stop twice",
    )
    .await;
    assert_status(&response, StatusCode::OK);

    for cookie in [&s.mo_cookie, &s.sam_cookie] {
        // The queue says who wrote what was reported, where, and why, but
        // staff can't read the conversation (DESIGN.md §3.9, §8): a report
        // carries no content.
        let queue = s.f.page_as("/mod/queue", cookie).await;
        assert!(queue.contains("Report on a message"), "{queue}");
        assert!(queue.contains("Asked him to stop twice"), "{queue}");
        assert!(queue.contains("A conversation you aren’t in"), "{queue}");
        assert!(!queue.contains("why are you ignoring me"), "{queue}");
        for path in [
            format!("/messages/{reported}"),
            format!("/messages/{other}"),
            format!("/p/{post}"),
            format!("/mod/p/{post}"),
        ] {
            let response = s.f.get_as(&path, Some(cookie)).await;
            assert_status(&response, StatusCode::NOT_FOUND);
            assert!(!response.body.contains("why are you ignoring me"));
            assert!(!response.body.contains("an entirely private aside"));
        }
    }
    // Under a break-glass grant on it, a moderator reads that
    // conversation and no other, without being in it: they can't write
    // there, and it isn't in their list.
    let mo: i64 = s.f.user_id("mo").await;
    parlour::staff_access::open(
        &s.f.db,
        &parlour::staff_access::NewAccess {
            actor_id: mo,
            scope: parlour::staff_access::Scope::Thread(reported),
            report_id: None,
            reason: "The report",
        },
    )
    .await
    .unwrap();
    let page =
        s.f.page_as(&format!("/messages/{reported}"), &s.mo_cookie)
            .await;
    assert!(page.contains("why are you ignoring me"), "{page}");
    assert!(page.contains("You aren’t in this conversation"), "{page}");
    assert!(!page.contains("id=\"reply-body\""), "{page}");
    let response =
        s.f.submit(
            &format!("/messages/{reported}"),
            &[("body", "staff butting in")],
            Some(&s.mo_cookie),
        )
        .await;
    assert!(
        response.status == StatusCode::FORBIDDEN || response.status == StatusCode::NOT_FOUND,
        "{}",
        response.body
    );
    let list = s.f.page_as("/messages", &s.mo_cookie).await;
    assert!(!list.contains("why are you ignoring me"), "{list}");
    let response =
        s.f.get_as(&format!("/messages/{other}"), Some(&s.mo_cookie))
            .await;
    assert_status(&response, StatusCode::NOT_FOUND);
    // Other members still can't.
    let response =
        s.f.get_as(&format!("/messages/{reported}"), Some(&s.maya_cookie))
            .await;
    assert_status(&response, StatusCode::NOT_FOUND);
}

#[sqlx::test]
async fn the_contact_form_works_without_an_account(pool: PgPool) {
    let s = setup(pool).await;
    let page = s.f.get("/contact").await;
    assert_status(&page, StatusCode::OK);
    assert!(page.header(header::SET_COOKIE).is_none());
    // From a post's report link, the message starts with its address.
    let t = s.f.thread(s.games, s.jonah, "Thread", "Body").await;
    let page =
        s.f.page(&format!("/contact?post={}", t.first_post_id))
            .await;
    assert!(page.contains(&format!("/p/{}", t.first_post_id)), "{page}");
    let thread_page = s.f.page(&format!("/t/{}/thread", t.thread_id)).await;
    assert!(
        thread_page.contains(&format!("/contact?post={}", t.first_post_id)),
        "{thread_page}"
    );

    let response =
        s.f.submit_as_ip(
            "/contact",
            &[
                ("email", "worried@example.org"),
                ("message", "Someone posted my address in Games"),
            ],
            "203.0.113.5",
        )
        .await;
    assert_status(&response, StatusCode::SEE_OTHER);
    assert!(response.header(header::SET_COOKIE).is_none());
    assert_eq!(response.location(), "/contact?sent=1");
    let response =
        s.f.submit_as_ip(
            "/contact",
            &[("email", "not an address"), ("message", "x")],
            "203.0.113.5",
        )
        .await;
    assert_status(&response, StatusCode::BAD_REQUEST);
    let response =
        s.f.submit_as_ip("/contact", &[("message", "  ")], "203.0.113.5")
            .await;
    assert_status(&response, StatusCode::BAD_REQUEST);
    // It is rate limited by address.
    let mut limited = false;
    for i in 0..5 {
        let response =
            s.f.submit_as_ip(
                "/contact",
                &[("message", &format!("again {i}"))],
                "203.0.113.5",
            )
            .await;
        assert!(response.header(header::SET_COOKIE).is_none());
        if response.status == StatusCode::TOO_MANY_REQUESTS {
            limited = true;
            break;
        }
        assert_status(&response, StatusCode::SEE_OTHER);
    }
    assert!(limited, "the contact form was never rate limited");
    // Another address is unaffected.
    let response =
        s.f.submit_as_ip("/contact", &[("message", "from elsewhere")], "198.51.100.7")
            .await;
    assert_status(&response, StatusCode::SEE_OTHER);

    let queue = s.f.page_as("/mod/queue", &s.mo_cookie).await;
    assert!(
        queue.contains("Someone posted my address in Games"),
        "{queue}"
    );
    assert!(queue.contains("mailto:worried@example.org"), "{queue}");
    assert!(queue.contains("from a visitor"), "{queue}");
}

/// The contact form's `?post=` only names posts the visitor can see.
/// Anything else is not found, so that made-up ids can't fill the
/// anonymous page cache with one page each.
#[sqlx::test]
async fn the_contact_form_only_names_posts_the_visitor_can_see(pool: PgPool) {
    let s = setup(pool).await;
    let public = s.f.thread(s.games, s.jonah, "Public", "Body").await;
    let private = s.f.thread(s.lounge, s.jonah, "Private", "Body").await;
    s.f.page(&format!("/contact?post={}", public.first_post_id))
        .await;
    for post in [private.first_post_id, 999_999_999] {
        let response = s.f.get(&format!("/contact?post={post}")).await;
        assert_status(&response, StatusCode::NOT_FOUND);
    }
    // A member who can read the post may start a message about it.
    s.f.page_as(
        &format!("/contact?post={}", private.first_post_id),
        &s.ruth_cookie,
    )
    .await;
    assert_eq!(
        s.f.state.cache().len(),
        1,
        "only the public post's page is cached"
    );
}

// ---------------------------------------------------------------------------
// Posts

#[sqlx::test]
async fn hidden_posts_are_tombstones_for_all_but_staff(pool: PgPool) {
    let s = setup(pool).await;
    let t = s.f.thread(s.games, s.ruth, "Venues", "Opening post").await;
    let post =
        s.f.post(t.thread_id, s.jonah, "Ruth lives at 12 Secret Street")
            .await;
    report(&s.f, &s.maya_cookie, post, "personal", "").await;
    let report_id = count(&s.f, "SELECT id FROM reports").await;
    // Warm the anonymous cache first: hiding must clear it.
    let path = format!("/t/{}/venues", t.thread_id);
    assert!(s.f.page(&path).await.contains("12 Secret Street"));
    let response = ok(
        &s.f,
        &format!("/mod/p/{post}"),
        &[
            ("action", "hide"),
            ("reason", "Personal information"),
            ("report", &report_id.to_string()),
            ("back", "/mod/queue?done=1"),
        ],
        &s.mo_cookie,
    )
    .await;
    assert_eq!(response.location(), "/mod/queue?done=1");
    // Visitors and members see a tombstone.
    for page in [
        s.f.page(&path).await,
        s.f.page_as(&path, &s.ruth_cookie).await,
        s.f.page_as(&path, &s.jonah_cookie).await,
    ] {
        assert!(!page.contains("12 Secret Street"));
        assert!(post_html(&page, post).contains("Post hidden by a moderator"));
    }
    // Staff still see it, marked.
    let page = s.f.page_as(&path, &s.sam_cookie).await;
    let html = post_html(&page, post);
    assert!(html.contains("12 Secret Street"), "{html}");
    assert!(html.contains("Hidden from members"), "{html}");
    // Its history is hidden with it, except from staff.
    let history = s.f.get(&format!("/p/{post}/revisions")).await;
    assert_status(&history, StatusCode::NOT_FOUND);
    s.f.page_as(&format!("/p/{post}/revisions"), &s.mo_cookie)
        .await;
    // The report is settled.
    assert_eq!(
        text(
            &s.f,
            &format!("SELECT status FROM reports WHERE id = {report_id}")
        )
        .await,
        "resolved"
    );
    // The author is told, and may contest it.
    let notes = s.f.page_as("/notifications", &s.jonah_cookie).await;
    assert!(
        notes.contains("A moderator hid your post № 2 in a thread. Reason: Personal information"),
        "{notes}"
    );
    let action = last_action(&s.f).await;
    assert!(notes.contains(&format!("/appeal/{action}")), "{notes}");
    // The public log says so, without the post's text.
    let public = s.f.page("/moderation").await;
    assert!(public.contains("hid"), "{public}");
    assert!(public.contains("Personal information"), "{public}");
    assert!(public.contains("Venues"), "{public}");
    assert!(!public.contains("12 Secret Street"));
    // Showing it again brings it back for everyone.
    ok(
        &s.f,
        &format!("/mod/p/{post}"),
        &[("action", "unhide"), ("reason", "Address was fake")],
        &s.mo_cookie,
    )
    .await;
    assert!(s.f.page(&path).await.contains("12 Secret Street"));
}

#[sqlx::test]
async fn moderators_delete_posts(pool: PgPool) {
    let s = setup(pool).await;
    let t = s.f.thread(s.games, s.ruth, "Thread", "Opening").await;
    let post = s.f.post(t.thread_id, s.jonah, "Delete me please").await;
    let response =
        s.f.submit(
            &format!("/mod/p/{post}"),
            &[("action", "delete"), ("reason", "  ")],
            Some(&s.mo_cookie),
        )
        .await;
    assert_status(&response, StatusCode::BAD_REQUEST);
    ok(
        &s.f,
        &format!("/mod/p/{post}"),
        &[("action", "delete"), ("reason", "Off topic")],
        &s.mo_cookie,
    )
    .await;
    for cookie in [None, Some(s.sam_cookie.as_str())] {
        let response =
            s.f.get_as(&format!("/t/{}/thread", t.thread_id), cookie)
                .await;
        assert!(!response.body.contains("Delete me please"));
        assert!(
            post_html(&response.body, post).contains("Post deleted by a moderator"),
            "{}",
            response.body
        );
    }
    // A moderator's deletion can be undone.
    ok(
        &s.f,
        &format!("/mod/p/{post}"),
        &[("action", "restore"), ("reason", "Mistake")],
        &s.mo_cookie,
    )
    .await;
    assert!(
        s.f.page(&format!("/t/{}/thread", t.thread_id))
            .await
            .contains("Delete me please")
    );
}

#[sqlx::test]
async fn staff_edit_posts_with_a_reason(pool: PgPool) {
    let s = setup(pool).await;
    let t =
        s.f.thread(s.games, s.jonah, "Thread", "My number is 0123 456")
            .await;
    let form =
        s.f.page_as(&format!("/p/{}/edit", t.first_post_id), &s.mo_cookie)
            .await;
    assert!(form.contains("name=\"reason\""), "{form}");
    // Authors aren't asked for a reason.
    let form =
        s.f.page_as(&format!("/p/{}/edit", t.first_post_id), &s.jonah_cookie)
            .await;
    assert!(!form.contains("name=\"reason\""), "{form}");
    // Other members can't edit it.
    let response =
        s.f.submit(
            &format!("/p/{}/edit", t.first_post_id),
            &[("body", "hacked")],
            Some(&s.ruth_cookie),
        )
        .await;
    assert_status(&response, StatusCode::FORBIDDEN);
    let response =
        s.f.submit(
            &format!("/p/{}/edit", t.first_post_id),
            &[("body", "My number is [removed]")],
            Some(&s.mo_cookie),
        )
        .await;
    assert_status(&response, StatusCode::BAD_REQUEST);
    ok(
        &s.f,
        &format!("/p/{}/edit", t.first_post_id),
        &[
            ("body", "My number is [removed]"),
            ("reason", "Phone number"),
        ],
        &s.mo_cookie,
    )
    .await;
    let page = s.f.page(&format!("/t/{}/thread", t.thread_id)).await;
    assert!(page.contains("My number is [removed]"));
    let log = s.f.page("/moderation").await;
    assert!(log.contains("edited"), "{log}");
    assert!(log.contains("Phone number"), "{log}");
    let notes = s.f.page_as("/notifications", &s.jonah_cookie).await;
    assert!(notes.contains("A moderator edited your post"), "{notes}");
}

#[sqlx::test]
async fn moderators_redact_earlier_versions(pool: PgPool) {
    let s = setup(pool).await;
    let t =
        s.f.thread(s.games, s.jonah, "Thread", "Lives at 9 Elm Road")
            .await;
    s.f.exec(&format!(
        "UPDATE posts SET created_at = now() - interval '1 hour' WHERE id = {}",
        t.first_post_id
    ))
    .await;
    ok(
        &s.f,
        &format!("/p/{}/edit", t.first_post_id),
        &[("body", "Lives nearby")],
        &s.jonah_cookie,
    )
    .await;
    let path = format!("/p/{}/revisions?v=1", t.first_post_id);
    assert!(s.f.page(&path).await.contains("9 Elm Road"));
    let page = s.f.page_as(&path, &s.mo_cookie).await;
    assert!(page.contains("Redact version 1"), "{page}");
    // Members aren't offered it, and can't do it.
    let page = s.f.page_as(&path, &s.ruth_cookie).await;
    assert!(!page.contains("Redact version"));
    let redact = format!("/mod/p/{}/revisions/1/redact", t.first_post_id);
    let response =
        s.f.submit(&redact, &[("reason", "Address")], Some(&s.ruth_cookie))
            .await;
    assert_status(&response, StatusCode::NOT_FOUND);
    ok(&s.f, &redact, &[("reason", "Home address")], &s.mo_cookie).await;
    for cookie in [None, Some(s.mo_cookie.as_str())] {
        let page = s.f.get_as(&path, cookie).await;
        assert!(!page.body.contains("9 Elm Road"));
        assert!(
            page.body.contains("redacted by a moderator"),
            "{}",
            page.body
        );
    }
    // The text is gone from the database too.
    assert_eq!(
        count(
            &s.f,
            "SELECT count(*) FROM post_revisions WHERE body_md LIKE '%Elm Road%'"
        )
        .await,
        0
    );
    // Redacting it again, or the current version, is refused.
    let response =
        s.f.submit(&redact, &[("reason", "again")], Some(&s.mo_cookie))
            .await;
    assert_status(&response, StatusCode::FORBIDDEN);
    let response =
        s.f.submit(
            &format!("/mod/p/{}/revisions/2/redact", t.first_post_id),
            &[("reason", "current")],
            Some(&s.mo_cookie),
        )
        .await;
    assert_status(&response, StatusCode::FORBIDDEN);
    let log = s.f.page("/moderation").await;
    assert!(log.contains("redacted version 1"), "{log}");
}

// ---------------------------------------------------------------------------
// Threads

#[sqlx::test]
async fn moderators_lock_pin_rename_move_and_delete_threads(pool: PgPool) {
    let s = setup(pool).await;
    let t = s.f.thread(s.games, s.jonah, "Old title", "Opening").await;
    let action = format!("/mod/t/{}", t.thread_id);
    let page = s.f.page_as(&action, &s.mo_cookie).await;
    assert!(page.contains("Move posts"), "{page}");
    let thread_page =
        s.f.page_as(&format!("/t/{}/old-title", t.thread_id), &s.mo_cookie)
            .await;
    assert!(thread_page.contains(&format!("href=\"{action}\"")));

    ok(
        &s.f,
        &action,
        &[("action", "lock"), ("reason", "Heated")],
        &s.mo_cookie,
    )
    .await;
    let response =
        s.f.submit(
            &format!("/t/{}/reply", t.thread_id),
            &[("body", "One more thing")],
            Some(&s.ruth_cookie),
        )
        .await;
    assert_status(&response, StatusCode::FORBIDDEN);
    ok(
        &s.f,
        &action,
        &[("action", "pin"), ("reason", "Useful")],
        &s.mo_cookie,
    )
    .await;
    let space = s.f.page("/c/games").await;
    assert!(
        space.contains("Pinned") || space.contains("pinned"),
        "{space}"
    );

    ok(
        &s.f,
        &action,
        &[
            ("action", "rename"),
            ("title", "New title"),
            ("reason", "Clearer"),
        ],
        &s.mo_cookie,
    )
    .await;
    let response = s.f.get(&format!("/t/{}/old-title", t.thread_id)).await;
    assert_status(&response, StatusCode::PERMANENT_REDIRECT);
    assert_eq!(response.location(), format!("/t/{}/new-title", t.thread_id));
    let log = s.f.page("/moderation").await;
    assert!(log.contains("renamed"), "{log}");
    assert!(log.contains("it was “Old title”"), "{log}");

    // Moving it into the members-only lounge takes it out of public view,
    // and the public log stops naming it.
    ok(
        &s.f,
        &action,
        &[
            ("action", "move"),
            ("space", "lounge"),
            ("reason", "Members only"),
        ],
        &s.mo_cookie,
    )
    .await;
    let response = s.f.get(&format!("/t/{}/new-title", t.thread_id)).await;
    assert_status(&response, StatusCode::NOT_FOUND);
    let log = s.f.page("/moderation").await;
    assert!(!log.contains("New title"), "{log}");
    assert!(!log.contains("Old title"), "{log}");
    assert!(log.contains("a thread you can’t see"), "{log}");
    // The move itself, into a members-only space, isn't public.
    assert!(!log.contains("Members only"), "{log}");
    let staff_log = s.f.page_as("/mod/log", &s.mo_cookie).await;
    assert!(staff_log.contains("Members only"), "{staff_log}");

    ok(
        &s.f,
        &action,
        &[("action", "delete"), ("reason", "Spam")],
        &s.mo_cookie,
    )
    .await;
    let response =
        s.f.get_as(
            &format!("/t/{}/new-title", t.thread_id),
            Some(&s.ruth_cookie),
        )
        .await;
    assert_status(&response, StatusCode::NOT_FOUND);
    let notes = s.f.page_as("/notifications", &s.jonah_cookie).await;
    assert!(
        notes.contains("A moderator deleted one of your threads"),
        "{notes}"
    );
    assert!(
        notes.contains("A moderator locked one of your threads"),
        "{notes}"
    );
    // Notices don't name threads, which may since have moved out of reach.
    assert!(!notes.contains("New title"), "{notes}");
    ok(
        &s.f,
        &action,
        &[("action", "restore"), ("reason", "Oops")],
        &s.mo_cookie,
    )
    .await;
    s.f.page_as(&format!("/t/{}/new-title", t.thread_id), &s.ruth_cookie)
        .await;
}

/// Splitting: posts moved to a new thread are numbered from 1 there, keep
/// their ids (so permalinks follow them) and leave their numbers behind.
#[sqlx::test]
async fn moving_posts_to_a_new_thread_keeps_numbers_and_links(pool: PgPool) {
    let s = setup(pool).await;
    let t =
        s.f.thread(s.games, s.ruth, "Spirit Island", "Post one")
            .await;
    let mut posts = vec![t.first_post_id];
    for i in 2..=6 {
        posts.push(s.f.post(t.thread_id, s.jonah, &format!("Post {i}")).await);
    }
    // № 4 replies to № 3 (which moves with it) and № 6 to № 2 (which
    // stays while № 6 moves).
    s.f.exec(&format!(
        "UPDATE posts SET reply_to_post_id = {} WHERE id = {}",
        posts[2], posts[3]
    ))
    .await;
    s.f.exec(&format!(
        "UPDATE posts SET reply_to_post_id = {} WHERE id = {}",
        posts[1], posts[5]
    ))
    .await;
    let response =
        s.f.submit(
            &format!("/mod/t/{}/move-posts", t.thread_id),
            &[
                ("numbers", "3-4, 6"),
                ("to", "new"),
                ("title", "Spirit Island strategy"),
                ("space", "games"),
                ("reason", "Split off"),
            ],
            Some(&s.mo_cookie),
        )
        .await;
    assert_status(&response, StatusCode::SEE_OTHER);
    let new_thread = count(&s.f, "SELECT max(id) FROM threads").await;
    assert!(
        response
            .location()
            .starts_with(&format!("/t/{new_thread}/"))
    );
    // The new thread holds them as № 1-3, in order.
    let seqs = |id: i64| {
        let f = &s.f;
        async move {
            sqlx::query_as::<_, (i64, i32)>(sqlx::AssertSqlSafe(format!(
                "SELECT thread_id, seq FROM posts WHERE id = {id}"
            )))
            .fetch_one(&f.db)
            .await
            .unwrap()
        }
    };
    assert_eq!(seqs(posts[2]).await, (new_thread, 1));
    assert_eq!(seqs(posts[3]).await, (new_thread, 2));
    assert_eq!(seqs(posts[5]).await, (new_thread, 3));
    assert_eq!(seqs(posts[4]).await, (t.thread_id, 5));
    let new_page =
        s.f.page(&format!("/t/{new_thread}/spirit-island-strategy"))
            .await;
    assert!(
        new_page.contains("Post 3") && new_page.contains("Post 4") && new_page.contains("Post 6")
    );
    // The reply between moved posts survives; the one across threads goes.
    assert!(
        post_html(&new_page, posts[3]).contains("in reply to"),
        "{new_page}"
    );
    assert!(!post_html(&new_page, posts[5]).contains("in reply to"));
    // The old thread keeps every number: the moved ones say where they went.
    let old = s.f.page(&format!("/t/{}/spirit-island", t.thread_id)).await;
    for (i, id) in posts.iter().enumerate() {
        assert!(
            old.contains(&format!("id=\"p{id}\"")),
            "№ {} missing",
            i + 1
        );
    }
    let moved = post_html(&old, posts[2]);
    assert!(moved.contains("Moved to"), "{moved}");
    assert!(moved.contains("Spirit Island strategy"), "{moved}");
    assert!(!old.contains("Post 3"));
    assert!(old.contains("Post 5"));
    // Permalinks lead to the posts' new places.
    let response = s.f.get(&format!("/p/{}", posts[3])).await;
    assert_eq!(
        response.location(),
        format!("/t/{new_thread}/spirit-island-strategy#p{}", posts[3])
    );
    // The old thread's count of posts and the first-unread jump still work.
    s.f.exec(&format!(
        "INSERT INTO thread_reads (user_id, thread_id, last_read_seq) VALUES ({}, {}, 2)
         ON CONFLICT (user_id, thread_id) DO UPDATE SET last_read_seq = 2",
        s.ruth, t.thread_id
    ))
    .await;
    let response =
        s.f.get_as(&format!("/t/{}/unread", t.thread_id), Some(&s.ruth_cookie))
            .await;
    assert_status(&response, StatusCode::TEMPORARY_REDIRECT);
    assert!(
        response.location().ends_with(&format!("#p{}", posts[4])),
        "{}",
        response.location()
    );
    let log = s.f.page("/moderation").await;
    assert!(log.contains("moved 3 posts from"), "{log}");
}

/// Merging: moving every post to an existing thread appends them there and
/// leaves the old thread leading to it.
#[sqlx::test]
async fn moving_all_posts_merges_threads(pool: PgPool) {
    let s = setup(pool).await;
    let target = s.f.thread(s.games, s.ruth, "Main thread", "Main one").await;
    s.f.post(target.thread_id, s.ruth, "Main two").await;
    let dupe = s.f.thread(s.games, s.jonah, "Duplicate", "Dupe one").await;
    let dupe_two = s.f.post(dupe.thread_id, s.maya, "Dupe two").await;
    // A link to the thread works as well as its number.
    let response =
        s.f.submit(
            &format!("/mod/t/{}/move-posts", dupe.thread_id),
            &[
                ("numbers", "1-2"),
                ("to", "existing"),
                (
                    "thread",
                    &format!("http://localhost:8080/t/{}/main-thread", target.thread_id),
                ),
                ("reason", "Merge duplicates"),
            ],
            Some(&s.mo_cookie),
        )
        .await;
    assert_status(&response, StatusCode::SEE_OTHER);
    let page =
        s.f.page(&format!("/t/{}/main-thread", target.thread_id))
            .await;
    let order: Vec<usize> = ["Main one", "Main two", "Dupe one", "Dupe two"]
        .iter()
        .map(|t| page.find(t).unwrap_or_else(|| panic!("{t} missing")))
        .collect();
    assert!(order.windows(2).all(|w| w[0] < w[1]), "{page}");
    assert!(post_html(&page, dupe_two).contains("№ 4"));
    // The emptied thread leads to the one it was merged into.
    let response = s.f.get(&format!("/t/{}/duplicate", dupe.thread_id)).await;
    assert_status(&response, StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(
        response.location(),
        format!("/t/{}/main-thread", target.thread_id)
    );
    let space = s.f.page("/c/games").await;
    assert!(!space.contains("Duplicate"), "{space}");
    // Nonsense is refused.
    for fields in [
        vec![
            ("numbers", "9"),
            ("to", "existing"),
            ("thread", &*target.thread_id.to_string()),
        ],
        vec![
            ("numbers", "1"),
            ("to", "existing"),
            ("thread", "not a thread"),
        ],
        vec![
            ("numbers", "1"),
            ("to", "new"),
            ("title", ""),
            ("space", "games"),
        ],
    ] {
        let mut fields = fields;
        fields.push(("reason", "x"));
        let response =
            s.f.submit(
                &format!("/mod/t/{}/move-posts", target.thread_id),
                &fields,
                Some(&s.mo_cookie),
            )
            .await;
        assert_status(&response, StatusCode::BAD_REQUEST);
    }
}

// ---------------------------------------------------------------------------
// Silences and suspensions

#[sqlx::test]
async fn silenced_members_read_but_dont_write(pool: PgPool) {
    let s = setup(pool).await;
    let t = s.f.thread(s.games, s.ruth, "Thread", "Opening").await;
    let own = s.f.post(t.thread_id, s.jonah, "Jonah's post").await;
    ok(
        &s.f,
        "/mod/users/jonah/silence",
        &[("days", "3"), ("reason", "Cool off")],
        &s.mo_cookie,
    )
    .await;
    let page =
        s.f.page_as(&format!("/t/{}/thread", t.thread_id), &s.jonah_cookie)
            .await;
    assert!(page.contains("Your account is silenced until"), "{page}");
    assert!(page.contains("Contest this"), "{page}");
    assert!(!page.contains("id=\"reply-body\""), "{page}");
    // Members-only content is still readable.
    s.f.page_as("/c/lounge", &s.jonah_cookie).await;
    for (path, fields) in [
        (
            "/c/games/new".to_owned(),
            vec![("title", "Hi"), ("body", "Hello")],
        ),
        (format!("/t/{}/reply", t.thread_id), vec![("body", "Hello")]),
        (format!("/p/{own}/edit"), vec![("body", "Edited")]),
        (
            format!("/p/{own}/reactions"),
            vec![("emoji", "👍"), ("action", "add")],
        ),
        (
            format!("/p/{}/fork", t.first_post_id),
            vec![("title", "Fork"), ("body", "Hello"), ("space", "games")],
        ),
        (
            "/messages/new".to_owned(),
            vec![("to", "ruth"), ("body", "Hello")],
        ),
        ("/invites".to_owned(), vec![("note", "x")]),
    ] {
        let response = s.f.submit(&path, &fields, Some(&s.jonah_cookie)).await;
        // Forms that can be shown again say why with a 400.
        assert!(
            response.status == StatusCode::FORBIDDEN || response.status == StatusCode::BAD_REQUEST,
            "{path}: {}",
            response.status
        );
        assert!(
            response.body.contains("silenced"),
            "{path}: {}",
            response.body
        );
    }
    assert_eq!(
        count(
            &s.f,
            &format!("SELECT count(*) FROM posts WHERE author_id = {}", s.jonah)
        )
        .await,
        1
    );
    assert_eq!(count(&s.f, "SELECT count(*) FROM invites").await, 0);
    assert_eq!(count(&s.f, "SELECT count(*) FROM reactions").await, 0);
    // They can still report, and delete their own posts.
    let response = report(&s.f, &s.jonah_cookie, t.first_post_id, "other", "").await;
    assert_status(&response, StatusCode::OK);
    ok(&s.f, &format!("/p/{own}/delete"), &[], &s.jonah_cookie).await;
    // Lifting the silence lets them write again.
    ok(
        &s.f,
        "/mod/users/jonah/unsilence",
        &[("reason", "Calmer now")],
        &s.mo_cookie,
    )
    .await;
    ok(
        &s.f,
        &format!("/t/{}/reply", t.thread_id),
        &[("body", "Back again")],
        &s.jonah_cookie,
    )
    .await;
    let public = s.f.page("/moderation").await;
    assert!(public.contains("silenced"), "{public}");
    assert!(public.contains("for 3 days"), "{public}");
    assert!(public.contains("lifted the silence on"), "{public}");
}

/// Reaction forms are offered only to those who may react: not to a
/// silenced member, and in a locked thread only to staff. Reacting there
/// anyway is refused.
#[sqlx::test]
async fn reaction_forms_are_only_for_those_who_may_react(pool: PgPool) {
    let s = setup(pool).await;
    let t = s.f.thread(s.games, s.ruth, "Thread", "Opening").await;
    let path = format!("/t/{}/thread", t.thread_id);
    let react = format!("/p/{}/reactions", t.first_post_id);
    ok(
        &s.f,
        &react,
        &[("emoji", "👍"), ("action", "add")],
        &s.maya_cookie,
    )
    .await;
    let conversation = store::create_conversation(
        &s.f.db,
        &NewConversation {
            author_id: s.ruth,
            title: String::new(),
            recipient_ids: vec![s.jonah],
            body_md: "Hello".into(),
            created_at: None,
        },
    )
    .await
    .unwrap();
    let dm = format!("/messages/{}", conversation.thread_id);
    let dm_react = format!("/p/{}/reactions", conversation.first_post_id);
    ok(
        &s.f,
        &dm_react,
        &[("emoji", "👍"), ("action", "add")],
        &s.ruth_cookie,
    )
    .await;
    for page in [
        s.f.page_as(&path, &s.jonah_cookie).await,
        s.f.page_as(&dm, &s.jonah_cookie).await,
    ] {
        assert!(page.contains("class=\"reaction-form\""), "{page}");
        assert!(page.contains("class=\"react-menu\""), "{page}");
    }

    ok(
        &s.f,
        "/mod/users/jonah/silence",
        &[("days", "3"), ("reason", "Cool off")],
        &s.mo_cookie,
    )
    .await;
    for page in [
        s.f.page_as(&path, &s.jonah_cookie).await,
        s.f.page_as(&dm, &s.jonah_cookie).await,
    ] {
        assert!(!page.contains("reaction-form"), "{page}");
        assert!(!page.contains("react-menu"), "{page}");
        assert!(!page.contains("data-island=\"reactions\""), "{page}");
        // The reactions are still shown.
        assert!(page.contains("class=\"reaction-count\">1<"), "{page}");
    }

    // A locked thread: members see the reactions but can't add theirs;
    // staff still can.
    ok(
        &s.f,
        &format!("/mod/t/{}", t.thread_id),
        &[("action", "lock"), ("reason", "Heated")],
        &s.mo_cookie,
    )
    .await;
    let page = s.f.page_as(&path, &s.ruth_cookie).await;
    assert!(!page.contains("reaction-form"), "{page}");
    assert!(!page.contains("react-menu"), "{page}");
    assert!(page.contains("class=\"reaction-count\">1<"), "{page}");
    let response =
        s.f.submit(
            &react,
            &[("emoji", "👍"), ("action", "add")],
            Some(&s.ruth_cookie),
        )
        .await;
    assert_status(&response, StatusCode::FORBIDDEN);
    assert!(response.body.contains("locked"), "{}", response.body);
    let page = s.f.page_as(&path, &s.mo_cookie).await;
    assert!(page.contains("class=\"react-menu\""), "{page}");
    ok(
        &s.f,
        &react,
        &[("emoji", "👍"), ("action", "add")],
        &s.mo_cookie,
    )
    .await;
    assert_eq!(count(&s.f, "SELECT count(*) FROM reactions").await, 3);
}

#[sqlx::test]
async fn silenced_members_can_still_answer_the_moderators(pool: PgPool) {
    let s = setup(pool).await;
    let response = ok(
        &s.f,
        "/messages/new",
        &[("to", "jonah"), ("body", "A word, please")],
        &s.mo_cookie,
    )
    .await;
    let conversation = response
        .location()
        .trim_start_matches("/messages/")
        .to_owned();
    let conversation: String = conversation
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    let peer = ok(
        &s.f,
        "/messages/new",
        &[("to", "jonah"), ("body", "Hi jonah")],
        &s.ruth_cookie,
    )
    .await;
    let peer: String = peer
        .location()
        .trim_start_matches("/messages/")
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    ok(
        &s.f,
        "/mod/users/jonah/silence",
        &[("days", "1"), ("reason", "x")],
        &s.mo_cookie,
    )
    .await;
    ok(
        &s.f,
        &format!("/messages/{conversation}"),
        &[("body", "Sorry")],
        &s.jonah_cookie,
    )
    .await;
    let response =
        s.f.submit(
            &format!("/messages/{peer}"),
            &[("body", "Hey")],
            Some(&s.jonah_cookie),
        )
        .await;
    assert_status(&response, StatusCode::FORBIDDEN);
}

#[sqlx::test]
async fn suspended_members_read_only_what_visitors_read(pool: PgPool) {
    let s = setup(pool).await;
    let public = s.f.thread(s.games, s.ruth, "Public", "Public text").await;
    let private =
        s.f.thread(s.lounge, s.ruth, "Members", "Members-only text")
            .await;
    let own = s.f.post(public.thread_id, s.jonah, "Jonah's").await;
    let conversation = ok(
        &s.f,
        "/messages/new",
        &[("to", "jonah"), ("body", "a private word")],
        &s.ruth_cookie,
    )
    .await;
    let conversation = conversation.location().to_owned();
    ok(
        &s.f,
        "/mod/users/jonah/suspend",
        &[("days", "7"), ("reason", "Spamming")],
        &s.sam_cookie,
    )
    .await;
    let page =
        s.f.page_as(&format!("/t/{}/public", public.thread_id), &s.jonah_cookie)
            .await;
    assert!(page.contains("Your account is suspended until"), "{page}");
    assert!(page.contains("Public text"));
    for path in [
        format!("/t/{}/members", private.thread_id),
        "/c/lounge".to_owned(),
        format!("/p/{}", private.first_post_id),
        conversation.clone(),
    ] {
        let response = s.f.get_as(&path, Some(&s.jonah_cookie)).await;
        assert_status(&response, StatusCode::NOT_FOUND);
        assert!(!response.body.contains("Members-only text"));
        assert!(!response.body.contains("a private word"));
    }
    let index = s.f.page_as("/", &s.jonah_cookie).await;
    assert!(!index.contains("Lounge"), "{index}");
    for (path, fields) in [
        (
            format!("/t/{}/reply", public.thread_id),
            vec![("body", "Hello")],
        ),
        (format!("/p/{own}/delete"), vec![]),
        (
            format!("/p/{}/report", public.first_post_id),
            vec![("reason", "spam")],
        ),
    ] {
        let response = s.f.submit(&path, &fields, Some(&s.jonah_cookie)).await;
        assert_status(&response, StatusCode::FORBIDDEN);
    }
    // They can contest it.
    let action = last_action(&s.f).await;
    assert!(page.contains(&format!("/appeal/{action}")), "{page}");
    s.f.page_as(&format!("/appeal/{action}"), &s.jonah_cookie)
        .await;
    ok_or_200(
        &s.f,
        &format!("/appeal/{action}"),
        &[("details", "It was a misunderstanding")],
        &s.jonah_cookie,
    )
    .await;
    // Only once, and only their own.
    let response =
        s.f.submit(
            &format!("/appeal/{action}"),
            &[("details", "again")],
            Some(&s.jonah_cookie),
        )
        .await;
    assert_status(&response, StatusCode::FORBIDDEN);
    let response =
        s.f.get_as(&format!("/appeal/{action}"), Some(&s.ruth_cookie))
            .await;
    assert_status(&response, StatusCode::NOT_FOUND);
    // The appeal is in the queue; reversing it lifts the suspension.
    let queue = s.f.page_as("/mod/queue", &s.mo_cookie).await;
    assert!(queue.contains("It was a misunderstanding"), "{queue}");
    assert!(queue.contains("contests a decision"), "{queue}");
    let appeal = count(&s.f, "SELECT id FROM reports WHERE kind = 'appeal'").await;
    ok(
        &s.f,
        &format!("/mod/appeals/{appeal}"),
        &[("decision", "overturn"), ("reason", "Fair enough")],
        &s.mo_cookie,
    )
    .await;
    s.f.page_as(
        &format!("/t/{}/members", private.thread_id),
        &s.jonah_cookie,
    )
    .await;
    let notes = s.f.page_as("/notifications", &s.jonah_cookie).await;
    assert!(notes.contains("reversed their decision"), "{notes}");
}

async fn ok_or_200(f: &Forum, path: &str, fields: &[(&str, &str)], cookie: &str) {
    let response = f.submit(path, fields, Some(cookie)).await;
    assert!(
        response.status == StatusCode::OK || response.status == StatusCode::SEE_OTHER,
        "{}: {}",
        response.status,
        response.body
    );
}

#[sqlx::test]
async fn staff_cant_be_silenced_or_suspended(pool: PgPool) {
    let s = setup(pool).await;
    for action in ["silence", "suspend", "suspend-branch", "spammer"] {
        let response =
            s.f.submit(
                &format!("/mod/users/sam/{action}"),
                &[("days", "7"), ("reason", "Coup")],
                Some(&s.mo_cookie),
            )
            .await;
        assert_status(&response, StatusCode::FORBIDDEN);
    }
    assert_eq!(
        count(&s.f, "SELECT count(*) FROM users WHERE suspended_until IS NOT NULL OR silenced_until IS NOT NULL OR deleted_at IS NOT NULL").await,
        0
    );
}

#[sqlx::test]
async fn suspending_an_invite_branch(pool: PgPool) {
    let s = setup(pool).await;
    let felix = s.f.invitee("felix", s.jonah).await;
    s.f.invitee("cheapdeals", felix).await;
    let mod_child = s.f.invitee("modchild", felix).await;
    s.f.exec(&format!(
        "UPDATE users SET role = 'moderator' WHERE id = {mod_child}"
    ))
    .await;
    s.f.user("other").await;
    let page = s.f.page_as("/mod/users/felix", &s.mo_cookie).await;
    assert!(
        page.contains("Suspend with everyone they invited (2 accounts)"),
        "{page}"
    );
    // Someone who invited nobody (or only staff) has no branch to suspend.
    let page = s.f.page_as("/mod/users/other", &s.mo_cookie).await;
    assert!(page.contains("/mod/users/other/suspend\""), "{page}");
    assert!(!page.contains("suspend-branch"), "{page}");
    ok(
        &s.f,
        "/mod/users/felix/suspend-branch",
        &[("days", "30"), ("reason", "Spam ring")],
        &s.mo_cookie,
    )
    .await;
    let suspended: Vec<String> = sqlx::query_scalar(
        "SELECT username::text FROM users WHERE suspended_until > now() ORDER BY username",
    )
    .fetch_all(&s.f.db)
    .await
    .unwrap();
    assert_eq!(suspended, vec!["cheapdeals", "felix"]);
    let log = s.f.page("/moderation").await;
    assert!(log.contains("and 1 other member they invited"), "{log}");
}

#[sqlx::test]
async fn deleting_a_spammer_removes_everything(pool: PgPool) {
    let s = setup(pool).await;
    let spammer = s.f.member("cheapdeals", "user", None).await;
    let cookie = s.f.session(spammer).await;
    let t =
        s.f.thread(s.games, spammer, "Best replica watches", "Buy now")
            .await;
    s.f.post(t.thread_id, s.ruth, "Ruth replies to the spam")
        .await;
    let other = s.f.thread(s.games, s.ruth, "Real thread", "Real").await;
    let spam_reply = s.f.post(other.thread_id, spammer, "Visit my shop").await;
    report(&s.f, &s.ruth_cookie, spam_reply, "spam", "").await;
    ok(
        &s.f,
        "/mod/users/cheapdeals/spammer",
        &[("reason", "Spam"), ("confirm", "yes")],
        &s.mo_cookie,
    )
    .await;
    let space = s.f.page("/c/games").await;
    assert!(!space.contains("Best replica watches"), "{space}");
    let page =
        s.f.page(&format!("/t/{}/real-thread", other.thread_id))
            .await;
    assert!(!page.contains("Visit my shop"));
    // Their session is over and they can't log back in.
    let response = s.f.get_as("/settings", Some(&cookie)).await;
    assert_status(&response, StatusCode::SEE_OTHER);
    assert!(s.f.get_as("/u/cheapdeals", None).await.status == StatusCode::NOT_FOUND);
    // The report about them is settled.
    assert_eq!(
        count(&s.f, "SELECT count(*) FROM reports WHERE status = 'open'").await,
        0
    );
    let log = s.f.page("/moderation").await;
    assert!(log.contains("as a spammer"), "{log}");
}

// ---------------------------------------------------------------------------
// New members

#[sqlx::test]
async fn new_members_first_posts_are_held_for_approval(pool: PgPool) {
    let mut s = setup(pool).await;
    // New members' lower rates are tested on their own below.
    s.f = s.f.without_rate_limits();
    settings::set(&s.f.db, "messages", &MessageSettings::default())
        .await
        .unwrap();
    let tomasz = s.f.member("tomasz", "user", None).await;
    let cookie = s.f.session(tomasz).await;
    let response = ok(
        &s.f,
        "/c/games/new",
        &[("title", "Hi from Leeds!"), ("body", "Hello, I love Brass")],
        &cookie,
    )
    .await;
    let held_path = response.location().to_owned();
    assert!(held_path.starts_with("/held/"), "{held_path}");
    // Only they and staff can see it.
    let page = s.f.page_as(&held_path, &cookie).await;
    assert!(page.contains("Waiting for approval"), "{page}");
    assert!(page.contains("Hello, I love Brass"));
    for cookie in [None, Some(s.ruth_cookie.as_str())] {
        let response = s.f.get_as(&held_path, cookie).await;
        assert!(!response.body.contains("Hello, I love Brass"));
    }
    assert!(!s.f.page("/c/games").await.contains("Hi from Leeds!"));
    let queue = s.f.page_as("/mod/queue", &s.mo_cookie).await;
    assert!(queue.contains("Held for approval"), "{queue}");
    assert!(queue.contains("Hello, I love Brass"), "{queue}");
    let held = count(&s.f, "SELECT id FROM held_posts").await;
    ok(
        &s.f,
        &format!("/mod/held/{held}/approve"),
        &[("reason", "")],
        &s.mo_cookie,
    )
    .await;
    assert!(s.f.page("/c/games").await.contains("Hi from Leeds!"));
    // Their own thread isn't new to them.
    let list = s.f.page_as("/c/games", &cookie).await;
    assert!(!list.contains(" new<"), "{list}");
    let notes = s.f.page_as("/notifications", &cookie).await;
    assert!(notes.contains("approved your post"), "{notes}");
    let log = s.f.page("/moderation").await;
    assert!(log.contains("approved a post by new member"), "{log}");
    // The second post is held too; rejecting it posts nothing.
    let thread = count(&s.f, "SELECT max(id) FROM threads").await;
    let response = ok(
        &s.f,
        &format!("/t/{thread}/reply"),
        &[("body", "Buy watches")],
        &cookie,
    )
    .await;
    assert!(response.location().starts_with("/held/"));
    let held = count(&s.f, "SELECT max(id) FROM held_posts").await;
    ok(
        &s.f,
        &format!("/mod/held/{held}/reject"),
        &[("reason", "Advertising")],
        &s.mo_cookie,
    )
    .await;
    assert_eq!(
        count(
            &s.f,
            &format!("SELECT count(*) FROM posts WHERE author_id = {tomasz}")
        )
        .await,
        1
    );
    let notes = s.f.page_as("/notifications", &cookie).await;
    assert!(
        notes.contains("didn’t approve your post. Reason: Advertising"),
        "{notes}"
    );
    // Rejections aren't in the public log.
    assert!(!s.f.page("/moderation").await.contains("Advertising"));
    // Once two have been approved, posts go straight up.
    let response = ok(
        &s.f,
        &format!("/t/{thread}/reply"),
        &[("body", "Second try")],
        &cookie,
    )
    .await;
    let held = count(&s.f, "SELECT max(id) FROM held_posts").await;
    assert_eq!(response.location(), format!("/held/{held}"));
    ok(
        &s.f,
        &format!("/mod/held/{held}/approve"),
        &[],
        &s.mo_cookie,
    )
    .await;
    let response = ok(
        &s.f,
        &format!("/t/{thread}/reply"),
        &[("body", "Straight up")],
        &cookie,
    )
    .await;
    assert!(
        response.location().contains("#p"),
        "{}",
        response.location()
    );
    // Approving twice does nothing more.
    let response =
        s.f.submit(
            &format!("/mod/held/{held}/approve"),
            &[],
            Some(&s.mo_cookie),
        )
        .await;
    assert_status(&response, StatusCode::NOT_FOUND);
    assert_eq!(
        count(
            &s.f,
            &format!("SELECT count(*) FROM posts WHERE author_id = {tomasz}")
        )
        .await,
        3
    );
}

#[sqlx::test]
async fn holding_can_be_turned_off_and_staff_are_never_held(pool: PgPool) {
    let s = setup(pool).await;
    settings::set(&s.f.db, "messages", &MessageSettings::default())
        .await
        .unwrap();
    let newbie = s.f.member("newbie", "user", None).await;
    let cookie = s.f.session(newbie).await;
    let newmod = s.f.member("newmod", "moderator", None).await;
    let mod_cookie = s.f.session(newmod).await;
    let response = ok(
        &s.f,
        "/c/games/new",
        &[("title", "Staff thread"), ("body", "Hello")],
        &mod_cookie,
    )
    .await;
    assert!(!response.location().starts_with("/held/"));
    // The admin turns holding off.
    let page = s.f.page_as("/admin/moderation", &s.sam_cookie).await;
    assert!(page.contains("name=\"hold_posts\""), "{page}");
    let response =
        s.f.submit(
            "/admin/moderation",
            &[("hold_posts", "0"), ("ip_retention_days", "90")],
            Some(&s.mo_cookie),
        )
        .await;
    assert_status(&response, StatusCode::FORBIDDEN);
    ok(
        &s.f,
        "/admin/moderation",
        &[("hold_posts", "0"), ("ip_retention_days", "90")],
        &s.sam_cookie,
    )
    .await;
    assert_eq!(
        settings::moderation_settings(s.f.state.db()).await.unwrap(),
        ModerationSettings {
            hold_posts: 0,
            ip_retention_days: 90
        }
    );
    let response = ok(
        &s.f,
        "/c/games/new",
        &[("title", "Straight up"), ("body", "Hello")],
        &cookie,
    )
    .await;
    assert!(!response.location().starts_with("/held/"));
}

#[sqlx::test]
async fn new_members_post_more_slowly(pool: PgPool) {
    let s = setup(pool).await;
    // Accounts are new until they have five posts.
    settings::set(
        &s.f.db,
        "messages",
        &MessageSettings {
            new_user_days: 0,
            new_user_posts: 5,
            ..MessageSettings::default()
        },
    )
    .await
    .unwrap();
    settings::set(
        &s.f.db,
        "moderation",
        &ModerationSettings {
            hold_posts: 0,
            ..ModerationSettings::default()
        },
    )
    .await
    .unwrap();
    let t = s.f.thread(s.games, s.ruth, "Thread", "Opening").await;
    for i in 0..4 {
        s.f.post(t.thread_id, s.ruth, &format!("Established {i}"))
            .await;
    }
    let newbie = s.f.member("newbie", "user", None).await;
    let cookie = s.f.session(newbie).await;
    let new_burst = parlour::ratelimit::rule(
        parlour::ratelimit::Action::NewMemberPost,
        parlour::ratelimit::Scope::User,
    )
    .burst;
    let burst = parlour::ratelimit::rule(
        parlour::ratelimit::Action::Post,
        parlour::ratelimit::Scope::User,
    )
    .burst;
    assert!(new_burst < burst);
    for i in 0..new_burst {
        ok(
            &s.f,
            &format!("/t/{}/reply", t.thread_id),
            &[("body", &format!("post {i}"))],
            &cookie,
        )
        .await;
    }
    let response =
        s.f.submit(
            &format!("/t/{}/reply", t.thread_id),
            &[("body", "one too many")],
            Some(&cookie),
        )
        .await;
    assert_status(&response, StatusCode::TOO_MANY_REQUESTS);
    // Established members aren't held to it.
    for i in 0..=new_burst {
        ok(
            &s.f,
            &format!("/t/{}/reply", t.thread_id),
            &[("body", &format!("ruth {i}"))],
            &s.ruth_cookie,
        )
        .await;
    }
}

// ---------------------------------------------------------------------------
// The member record: notes and addresses

#[sqlx::test]
async fn staff_keep_notes_and_see_shared_addresses(pool: PgPool) {
    let s = setup(pool).await;
    for (user, ip) in [
        (s.jonah, "198.51.100.1"),
        (s.maya, "198.51.100.1"),
        (s.ruth, "198.51.100.2"),
    ] {
        parlour::sessions::create(&s.f.db, user, &axum::http::HeaderMap::new(), ip)
            .await
            .unwrap();
    }
    // Using the site records the address too.
    let response =
        s.f.request(
            axum::http::Request::get("/")
                .header(header::COOKIE, &s.jonah_cookie)
                .header("x-forwarded-for", "192.0.2.77")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await;
    assert_status(&response, StatusCode::OK);
    s.f.exec("UPDATE sessions SET last_seen_at = now() - interval '1 hour'")
        .await;
    s.f.request(
        axum::http::Request::get("/")
            .header(header::COOKIE, &s.jonah_cookie)
            .header("x-forwarded-for", "192.0.2.77")
            .body(axum::body::Body::empty())
            .unwrap(),
    )
    .await;
    ok(
        &s.f,
        "/mod/users/jonah/notes",
        &[("body", "Was warned about spoilers in March")],
        &s.mo_cookie,
    )
    .await;
    let page = s.f.page_as("/mod/users/jonah", &s.sam_cookie).await;
    assert!(
        page.contains("Was warned about spoilers in March"),
        "{page}"
    );
    assert!(page.contains("198.51.100.1"), "{page}");
    assert!(page.contains("192.0.2.77"), "{page}");
    assert!(page.contains("/mod/users/maya"), "{page}");
    assert!(page.contains("jonah@example.com"), "{page}");
    // Notes are for staff only.
    let profile = s.f.page_as("/u/jonah", &s.ruth_cookie).await;
    assert!(!profile.contains("spoilers in March"));
    assert!(!s.f.page("/moderation").await.contains("note"));
    // Addresses older than the retention period are forgotten.
    s.f.exec(
        "UPDATE user_ips SET last_seen_at = now() - interval '100 days' WHERE ip = '198.51.100.1'",
    )
    .await;
    let page = s.f.page_as("/mod/users/jonah", &s.sam_cookie).await;
    assert!(!page.contains("198.51.100.1"), "{page}");
    parlour::moderation::run_forget_ips(s.f.state.clone(), serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(
        count(
            &s.f,
            "SELECT count(*) FROM user_ips WHERE ip = '198.51.100.1'"
        )
        .await,
        0
    );
    assert_eq!(
        count(&s.f, "SELECT count(*) FROM jobs WHERE kind = 'forget_ips'").await,
        1
    );
    // The member list finds people.
    let list = s.f.page_as("/mod/users?q=jon", &s.mo_cookie).await;
    assert!(list.contains("/mod/users/jonah"), "{list}");
    assert!(!list.contains("/mod/users/maya"), "{list}");
}

// ---------------------------------------------------------------------------
// The public log

#[sqlx::test]
async fn the_public_log_shows_only_public_content(pool: PgPool) {
    let s = setup(pool).await;
    let public = s.f.thread(s.games, s.jonah, "Open thread", "x").await;
    let private =
        s.f.thread(s.lounge, s.jonah, "Lounge secret title", "y")
            .await;
    // Warm the cache.
    let before = s.f.get("/moderation").await;
    assert_status(&before, StatusCode::OK);
    assert!(before.header(header::SET_COOKIE).is_none());
    for t in [&public, &private] {
        ok(
            &s.f,
            &format!("/mod/p/{}", t.first_post_id),
            &[
                ("action", "hide"),
                ("reason", &format!("reason for {}", t.thread_id)),
            ],
            &s.mo_cookie,
        )
        .await;
    }
    let log = s.f.page("/moderation").await;
    assert!(log.contains("Open thread"), "{log}");
    assert!(log.contains(&format!("reason for {}", public.thread_id)));
    assert!(!log.contains("Lounge secret title"), "{log}");
    assert!(!log.contains(&format!("reason for {}", private.thread_id)));
    // Members see the same public log; staff see both in theirs.
    let log = s.f.page_as("/moderation", &s.ruth_cookie).await;
    assert!(!log.contains("Lounge secret title"));
    let staff = s.f.page_as("/mod/log", &s.mo_cookie).await;
    assert!(staff.contains("Lounge secret title"), "{staff}");
    // Footer links to it from every page.
    assert!(s.f.page("/").await.contains("href=\"/moderation\""));
}

// ---------------------------------------------------------------------------
// Silences and suspensions, over random members and actions

/// What a member is under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Standing {
    Good,
    Silenced,
    Suspended,
    /// Silenced once, but it has run out.
    SilenceOver,
    /// Suspended once, but it has run out.
    SuspensionOver,
}

/// Everything a member might try, in an order where none undoes another.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Try {
    ReadMembersThread,
    ReadConversation,
    NewThread,
    Reply,
    Edit,
    React,
    Fork,
    StartConversation,
    MessageMember,
    MessageStaff,
    MessageStaffAndMember,
    Invite,
    Report,
    Delete,
}

const TRIES: [Try; 14] = [
    Try::ReadMembersThread,
    Try::ReadConversation,
    Try::NewThread,
    Try::Reply,
    Try::Edit,
    Try::React,
    Try::Fork,
    Try::StartConversation,
    Try::MessageMember,
    Try::MessageStaff,
    Try::MessageStaffAndMember,
    Try::Invite,
    Try::Report,
    Try::Delete,
];

/// The reference rule (DESIGN.md §3.1, §8), written separately from the
/// app's: a suspended member does nothing but read what visitors read; a
/// silenced one reads everything, and may report, delete their own posts
/// and answer staff (in conversations with nobody else), but writes
/// nothing else; a restriction that has run out restricts nothing.
fn may(standing: Standing, attempt: Try) -> bool {
    let suspended = standing == Standing::Suspended;
    let silenced = suspended || standing == Standing::Silenced;
    match attempt {
        Try::ReadMembersThread | Try::ReadConversation => !suspended,
        Try::MessageStaff | Try::Report | Try::Delete => !suspended,
        Try::NewThread
        | Try::Reply
        | Try::Edit
        | Try::React
        | Try::Fork
        | Try::StartConversation
        | Try::MessageMember
        | Try::MessageStaffAndMember
        | Try::Invite => !silenced,
    }
}

/// Everything a member's writing could change, as text to compare.
async fn state(f: &Forum) -> String {
    text(
        f,
        "SELECT concat_ws(' | ',
            (SELECT string_agg(id || ':' || body_md || ':' || coalesce(deleted_at::text, ''), ',' ORDER BY id) FROM posts),
            (SELECT string_agg(post_id || emoji, ',' ORDER BY post_id, emoji) FROM reactions),
            (SELECT count(*) FROM invites),
            (SELECT count(*) FROM reports),
            (SELECT count(*) FROM threads))",
    )
    .await
}

#[hegel::standalone_function(test_cases = 60, suppress_health_check = [hegel::HealthCheck::TooSlow])]
fn restrictions_follow_the_rule(tc: hegel::TestCase, rt: &tokio::runtime::Handle, db: &PgPool) {
    use hegel::generators::{self as gs, Generator};
    let standing = tc.draw(
        gs::sampled_from(vec![
            Standing::Good,
            Standing::Silenced,
            Standing::Suspended,
            Standing::SilenceOver,
            Standing::SuspensionOver,
        ])
        .print_as_debug(),
    );
    let mut tries: Vec<Try> = TRIES
        .iter()
        .copied()
        .filter(|_| tc.draw(gs::booleans()))
        .collect();
    tries.sort();
    rt.block_on(async {
        sqlx::raw_sql(
            "TRUNCATE settings, jobs, reports, mod_actions, posts, threads, spaces, layouts, users
             RESTART IDENTITY CASCADE",
        )
        .execute(db)
        .await
        .unwrap();
        let f = Forum::new(db.clone()).without_rate_limits();
        f.no_new_members().await;
        let games = f.space("games", "public", None, 0).await;
        let lounge = f.space("lounge", "members", None, 1).await;
        let me = f.member("me", "user", None).await;
        let ruth = f.member("ruth", "user", None).await;
        f.member("mo", "moderator", None).await;
        let my_cookie = f.session(me).await;
        let ruth_cookie = f.session(ruth).await;
        let mo_cookie = f.session(f.user_id("mo").await).await;
        let t = f.thread(games, ruth, "Open", "Open thread").await;
        let mine = f.post(t.thread_id, me, "My post").await;
        let members = f.thread(lounge, ruth, "Lounge", "Members only text").await;
        let start = |cookie: &str, to: &'static str| {
            let f = &f;
            let cookie = cookie.to_owned();
            async move {
                let response = f
                    .submit(
                        "/messages/new",
                        &[("to", to), ("body", "Hello")],
                        Some(&cookie),
                    )
                    .await;
                assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
                response
                    .location()
                    .trim_start_matches("/messages/")
                    .chars()
                    .take_while(char::is_ascii_digit)
                    .collect::<String>()
            }
        };
        let with_ruth = start(&ruth_cookie, "me").await;
        let with_staff = start(&mo_cookie, "me").await;
        let with_both = start(&mo_cookie, "me, ruth").await;
        let restriction = match standing {
            Standing::Good => None,
            Standing::Silenced => Some(("silenced_until", "now() + interval '1 day'")),
            Standing::Suspended => Some(("suspended_until", "now() + interval '1 day'")),
            Standing::SilenceOver => Some(("silenced_until", "now() - interval '1 second'")),
            Standing::SuspensionOver => Some(("suspended_until", "now() - interval '1 second'")),
        };
        if let Some((column, until)) = restriction {
            f.exec(&format!(
                "UPDATE users SET {column} = {until} WHERE id = {me}"
            ))
            .await;
        }
        for attempt in tries {
            let before = state(&f).await;
            let path_and_fields: (String, Vec<(&str, String)>) = match attempt {
                Try::ReadMembersThread => (format!("/t/{}/lounge", members.thread_id), vec![]),
                Try::ReadConversation => (format!("/messages/{with_ruth}"), vec![]),
                Try::NewThread => (
                    "/c/games/new".into(),
                    vec![("title", "Mine".into()), ("body", "New".into())],
                ),
                Try::Reply => (
                    format!("/t/{}/reply", t.thread_id),
                    vec![("body", "Reply".into())],
                ),
                Try::Edit => (format!("/p/{mine}/edit"), vec![("body", "Changed".into())]),
                Try::React => (
                    format!("/p/{}/reactions", t.first_post_id),
                    vec![("emoji", "👍".into()), ("action", "add".into())],
                ),
                Try::Fork => (
                    format!("/p/{}/fork", t.first_post_id),
                    vec![
                        ("title", "Fork".into()),
                        ("body", "Forked".into()),
                        ("space", "games".into()),
                    ],
                ),
                Try::StartConversation => (
                    "/messages/new".into(),
                    vec![("to", "ruth".into()), ("body", "Hi".into())],
                ),
                Try::MessageMember => (
                    format!("/messages/{with_ruth}"),
                    vec![("body", "Hi".into())],
                ),
                Try::MessageStaff => (
                    format!("/messages/{with_staff}"),
                    vec![("body", "Sorry".into())],
                ),
                Try::MessageStaffAndMember => (
                    format!("/messages/{with_both}"),
                    vec![("body", "Ruth, listen".into())],
                ),
                Try::Invite => ("/invites".into(), vec![("note", "A friend".into())]),
                Try::Report => (
                    format!("/p/{}/report", t.first_post_id),
                    vec![("reason", "spam".into())],
                ),
                Try::Delete => (format!("/p/{mine}/delete"), vec![]),
            };
            let (path, fields) = path_and_fields;
            let response = if matches!(attempt, Try::ReadMembersThread | Try::ReadConversation) {
                f.get_as(&path, Some(&my_cookie)).await
            } else {
                let fields: Vec<(&str, &str)> =
                    fields.iter().map(|(k, v)| (*k, v.as_str())).collect();
                f.submit(&path, &fields, Some(&my_cookie)).await
            };
            let after = state(&f).await;
            let reading = matches!(attempt, Try::ReadMembersThread | Try::ReadConversation);
            let succeeded = if reading {
                response.status == StatusCode::OK
            } else {
                (response.status == StatusCode::SEE_OTHER || response.status == StatusCode::OK)
                    && after != before
            };
            assert_eq!(
                succeeded,
                may(standing, attempt),
                "{standing:?} {attempt:?}: {} {}",
                response.status,
                response.body
            );
            if !succeeded && !reading {
                assert_eq!(after, before, "{standing:?} {attempt:?} changed something");
                assert!(
                    response.status.is_client_error(),
                    "{attempt:?}: {}",
                    response.status
                );
            }
            if reading && !succeeded {
                assert!(!response.body.contains("Members only text"));
                assert!(!response.body.contains("Hello"));
            }
        }
    });
}

#[sqlx::test]
async fn silenced_and_suspended_members_are_kept_to_what_they_may_do(pool: PgPool) {
    let rt = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || restrictions_follow_the_rule(&rt, &pool))
        .await
        .unwrap();
}

// ---------------------------------------------------------------------------
// Appeals

/// The appeal a member made against an action.
async fn appeal_against(f: &Forum, action: i64, user: i64) -> i64 {
    count(
        f,
        &format!(
            "SELECT id FROM reports WHERE kind = 'appeal' AND mod_action_id = {action} AND user_id = {user}"
        ),
    )
    .await
}

/// Reverses an appeal, and returns the page the moderator is sent to.
async fn reverse(f: &Forum, appeal: i64, cookie: &str) -> String {
    let response = ok(
        f,
        &format!("/mod/appeals/{appeal}"),
        &[("decision", "overturn"), ("reason", "Fair enough")],
        cookie,
    )
    .await;
    f.page_as(response.location(), cookie).await
}

async fn holds(f: &Forum, condition: &str) -> bool {
    count(f, &format!("SELECT count(*) FROM {condition}")).await == 1
}

/// Reversing an appeal undoes the action contested only while what it did
/// still stands. A later, separate action is left alone, and the moderator
/// is told so.
#[sqlx::test]
async fn reversing_an_appeal_leaves_later_actions_alone(pool: PgPool) {
    let mut s = setup(pool).await;
    s.f = s.f.without_rate_limits();
    let jonah = s.jonah;
    let contest = |action: i64| {
        let f = &s.f;
        let cookie = s.jonah_cookie.clone();
        async move {
            ok_or_200(
                f,
                &format!("/appeal/{action}"),
                &[("details", "That was unfair")],
                &cookie,
            )
            .await;
            appeal_against(f, action, jonah).await
        }
    };
    let act = |path: String, fields: Vec<(&'static str, &'static str)>| {
        let f = &s.f;
        let cookie = s.mo_cookie.clone();
        async move {
            ok(f, &path, &fields, &cookie).await;
            last_action(f).await
        }
    };

    // A day's suspension, contested; then a separate year's suspension.
    let short = act(
        "/mod/users/jonah/suspend".into(),
        vec![("days", "1"), ("reason", "Rude")],
    )
    .await;
    let appeal = contest(short).await;
    act(
        "/mod/users/jonah/suspend".into(),
        vec![("days", "365"), ("reason", "Threats")],
    )
    .await;
    let page = reverse(&s.f, appeal, &s.mo_cookie).await;
    assert!(page.contains("a later action"), "{page}");
    assert!(
        holds(
            &s.f,
            &format!("users WHERE id = {jonah} AND suspended_until > now() + interval '300 days'")
        )
        .await
    );
    // He is told it was reversed all the same.
    let notes = s.f.page_as("/notifications", &s.jonah_cookie).await;
    assert!(notes.contains("reversed their decision"), "{notes}");
    s.f.exec(&format!(
        "UPDATE users SET suspended_until = NULL WHERE id = {jonah}"
    ))
    .await;

    // The same for a silence lifted and then imposed again.
    let first = act(
        "/mod/users/jonah/silence".into(),
        vec![("days", "1"), ("reason", "Rude")],
    )
    .await;
    let appeal = contest(first).await;
    act(
        "/mod/users/jonah/unsilence".into(),
        vec![("reason", "Calmer")],
    )
    .await;
    act(
        "/mod/users/jonah/silence".into(),
        vec![("days", "30"), ("reason", "Rude again")],
    )
    .await;
    reverse(&s.f, appeal, &s.mo_cookie).await;
    assert!(
        holds(
            &s.f,
            &format!("users WHERE id = {jonah} AND silenced_until > now() + interval '20 days'")
        )
        .await
    );
    s.f.exec(&format!(
        "UPDATE users SET silenced_until = NULL WHERE id = {jonah}"
    ))
    .await;

    // A post hidden, shown, and hidden again for another reason.
    let t = s.f.thread(s.games, s.ruth, "Thread", "Opening").await;
    let post = s.f.post(t.thread_id, jonah, "A reply").await;
    let hide = |action: &'static str, reason: &'static str| {
        act(
            format!("/mod/p/{post}"),
            vec![("action", action), ("reason", reason)],
        )
    };
    let first = hide("hide", "Off topic").await;
    let appeal = contest(first).await;
    hide("unhide", "On reflection").await;
    hide("hide", "Personal information").await;
    let page = reverse(&s.f, appeal, &s.mo_cookie).await;
    assert!(page.contains("a later action"), "{page}");
    assert!(
        holds(
            &s.f,
            &format!("posts WHERE id = {post} AND hidden_at IS NOT NULL")
        )
        .await
    );
    // Deleted, restored and deleted again.
    hide("unhide", "Fixed").await;
    let first = hide("delete", "Spam").await;
    let appeal = contest(first).await;
    hide("restore", "Not spam").await;
    hide("delete", "Spam after all").await;
    reverse(&s.f, appeal, &s.mo_cookie).await;
    assert!(
        holds(
            &s.f,
            &format!("posts WHERE id = {post} AND deleted_at IS NOT NULL")
        )
        .await
    );

    // A thread locked, unlocked and locked again; then the same for
    // deleting it.
    let own = s.f.thread(s.games, jonah, "Jonah's", "Mine").await;
    let thread = |action: &'static str, reason: &'static str| {
        act(
            format!("/mod/t/{}", own.thread_id),
            vec![("action", action), ("reason", reason)],
        )
    };
    let first = thread("lock", "Heated").await;
    let appeal = contest(first).await;
    thread("unlock", "Cooler").await;
    thread("lock", "Heated again").await;
    reverse(&s.f, appeal, &s.mo_cookie).await;
    assert!(
        holds(
            &s.f,
            &format!("threads WHERE id = {} AND locked", own.thread_id)
        )
        .await
    );
    let first = thread("delete", "Spam").await;
    let appeal = contest(first).await;
    thread("restore", "Not spam").await;
    thread("delete", "Spam after all").await;
    reverse(&s.f, appeal, &s.mo_cookie).await;
    assert!(
        holds(
            &s.f,
            &format!(
                "threads WHERE id = {} AND deleted_at IS NOT NULL",
                own.thread_id
            )
        )
        .await
    );

    // With nothing later, reversing undoes it.
    let other = s.f.post(t.thread_id, jonah, "Another reply").await;
    ok(
        &s.f,
        &format!("/mod/p/{other}"),
        &[("action", "hide"), ("reason", "Hmm")],
        &s.mo_cookie,
    )
    .await;
    let appeal = contest(last_action(&s.f).await).await;
    let page = reverse(&s.f, appeal, &s.mo_cookie).await;
    assert!(!page.contains("a later action"), "{page}");
    assert!(
        holds(
            &s.f,
            &format!("posts WHERE id = {other} AND hidden_at IS NULL")
        )
        .await
    );
}

/// Everyone suspended with an invite branch can contest it. Reversing a
/// member's appeal lifts their own suspension; reversing the appeal of
/// the member the branch was suspended for lifts it for the whole branch,
/// except for anyone suspended again since.
#[sqlx::test]
async fn invite_branch_members_contest_a_branch_suspension(pool: PgPool) {
    let mut s = setup(pool).await;
    s.f = s.f.without_rate_limits();
    let nell = s.f.invitee("nell", s.jonah).await;
    let otto = s.f.invitee("otto", nell).await;
    s.f.invitee("pia", otto).await;
    let nell_cookie = s.f.session(nell).await;
    let otto_cookie = s.f.session(otto).await;
    ok(
        &s.f,
        "/mod/users/nell/suspend-branch",
        &[("days", "30"), ("reason", "Spam ring")],
        &s.mo_cookie,
    )
    .await;
    let action = last_action(&s.f).await;
    for cookie in [&nell_cookie, &otto_cookie] {
        let page = s.f.page_as("/", cookie).await;
        assert!(page.contains("Your account is suspended"), "{page}");
        assert!(page.contains(&format!("/appeal/{action}")), "{page}");
        let notes = s.f.page_as("/notifications", cookie).await;
        assert!(notes.contains(&format!("/appeal/{action}")), "{notes}");
        s.f.page_as(&format!("/appeal/{action}"), cookie).await;
        ok_or_200(
            &s.f,
            &format!("/appeal/{action}"),
            &[("details", "I’m not a spammer")],
            cookie,
        )
        .await;
        // Once each.
        let response =
            s.f.submit(
                &format!("/appeal/{action}"),
                &[("details", "again")],
                Some(cookie),
            )
            .await;
        assert_status(&response, StatusCode::FORBIDDEN);
    }
    // Someone outside the branch can't.
    let response =
        s.f.get_as(&format!("/appeal/{action}"), Some(&s.ruth_cookie))
            .await;
    assert_status(&response, StatusCode::NOT_FOUND);
    let suspended = || async {
        sqlx::query_scalar::<_, String>(
            "SELECT username::text FROM users WHERE suspended_until > now() ORDER BY username",
        )
        .fetch_all(&s.f.db)
        .await
        .unwrap()
    };
    reverse(&s.f, appeal_against(&s.f, action, otto).await, &s.mo_cookie).await;
    assert_eq!(suspended().await, vec!["nell", "pia"]);
    // Pia is suspended again on her own account; reversing Nell's appeal
    // lifts the rest of the branch but not that.
    ok(
        &s.f,
        "/mod/users/pia/suspend",
        &[("days", "7"), ("reason", "Spam of her own")],
        &s.mo_cookie,
    )
    .await;
    reverse(&s.f, appeal_against(&s.f, action, nell).await, &s.mo_cookie).await;
    assert_eq!(suspended().await, vec!["pia"]);
}

/// Reversing a rejected post's appeal posts it after all.
#[sqlx::test]
async fn reversing_a_rejection_posts_it(pool: PgPool) {
    let mut s = setup(pool).await;
    s.f = s.f.without_rate_limits();
    settings::set(&s.f.db, "messages", &MessageSettings::default())
        .await
        .unwrap();
    let tomasz = s.f.member("tomasz", "user", None).await;
    let cookie = s.f.session(tomasz).await;
    ok(
        &s.f,
        "/c/games/new",
        &[("title", "Hi from Leeds!"), ("body", "Hello, I love Brass")],
        &cookie,
    )
    .await;
    let held = count(&s.f, "SELECT id FROM held_posts").await;
    ok(
        &s.f,
        &format!("/mod/held/{held}/reject"),
        &[("reason", "Looks like spam")],
        &s.mo_cookie,
    )
    .await;
    let action = last_action(&s.f).await;
    ok_or_200(
        &s.f,
        &format!("/appeal/{action}"),
        &[("details", "I really do love Brass")],
        &cookie,
    )
    .await;
    reverse(
        &s.f,
        appeal_against(&s.f, action, tomasz).await,
        &s.mo_cookie,
    )
    .await;
    assert!(s.f.page("/c/games").await.contains("Hi from Leeds!"));
    assert!(
        holds(
            &s.f,
            &format!("held_posts WHERE id = {held} AND status = 'approved'")
        )
        .await
    );
}

/// The notice on every page links to the restriction in force, not merely
/// the newest one.
#[sqlx::test]
async fn the_restriction_notice_contests_the_one_in_force(pool: PgPool) {
    let s = setup(pool).await;
    ok(
        &s.f,
        "/mod/users/jonah/silence",
        &[("days", "30"), ("reason", "Rude")],
        &s.mo_cookie,
    )
    .await;
    let silence = last_action(&s.f).await;
    ok(
        &s.f,
        "/mod/users/jonah/suspend",
        &[("days", "1"), ("reason", "Very rude")],
        &s.mo_cookie,
    )
    .await;
    let suspension = last_action(&s.f).await;
    let page = s.f.page_as("/", &s.jonah_cookie).await;
    assert!(page.contains(&format!("/appeal/{suspension}")), "{page}");
    // The suspension runs out.
    s.f.exec(&format!(
        "UPDATE users SET suspended_until = now() - interval '1 second' WHERE id = {}",
        s.jonah
    ))
    .await;
    let page = s.f.page_as("/", &s.jonah_cookie).await;
    assert!(page.contains("Your account is silenced"), "{page}");
    assert!(page.contains(&format!("/appeal/{silence}")), "{page}");
    assert!(!page.contains(&format!("/appeal/{suspension}")), "{page}");
}

// ---------------------------------------------------------------------------
// The moderators' pages

/// Staff reading a conversation under a break-glass grant can't react in it.
#[sqlx::test]
async fn staff_dont_react_in_conversations_they_read_under_a_grant(pool: PgPool) {
    let s = setup(pool).await;
    let response = ok(
        &s.f,
        "/messages/new",
        &[("to", "ruth"), ("body", "why are you ignoring me")],
        &s.jonah_cookie,
    )
    .await;
    let conversation: String = response
        .location()
        .trim_start_matches("/messages/")
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    let post = count(&s.f, "SELECT max(id) FROM posts").await;
    report(&s.f, &s.ruth_cookie, post, "harassment", "").await;
    parlour::staff_access::open(
        &s.f.db,
        &parlour::staff_access::NewAccess {
            actor_id: s.f.user_id("mo").await,
            scope: parlour::staff_access::Scope::Thread(conversation.parse().unwrap()),
            report_id: None,
            reason: "The report",
        },
    )
    .await
    .unwrap();
    let page =
        s.f.page_as(&format!("/messages/{conversation}"), &s.mo_cookie)
            .await;
    assert!(page.contains("why are you ignoring me"), "{page}");
    assert!(
        !page.contains(&format!("action=\"/p/{post}/reactions\"")),
        "{page}"
    );
    let response =
        s.f.submit(
            &format!("/p/{post}/reactions"),
            &[("emoji", "👍"), ("action", "add")],
            Some(&s.mo_cookie),
        )
        .await;
    // A grant opens the conversation's page to read, and nothing else.
    assert_status(&response, StatusCode::NOT_FOUND);
    assert_eq!(count(&s.f, "SELECT count(*) FROM reactions").await, 0);
    // Those in it still can.
    ok(
        &s.f,
        &format!("/p/{post}/reactions"),
        &[("emoji", "👍"), ("action", "add")],
        &s.ruth_cookie,
    )
    .await;
    assert_eq!(count(&s.f, "SELECT count(*) FROM reactions").await, 1);
}

/// Deleted threads and posts can be found and restored from the pages.
#[sqlx::test]
async fn deleted_threads_and_posts_can_be_restored_from_the_pages(pool: PgPool) {
    let s = setup(pool).await;
    let t =
        s.f.thread(s.games, s.jonah, "Christmas market", "Who’s going?")
            .await;
    let reply = s.f.post(t.thread_id, s.maya, "Buy watches").await;
    let action = format!("/mod/t/{}", t.thread_id);
    ok(
        &s.f,
        &format!("/mod/p/{reply}"),
        &[("action", "delete"), ("reason", "Spam")],
        &s.mo_cookie,
    )
    .await;
    // The tombstone leads staff to the post, and the log does too.
    let page =
        s.f.page_as(
            &format!("/t/{}/christmas-market", t.thread_id),
            &s.mo_cookie,
        )
        .await;
    assert!(
        post_html(&page, reply).contains(&format!("href=\"/mod/p/{reply}\"")),
        "{page}"
    );
    let page =
        s.f.page_as(
            &format!("/t/{}/christmas-market", t.thread_id),
            &s.ruth_cookie,
        )
        .await;
    assert!(!page.contains("/mod/p/"), "{page}");
    let log = s.f.page_as("/mod/log", &s.mo_cookie).await;
    assert!(log.contains(&format!("href=\"/mod/p/{reply}\"")), "{log}");
    let page = s.f.page_as(&format!("/mod/p/{reply}"), &s.mo_cookie).await;
    assert!(page.contains("value=\"restore\""), "{page}");

    // Deleting the thread leads to its page, which offers to restore it.
    let response = ok(
        &s.f,
        &action,
        &[("action", "delete"), ("reason", "Spam")],
        &s.mo_cookie,
    )
    .await;
    assert!(
        response.location().starts_with(&action),
        "{}",
        response.location()
    );
    let page = s.f.page_as(response.location(), &s.mo_cookie).await;
    assert!(page.contains("Christmas market"), "{page}");
    assert!(page.contains("value=\"restore\""), "{page}");
    assert!(!page.contains("value=\"lock\""), "{page}");
    let log = s.f.page_as("/mod/log", &s.mo_cookie).await;
    assert!(log.contains(&format!("href=\"{action}\"")), "{log}");
    // Members still find nothing there.
    let response = s.f.get_as(&action, Some(&s.ruth_cookie)).await;
    assert_status(&response, StatusCode::NOT_FOUND);
    assert!(!response.body.contains("Christmas market"));
    ok(
        &s.f,
        &action,
        &[("action", "restore"), ("reason", "Not spam")],
        &s.mo_cookie,
    )
    .await;
    s.f.page_as(
        &format!("/t/{}/christmas-market", t.thread_id),
        &s.ruth_cookie,
    )
    .await;
}

/// The parts of a form, one string for each `<form>`.
fn forms(html: &str) -> Vec<&str> {
    html.split("<form")
        .skip(1)
        .map(|rest| rest.split("</form>").next().unwrap())
        .collect()
}

/// Pressing Enter in a moderation form's text box does nothing: in forms
/// with several actions, the first submit button, which implicit
/// submission uses, is disabled.
fn assert_enter_does_nothing(html: &str) {
    let mut checked = 0;
    for form in forms(html) {
        let buttons: Vec<&str> = form
            .split("<button")
            .skip(1)
            .map(|b| b.split('>').next().unwrap())
            .filter(|b| !b.contains("type=\"button\""))
            .collect();
        if buttons.len() > 1 {
            assert!(buttons[0].contains("disabled"), "{form}");
            checked += 1;
        }
    }
    assert!(checked > 0, "{html}");
}

#[sqlx::test]
async fn enter_in_a_moderation_form_does_nothing(pool: PgPool) {
    let mut s = setup(pool).await;
    s.f = s.f.without_rate_limits();
    let t = s.f.thread(s.games, s.jonah, "Thread", "Spam here").await;
    report(&s.f, &s.ruth_cookie, t.first_post_id, "spam", "").await;
    s.f.submit(
        "/u/jonah/report",
        &[("reason", "spam")],
        Some(&s.ruth_cookie),
    )
    .await;
    ok(
        &s.f,
        "/mod/users/maya/silence",
        &[("days", "1"), ("reason", "Rude")],
        &s.mo_cookie,
    )
    .await;
    ok_or_200(
        &s.f,
        &format!("/appeal/{}", last_action(&s.f).await),
        &[("details", "Unfair")],
        &s.maya_cookie,
    )
    .await;
    settings::set(&s.f.db, "messages", &MessageSettings::default())
        .await
        .unwrap();
    let tomasz = s.f.member("tomasz", "user", None).await;
    let cookie = s.f.session(tomasz).await;
    ok(
        &s.f,
        "/c/games/new",
        &[("title", "Hi"), ("body", "Hello")],
        &cookie,
    )
    .await;
    let queue = s.f.page_as("/mod/queue", &s.mo_cookie).await;
    for kind in [
        "Report",
        "Report on a member",
        "Appeal",
        "Held for approval",
    ] {
        let item = queue
            .split("<article class=\"panel queue-item\"")
            .find(|item| item.contains(&format!(">{kind}</span>")))
            .unwrap_or_else(|| panic!("{kind}: {queue}"));
        assert_enter_does_nothing(item);
    }
    // The held post's item has no use for a duration.
    let held = queue
        .split("<article class=\"panel queue-item\"")
        .find(|item| item.contains("Held for approval"))
        .unwrap();
    assert!(!held.contains("name=\"days\""), "{held}");
    for path in [
        "/mod/users/jonah".to_owned(),
        format!("/mod/t/{}", t.thread_id),
        format!("/mod/p/{}", t.first_post_id),
    ] {
        assert_enter_does_nothing(&s.f.page_as(&path, &s.mo_cookie).await);
    }
}

/// Deleting a spammer can't be undone, so it is asked about first.
#[sqlx::test]
async fn deleting_a_spammer_is_confirmed_first(pool: PgPool) {
    let s = setup(pool).await;
    let t =
        s.f.thread(s.games, s.jonah, "Jonah's thread", "Hello")
            .await;
    s.f.post(t.thread_id, s.ruth, "Ruth's reply").await;
    let response =
        s.f.submit(
            "/mod/users/jonah/spammer",
            &[("reason", "Spam"), ("back", "/mod/queue?done=1")],
            Some(&s.mo_cookie),
        )
        .await;
    assert_status(&response, StatusCode::OK);
    assert!(
        response.body.contains("Delete jonah as a spammer?"),
        "{}",
        response.body
    );
    assert!(response.body.contains("1 thread"), "{}", response.body);
    assert!(
        response.body.contains("1 reply by other members"),
        "{}",
        response.body
    );
    assert!(
        response.body.contains("value=\"Spam\""),
        "{}",
        response.body
    );
    assert_eq!(
        count(
            &s.f,
            "SELECT count(*) FROM users WHERE deleted_at IS NOT NULL"
        )
        .await,
        0
    );
    assert_eq!(count(&s.f, "SELECT count(*) FROM mod_actions").await, 0);
    ok(
        &s.f,
        "/mod/users/jonah/spammer",
        &[("reason", "Spam"), ("confirm", "yes")],
        &s.mo_cookie,
    )
    .await;
    assert_eq!(
        count(
            &s.f,
            "SELECT count(*) FROM users WHERE deleted_at IS NOT NULL"
        )
        .await,
        1
    );
}

/// A silenced member may answer the moderators, but not other members in
/// the same conversation.
#[sqlx::test]
async fn silenced_members_dont_message_members_through_staff(pool: PgPool) {
    let s = setup(pool).await;
    let response = ok(
        &s.f,
        "/messages/new",
        &[("to", "jonah, ruth"), ("body", "Let’s sort this out")],
        &s.mo_cookie,
    )
    .await;
    let conversation: String = response
        .location()
        .trim_start_matches("/messages/")
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    ok(
        &s.f,
        "/mod/users/jonah/silence",
        &[("days", "1"), ("reason", "x")],
        &s.mo_cookie,
    )
    .await;
    let response =
        s.f.submit(
            &format!("/messages/{conversation}"),
            &[("body", "Ruth, listen")],
            Some(&s.jonah_cookie),
        )
        .await;
    assert_status(&response, StatusCode::FORBIDDEN);
}

/// A post moved out of a thread and back has one anchor there.
#[sqlx::test]
async fn a_post_moved_away_and_back_has_one_anchor(pool: PgPool) {
    let s = setup(pool).await;
    let a = s.f.thread(s.games, s.ruth, "Thread A", "A one").await;
    let moving = s.f.post(a.thread_id, s.jonah, "Wanderer").await;
    s.f.post(a.thread_id, s.ruth, "A three").await;
    let b = s.f.thread(s.games, s.ruth, "Thread B", "B one").await;
    let move_to = |from: i64, numbers: &'static str, to: i64| {
        let f = &s.f;
        let cookie = s.mo_cookie.clone();
        async move {
            ok(
                f,
                &format!("/mod/t/{from}/move-posts"),
                &[
                    ("numbers", numbers),
                    ("to", "existing"),
                    ("thread", &to.to_string()),
                    ("reason", "Tidying"),
                ],
                &cookie,
            )
            .await;
        }
    };
    move_to(a.thread_id, "2", b.thread_id).await;
    move_to(b.thread_id, "2", a.thread_id).await;
    let page = s.f.page(&format!("/t/{}/thread-a", a.thread_id)).await;
    assert!(page.contains("Wanderer"), "{page}");
    assert_eq!(
        page.matches(&format!("id=\"p{moving}\"")).count(),
        1,
        "{page}"
    );
}

/// The first unread post of a thread merged into another is found there.
#[sqlx::test]
async fn unread_follows_a_merge(pool: PgPool) {
    let s = setup(pool).await;
    let target = s.f.thread(s.games, s.ruth, "Main thread", "Main one").await;
    let dupe = s.f.thread(s.games, s.jonah, "Duplicate", "Dupe one").await;
    ok(
        &s.f,
        &format!("/mod/t/{}/move-posts", dupe.thread_id),
        &[
            ("numbers", "1"),
            ("to", "existing"),
            ("thread", &target.thread_id.to_string()),
            ("reason", "Merge"),
        ],
        &s.mo_cookie,
    )
    .await;
    let response =
        s.f.get_as(
            &format!("/t/{}/unread", dupe.thread_id),
            Some(&s.maya_cookie),
        )
        .await;
    assert_status(&response, StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(
        response.location(),
        format!("/t/{}/unread", target.thread_id)
    );
}
