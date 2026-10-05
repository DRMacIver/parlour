//! Those who run a space moderate in it (DESIGN.md §3.1, §3.2): a space's
//! administrators, and a feed's owner, pin, lock, rename and delete its
//! threads, hide and delete posts in it, and move threads out of it to
//! another space they run, from the same moderation pages staff use. What
//! they do is logged like any moderator's action. Acting on members
//! (silences, suspensions, notes, the queue) stays with staff.

mod common;

use axum::http::StatusCode;
use common::{Forum, Response, post_html};
use parlour::permissions::Grant;
use sqlx::PgPool;

/// A forum where ruth runs her feed and a topic space, `boardgames`, which
/// any member reads and posts in; jonah and maya are members, and mo is a
/// moderator. General is an ordinary members' space run by staff.
struct Setup {
    f: Forum,
    ruth: i64,
    jonah: i64,
    maya: i64,
    boardgames: i64,
    general: i64,
    ruth_cookie: String,
    jonah_cookie: String,
    maya_cookie: String,
    mo_cookie: String,
}

async fn setup(pool: PgPool) -> Setup {
    let f = Forum::new(pool);
    f.no_new_members().await;
    let ruth = f.member("ruth", "user", None).await;
    let jonah = f.member("jonah", "user", None).await;
    let maya = f.member("maya", "user", None).await;
    let mo = f.member("mo", "moderator", None).await;
    let boardgames = f.space("boardgames", "members", None, 0).await;
    f.set_grant(boardgames, "administer", Grant::listed([], [ruth]))
        .await;
    let general = f.space("general", "members", None, 1).await;
    Setup {
        ruth_cookie: f.session(ruth).await,
        jonah_cookie: f.session(jonah).await,
        maya_cookie: f.session(maya).await,
        mo_cookie: f.session(mo).await,
        f,
        ruth,
        jonah,
        maya,
        boardgames,
        general,
    }
}

async fn feed(f: &Forum, owner: i64) -> i64 {
    sqlx::query_scalar("SELECT id FROM spaces WHERE kind = 'feed' AND owner_user_id = $1")
        .bind(owner)
        .fetch_one(&f.db)
        .await
        .unwrap()
}

async fn count(f: &Forum, sql: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(sql.to_owned()))
        .fetch_one(&f.db)
        .await
        .unwrap()
}

fn assert_status(response: &Response, status: StatusCode) {
    assert_eq!(response.status, status, "{}", response.body);
}

async fn act(f: &Forum, path: &str, fields: &[(&str, &str)], cookie: &str) -> Response {
    f.submit(path, fields, Some(cookie)).await
}

