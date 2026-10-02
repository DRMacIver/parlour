//! Notifications (milestone 6, DESIGN.md §9): who is told about what,
//! watching and muting, the notifications page and badge, email for
//! members who are away with one-click unsubscribe, and live updates over
//! server-sent events.

mod common;

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{Forum, ORIGIN, Response};
use http_body_util::BodyExt;
use parlour::app;
use parlour::settings::{self, MessageSettings, NotificationSettings};
use sqlx::PgPool;
use tower::ServiceExt;

/// A forum with a public category `games`, a members-only `lounge` and a
/// group category `council` only the Council group (ruth) reads, members
/// ruth, jonah and maya with sessions, and notification email sent as soon
/// as it is due.
struct Setup {
    f: Forum,
    games: i64,
    lounge: i64,
    council: i64,
    ruth: i64,
    ruth_cookie: String,
    jonah: i64,
    jonah_cookie: String,
    maya: i64,
    maya_cookie: String,
}

async fn setup(pool: PgPool) -> Setup {
    let f = Forum::new(pool);
    let games = f.category("games", "public", None, 0).await;
    let lounge = f.category("lounge", "members", None, 1).await;
    let council = f.category("council", "public", None, 2).await;
    let ruth = f.member("ruth", "user", None).await;
    let jonah = f.member("jonah", "user", None).await;
    let maya = f.member("maya", "user", None).await;
    let group = f.group("council", "Council").await;
    f.join_group(group, ruth).await;
    f.restrict(council, &[group], None).await;
    let ruth_cookie = f.session(ruth).await;
    let jonah_cookie = f.session(jonah).await;
    let maya_cookie = f.session(maya).await;
    settings::set(
        &f.db,
        "notifications",
        &NotificationSettings {
            email_delay_minutes: 0,
        },
    )
    .await
    .unwrap();
    settings::set(
        &f.db,
        "messages",
        &MessageSettings {
            new_user_days: 0,
            new_user_posts: 0,
            ..MessageSettings::default()
        },
    )
    .await
    .unwrap();
    Setup {
        f,
        games,
        lounge,
        council,
        ruth,
        ruth_cookie,
        jonah,
        jonah_cookie,
        maya,
        maya_cookie,
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

/// The post id at the end of a `#p{id}` location.
fn anchored_post(location: &str) -> i64 {
    location.rsplit_once("#p").unwrap().1.parse().unwrap()
}

/// Starts a thread over HTTP, as a member would, returning its id and
/// first post.
async fn new_thread(
    f: &Forum,
    cookie: &str,
    category: &str,
    title: &str,
    body: &str,
) -> (i64, i64) {
    let response = f
        .submit(
            &format!("/c/{category}/new"),
            &[("title", title), ("body", body)],
            Some(cookie),
        )
        .await;
    assert_status(&response, StatusCode::SEE_OTHER);
    let post = anchored_post(response.location());
    let thread = count(f, &format!("SELECT thread_id FROM posts WHERE id = {post}")).await;
    (thread, post)
}

async fn reply(f: &Forum, cookie: &str, thread: i64, body: &str, to: Option<i64>) -> i64 {
    let to = to.map(|p| p.to_string());
    let mut fields = vec![("body", body)];
    if let Some(to) = &to {
        fields.push(("reply_to", to));
    }
    let response = f
        .submit(&format!("/t/{thread}/reply"), &fields, Some(cookie))
        .await;
    assert_status(&response, StatusCode::SEE_OTHER);
    anchored_post(response.location())
}

/// The kinds of notification a member has, after running the jobs that
/// make them.
async fn kinds(f: &Forum, user: i64) -> Vec<String> {
    f.run_jobs().await;
    sqlx::query_scalar::<_, String>(
        "SELECT kind || ':' || coalesce(post_id::text, '') FROM notifications WHERE user_id = $1 ORDER BY id",
    )
    .bind(user)
    .fetch_all(&f.db)
    .await
    .unwrap()
}

async fn watch(f: &Forum, cookie: &str, path: &str, level: &str) {
    let response = f.submit(path, &[("level", level)], Some(cookie)).await;
    assert_status(&response, StatusCode::SEE_OTHER);
}

// ---------------------------------------------------------------------------
// Who is told

#[sqlx::test]
async fn replies_and_mentions_notify(pool: PgPool) {
    let s = setup(pool).await;
    let (thread, first) = new_thread(&s.f, &s.ruth_cookie, "games", "Brass", "Who's in?").await;
    let answer = reply(
        &s.f,
        &s.jonah_cookie,
        thread,
        "Me! And @maya too",
        Some(first),
    )
    .await;
    assert_eq!(kinds(&s.f, s.ruth).await, vec![format!("reply:{answer}")]);
    assert_eq!(kinds(&s.f, s.maya).await, vec![format!("mention:{answer}")]);
    // Nobody hears about their own posts.
    assert!(kinds(&s.f, s.jonah).await.is_empty());
    // One notification a post, of the most specific kind.
    let both = reply(&s.f, &s.jonah_cookie, thread, "Right, @ruth?", Some(first)).await;
    assert_eq!(
        kinds(&s.f, s.ruth).await,
        vec![format!("reply:{answer}"), format!("mention:{both}")]
    );
    // Running the jobs again changes nothing.
    s.f.exec("UPDATE jobs SET run_at = now()").await;
    assert_eq!(kinds(&s.f, s.ruth).await.len(), 2);
    let _ = (s.games, s.lounge);
}

#[sqlx::test]
async fn nobody_is_told_about_posts_they_cannot_read(pool: PgPool) {
    let s = setup(pool).await;
    // Only ruth reads the council category.
    let (thread, _) = new_thread(&s.f, &s.ruth_cookie, "council", "Agenda", "Hello @maya").await;
    assert!(kinds(&s.f, s.maya).await.is_empty());
    let _ = thread;
    // A notification about a post that later moves out of sight goes too.
    let (open, first) = new_thread(&s.f, &s.maya_cookie, "games", "Open", "Hi").await;
    reply(&s.f, &s.jonah_cookie, open, "Hello", Some(first)).await;
    assert_eq!(kinds(&s.f, s.maya).await.len(), 1);
    let page = s.f.page_as("/notifications", &s.maya_cookie).await;
    assert!(page.contains("Open"), "{page}");
    assert!(page.contains("count-badge"), "{page}");
    parlour::store::move_thread(s.f.state.db(), open, s.council)
        .await
        .unwrap();
    let page = s.f.page_as("/notifications", &s.maya_cookie).await;
    assert!(!page.contains("Open"), "{page}");
    assert!(!page.contains("count-badge"), "{page}");
}

#[sqlx::test]
async fn watching_and_muting_threads(pool: PgPool) {
    let s = setup(pool).await;
    let (thread, _) = new_thread(&s.f, &s.ruth_cookie, "games", "Brass", "Who's in?").await;
    // The author watches what they start; others hear nothing by default.
    let one = reply(&s.f, &s.jonah_cookie, thread, "Me", None).await;
    assert_eq!(
        kinds(&s.f, s.ruth).await,
        vec![format!("watched_thread:{one}")]
    );
    assert!(kinds(&s.f, s.maya).await.is_empty());
    // Maya watches it.
    let page =
        s.f.page_as(&format!("/t/{thread}/brass"), &s.maya_cookie)
            .await;
    assert!(
        page.contains(&format!("action=\"/t/{thread}/watch\"")),
        "{page}"
    );
    watch(
        &s.f,
        &s.maya_cookie,
        &format!("/t/{thread}/watch"),
        "watching",
    )
    .await;
    let two = reply(&s.f, &s.jonah_cookie, thread, "Still me", None).await;
    assert_eq!(
        kinds(&s.f, s.maya).await,
        vec![format!("watched_thread:{two}")]
    );
    // Ruth mutes it: not even replies to her reach her.
    watch(&s.f, &s.ruth_cookie, &format!("/t/{thread}/watch"), "muted").await;
    let first = count(
        &s.f,
        &format!("SELECT min(id) FROM posts WHERE thread_id = {thread}"),
    )
    .await;
    assert_eq!(kinds(&s.f, s.ruth).await.len(), 2);
    reply(&s.f, &s.jonah_cookie, thread, "@ruth?", Some(first)).await;
    assert_eq!(kinds(&s.f, s.ruth).await.len(), 2);
    let response =
        s.f.submit(
            &format!("/t/{thread}/watch"),
            &[("level", "loud")],
            Some(&s.ruth_cookie),
        )
        .await;
    assert_status(&response, StatusCode::BAD_REQUEST);
}

#[sqlx::test]
async fn watching_and_muting_categories(pool: PgPool) {
    let s = setup(pool).await;
    let page = s.f.page_as("/c/games", &s.maya_cookie).await;
    assert!(page.contains("action=\"/c/games/watch\""), "{page}");
    watch(&s.f, &s.maya_cookie, "/c/games/watch", "watching").await;
    let (thread, first) = new_thread(&s.f, &s.jonah_cookie, "games", "Heat", "Racing").await;
    assert_eq!(
        kinds(&s.f, s.maya).await,
        vec![format!("watched_category:{first}")]
    );
    // Watching a category means new threads, not every reply.
    reply(&s.f, &s.ruth_cookie, thread, "Vroom", None).await;
    assert_eq!(kinds(&s.f, s.maya).await.len(), 1);
    // Muting a category silences its threads, mentions included...
    watch(&s.f, &s.ruth_cookie, "/c/lounge/watch", "muted").await;
    let (lounge_thread, _) = new_thread(&s.f, &s.jonah_cookie, "lounge", "Chat", "Hi @ruth").await;
    assert!(kinds(&s.f, s.ruth).await.is_empty());
    // ...unless the member watches a thread there themselves.
    watch(
        &s.f,
        &s.ruth_cookie,
        &format!("/t/{lounge_thread}/watch"),
        "watching",
    )
    .await;
    let post = reply(&s.f, &s.jonah_cookie, lounge_thread, "More", None).await;
    assert_eq!(
        kinds(&s.f, s.ruth).await,
        vec![format!("watched_thread:{post}")]
    );
    let _ = s.lounge;
}

#[sqlx::test]
async fn messages_notify_the_others_in_the_conversation(pool: PgPool) {
    let s = setup(pool).await;
    let response =
        s.f.submit(
            "/messages/new",
            &[
                ("to", "jonah, maya"),
                ("title", "Brass"),
                ("body", "Tuesday?"),
            ],
            Some(&s.ruth_cookie),
        )
        .await;
    assert_status(&response, StatusCode::SEE_OTHER);
    let first = anchored_post(response.location());
    let conversation = count(
        &s.f,
        &format!("SELECT thread_id FROM posts WHERE id = {first}"),
    )
    .await;
    assert_eq!(kinds(&s.f, s.jonah).await, vec![format!("message:{first}")]);
    assert_eq!(kinds(&s.f, s.maya).await, vec![format!("message:{first}")]);
    assert!(kinds(&s.f, s.ruth).await.is_empty());
    // Maya mutes it; Jonah leaves.
    watch(
        &s.f,
        &s.maya_cookie,
        &format!("/t/{conversation}/watch"),
        "muted",
    )
    .await;
    s.f.submit(
        &format!("/messages/{conversation}/leave"),
        &[],
        Some(&s.jonah_cookie),
    )
    .await;
    let second = anchored_post(
        s.f.submit(
            &format!("/messages/{conversation}"),
            &[("body", "Anyone?")],
            Some(&s.ruth_cookie),
        )
        .await
        .location(),
    );
    assert_eq!(kinds(&s.f, s.maya).await.len(), 1);
    assert_eq!(kinds(&s.f, s.jonah).await.len(), 1);
    // A mention of someone outside the conversation tells them nothing.
    let kit = s.f.member("kit", "user", None).await;
    s.f.submit(
        &format!("/messages/{conversation}"),
        &[("body", "What would @kit say?")],
        Some(&s.ruth_cookie),
    )
    .await;
    assert!(kinds(&s.f, kit).await.is_empty());
    // Jonah's notification about the first message went when he left.
    let page = s.f.page_as("/notifications", &s.jonah_cookie).await;
    assert!(!page.contains("Brass"), "{page}");
    let _ = second;
}

#[sqlx::test]
async fn blocked_members_dont_notify(pool: PgPool) {
    let s = setup(pool).await;
    let (thread, first) = new_thread(&s.f, &s.ruth_cookie, "games", "Brass", "Hi").await;
    s.f.submit("/u/jonah/block", &[], Some(&s.ruth_cookie))
        .await;
    reply(&s.f, &s.jonah_cookie, thread, "@ruth hello", Some(first)).await;
    assert!(kinds(&s.f, s.ruth).await.is_empty());
    // Staff can't be blocked: a block on someone who becomes staff lapses.
    s.f.exec(&format!(
        "UPDATE users SET role = 'moderator' WHERE id = {}",
        s.jonah
    ))
    .await;
    let post = reply(&s.f, &s.jonah_cookie, thread, "@ruth a word", Some(first)).await;
    assert_eq!(kinds(&s.f, s.ruth).await, vec![format!("mention:{post}")]);
}

// ---------------------------------------------------------------------------
// Reading notifications

#[sqlx::test]
async fn the_notifications_page_and_badge(pool: PgPool) {
    let s = setup(pool).await;
    let page = s.f.page_as("/notifications", &s.ruth_cookie).await;
    assert!(page.contains("Nothing yet"), "{page}");
    let (thread, first) = new_thread(&s.f, &s.ruth_cookie, "games", "Brass", "Hi").await;
    let answer = reply(&s.f, &s.jonah_cookie, thread, "Hello", Some(first)).await;
    s.f.run_jobs().await;
    // Every page shows the badge.
    let index = s.f.page_as("/", &s.ruth_cookie).await;
    assert!(index.contains("data-count=\"notifications\">1<"), "{index}");
    let page = s.f.page_as("/notifications", &s.ruth_cookie).await;
    assert!(page.contains("Jonah"), "{page}");
    assert!(page.contains("replied to you in"), "{page}");
    let id = count(&s.f, "SELECT id FROM notifications").await;
    // Following it marks it read and goes to the post.
    let response =
        s.f.get_as(&format!("/notifications/{id}"), Some(&s.ruth_cookie))
            .await;
    assert_status(&response, StatusCode::SEE_OTHER);
    assert_eq!(response.location(), format!("/t/{thread}/brass#p{answer}"));
    let index = s.f.page_as("/", &s.ruth_cookie).await;
    assert!(!index.contains("data-count=\"notifications\""), "{index}");
    // Someone else's notification is nobody else's business.
    let response =
        s.f.get_as(&format!("/notifications/{id}"), Some(&s.maya_cookie))
            .await;
    assert_status(&response, StatusCode::NOT_FOUND);
    // Visitors are asked to log in.
    let response = s.f.get("/notifications").await;
    assert_status(&response, StatusCode::SEE_OTHER);
    assert!(response.header(header::SET_COOKIE).is_none());
}

#[sqlx::test]
async fn reading_a_thread_reads_its_notifications(pool: PgPool) {
    let s = setup(pool).await;
    let (thread, first) = new_thread(&s.f, &s.ruth_cookie, "games", "Brass", "Hi").await;
    reply(&s.f, &s.jonah_cookie, thread, "One", Some(first)).await;
    reply(&s.f, &s.maya_cookie, thread, "Two", None).await;
    assert_eq!(kinds(&s.f, s.ruth).await.len(), 2);
    s.f.page_as(&format!("/t/{thread}/brass"), &s.ruth_cookie)
        .await;
    assert_eq!(
        count(
            &s.f,
            &format!(
                "SELECT count(*) FROM notifications WHERE user_id = {} AND read_at IS NULL",
                s.ruth
            )
        )
        .await,
        0
    );
    // And "mark all read".
    reply(&s.f, &s.jonah_cookie, thread, "Three", None).await;
    s.f.run_jobs().await;
    let response =
        s.f.submit("/notifications/read", &[], Some(&s.ruth_cookie))
            .await;
    assert_status(&response, StatusCode::SEE_OTHER);
    assert_eq!(
        count(
            &s.f,
            "SELECT count(*) FROM notifications WHERE read_at IS NULL"
        )
        .await,
        0
    );
}

#[sqlx::test]
async fn moderation_notices(pool: PgPool) {
    let s = setup(pool).await;
    parlour::notify::moderation(&s.f.state, s.maya, None, "Please keep it civil.", None)
        .await
        .unwrap();
    let page = s.f.page_as("/notifications", &s.maya_cookie).await;
    assert!(page.contains("Please keep it civil."), "{page}");
    assert!(page.contains("data-count=\"notifications\">1<"), "{page}");
    let email = s.f.last_email_to("maya@example.com").await;
    assert!(
        email.body.contains("Please keep it civil."),
        "{}",
        email.body
    );
}

// ---------------------------------------------------------------------------
// Email

#[sqlx::test]
async fn unread_notifications_are_emailed(pool: PgPool) {
    let s = setup(pool).await;
    let (thread, first) = new_thread(&s.f, &s.ruth_cookie, "games", "Brass", "Hi").await;
    let answer = reply(&s.f, &s.jonah_cookie, thread, "Count me in", Some(first)).await;
    reply(&s.f, &s.maya_cookie, thread, "And me", None).await;
    let emails: Vec<_> =
        s.f.emails()
            .await
            .into_iter()
            .filter(|e| e.to == "ruth@example.com")
            .collect();
    // One email covers both.
    assert_eq!(emails.len(), 1, "{emails:?}");
    let email = &emails[0];
    assert!(email.body.contains("Count me in"), "{}", email.body);
    assert!(email.body.contains("And me"), "{}", email.body);
    assert!(
        email
            .body
            .contains(&format!("{ORIGIN}/t/{thread}/brass#p{answer}")),
        "{}",
        email.body
    );
    let unsubscribe = email.unsubscribe.clone().expect("an unsubscribe link");
    assert!(unsubscribe.starts_with(&format!("{ORIGIN}/unsubscribe/")));
    assert!(email.body.contains(&unsubscribe));
    // Nothing more is sent about them.
    s.f.exec("UPDATE jobs SET run_at = now()").await;
    assert_eq!(
        s.f.emails()
            .await
            .iter()
            .filter(|e| e.to == "ruth@example.com")
            .count(),
        1
    );
}

/// Regression: two email jobs for one member falling due together (as
/// they do after a burst of activity, with two workers polling on the same
/// tick) each emailed the same notifications.
#[sqlx::test]
async fn email_jobs_running_together_send_one_email(pool: PgPool) {
    let s = setup(pool).await;
    settings::set(
        &s.f.db,
        "notifications",
        &NotificationSettings {
            email_delay_minutes: 15,
        },
    )
    .await
    .unwrap();
    let (thread, first) = new_thread(&s.f, &s.ruth_cookie, "games", "Brass", "Hi").await;
    for body in ["One", "Two", "Three"] {
        reply(&s.f, &s.jonah_cookie, thread, body, Some(first)).await;
    }
    s.f.run_jobs().await;
    s.f.exec("UPDATE notifications SET created_at = created_at - interval '20 minutes'")
        .await;
    s.f.exec("UPDATE users SET last_seen_at = now() - interval '1 hour'")
        .await;
    s.f.exec("DELETE FROM jobs").await;
    for round in 0..10 {
        s.f.exec("UPDATE notifications SET emailed_at = NULL").await;
        let ids: Vec<i64> = sqlx::query_scalar(
            "SELECT id FROM notifications WHERE user_id = $1 AND emailed_at IS NULL ORDER BY id",
        )
        .bind(s.ruth)
        .fetch_all(&s.f.db)
        .await
        .unwrap();
        assert_eq!(ids.len(), 3);
        let state = &s.f.state;
        let (a, b, c) = tokio::join!(
            parlour::notify::email_if_away(state, ids[0]),
            parlour::notify::email_if_away(state, ids[1]),
            parlour::notify::email_if_away(state, ids[2]),
        );
        let queued = [a.unwrap(), b.unwrap(), c.unwrap()];
        assert_eq!(
            queued.iter().filter(|q| **q).count(),
            1,
            "round {round}: {queued:?}"
        );
        assert_eq!(
            count(&s.f, "SELECT count(*) FROM jobs WHERE kind = 'email'").await,
            1,
            "round {round}"
        );
        s.f.exec("DELETE FROM jobs").await;
    }
}

/// Regression: the email job didn't look again at blocks and mutes made
/// after the notification, though the notifications page does.
#[sqlx::test]
async fn blocks_and_mutes_made_before_the_email_stop_it(pool: PgPool) {
    let s = setup(pool).await;
    settings::set(
        &s.f.db,
        "notifications",
        &NotificationSettings {
            email_delay_minutes: 15,
        },
    )
    .await
    .unwrap();
    let away = "UPDATE users SET last_seen_at = now() - interval '1 hour'";
    let due = "UPDATE notifications SET created_at = created_at - interval '20 minutes'";
    let (thread, first) = new_thread(&s.f, &s.ruth_cookie, "games", "Brass", "Hi").await;
    // Jonah replies; Ruth blocks him before the email is due.
    reply(&s.f, &s.jonah_cookie, thread, "Blocked words", Some(first)).await;
    s.f.run_jobs().await;
    s.f.submit("/u/jonah/block", &[], Some(&s.ruth_cookie))
        .await;
    s.f.exec(away).await;
    s.f.exec(due).await;
    s.f.exec("UPDATE jobs SET run_at = now()").await;
    assert!(
        s.f.emails().await.is_empty(),
        "emailed a blocked member's post"
    );
    // Maya replies; Ruth mutes the thread before the email is due.
    reply(&s.f, &s.maya_cookie, thread, "Muted words", Some(first)).await;
    s.f.run_jobs().await;
    watch(&s.f, &s.ruth_cookie, &format!("/t/{thread}/watch"), "muted").await;
    s.f.exec(away).await;
    s.f.exec(due).await;
    s.f.exec("UPDATE jobs SET run_at = now()").await;
    assert!(s.f.emails().await.is_empty(), "emailed a muted thread");
    // Muting the category does the same for a thread with no setting of
    // its own.
    let (other, other_first) = new_thread(&s.f, &s.ruth_cookie, "games", "Other", "Hi").await;
    watch(&s.f, &s.ruth_cookie, &format!("/t/{other}/watch"), "normal").await;
    reply(
        &s.f,
        &s.maya_cookie,
        other,
        "Category words",
        Some(other_first),
    )
    .await;
    s.f.run_jobs().await;
    watch(&s.f, &s.ruth_cookie, "/c/games/watch", "muted").await;
    s.f.exec(away).await;
    s.f.exec(due).await;
    s.f.exec("UPDATE jobs SET run_at = now()").await;
    assert!(s.f.emails().await.is_empty(), "emailed a muted category");
    // Unmuted, a new reply is emailed as usual (and only that one).
    watch(&s.f, &s.ruth_cookie, "/c/games/watch", "normal").await;
    reply(&s.f, &s.maya_cookie, other, "Loud words", Some(other_first)).await;
    s.f.run_jobs().await;
    s.f.exec(away).await;
    s.f.exec(due).await;
    s.f.exec("UPDATE jobs SET run_at = now()").await;
    let emails = s.f.emails().await;
    assert_eq!(emails.len(), 1, "{emails:?}");
    assert!(emails[0].body.contains("Loud words"));
    for quiet in ["Blocked words", "Muted words", "Category words"] {
        assert!(!emails[0].body.contains(quiet), "{}", emails[0].body);
    }
}

/// A notification email that can't be sent isn't kept with the private
/// text it was to carry.
#[sqlx::test]
async fn failed_notification_email_keeps_no_private_text(pool: PgPool) {
    let s = setup(pool).await;
    let response =
        s.f.submit(
            "/messages/new",
            &[
                ("to", "jonah"),
                ("title", "Secret plans"),
                ("body", "Private words"),
            ],
            Some(&s.ruth_cookie),
        )
        .await;
    let post = anchored_post(response.location());
    let notified = parlour::notify::post_created(&s.f.state, post)
        .await
        .unwrap();
    assert_eq!(notified, vec![s.jonah]);
    let id = count(
        &s.f,
        &format!(
            "SELECT id FROM notifications WHERE user_id = {} AND post_id = {post}",
            s.jonah
        ),
    )
    .await;
    assert!(
        parlour::notify::email_if_away(&s.f.state, id)
            .await
            .unwrap()
    );
    let payload: serde_json::Value =
        sqlx::query_scalar("SELECT payload FROM jobs WHERE kind = 'email'")
            .fetch_one(&s.f.db)
            .await
            .unwrap();
    assert!(payload.to_string().contains("Private words"));
    let kept = parlour::mail::scrub(payload).to_string();
    for private in ["Private words", "Secret plans"] {
        assert!(!kept.contains(private), "{kept}");
    }
    assert!(kept.contains("jonah@example.com"), "{kept}");
}

#[sqlx::test]
async fn members_who_are_around_are_not_emailed(pool: PgPool) {
    let s = setup(pool).await;
    settings::set(
        &s.f.db,
        "notifications",
        &NotificationSettings {
            email_delay_minutes: 15,
        },
    )
    .await
    .unwrap();
    let (thread, first) = new_thread(&s.f, &s.ruth_cookie, "games", "Brass", "Hi").await;
    // Read in time: not emailed.
    reply(&s.f, &s.jonah_cookie, thread, "One", Some(first)).await;
    s.f.run_jobs().await;
    assert!(s.f.emails().await.is_empty(), "sent before the delay");
    s.f.page_as(&format!("/t/{thread}/brass"), &s.ruth_cookie)
        .await;
    // Seen on the site since, though not read: not emailed either.
    reply(&s.f, &s.maya_cookie, thread, "Two", Some(first)).await;
    s.f.run_jobs().await;
    s.f.exec(&format!(
        "UPDATE users SET last_seen_at = now() + interval '1 minute' WHERE id = {}",
        s.ruth
    ))
    .await;
    s.f.exec("UPDATE notifications SET created_at = created_at - interval '20 minutes'")
        .await;
    s.f.exec("UPDATE jobs SET run_at = now()").await;
    assert!(s.f.emails().await.is_empty());
    // Away since: emailed.
    s.f.exec(&format!(
        "UPDATE users SET last_seen_at = now() - interval '1 hour' WHERE id = {}",
        s.ruth
    ))
    .await;
    let three = reply(&s.f, &s.jonah_cookie, thread, "Three", Some(first)).await;
    s.f.run_jobs().await;
    s.f.exec("UPDATE notifications SET created_at = created_at - interval '20 minutes'")
        .await;
    s.f.exec("UPDATE jobs SET run_at = now()").await;
    let emails = s.f.emails().await;
    assert_eq!(emails.len(), 1, "{emails:?}");
    assert!(emails[0].body.contains("Three"));
    assert!(!emails[0].body.contains("Two"));
    let _ = three;
}

#[sqlx::test]
async fn email_preferences_and_one_click_unsubscribe(pool: PgPool) {
    let s = setup(pool).await;
    let page = s.f.page_as("/settings", &s.ruth_cookie).await;
    assert!(
        page.contains("action=\"/settings/notifications\""),
        "{page}"
    );
    // Ruth turns off emails about replies.
    let response =
        s.f.submit(
            "/settings/notifications",
            &[("mentions", "on"), ("messages", "on"), ("watching", "on")],
            Some(&s.ruth_cookie),
        )
        .await;
    assert_status(&response, StatusCode::SEE_OTHER);
    let (thread, first) = new_thread(&s.f, &s.ruth_cookie, "games", "Brass", "Hi").await;
    reply(&s.f, &s.jonah_cookie, thread, "A reply", Some(first)).await;
    assert!(s.f.emails().await.is_empty());
    // Mentions still come, with a link to stop them.
    reply(&s.f, &s.jonah_cookie, thread, "@ruth look", None).await;
    let email = s.f.last_email_to("ruth@example.com").await;
    let url = email.unsubscribe.unwrap();
    let path = url.strip_prefix(ORIGIN).unwrap().to_owned();
    // The link opens a page with a button, cached by nobody...
    let page = s.f.get(&path).await;
    assert_status(&page, StatusCode::OK);
    assert!(page.body.contains("mentions of you"), "{}", page.body);
    assert!(page.header(header::SET_COOKIE).is_none());
    // ...and a mail provider can POST to it with no Origin at all.
    let response =
        s.f.submit_from(&path, &[("List-Unsubscribe", "One-Click")], None, None)
            .await;
    assert_status(&response, StatusCode::OK);
    assert!(response.header(header::SET_COOKIE).is_none());
    reply(&s.f, &s.maya_cookie, thread, "@ruth again", None).await;
    assert_eq!(s.f.emails().await.len(), 1);
    let page = s.f.page_as("/settings", &s.ruth_cookie).await;
    assert!(
        !page.contains("name=\"mentions\" value=\"on\" checked"),
        "{page}"
    );
    // A tampered token does nothing.
    let forged = path.replace(&format!("/{}.", s.ruth), &format!("/{}.", s.maya));
    assert_status(&s.f.get(&forged).await, StatusCode::NOT_FOUND);
    assert_status(
        &s.f.submit_from(&forged, &[], None, None).await,
        StatusCode::NOT_FOUND,
    );
    // Other forms still need to come from this site.
    let response =
        s.f.submit_from("/settings/notifications", &[], Some(&s.maya_cookie), None)
            .await;
    assert_status(&response, StatusCode::FORBIDDEN);
}

#[sqlx::test]
async fn unconfirmed_addresses_are_not_emailed(pool: PgPool) {
    let s = setup(pool).await;
    s.f.exec(&format!(
        "UPDATE users SET email_verified_at = NULL WHERE id = {}",
        s.ruth
    ))
    .await;
    let (thread, first) = new_thread(&s.f, &s.ruth_cookie, "games", "Brass", "Hi").await;
    reply(&s.f, &s.jonah_cookie, thread, "Hello", Some(first)).await;
    assert!(s.f.emails().await.is_empty());
}

#[sqlx::test]
async fn admins_set_the_rules_and_send_a_test(pool: PgPool) {
    let s = setup(pool).await;
    let sam = s.f.member("sam", "admin", None).await;
    let sam_cookie = s.f.session(sam).await;
    let page = s.f.page_as("/admin/messages", &sam_cookie).await;
    assert!(page.contains("name=\"max_recipients\""), "{page}");
    assert_status(
        &s.f.get_as("/admin/messages", Some(&s.ruth_cookie)).await,
        StatusCode::FORBIDDEN,
    );
    assert_status(
        &s.f.submit("/admin/messages/test-email", &[], Some(&s.ruth_cookie))
            .await,
        StatusCode::FORBIDDEN,
    );
    let response =
        s.f.submit(
            "/admin/messages",
            &[
                ("max_recipients", "4"),
                ("new_user_days", "7"),
                ("new_user_posts", "2"),
                ("email_delay_minutes", "30"),
            ],
            Some(&sam_cookie),
        )
        .await;
    assert_status(&response, StatusCode::SEE_OTHER);
    let messages = settings::message_settings(s.f.state.db()).await.unwrap();
    assert_eq!(
        messages,
        MessageSettings {
            enabled: false,
            max_recipients: 4,
            new_user_days: 7,
            new_user_posts: 2
        }
    );
    assert_eq!(
        settings::notification_settings(s.f.state.db())
            .await
            .unwrap()
            .email_delay_minutes,
        30
    );
    let response =
        s.f.submit(
            "/admin/messages",
            &[("max_recipients", "lots"), ("email_delay_minutes", "-1")],
            Some(&sam_cookie),
        )
        .await;
    assert_status(&response, StatusCode::BAD_REQUEST);
    let response =
        s.f.submit("/admin/messages/test-email", &[], Some(&sam_cookie))
            .await;
    assert_status(&response, StatusCode::SEE_OTHER);
    let email = s.f.last_email_to("sam@example.com").await;
    assert!(email.unsubscribe.is_some());
    assert!(email.body.contains("test"), "{}", email.body);
}

// ---------------------------------------------------------------------------
// Live updates

/// An open server-sent events stream.
struct Stream {
    body: Body,
    buffer: String,
}

impl Stream {
    async fn open(f: &Forum, path: &str, cookie: Option<&str>) -> Result<Stream, StatusCode> {
        Stream::open_from(f, path, cookie, None).await
    }

    /// Opens a stream from the client address `ip` (with
    /// [`Forum::trusting_proxy`]).
    async fn open_from(
        f: &Forum,
        path: &str,
        cookie: Option<&str>,
        ip: Option<&str>,
    ) -> Result<Stream, StatusCode> {
        let mut request = Request::get(path);
        if let Some(cookie) = cookie {
            request = request.header(header::COOKIE, cookie);
        }
        if let Some(ip) = ip {
            request = request.header("x-forwarded-for", ip);
        }
        let response = app(f.state.clone())
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        if response.status() != StatusCode::OK {
            return Err(response.status());
        }
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/event-stream"
        );
        assert!(response.headers().get(header::SET_COOKIE).is_none());
        Ok(Stream {
            body: response.into_body(),
            buffer: String::new(),
        })
    }

    /// The next event's data, skipping keep-alive comments, or `None` if
    /// the stream ended or nothing came within a second.
    async fn next(&mut self) -> Option<String> {
        loop {
            if let Some(end) = self.buffer.find("\n\n") {
                let event: String = self.buffer.drain(..end + 2).collect();
                if let Some(data) = event.lines().find_map(|l| l.strip_prefix("data: ")) {
                    return Some(data.to_owned());
                }
                continue;
            }
            let frame = tokio::time::timeout(Duration::from_secs(1), self.body.frame())
                .await
                .ok()??
                .ok()?;
            if let Ok(data) = frame.into_data() {
                self.buffer.push_str(std::str::from_utf8(&data).unwrap());
            }
        }
    }
}

#[sqlx::test]
async fn threads_stream_new_posts(pool: PgPool) {
    let s = setup(pool).await;
    let (thread, _) = new_thread(&s.f, &s.ruth_cookie, "games", "Brass", "Hi").await;
    // Anyone who can read the thread can follow it, visitors included.
    let mut stream = Stream::open(&s.f, &format!("/events/t/{thread}"), None)
        .await
        .unwrap();
    let post = reply(&s.f, &s.jonah_cookie, thread, "Hello", None).await;
    let event = stream.next().await.expect("an event");
    assert_eq!(
        event,
        format!(r#"{{"thread":{thread},"post":{post},"seq":2}}"#)
    );
    // Posts elsewhere aren't sent.
    let (other, _) = new_thread(&s.f, &s.ruth_cookie, "games", "Other", "Hi").await;
    reply(&s.f, &s.jonah_cookie, other, "Elsewhere", None).await;
    assert_eq!(stream.next().await, None);
    // Once the thread moves out of sight, the stream ends without telling.
    parlour::store::move_thread(s.f.state.db(), thread, s.lounge)
        .await
        .unwrap();
    reply(&s.f, &s.jonah_cookie, thread, "Secret", None).await;
    assert_eq!(stream.next().await, None);
}

#[sqlx::test]
async fn private_threads_cannot_be_followed_by_outsiders(pool: PgPool) {
    let s = setup(pool).await;
    let (council, _) = new_thread(&s.f, &s.ruth_cookie, "council", "Agenda", "Hi").await;
    assert_eq!(
        Stream::open(&s.f, &format!("/events/t/{council}"), Some(&s.maya_cookie))
            .await
            .err(),
        Some(StatusCode::NOT_FOUND)
    );
    assert!(
        Stream::open(&s.f, &format!("/events/t/{council}"), Some(&s.ruth_cookie))
            .await
            .is_ok()
    );
    let response =
        s.f.submit(
            "/messages/new",
            &[("to", "jonah"), ("body", "Psst")],
            Some(&s.ruth_cookie),
        )
        .await;
    let conversation = count(
        &s.f,
        &format!(
            "SELECT thread_id FROM posts WHERE id = {}",
            anchored_post(response.location())
        ),
    )
    .await;
    for cookie in [None, Some(s.maya_cookie.as_str())] {
        assert_eq!(
            Stream::open(&s.f, &format!("/events/t/{conversation}"), cookie)
                .await
                .err(),
            Some(StatusCode::NOT_FOUND)
        );
    }
    let mut stream = Stream::open(
        &s.f,
        &format!("/events/t/{conversation}"),
        Some(&s.jonah_cookie),
    )
    .await
    .unwrap();
    s.f.submit(
        &format!("/messages/{conversation}"),
        &[("body", "Still there?")],
        Some(&s.ruth_cookie),
    )
    .await;
    assert!(stream.next().await.unwrap().contains("\"seq\":2"));
    assert_eq!(
        Stream::open(&s.f, "/events/t/x", None).await.err(),
        Some(StatusCode::NOT_FOUND)
    );
}

#[sqlx::test]
async fn the_badge_count_streams(pool: PgPool) {
    let s = setup(pool).await;
    assert_eq!(
        Stream::open(&s.f, "/events/notifications", None)
            .await
            .err(),
        Some(StatusCode::NOT_FOUND)
    );
    let mut stream = Stream::open(&s.f, "/events/notifications", Some(&s.ruth_cookie))
        .await
        .unwrap();
    assert_eq!(
        stream.next().await.unwrap(),
        r#"{"notifications":0,"messages":0}"#
    );
    let (thread, first) = new_thread(&s.f, &s.ruth_cookie, "games", "Brass", "Hi").await;
    reply(&s.f, &s.jonah_cookie, thread, "Hello", Some(first)).await;
    s.f.run_jobs().await;
    let mut last = String::new();
    while let Some(event) = stream.next().await {
        last = event;
    }
    assert_eq!(last, r#"{"notifications":1,"messages":0}"#);
    s.f.submit(
        "/messages/new",
        &[("to", "ruth"), ("body", "Psst")],
        Some(&s.maya_cookie),
    )
    .await;
    s.f.run_jobs().await;
    while let Some(event) = stream.next().await {
        last = event;
    }
    assert_eq!(last, r#"{"notifications":2,"messages":1}"#);
    // Other members' changes aren't sent to Ruth: Jonah replying to Maya
    // changes only Maya's counts.
    let (mine, maya_first) = new_thread(&s.f, &s.maya_cookie, "games", "Mine", "Hi").await;
    reply(&s.f, &s.jonah_cookie, mine, "Hi Maya", Some(maya_first)).await;
    s.f.run_jobs().await;
    assert_eq!(
        count(
            &s.f,
            &format!(
                "SELECT count(*) FROM notifications WHERE user_id = {}",
                s.maya
            )
        )
        .await,
        1
    );
    assert_eq!(stream.next().await, None);
}

/// Regression: live streams carried on after their session ended (logging
/// out, or "log out everywhere else" after a device was lost).
#[sqlx::test]
async fn live_streams_end_with_their_session(pool: PgPool) {
    let s = setup(pool).await;
    let (thread, _) = new_thread(&s.f, &s.ruth_cookie, "games", "Brass", "Hi").await;
    let mut badge = Stream::open(&s.f, "/events/notifications", Some(&s.maya_cookie))
        .await
        .unwrap();
    assert_eq!(
        badge.next().await.unwrap(),
        r#"{"notifications":0,"messages":0}"#
    );
    let mut posts = Stream::open(&s.f, &format!("/events/t/{thread}"), Some(&s.maya_cookie))
        .await
        .unwrap();
    let response = s.f.submit("/logout", &[], Some(&s.maya_cookie)).await;
    assert_status(&response, StatusCode::SEE_OTHER);
    s.f.submit(
        "/messages/new",
        &[("to", "maya"), ("body", "Psst")],
        Some(&s.jonah_cookie),
    )
    .await;
    reply(&s.f, &s.jonah_cookie, thread, "Hello", None).await;
    s.f.run_jobs().await;
    assert_eq!(badge.next().await, None);
    assert_eq!(posts.next().await, None);
}

/// Regression: the badge stream counted with the groups the member had
/// when it opened.
#[sqlx::test]
async fn the_badge_count_follows_group_changes(pool: PgPool) {
    let s = setup(pool).await;
    let council_group = count(&s.f, "SELECT id FROM groups WHERE slug = 'council'").await;
    s.f.join_group(council_group, s.jonah).await;
    let (agenda, first) = new_thread(&s.f, &s.ruth_cookie, "council", "Agenda", "Hi").await;
    reply(&s.f, &s.jonah_cookie, agenda, "Item one", Some(first)).await;
    s.f.run_jobs().await;
    let mut stream = Stream::open(&s.f, "/events/notifications", Some(&s.ruth_cookie))
        .await
        .unwrap();
    assert_eq!(
        stream.next().await.unwrap(),
        r#"{"notifications":1,"messages":0}"#
    );
    // Ruth leaves the Council; the notification about its category goes
    // with it, on the next count she is sent.
    s.f.exec(&format!(
        "DELETE FROM group_members WHERE group_id = {council_group} AND user_id = {}",
        s.ruth
    ))
    .await;
    let (games, games_first) = new_thread(&s.f, &s.ruth_cookie, "games", "Brass", "Hi").await;
    reply(&s.f, &s.maya_cookie, games, "Hello", Some(games_first)).await;
    s.f.run_jobs().await;
    let mut last = String::new();
    while let Some(event) = stream.next().await {
        last = event;
    }
    assert_eq!(last, r#"{"notifications":1,"messages":0}"#);
}

/// Live streams are limited per address and per member, so they can't be
/// opened without end.
#[sqlx::test]
async fn live_streams_are_limited(pool: PgPool) {
    let s = setup(pool).await;
    let f = Forum {
        db: s.f.db.clone(),
        state: s.f.state.clone(),
    }
    .trusting_proxy();
    let (thread, _) = new_thread(&f, &s.ruth_cookie, "games", "Brass", "Hi").await;
    let path = format!("/events/t/{thread}");
    let mut open = Vec::new();
    for _ in 0..parlour::live::MAX_STREAMS_PER_ADDRESS {
        open.push(
            Stream::open_from(&f, &path, None, Some("203.0.113.1"))
                .await
                .unwrap(),
        );
    }
    assert_eq!(
        Stream::open_from(&f, &path, None, Some("203.0.113.1"))
            .await
            .err(),
        Some(StatusCode::TOO_MANY_REQUESTS)
    );
    // Another address may still open one.
    let other = Stream::open_from(&f, &path, None, Some("203.0.113.2"))
        .await
        .unwrap();
    // Closing one makes room again.
    open.pop();
    open.push(
        Stream::open_from(&f, &path, None, Some("203.0.113.1"))
            .await
            .unwrap(),
    );
    drop(open);
    drop(other);
    // A member is limited wherever they connect from.
    let mut mine = Vec::new();
    for n in 0..parlour::live::MAX_STREAMS_PER_MEMBER {
        let ip = format!("198.51.100.{n}");
        mine.push(
            Stream::open_from(&f, "/events/notifications", Some(&s.ruth_cookie), Some(&ip))
                .await
                .unwrap(),
        );
    }
    assert_eq!(
        Stream::open_from(&f, &path, Some(&s.ruth_cookie), Some("192.0.2.1"))
            .await
            .err(),
        Some(StatusCode::TOO_MANY_REQUESTS)
    );
    assert!(
        Stream::open_from(&f, &path, Some(&s.jonah_cookie), Some("192.0.2.1"))
            .await
            .is_ok()
    );
}
