//! Direct messages (milestone 6, DESIGN.md §3.7): private threads with a
//! participant list, the pages for them, and the safety controls, over HTTP
//! as a browser without JavaScript would.

mod common;

use axum::http::{StatusCode, header};
use common::{Forum, Response};
use parlour::settings::{self, MessageSettings};
use sqlx::PgPool;

/// A forum with a public category `games`, members ruth, jonah and maya
/// (with sessions), and an admin, sam. New-member limits are off unless a
/// test turns them on.
struct Setup {
    f: Forum,
    games: i64,
    ruth: i64,
    ruth_cookie: String,
    jonah: i64,
    jonah_cookie: String,
    maya: i64,
    maya_cookie: String,
    sam: i64,
    sam_cookie: String,
}

async fn setup(pool: PgPool) -> Setup {
    let f = Forum::new(pool);
    let games = f.category("games", "public", None, 0).await;
    let ruth = f.member("ruth", "user", None).await;
    let jonah = f.member("jonah", "user", None).await;
    let maya = f.member("maya", "user", None).await;
    let sam = f.member("sam", "admin", None).await;
    let ruth_cookie = f.session(ruth).await;
    let jonah_cookie = f.session(jonah).await;
    let maya_cookie = f.session(maya).await;
    let sam_cookie = f.session(sam).await;
    set_messages(
        &f,
        MessageSettings {
            new_user_days: 0,
            new_user_posts: 0,
            ..MessageSettings::default()
        },
    )
    .await;
    Setup {
        f,
        games,
        ruth,
        ruth_cookie,
        jonah,
        jonah_cookie,
        maya,
        maya_cookie,
        sam,
        sam_cookie,
    }
}