#[sqlx::test]
async fn a_feed_owner_hides_a_reply_and_locks_the_thread(pool: PgPool) {
    let s = setup(pool).await;
    let ruth_feed = feed(&s.f, s.ruth).await;
    let t =
        s.f.thread(ruth_feed, s.ruth, "My week", "Went walking.")
            .await;
    let reply = s.f.post(t.thread_id, s.jonah, "You are awful").await;
    let path = format!("/t/{}/my-week", t.thread_id);
    // Ruth is offered the controls on the thread page; Maya isn't.
    let page = s.f.page_as(&path, &s.ruth_cookie).await;
    assert!(
        page.contains(&format!("href=\"/mod/t/{}\"", t.thread_id)),
        "{page}"
    );
    assert!(
        post_html(&page, reply).contains(&format!("href=\"/mod/p/{reply}\"")),
        "{page}"
    );
    let page = s.f.page_as(&path, &s.maya_cookie).await;
    assert!(!page.contains("/mod/"), "{page}");
    // The post's moderation page is hers, without the staff's tools.
    let mod_page =
        s.f.page_as(&format!("/mod/p/{reply}"), &s.ruth_cookie)
            .await;
    assert!(mod_page.contains("Hide from members"), "{mod_page}");
    assert!(!mod_page.contains("/mod/queue"), "{mod_page}");
    assert!(!mod_page.contains("/mod/users/"), "{mod_page}");
    assert!(!mod_page.contains("/edit"), "{mod_page}");
    // She hides it.
    let response = act(
        &s.f,
        &format!("/mod/p/{reply}"),
        &[("action", "hide"), ("reason", "Rude")],
        &s.ruth_cookie,
    )
    .await;
    assert_status(&response, StatusCode::SEE_OTHER);
    let page = s.f.page_as(&path, &s.maya_cookie).await;
    assert!(!page.contains("You are awful"), "{page}");
    // She still sees it, marked, so that she can show it again.
    let page = s.f.page_as(&path, &s.ruth_cookie).await;
    let html = post_html(&page, reply);
    assert!(html.contains("You are awful"), "{html}");
    assert!(html.contains("Hidden from members"), "{html}");
    // It is logged as hers, and Jonah is told who did it.
    assert_eq!(
        count(
            &s.f,
            &format!(
                "SELECT count(*) FROM mod_actions WHERE actor_id = {} AND action = 'hide_post' AND post_id = {reply}",
                s.ruth
            )
        )
        .await,
        1
    );
    let notes = s.f.page_as("/notifications", &s.jonah_cookie).await;
    assert!(
        notes.contains(
            "Ruth, who runs the space it is in, hid your post № 2 in a thread. Reason: Rude"
        ),
        "{notes}"
    );
    let log = s.f.page("/moderation").await;
    assert!(log.contains("ruth</strong> hid"), "{log}");
    // She locks the thread: Jonah can no longer reply, but she can.
    let response = act(
        &s.f,
        &format!("/mod/t/{}", t.thread_id),
        &[("action", "lock"), ("reason", "Enough")],
        &s.ruth_cookie,
    )
    .await;
    assert_status(&response, StatusCode::SEE_OTHER);
    let response = act(
        &s.f,
        &format!("/t/{}/reply", t.thread_id),
        &[("body", "One more thing")],
        &s.jonah_cookie,
    )
    .await;
    assert_ne!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    let response = act(
        &s.f,
        &format!("/t/{}/reply", t.thread_id),
        &[("body", "Closing this, thanks all.")],
        &s.ruth_cookie,
    )
    .await;
    assert_status(&response, StatusCode::SEE_OTHER);
    let notes = s.f.page_as("/notifications", &s.ruth_cookie).await;
    assert!(!notes.contains("locked one of your threads"), "{notes}");
    // And shows the reply again.
    let response = act(
        &s.f,
        &format!("/mod/p/{reply}"),
        &[("action", "unhide"), ("reason", "Apologised")],
        &s.ruth_cookie,
    )
    .await;
    assert_status(&response, StatusCode::SEE_OTHER);
    assert!(
        s.f.page_as(&path, &s.maya_cookie)
            .await
            .contains("You are awful")
    );
}

#[sqlx::test]
async fn a_space_administrator_pins_renames_deletes_and_moves_threads(pool: PgPool) {
    let s = setup(pool).await;
    let rules = s.f.thread(s.boardgames, s.jonah, "Rules", "Be kind.").await;
    let action = format!("/mod/t/{}", rules.thread_id);
    // The thread's moderation page is hers, without moving posts or the
    // staff's navigation.
    let page = s.f.page_as(&action, &s.ruth_cookie).await;
    assert!(page.contains("value=\"pin\""), "{page}");
    assert!(!page.contains("move-posts"), "{page}");
    assert!(!page.contains("/mod/queue"), "{page}");
    // She may move it only to spaces she runs, which General isn't.
    assert!(!page.contains("value=\"general\""), "{page}");
    for (fields, done) in [
        (vec![("action", "pin"), ("reason", "House rules")], "pinned"),
        (
            vec![
                ("action", "rename"),
                ("title", "House rules"),
                ("reason", "Clearer"),
            ],
            "title",
        ),
    ] {
        let response = act(&s.f, &action, &fields, &s.ruth_cookie).await;
        assert_status(&response, StatusCode::SEE_OTHER);
        let _ = done;
    }
    assert_eq!(
        count(
            &s.f,
            &format!(
                "SELECT count(*) FROM threads WHERE id = {} AND pinned AND title = 'House rules'",
                rules.thread_id
            )
        )
        .await,
        1
    );
    // Moving it to a space she doesn't run is refused.
    let response = act(
        &s.f,
        &action,
        &[("action", "move"), ("space", "general"), ("reason", "Tidy")],
        &s.ruth_cookie,
    )
    .await;
    assert_ne!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    assert_eq!(
        count(
            &s.f,
            &format!(
                "SELECT space_id FROM threads WHERE id = {}",
                rules.thread_id
            )
        )
        .await,
        s.boardgames
    );
    // To one she runs too, it goes.
    let ruth_feed = feed(&s.f, s.ruth).await;
    let response = act(
        &s.f,
        &action,
        &[("action", "move"), ("space", "ruth"), ("reason", "Mine")],
        &s.ruth_cookie,
    )
    .await;
    assert_status(&response, StatusCode::SEE_OTHER);
    assert_eq!(
        count(
            &s.f,
            &format!(
                "SELECT space_id FROM threads WHERE id = {}",
                rules.thread_id
            )
        )
        .await,
        ruth_feed
    );
    // She deletes another; restoring it is for staff.
    let spam = s.f.thread(s.boardgames, s.jonah, "Buy now", "Cheap").await;
    let response = act(
        &s.f,
        &format!("/mod/t/{}", spam.thread_id),
        &[("action", "delete"), ("reason", "Spam")],
        &s.ruth_cookie,
    )
    .await;
    assert_status(&response, StatusCode::SEE_OTHER);
    assert_eq!(
        count(
            &s.f,
            &format!(
                "SELECT count(*) FROM threads WHERE id = {} AND deleted_at IS NOT NULL",
                spam.thread_id
            )
        )
        .await,
        1
    );
    let response = act(
        &s.f,
        &format!("/mod/t/{}", spam.thread_id),
        &[("action", "restore"), ("reason", "Oops")],
        &s.ruth_cookie,
    )
    .await;
    assert_status(&response, StatusCode::NOT_FOUND);
    let notes = s.f.page_as("/notifications", &s.jonah_cookie).await;
    assert!(
        notes.contains(
            "Ruth, who runs the space it is in, deleted one of your threads. Reason: Spam"
        ),
        "{notes}"
    );
    // Each is in the log under her name.
    assert_eq!(
        count(
            &s.f,
            &format!(
                "SELECT count(*) FROM mod_actions WHERE actor_id = {} AND action IN ('pin_thread', 'rename_thread', 'move_thread', 'delete_thread')",
                s.ruth
            )
        )
        .await,
        4
    );
}

#[sqlx::test]
async fn administrators_moderate_only_their_own_spaces(pool: PgPool) {
    let s = setup(pool).await;
    let elsewhere = s.f.thread(s.general, s.jonah, "Elsewhere", "Hello").await;
    let reply = s.f.post(elsewhere.thread_id, s.maya, "Hi").await;
    // Ruth runs nothing in General: its moderation pages are not there for
    // her, and nor are its actions.
    for path in [
        format!("/mod/t/{}", elsewhere.thread_id),
        format!("/mod/p/{reply}"),
    ] {
        assert_status(
            &s.f.get_as(&path, Some(&s.ruth_cookie)).await,
            StatusCode::NOT_FOUND,
        );
    }
    let response = act(
        &s.f,
        &format!("/mod/p/{reply}"),
        &[("action", "hide"), ("reason", "Mine now")],
        &s.ruth_cookie,
    )
    .await;
    assert_status(&response, StatusCode::NOT_FOUND);
    let response = act(
        &s.f,
        &format!("/mod/t/{}", elsewhere.thread_id),
        &[("action", "lock"), ("reason", "Mine now")],
        &s.ruth_cookie,
    )
    .await;
    assert_status(&response, StatusCode::NOT_FOUND);
    // A locked thread there stays closed to her.
    let locked = s.f.thread(s.general, s.jonah, "Closed", "Done").await;
    act(
        &s.f,
        &format!("/mod/t/{}", locked.thread_id),
        &[("action", "lock"), ("reason", "Done")],
        &s.mo_cookie,
    )
    .await;
    let response = act(
        &s.f,
        &format!("/t/{}/reply", locked.thread_id),
        &[("body", "Me too")],
        &s.ruth_cookie,
    )
    .await;
    assert_ne!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    // Acting on members, and the queue, stay with staff.
    for path in ["/mod/queue", "/mod/users", "/mod/users/jonah", "/mod/log"] {
        assert_status(
            &s.f.get_as(path, Some(&s.ruth_cookie)).await,
            StatusCode::NOT_FOUND,
        );
    }
    let response = act(
        &s.f,
        "/mod/users/jonah/silence",
        &[("days", "1"), ("reason", "Rude")],
        &s.ruth_cookie,
    )
    .await;
    assert_status(&response, StatusCode::NOT_FOUND);
    // Moving posts is for staff too.
    let t = s.f.thread(s.boardgames, s.jonah, "Two topics", "One").await;
    s.f.post(t.thread_id, s.jonah, "Two").await;
    let response = act(
        &s.f,
        &format!("/mod/t/{}/move-posts", t.thread_id),
        &[
            ("numbers", "2"),
            ("to", "new"),
            ("title", "Another"),
            ("space", "boardgames"),
            ("reason", "Split"),
        ],
        &s.ruth_cookie,
    )
    .await;
    assert_status(&response, StatusCode::NOT_FOUND);
}