async fn set_messages(f: &Forum, value: MessageSettings) {
    settings::set(&f.db, "messages", &value).await.unwrap();
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

/// The conversation id in a `/messages/{id}…` location.
fn conversation_in(location: &str) -> i64 {
    let rest = location
        .strip_prefix("/messages/")
        .unwrap_or_else(|| panic!("not a conversation: {location}"));
    rest.split(|c: char| !c.is_ascii_digit())
        .next()
        .unwrap()
        .parse()
        .unwrap()
}

/// Starts a conversation as `cookie`, returning its id.
async fn start(f: &Forum, cookie: &str, to: &str, title: &str, body: &str) -> i64 {
    let response = f
        .submit(
            "/messages/new",
            &[("to", to), ("title", title), ("body", body)],
            Some(cookie),
        )
        .await;
    assert_status(&response, StatusCode::SEE_OTHER);
    conversation_in(response.location())
}

async fn send(f: &Forum, cookie: &str, conversation: i64, body: &str) -> Response {
    f.submit(
        &format!("/messages/{conversation}"),
        &[("body", body)],
        Some(cookie),
    )
    .await
}

async fn last_post(f: &Forum, thread_id: i64) -> i64 {
    count(
        f,
        &format!("SELECT max(id) FROM posts WHERE thread_id = {thread_id}"),
    )
    .await
}

// ---------------------------------------------------------------------------
// Conversations

#[sqlx::test]
async fn visitors_are_asked_to_log_in(pool: PgPool) {
    let s = setup(pool).await;
    for path in ["/messages", "/messages/new", "/messages/new?to=ruth"] {
        let response = s.f.get(path).await;
        assert_status(&response, StatusCode::SEE_OTHER);
        assert!(response.location().starts_with("/login?next="), "{path}");
        assert!(response.header(header::SET_COOKIE).is_none(), "{path}");
    }
    let id = start(&s.f, &s.ruth_cookie, "jonah", "", "Psst").await;
    let response = s.f.get(&format!("/messages/{id}")).await;
    assert_status(&response, StatusCode::SEE_OTHER);
    assert!(response.header(header::SET_COOKIE).is_none());
    let response =
        s.f.submit("/messages/new", &[("to", "ruth"), ("body", "hi")], None)
            .await;
    assert_status(&response, StatusCode::SEE_OTHER);
    assert!(response.location().starts_with("/login"));
}

#[sqlx::test]
async fn members_message_each_other(pool: PgPool) {
    let s = setup(pool).await;
    let form = s.f.page_as("/messages/new?to=jonah", &s.ruth_cookie).await;
    assert!(form.contains("value=\"jonah\""), "{form}");
    let id = start(
        &s.f,
        &s.ruth_cookie,
        "jonah",
        "",
        "Did you still want to borrow Mage Knight?",
    )
    .await;
    // The sender lands on the conversation.
    let page =
        s.f.page_as(&format!("/messages/{id}"), &s.ruth_cookie)
            .await;
    assert!(page.contains("Did you still want to borrow Mage Knight?"));
    // The recipient has it in their list, unread, under the sender's name.
    let list = s.f.page_as("/messages", &s.jonah_cookie).await;
    assert!(list.contains(&format!("href=\"/messages/{id}")), "{list}");
    assert!(list.contains("Ruth"), "{list}");
    assert!(list.contains("1 new"), "{list}");
    // Their reply goes to the same conversation.
    let response = send(&s.f, &s.jonah_cookie, id, "Yes please!").await;
    assert_status(&response, StatusCode::SEE_OTHER);
    assert!(
        response.location().starts_with(&format!("/messages/{id}")),
        "{}",
        response.location()
    );
    let page =
        s.f.page_as(&format!("/messages/{id}"), &s.jonah_cookie)
            .await;
    assert!(page.contains("Yes please!"));
    let list = s.f.page_as("/messages", &s.jonah_cookie).await;
    assert!(!list.contains("1 new"), "{list}");
    let list = s.f.page_as("/messages", &s.ruth_cookie).await;
    assert!(list.contains("1 new"), "{list}");
    // Logged-in pages are never cached.
    let response = s.f.get_as("/messages", Some(&s.ruth_cookie)).await;
    assert_eq!(
        response.header(header::CACHE_CONTROL),
        Some(parlour::http_cache::PRIVATE_CACHE_CONTROL)
    );
}

#[sqlx::test]
async fn group_conversations_have_titles(pool: PgPool) {
    let s = setup(pool).await;
    let id = start(
        &s.f,
        &s.ruth_cookie,
        "jonah, maya",
        "Tuesday 7th: Brass",
        "Booked the back room",
    )
    .await;
    for cookie in [&s.ruth_cookie, &s.jonah_cookie, &s.maya_cookie] {
        let list = s.f.page_as("/messages", cookie).await;
        assert!(list.contains("Tuesday 7th: Brass"), "{list}");
        let page = s.f.page_as(&format!("/messages/{id}"), cookie).await;
        assert!(page.contains("Booked the back room"));
    }
}

#[sqlx::test]
async fn nobody_else_can_see_a_conversation(pool: PgPool) {
    let s = setup(pool).await;
    let id = start(
        &s.f,
        &s.ruth_cookie,
        "jonah",
        "Secret plans",
        "zebra-secret",
    )
    .await;
    let post = last_post(&s.f, id).await;
    let thread_paths = [
        format!("/messages/{id}"),
        format!("/messages/{id}/unread"),
        format!("/t/{id}"),
        format!("/t/{id}/message"),
        format!("/t/{id}/unread"),
        format!("/t/{id}/reply"),
        format!("/p/{post}"),
        format!("/p/{post}/revisions"),
        format!("/p/{post}/reactions"),
        format!("/p/{post}/edit"),
        format!("/p/{post}/delete"),
        format!("/p/{post}/fork"),
    ];
    // Maya isn't in it, and neither is the admin: staff don't read DMs.
    for cookie in [&s.maya_cookie, &s.sam_cookie] {
        for path in &thread_paths {
            let response = s.f.get_as(path, Some(cookie)).await;
            assert_status(&response, StatusCode::NOT_FOUND);
            assert!(!response.body.contains("zebra-secret"), "{path}");
        }
        let list = s.f.page_as("/messages", cookie).await;
        assert!(!list.contains("Secret plans"));
        let response = send(&s.f, cookie, id, "Let me in").await;
        assert_status(&response, StatusCode::NOT_FOUND);
    }
    // Anonymous visitors never get anywhere near it.
    for path in &thread_paths {
        let response = s.f.get(path).await;
        assert!(!response.body.contains("zebra-secret"), "{path}");
        assert!(
            response.status == StatusCode::NOT_FOUND || response.status == StatusCode::SEE_OTHER,
            "{path}: {}",
            response.status
        );
    }
    // DMs never show up in category lists or post counts, even for the
    // people in them.
    for cookie in [None, Some(s.ruth_cookie.as_str()), Some(&s.sam_cookie)] {
        for path in ["/", "/latest", "/c/games", "/u/ruth", "/users/tree"] {
            let response = s.f.get_as(path, cookie).await;
            assert_status(&response, StatusCode::OK);
            assert!(!response.body.contains("Secret plans"), "{path}");
            assert!(!response.body.contains("zebra-secret"), "{path}");
        }
    }
    let _ = s.games;
}

#[sqlx::test]
async fn participants_use_the_usual_post_tools(pool: PgPool) {
    let s = setup(pool).await;
    let id = start(&s.f, &s.ruth_cookie, "jonah", "", "Frist").await;
    let post = last_post(&s.f, id).await;
    // Editing: the composer, then back to the conversation.
    let form =
        s.f.page_as(&format!("/p/{post}/edit"), &s.ruth_cookie)
            .await;
    assert!(form.contains("Frist"));
    assert!(form.contains("href=\"/messages\""), "{form}");
    s.f.exec(&format!(
        "UPDATE posts SET created_at = created_at - interval '10 minutes' WHERE id = {post}"
    ))
    .await;
    let response =
        s.f.submit(
            &format!("/p/{post}/edit"),
            &[("body", "First")],
            Some(&s.ruth_cookie),
        )
        .await;
    assert_status(&response, StatusCode::SEE_OTHER);
    assert_eq!(response.location(), format!("/messages/{id}#p{post}"));
    let history =
        s.f.page_as(&format!("/p/{post}/revisions?v=1"), &s.jonah_cookie)
            .await;
    assert!(history.contains("Frist"));
    // Jonah can't edit Ruth's message.
    let response =
        s.f.get_as(&format!("/p/{post}/edit"), Some(&s.jonah_cookie))
            .await;
    assert_status(&response, StatusCode::FORBIDDEN);
    // Reactions.
    let response =
        s.f.submit(
            &format!("/p/{post}/reactions"),
            &[("emoji", "👍"), ("action", "add")],
            Some(&s.jonah_cookie),
        )
        .await;
    assert_status(&response, StatusCode::SEE_OTHER);
    let page =
        s.f.page_as(&format!("/messages/{id}"), &s.ruth_cookie)
            .await;
    assert!(page.contains("reaction-count\">1<"), "{page}");
    // Quoting in a reply.
    let reply =
        s.f.page_as(&format!("/t/{id}/reply?quote={post}"), &s.jonah_cookie)
            .await;
    assert!(
        reply.contains("@ruth wrote:") && reply.contains("First"),
        "{reply}"
    );
    // Deleting.
    let response =
        s.f.submit(&format!("/p/{post}/delete"), &[], Some(&s.ruth_cookie))
            .await;
    assert_status(&response, StatusCode::SEE_OTHER);
    let page =
        s.f.page_as(&format!("/messages/{id}"), &s.jonah_cookie)
            .await;
    assert!(!page.contains("First"));
    assert!(page.contains("deleted"));
}

#[sqlx::test]
async fn messages_cannot_be_quoted_into_threads(pool: PgPool) {
    let s = setup(pool).await;
    let id = start(&s.f, &s.ruth_cookie, "jonah", "", "Private words").await;
    let post = last_post(&s.f, id).await;
    let response =
        s.f.get_as(&format!("/p/{post}/fork"), Some(&s.ruth_cookie))
            .await;
    assert_status(&response, StatusCode::FORBIDDEN);
    let response =
        s.f.submit(
            &format!("/p/{post}/fork"),
            &[("category", "games"), ("title", "Leak"), ("body", "Look")],
            Some(&s.ruth_cookie),
        )
        .await;
    assert_status(&response, StatusCode::BAD_REQUEST);
    assert_eq!(
        count(
            &s.f,
            "SELECT count(*) FROM threads WHERE forked_from_post_id IS NOT NULL"
        )
        .await,
        0
    );
    // Nor can a thread's reply quote a message from elsewhere.
    let t = s.f.thread(s.games, s.ruth, "Open", "Hello").await;
    let response =
        s.f.get_as(
            &format!("/t/{}/reply?quote={post}", t.thread_id),
            Some(&s.ruth_cookie),
        )
        .await;
    assert_status(&response, StatusCode::NOT_FOUND);
}

#[sqlx::test]
async fn leaving_a_conversation(pool: PgPool) {
    let s = setup(pool).await;
    let id = start(&s.f, &s.ruth_cookie, "jonah, maya", "Brass", "Hello all").await;
    let response =
        s.f.submit(&format!("/messages/{id}/leave"), &[], Some(&s.maya_cookie))
            .await;
    assert_status(&response, StatusCode::SEE_OTHER);
    assert_eq!(response.location(), "/messages");
    let list = s.f.page_as("/messages", &s.maya_cookie).await;
    assert!(!list.contains("Brass"));
    let response =
        s.f.get_as(&format!("/messages/{id}"), Some(&s.maya_cookie))
            .await;
    assert_status(&response, StatusCode::NOT_FOUND);
    // The others carry on, and see that she left.
    let page =
        s.f.page_as(&format!("/messages/{id}"), &s.ruth_cookie)
            .await;
    assert!(page.contains("Hello all"));
    assert!(
        !page.contains("Maya,") && !page.contains("and Maya"),
        "{page}"
    );
    assert_status(
        &send(&s.f, &s.jonah_cookie, id, "Bye Maya").await,
        StatusCode::SEE_OTHER,
    );
}

#[sqlx::test]
async fn adding_someone_to_a_group_conversation(pool: PgPool) {
    let s = setup(pool).await;
    let kit = s.f.member("kit", "user", None).await;
    let kit_cookie = s.f.session(kit).await;
    let group = start(
        &s.f,
        &s.ruth_cookie,
        "jonah, maya",
        "Brass",
        "Earlier words",
    )
    .await;
    let response =
        s.f.submit(
            &format!("/messages/{group}/people"),
            &[("username", "kit")],
            Some(&s.jonah_cookie),
        )
        .await;
    assert_status(&response, StatusCode::SEE_OTHER);
    let page =
        s.f.page_as(&format!("/messages/{group}"), &kit_cookie)
            .await;
    assert!(page.contains("Earlier words"));
    // A conversation between two people stays between them.
    let pair = start(&s.f, &s.ruth_cookie, "jonah", "", "Just us").await;
    let page =
        s.f.page_as(&format!("/messages/{pair}"), &s.ruth_cookie)
            .await;
    assert!(!page.contains(&format!("action=\"/messages/{pair}/people\"")));
    let response =
        s.f.submit(
            &format!("/messages/{pair}/people"),
            &[("username", "kit")],
            Some(&s.ruth_cookie),
        )
        .await;
    assert_status(&response, StatusCode::FORBIDDEN);
    let response =
        s.f.get_as(&format!("/messages/{pair}"), Some(&kit_cookie))
            .await;
    assert_status(&response, StatusCode::NOT_FOUND);
}

#[sqlx::test]
async fn recipients_must_exist_and_be_someone_else(pool: PgPool) {
    let s = setup(pool).await;
    for (to, problem) in [
        ("", "Say who the message is for"),
        ("nobody_here", "There’s no member called nobody_here"),
        ("ruth", "You can’t send a message to yourself"),
    ] {
        let response =
            s.f.submit(
                "/messages/new",
                &[("to", to), ("body", "Hello")],
                Some(&s.ruth_cookie),
            )
            .await;
        assert_status(&response, StatusCode::BAD_REQUEST);
        assert!(response.body.contains(problem), "{to}: {}", response.body);
        // What was typed is kept.
        assert!(response.body.contains("Hello"));
    }
    assert_eq!(count(&s.f, "SELECT count(*) FROM threads").await, 0);
}

// ---------------------------------------------------------------------------
// Safety controls

#[sqlx::test]
async fn the_site_can_turn_messages_off(pool: PgPool) {
    let s = setup(pool).await;
    let id = start(&s.f, &s.ruth_cookie, "jonah", "", "Before").await;
    set_messages(
        &s.f,
        MessageSettings {
            enabled: false,
            new_user_days: 0,
            new_user_posts: 0,
            ..MessageSettings::default()
        },
    )
    .await;
    let response =
        s.f.submit(
            "/messages/new",
            &[("to", "jonah"), ("body", "After")],
            Some(&s.ruth_cookie),
        )
        .await;
    assert_status(&response, StatusCode::FORBIDDEN);
    assert_status(
        &send(&s.f, &s.jonah_cookie, id, "After").await,
        StatusCode::FORBIDDEN,
    );
    let list = s.f.page_as("/messages", &s.ruth_cookie).await;
    assert!(list.contains("turned off"), "{list}");
    // What was said can still be read, and staff can still write.
    assert!(
        s.f.page_as(&format!("/messages/{id}"), &s.jonah_cookie)
            .await
            .contains("Before")
    );
    start(&s.f, &s.sam_cookie, "jonah", "From the organisers", "Hello").await;
}

#[sqlx::test]
async fn new_members_cannot_start_conversations(pool: PgPool) {
    let s = setup(pool).await;
    set_messages(&s.f, MessageSettings::default()).await;
    let response =
        s.f.submit(
            "/messages/new",
            &[("to", "jonah"), ("body", "Hi")],
            Some(&s.ruth_cookie),
        )
        .await;
    assert_status(&response, StatusCode::FORBIDDEN);
    assert!(response.body.contains("new"), "{}", response.body);
    // Old enough, but too few posts.
    s.f.exec(&format!(
        "UPDATE users SET created_at = now() - interval '4 days' WHERE id = {}",
        s.ruth
    ))
    .await;
    let response =
        s.f.submit(
            "/messages/new",
            &[("to", "jonah"), ("body", "Hi")],
            Some(&s.ruth_cookie),
        )
        .await;
    assert_status(&response, StatusCode::FORBIDDEN);
    let t = s.f.thread(s.games, s.ruth, "Hello", "One").await;
    for n in 0..4 {
        s.f.post(t.thread_id, s.ruth, &format!("Post {n}")).await;
    }
    start(&s.f, &s.ruth_cookie, "jonah", "", "Hi").await;
    // Jonah is new, but may answer.
    let id = start(&s.f, &s.ruth_cookie, "jonah", "", "Again").await;
    assert_status(
        &send(&s.f, &s.jonah_cookie, id, "Hello back").await,
        StatusCode::SEE_OTHER,
    );
    // Staff are never new.
    let new_admin = s.f.member("boss", "admin", None).await;
    let boss = s.f.session(new_admin).await;
    start(&s.f, &boss, "jonah", "", "Welcome").await;
}

#[sqlx::test]
async fn conversations_have_a_recipient_cap(pool: PgPool) {
    let s = setup(pool).await;
    set_messages(
        &s.f,
        MessageSettings {
            max_recipients: 2,
            new_user_days: 0,
            new_user_posts: 0,
            ..MessageSettings::default()
        },
    )
    .await;
    let kit = s.f.member("kit", "user", None).await;
    let response =
        s.f.submit(
            "/messages/new",
            &[("to", "jonah maya kit"), ("body", "Everyone")],
            Some(&s.ruth_cookie),
        )
        .await;
    assert_status(&response, StatusCode::BAD_REQUEST);
    assert!(response.body.contains("at most 2"), "{}", response.body);
    let id = start(&s.f, &s.ruth_cookie, "jonah maya", "Two", "Hi").await;
    let response =
        s.f.submit(
            &format!("/messages/{id}/people"),
            &[("username", "kit")],
            Some(&s.ruth_cookie),
        )
        .await;
    assert_status(&response, StatusCode::BAD_REQUEST);
    let _ = kit;
}

#[sqlx::test]
async fn starting_conversations_is_rate_limited(pool: PgPool) {
    let s = setup(pool).await;
    let mut refused = false;
    for n in 0..20 {
        let response =
            s.f.submit(
                "/messages/new",
                &[("to", "jonah"), ("body", &format!("Message {n}"))],
                Some(&s.ruth_cookie),
            )
            .await;
        if response.status == StatusCode::TOO_MANY_REQUESTS {
            refused = true;
            assert!(response.body.contains(&format!("Message {n}")));
            break;
        }
        assert_status(&response, StatusCode::SEE_OTHER);
    }
    assert!(refused, "twenty conversations in a row were all allowed");
}

#[sqlx::test]
async fn members_choose_who_can_message_them(pool: PgPool) {
    let s = setup(pool).await;
    let page = s.f.page_as("/settings", &s.jonah_cookie).await;
    assert!(page.contains("name=\"dm_policy\""), "{page}");
    let response =
        s.f.submit(
            "/settings/messages",
            &[("dm_policy", "nobody")],
            Some(&s.jonah_cookie),
        )
        .await;
    assert_status(&response, StatusCode::SEE_OTHER);
    let response =
        s.f.submit(
            "/messages/new",
            &[("to", "jonah"), ("body", "Hi")],
            Some(&s.ruth_cookie),
        )
        .await;
    assert_status(&response, StatusCode::BAD_REQUEST);
    assert!(
        response.body.contains("isn’t taking messages"),
        "{}",
        response.body
    );
    // The profile doesn't offer it either.
    let profile = s.f.page_as("/u/jonah", &s.ruth_cookie).await;
    assert!(!profile.contains("/messages/new?to=jonah"), "{profile}");
    // Staff can always write.
    start(&s.f, &s.sam_cookie, "jonah", "", "From the admin").await;

    // "People I've interacted with": only once Jonah has replied to Ruth.
    s.f.submit(
        "/settings/messages",
        &[("dm_policy", "interacted")],
        Some(&s.jonah_cookie),
    )
    .await;
    let refused =
        s.f.submit(
            "/messages/new",
            &[("to", "jonah"), ("body", "Hi")],
            Some(&s.ruth_cookie),
        )
        .await;
    assert_status(&refused, StatusCode::BAD_REQUEST);
    // Ruth replying to Jonah doesn't count: it is Jonah who must have
    // interacted.
    let t = s.f.thread(s.games, s.jonah, "Hello", "From Jonah").await;
    s.f.reply(t.thread_id, s.ruth, "From Ruth", Some(t.first_post_id))
        .await;
    let refused =
        s.f.submit(
            "/messages/new",
            &[("to", "jonah"), ("body", "Hi")],
            Some(&s.ruth_cookie),
        )
        .await;
    assert_status(&refused, StatusCode::BAD_REQUEST);
    let ruths = last_post(&s.f, t.thread_id).await;
    s.f.reply(t.thread_id, s.jonah, "Back at you", Some(ruths))
        .await;
    start(&s.f, &s.ruth_cookie, "jonah", "", "Hi").await;
    let profile = s.f.page_as("/u/jonah", &s.ruth_cookie).await;
    assert!(profile.contains("/messages/new?to=jonah"), "{profile}");
}

#[sqlx::test]
async fn blocking_someone(pool: PgPool) {
    let s = setup(pool).await;
    let id = start(&s.f, &s.ruth_cookie, "jonah", "", "Hello").await;
    let confirm = s.f.page_as("/u/ruth/block", &s.jonah_cookie).await;
    assert!(confirm.contains("action=\"/u/ruth/block\""), "{confirm}");
    let response =
        s.f.submit("/u/ruth/block", &[], Some(&s.jonah_cookie))
            .await;
    assert_status(&response, StatusCode::SEE_OTHER);
    // Neither can start a conversation with the other...
    for (cookie, to) in [(&s.ruth_cookie, "jonah"), (&s.jonah_cookie, "ruth")] {
        let response =
            s.f.submit("/messages/new", &[("to", to), ("body", "Hi")], Some(cookie))
                .await;
        assert_status(&response, StatusCode::BAD_REQUEST);
    }
    // ...or carry on the one they had.
    assert_status(
        &send(&s.f, &s.ruth_cookie, id, "Hello?").await,
        StatusCode::FORBIDDEN,
    );
    // Nor add the other to a group.
    let group = start(&s.f, &s.maya_cookie, "ruth, sam", "Group", "Hi").await;
    let response =
        s.f.submit(
            &format!("/messages/{group}/people"),
            &[("username", "jonah")],
            Some(&s.ruth_cookie),
        )
        .await;
    assert_status(&response, StatusCode::BAD_REQUEST);
    // Ruth's posts in threads are folded away for Jonah, and only for him.
    let t =
        s.f.thread(s.games, s.ruth, "Spirit Island", "Ruth's opening")
            .await;
    let page =
        s.f.page_as(
            &format!("/t/{}/spirit-island", t.thread_id),
            &s.jonah_cookie,
        )
        .await;
    assert!(page.contains("someone you’ve blocked"), "{page}");
    let page =
        s.f.page_as(&format!("/t/{}/spirit-island", t.thread_id), &s.maya_cookie)
            .await;
    assert!(!page.contains("someone you’ve blocked"));
    // The settings page lists blocks, and unblocking works.
    let settings = s.f.page_as("/settings", &s.jonah_cookie).await;
    assert!(
        settings.contains("action=\"/u/ruth/unblock\""),
        "{settings}"
    );
    let response =
        s.f.submit("/u/ruth/unblock", &[], Some(&s.jonah_cookie))
            .await;
    assert_status(&response, StatusCode::SEE_OTHER);
    assert_status(
        &send(&s.f, &s.ruth_cookie, id, "Hello?").await,
        StatusCode::SEE_OTHER,
    );
}

#[sqlx::test]
async fn staff_cannot_be_blocked(pool: PgPool) {
    let s = setup(pool).await;
    let id = start(&s.f, &s.sam_cookie, "ruth", "From the organisers", "A word").await;
    let page =
        s.f.page_as(&format!("/messages/{id}"), &s.ruth_cookie)
            .await;
    assert!(page.contains("Staff message"), "{page}");
    assert!(!page.contains("/u/sam/block"), "{page}");
    let response = s.f.submit("/u/sam/block", &[], Some(&s.ruth_cookie)).await;
    assert_status(&response, StatusCode::FORBIDDEN);
    assert_eq!(count(&s.f, "SELECT count(*) FROM blocks").await, 0);
    // A block made before someone became staff doesn't stop them.
    s.f.exec(&format!(
        "INSERT INTO blocks (user_id, blocked_user_id) VALUES ({}, {})",
        s.jonah, s.maya
    ))
    .await;
    s.f.exec(&format!(
        "UPDATE users SET role = 'moderator' WHERE id = {}",
        s.maya
    ))
    .await;
    start(&s.f, &s.maya_cookie, "jonah", "", "Moderator here").await;
    let _ = (s.ruth, s.sam);
}