#[sqlx::test]
async fn administrators_dont_undo_what_staff_did(pool: PgPool) {
    let s = setup(pool).await;
    let t = s.f.thread(s.boardgames, s.jonah, "Trades", "Swap?").await;
    let reply = s.f.post(t.thread_id, s.maya, "My address is 1 Lane").await;
    let response = act(
        &s.f,
        &format!("/mod/p/{reply}"),
        &[("action", "hide"), ("reason", "Personal information")],
        &s.mo_cookie,
    )
    .await;
    assert_status(&response, StatusCode::SEE_OTHER);
    let response = act(
        &s.f,
        &format!("/mod/p/{reply}"),
        &[("action", "unhide"), ("reason", "Fine by me")],
        &s.ruth_cookie,
    )
    .await;
    assert_status(&response, StatusCode::FORBIDDEN);
    assert_eq!(
        count(
            &s.f,
            &format!("SELECT count(*) FROM posts WHERE id = {reply} AND hidden_at IS NOT NULL")
        )
        .await,
        1
    );
    // A post she deleted herself she may restore; one a moderator deleted
    // she may not.
    let other = s.f.post(t.thread_id, s.maya, "Spam").await;
    for (cookie, action, status) in [
        (&s.ruth_cookie, "delete", StatusCode::SEE_OTHER),
        (&s.ruth_cookie, "restore", StatusCode::SEE_OTHER),
        (&s.mo_cookie, "delete", StatusCode::SEE_OTHER),
        (&s.ruth_cookie, "restore", StatusCode::FORBIDDEN),
    ] {
        let response = act(
            &s.f,
            &format!("/mod/p/{other}"),
            &[("action", action), ("reason", "Because")],
            cookie,
        )
        .await;
        assert_status(&response, status);
    }
}

#[sqlx::test]
async fn administrators_dont_moderate_threads_they_cant_read(pool: PgPool) {
    let s = setup(pool).await;
    // Jonah's thread in Boardgames is for Maya and him alone.
    let t = s.f.thread(s.boardgames, s.jonah, "Just us", "Secret").await;
    let response = act(
        &s.f,
        &format!("/t/{}/audience", t.thread_id),
        &[("read", "only"), ("read_users", "maya"), ("reply", "space")],
        &s.jonah_cookie,
    )
    .await;
    assert_status(&response, StatusCode::SEE_OTHER);
    assert_status(
        &s.f.get_as(&format!("/mod/t/{}", t.thread_id), Some(&s.ruth_cookie))
            .await,
        StatusCode::NOT_FOUND,
    );
    let response = act(
        &s.f,
        &format!("/mod/t/{}", t.thread_id),
        &[("action", "lock"), ("reason", "Unseen")],
        &s.ruth_cookie,
    )
    .await;
    assert_status(&response, StatusCode::NOT_FOUND);
    assert_eq!(
        count(
            &s.f,
            &format!(
                "SELECT count(*) FROM threads WHERE id = {} AND locked",
                t.thread_id
            )
        )
        .await,
        0
    );
    let _ = s.maya;
}
