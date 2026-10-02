//! Writing (milestone 4): new threads and replies with previews, quoting,
//! mentions, editing and edit history, deleting, reactions, read markers
//! and quote-forks, over HTTP as a browser without JavaScript would.

mod common;

use axum::http::{StatusCode, header};
use common::{Forum, Response, post_html};
use parlour::store::NewThread;
use sqlx::PgPool;

/// A forum with a public category `games`, member ruth (with a session)
/// and member jonah.
struct Setup {
    f: Forum,
    games: i64,
    ruth: i64,
    ruth_cookie: String,
    jonah: i64,
    jonah_cookie: String,
}

async fn setup(pool: PgPool) -> Setup {
    let f = Forum::new(pool);
    let games = f.category("games", "public", None, 0).await;
    let ruth = f.member("ruth", "user", None).await;
    let jonah = f.member("jonah", "user", None).await;
    let ruth_cookie = f.session(ruth).await;
    let jonah_cookie = f.session(jonah).await;
    Setup {
        f,
        games,
        ruth,
        ruth_cookie,
        jonah,
        jonah_cookie,
    }
}

fn assert_status(response: &Response, status: StatusCode) {
    assert_eq!(response.status, status, "{}", response.body);
}

/// The id at the end of a `#p{id}` location.
fn anchored_post(location: &str) -> i64 {
    location
        .rsplit_once("#p")
        .unwrap_or_else(|| panic!("no post anchor in {location}"))
        .1
        .parse()
        .unwrap()
}

async fn count(f: &Forum, sql: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(sql.to_owned()))
        .fetch_one(&f.db)
        .await
        .unwrap()
}

/// Makes a post older than the edit grace period.
async fn age_post(f: &Forum, post_id: i64) {
    f.exec(&format!(
        "UPDATE posts SET created_at = created_at - interval '10 minutes' WHERE id = {post_id}"
    ))
    .await;
}

// ---------------------------------------------------------------------------
// New threads

#[sqlx::test]
async fn visitors_are_asked_to_log_in_to_write(pool: PgPool) {
    let s = setup(pool).await;
    let t = s.f.thread(s.games, s.ruth, "Spirit Island", "First").await;
    for path in [
        "/c/games/new".to_owned(),
        format!("/t/{}/reply", t.thread_id),
        format!("/p/{}/edit", t.first_post_id),
        format!("/p/{}/fork", t.first_post_id),
    ] {
        let response = s.f.get(&path).await;
        assert_status(&response, StatusCode::SEE_OTHER);
        assert!(response.location().starts_with("/login?next="), "{path}");
        assert!(response.header(header::SET_COOKIE).is_none());
    }
    for path in [
        "/c/games/new".to_owned(),
        format!("/t/{}/reply", t.thread_id),
        format!("/p/{}/edit", t.first_post_id),
        format!("/p/{}/delete", t.first_post_id),
        format!("/p/{}/reactions", t.first_post_id),
        format!("/p/{}/fork", t.first_post_id),
        "/c/games/read".to_owned(),
    ] {
        let response =
            s.f.submit(
                &path,
                &[("title", "x"), ("body", "y"), ("emoji", "👍")],
                None,
            )
            .await;
        assert_status(&response, StatusCode::SEE_OTHER);
        assert!(response.location().starts_with("/login"), "{path}");
        assert!(response.header(header::SET_COOKIE).is_none(), "{path}");
    }
    assert_eq!(count(&s.f, "SELECT count(*) FROM posts").await, 1);
}

#[sqlx::test]
async fn members_start_threads(pool: PgPool) {
    let s = setup(pool).await;
    // An anonymous visitor's cached copy of the category, to be cleared.
    assert!(!s.f.page("/c/games").await.contains("Heat with five"));

    let form = s.f.page_as("/c/games/new", &s.ruth_cookie).await;
    assert!(form.contains("<textarea"), "{form}");
    assert!(form.contains("name=\"title\""));
    assert!(form.contains("action=\"/c/games/new\""));

    let response =
        s.f.submit(
            "/c/games/new",
            &[
                ("title", "  Heat with five  "),
                ("body", "We played **Heat**.\r\n\r\nIt was great."),
            ],
            Some(&s.ruth_cookie),
        )
        .await;
    assert_status(&response, StatusCode::SEE_OTHER);
    let location = response.location().to_owned();
    assert!(location.starts_with("/t/"), "{location}");
    assert!(location.contains("/heat-with-five#p"), "{location}");
    let post_id = anchored_post(&location);

    let page = s.f.page(location.split('#').next().unwrap()).await;
    assert!(page.contains("<h1 class=\"page-title\">Heat with five</h1>"));
    let post = post_html(&page, post_id);
    assert!(post.contains("We played <strong>Heat</strong>."), "{post}");
    assert!(post.contains("№ 1"));
    let source: String = sqlx::query_scalar("SELECT body_md FROM posts WHERE id = $1")
        .bind(post_id)
        .fetch_one(&s.f.db)
        .await
        .unwrap();
    assert_eq!(source, "We played **Heat**.\n\nIt was great.");
    // The anonymous cache was cleared.
    assert!(s.f.page("/c/games").await.contains("Heat with five"));
    assert!(s.f.page("/").await.contains("Heat with five"));
}

#[sqlx::test]
async fn preview_shows_the_rendered_post_without_saving_it(pool: PgPool) {
    let s = setup(pool).await;
    let response =
        s.f.submit(
            "/c/games/new",
            &[
                ("title", "Draft title"),
                ("body", "Some *emphasis* and <script>alert(1)</script>"),
                ("preview", "1"),
            ],
            Some(&s.ruth_cookie),
        )
        .await;
    assert_status(&response, StatusCode::OK);
    assert!(
        response.body.contains("Some <em>emphasis</em>"),
        "{}",
        response.body
    );
    assert!(!response.body.contains("<script>alert"));
    // The text is still in the form, escaped.
    assert!(
        response
            .body
            .contains("Some *emphasis* and &#60;script&#62;")
            || response.body.contains("Some *emphasis* and &lt;script&gt;"),
        "{}",
        response.body
    );
    assert!(response.body.contains("value=\"Draft title\""));
    assert_eq!(count(&s.f, "SELECT count(*) FROM threads").await, 0);
}

#[sqlx::test]
async fn new_threads_need_a_title_and_a_body(pool: PgPool) {
    let s = setup(pool).await;
    for (title, body) in [
        ("", "body"),
        ("   ", "body"),
        ("Title", ""),
        ("Title", " \n "),
    ] {
        let response =
            s.f.submit(
                "/c/games/new",
                &[("title", title), ("body", body)],
                Some(&s.ruth_cookie),
            )
            .await;
        assert_status(&response, StatusCode::BAD_REQUEST);
        assert!(response.body.contains("notice-error"), "{}", response.body);
    }
    let long_title = "x".repeat(151);
    let response =
        s.f.submit(
            "/c/games/new",
            &[("title", &long_title), ("body", "kept text")],
            Some(&s.ruth_cookie),
        )
        .await;
    assert_status(&response, StatusCode::BAD_REQUEST);
    assert!(response.body.contains("kept text"));
    let long_body = "y".repeat(50_001);
    let response =
        s.f.submit(
            "/c/games/new",
            &[("title", "Fine"), ("body", &long_body)],
            Some(&s.ruth_cookie),
        )
        .await;
    assert_status(&response, StatusCode::BAD_REQUEST);
    assert_eq!(count(&s.f, "SELECT count(*) FROM threads").await, 0);
}

#[sqlx::test]
async fn write_levels_are_enforced(pool: PgPool) {
    let s = setup(pool).await;
    let mod_id = s.f.member("sam", "moderator", None).await;
    let mod_cookie = s.f.session(mod_id).await;
    s.f.exec("UPDATE categories SET write_level = 'staff' WHERE slug = 'games'")
        .await;
    let response = s.f.get_as("/c/games/new", Some(&s.ruth_cookie)).await;
    assert_status(&response, StatusCode::FORBIDDEN);
    let response =
        s.f.submit(
            "/c/games/new",
            &[("title", "Sneaky"), ("body", "x")],
            Some(&s.ruth_cookie),
        )
        .await;
    assert_status(&response, StatusCode::FORBIDDEN);
    let response =
        s.f.submit(
            "/c/games/new",
            &[("title", "Announcement"), ("body", "x")],
            Some(&mod_cookie),
        )
        .await;
    assert_status(&response, StatusCode::SEE_OTHER);

    // Replies follow the category's write level too.
    let t = s.f.thread(s.games, mod_id, "Rules", "Read these").await;
    let response =
        s.f.submit(
            &format!("/t/{}/reply", t.thread_id),
            &[("body", "me too")],
            Some(&s.ruth_cookie),
        )
        .await;
    assert_status(&response, StatusCode::FORBIDDEN);

    // Group-restricted writing: no member is in a group yet.
    s.f.exec("UPDATE categories SET write_level = 'groups' WHERE slug = 'games'")
        .await;
    let response = s.f.get_as("/c/games/new", Some(&s.ruth_cookie)).await;
    assert_status(&response, StatusCode::FORBIDDEN);
    let response = s.f.get_as("/c/games/new", Some(&mod_cookie)).await;
    assert_status(&response, StatusCode::OK);

    // The category page offers a new thread only to those who may post.
    let page = s.f.page_as("/c/games", &s.ruth_cookie).await;
    assert!(!page.contains("href=\"/c/games/new\""));
    let page = s.f.page_as("/c/games", &mod_cookie).await;
    assert!(page.contains("href=\"/c/games/new\""));
}

#[sqlx::test]
async fn categories_members_cant_read_are_not_found(pool: PgPool) {
    let s = setup(pool).await;
    s.f.category("organisers", "groups", None, 1).await;
    s.f.exec("UPDATE categories SET write_level = 'members' WHERE slug = 'organisers'")
        .await;
    let response = s.f.get_as("/c/organisers/new", Some(&s.ruth_cookie)).await;
    assert_status(&response, StatusCode::NOT_FOUND);
    let response =
        s.f.submit(
            "/c/organisers/new",
            &[("title", "x"), ("body", "y")],
            Some(&s.ruth_cookie),
        )
        .await;
    assert_status(&response, StatusCode::NOT_FOUND);
    let response = s.f.get_as("/c/nope/new", Some(&s.ruth_cookie)).await;
    assert_status(&response, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Replies

#[sqlx::test]
async fn members_reply_and_land_on_their_post(pool: PgPool) {
    let s = setup(pool).await;
    let t = s.f.thread(s.games, s.ruth, "Spirit Island", "First").await;
    for i in 0..50 {
        s.f.post(t.thread_id, s.ruth, &format!("filler {i}")).await;
    }
    let thread_path = format!("/t/{}/spirit-island", t.thread_id);
    assert!(
        s.f.page(&format!("{thread_path}?page=2"))
            .await
            .contains("filler 49")
    );

    // The thread page has a quick reply box for members.
    let page = s.f.page_as(&thread_path, &s.jonah_cookie).await;
    assert!(
        page.contains(&format!("action=\"/t/{}/reply\"", t.thread_id)),
        "{page}"
    );
    // And asks visitors to log in instead.
    let page = s.f.page(&thread_path).await;
    assert!(!page.contains("/reply\""));
    assert!(page.contains("/login"));

    let response =
        s.f.submit(
            &format!("/t/{}/reply", t.thread_id),
            &[("body", "Second page reply")],
            Some(&s.jonah_cookie),
        )
        .await;
    assert_status(&response, StatusCode::SEE_OTHER);
    let id = anchored_post(response.location());
    assert_eq!(response.location(), format!("{thread_path}?page=2#p{id}"));
    let page = s.f.page(&format!("{thread_path}?page=2")).await;
    let post = post_html(&page, id);
    assert!(post.contains("Second page reply"));
    assert!(post.contains("№ 52"));
    assert!(s.f.page("/c/games").await.contains("Jonah"));
}

#[sqlx::test]
async fn reply_preview_and_errors_keep_the_text(pool: PgPool) {
    let s = setup(pool).await;
    let t = s.f.thread(s.games, s.ruth, "Spirit Island", "First").await;
    let path = format!("/t/{}/reply", t.thread_id);
    let response =
        s.f.submit(
            &path,
            &[("body", "A ~~bad~~ good idea"), ("preview", "1")],
            Some(&s.jonah_cookie),
        )
        .await;
    assert_status(&response, StatusCode::OK);
    assert!(response.body.contains("A <del>bad</del> good idea"));
    assert!(response.body.contains("A ~~bad~~ good idea</textarea>"));
    let response =
        s.f.submit(&path, &[("body", "")], Some(&s.jonah_cookie))
            .await;
    assert_status(&response, StatusCode::BAD_REQUEST);
    assert_eq!(count(&s.f, "SELECT count(*) FROM posts").await, 1);
}

#[sqlx::test]
async fn locked_threads_refuse_replies_except_from_staff(pool: PgPool) {
    let s = setup(pool).await;
    let sam = s.f.member("sam", "moderator", None).await;
    let sam_cookie = s.f.session(sam).await;
    let t =
        s.f.new_thread(NewThread {
            category_id: s.games,
            title: "Closed".into(),
            author_id: s.ruth,
            body_md: "No more".into(),
            forked_from_post_id: None,
            pinned: false,
            locked: true,
            created_at: None,
        })
        .await;
    let path = format!("/t/{}/reply", t.thread_id);
    assert_status(
        &s.f.get_as(&path, Some(&s.jonah_cookie)).await,
        StatusCode::FORBIDDEN,
    );
    let response =
        s.f.submit(&path, &[("body", "but")], Some(&s.jonah_cookie))
            .await;
    assert_status(&response, StatusCode::FORBIDDEN);
    // Nor can posts in it be edited.
    let response =
        s.f.submit(
            &format!("/p/{}/edit", t.first_post_id),
            &[("body", "changed")],
            Some(&s.ruth_cookie),
        )
        .await;
    assert_status(&response, StatusCode::FORBIDDEN);
    let page = s.f.page_as("/t/1/closed", &s.jonah_cookie).await;
    assert!(page.contains("This thread is locked"), "{page}");
    assert!(!page.contains(&format!("action=\"{path}\"")));

    let response =
        s.f.submit(&path, &[("body", "Staff note")], Some(&sam_cookie))
            .await;
    assert_status(&response, StatusCode::SEE_OTHER);
}

#[sqlx::test]
async fn deleted_threads_take_no_replies(pool: PgPool) {
    let s = setup(pool).await;
    let t = s.f.thread(s.games, s.ruth, "Gone", "x").await;
    s.f.exec(&format!(
        "UPDATE threads SET deleted_at = now() WHERE id = {}",
        t.thread_id
    ))
    .await;
    let response =
        s.f.submit(
            &format!("/t/{}/reply", t.thread_id),
            &[("body", "hello?")],
            Some(&s.jonah_cookie),
        )
        .await;
    assert_status(&response, StatusCode::NOT_FOUND);
}

#[sqlx::test]
async fn quoting_prefills_a_blockquote_and_records_the_reply(pool: PgPool) {
    let s = setup(pool).await;
    let t =
        s.f.thread(
            s.games,
            s.ruth,
            "Spirit Island",
            "We lost on turn six.\n\nAgain.",
        )
        .await;
    let form =
        s.f.page_as(
            &format!("/t/{}/reply?quote={}", t.thread_id, t.first_post_id),
            &s.jonah_cookie,
        )
        .await;
    assert!(
        form.contains(
            "&gt; @ruth wrote:\n&gt; We lost on turn six.\n&gt;\n&gt; Again.\n\n</textarea>"
        ) || form.contains(
            "&#62; @ruth wrote:\n&#62; We lost on turn six.\n&#62;\n&#62; Again.\n\n</textarea>"
        ),
        "{form}"
    );
    assert!(form.contains(&format!("name=\"reply_to\" value=\"{}\"", t.first_post_id)));

    let response =
        s.f.submit(
            &format!("/t/{}/reply", t.thread_id),
            &[
                ("body", "> @ruth wrote:\n> We lost\n\nClassic."),
                ("reply_to", &t.first_post_id.to_string()),
            ],
            Some(&s.jonah_cookie),
        )
        .await;
    assert_status(&response, StatusCode::SEE_OTHER);
    let id = anchored_post(response.location());
    let page = s.f.page(&format!("/t/{}/spirit-island", t.thread_id)).await;
    let post = post_html(&page, id);
    assert!(
        post.contains(&format!("href=\"#p{}\"", t.first_post_id)),
        "{post}"
    );
    assert!(post.contains("in reply to <a"), "{post}");
    assert!(post.contains(">@ruth ↑</a>"), "{post}");
    assert!(post.contains("<blockquote>"), "{post}");
    // The quote's mention links to ruth's profile.
    assert!(post.contains("href=\"/u/ruth\""), "{post}");

    // Plain "reply" sets the link without quoting.
    let form =
        s.f.page_as(
            &format!("/t/{}/reply?to={}", t.thread_id, id),
            &s.ruth_cookie,
        )
        .await;
    assert!(form.contains(&format!("name=\"reply_to\" value=\"{id}\"")));
    assert!(form.contains("></textarea>"), "{form}");
}

#[sqlx::test]
async fn replies_can_only_point_into_their_own_thread(pool: PgPool) {
    let s = setup(pool).await;
    let a = s.f.thread(s.games, s.ruth, "Thread A", "a").await;
    let b = s.f.thread(s.games, s.ruth, "Thread B", "b").await;
    for reply_to in [
        b.first_post_id.to_string(),
        "999999".into(),
        "nonsense".into(),
    ] {
        let response =
            s.f.submit(
                &format!("/t/{}/reply", a.thread_id),
                &[("body", "hmm"), ("reply_to", &reply_to)],
                Some(&s.jonah_cookie),
            )
            .await;
        assert_status(&response, StatusCode::BAD_REQUEST);
    }
    let response =
        s.f.get_as(
            &format!("/t/{}/reply?quote={}", a.thread_id, b.first_post_id),
            Some(&s.jonah_cookie),
        )
        .await;
    assert_status(&response, StatusCode::NOT_FOUND);
    assert_eq!(count(&s.f, "SELECT count(*) FROM posts").await, 2);
}

#[sqlx::test]
async fn mentions_link_to_members_who_exist(pool: PgPool) {
    let s = setup(pool).await;
    let t = s.f.thread(s.games, s.ruth, "Hello", "x").await;
    let response =
        s.f.submit(
            &format!("/t/{}/reply", t.thread_id),
            &[("body", "Thanks @Ruth, and hi @nobody. `@jonah`")],
            Some(&s.jonah_cookie),
        )
        .await;
    let id = anchored_post(response.location());
    let page = s.f.page(&format!("/t/{}/hello", t.thread_id)).await;
    let post = post_html(&page, id);
    assert!(post.contains("<a href=\"/u/ruth\""), "{post}");
    assert!(post.contains(">@Ruth</a>"), "{post}");
    assert!(!post.contains("/u/nobody"));
    assert!(!post.contains("/u/jonah"));
}

// ---------------------------------------------------------------------------
// Editing and history

#[sqlx::test]
async fn edits_soon_after_posting_replace_the_original(pool: PgPool) {
    let s = setup(pool).await;
    let t = s.f.thread(s.games, s.ruth, "Typos", "Teh original").await;
    let form =
        s.f.page_as(&format!("/p/{}/edit", t.first_post_id), &s.ruth_cookie)
            .await;
    assert!(form.contains("Teh original</textarea>"), "{form}");
    let response =
        s.f.submit(
            &format!("/p/{}/edit", t.first_post_id),
            &[("body", "The original")],
            Some(&s.ruth_cookie),
        )
        .await;
    assert_status(&response, StatusCode::SEE_OTHER);
    assert_eq!(anchored_post(response.location()), t.first_post_id);
    let page = s.f.page(&format!("/t/{}/typos", t.thread_id)).await;
    let post = post_html(&page, t.first_post_id);
    assert!(post.contains("The original"));
    assert!(!post.contains("edited"), "{post}");
    assert_eq!(count(&s.f, "SELECT count(*) FROM post_revisions").await, 0);
}

#[sqlx::test]
async fn later_edits_keep_the_history(pool: PgPool) {
    let s = setup(pool).await;
    let t =
        s.f.thread(
            s.games,
            s.ruth,
            "Solo",
            "I think solo play is its own hobby.",
        )
        .await;
    let thread_path = format!("/t/{}/solo", t.thread_id);
    // Cached copies to be cleared.
    s.f.page(&thread_path).await;
    age_post(&s.f, t.first_post_id).await;
    let edit = format!("/p/{}/edit", t.first_post_id);
    for body in [
        "I think solo play is its own hobby that shares equipment.",
        "I’m starting to think solo play is its own hobby that shares equipment.",
    ] {
        let response =
            s.f.submit(&edit, &[("body", body)], Some(&s.ruth_cookie))
                .await;
        assert_status(&response, StatusCode::SEE_OTHER);
    }
    // Saving the same text again changes nothing.
    s.f.submit(
        &edit,
        &[(
            "body",
            "I’m starting to think solo play is its own hobby that shares equipment.",
        )],
        Some(&s.ruth_cookie),
    )
    .await;
    assert_eq!(count(&s.f, "SELECT count(*) FROM post_revisions").await, 2);

    let page = s.f.page(&thread_path).await;
    let post = post_html(&page, t.first_post_id);
    assert!(post.contains("I’m starting to think"), "{post}");
    assert!(
        post.contains(&format!("href=\"/p/{}/revisions\"", t.first_post_id)),
        "{post}"
    );
    assert!(post.contains("edited"));

    let history = s.f.page(&format!("/p/{}/revisions", t.first_post_id)).await;
    assert!(history.contains("Version 3"), "{history}");
    assert!(history.contains("Version 2"));
    assert!(history.contains("Version 1"));
    // The newest version is shown against the one before.
    assert!(
        history.contains("<del>I</del><ins>I’m starting to</ins> think solo play"),
        "{history}"
    );
    let original =
        s.f.page(&format!("/p/{}/revisions?v=1", t.first_post_id))
            .await;
    assert!(
        original.contains("I think solo play is its own hobby."),
        "{original}"
    );
    assert!(original.contains("The original post"));
    assert_status(
        &s.f.get(&format!("/p/{}/revisions?v=4", t.first_post_id))
            .await,
        StatusCode::NOT_FOUND,
    );
    assert_status(
        &s.f.get(&format!("/p/{}/revisions?v=x", t.first_post_id))
            .await,
        StatusCode::NOT_FOUND,
    );
}

#[sqlx::test]
async fn only_authors_edit_and_delete_their_posts(pool: PgPool) {
    let s = setup(pool).await;
    let t = s.f.thread(s.games, s.ruth, "Mine", "Ruth's words").await;
    let edit = format!("/p/{}/edit", t.first_post_id);
    let delete = format!("/p/{}/delete", t.first_post_id);
    assert_status(
        &s.f.get_as(&edit, Some(&s.jonah_cookie)).await,
        StatusCode::FORBIDDEN,
    );
    assert_status(
        &s.f.submit(&edit, &[("body", "Jonah's words")], Some(&s.jonah_cookie))
            .await,
        StatusCode::FORBIDDEN,
    );
    assert_status(
        &s.f.submit(&delete, &[], Some(&s.jonah_cookie)).await,
        StatusCode::FORBIDDEN,
    );
    let page = s.f.page_as("/t/1/mine", &s.jonah_cookie).await;
    assert!(!page.contains(&edit));
    let page = s.f.page_as("/t/1/mine", &s.ruth_cookie).await;
    assert!(page.contains(&edit));
    assert!(page.contains(&delete));
    assert!(s.f.page("/t/1/mine").await.contains("Ruth's words"));
}

#[sqlx::test]
async fn deleting_a_post_leaves_a_tombstone_and_hides_its_history(pool: PgPool) {
    let s = setup(pool).await;
    let t = s.f.thread(s.games, s.ruth, "Oops", "First").await;
    let reply = s.f.post(t.thread_id, s.ruth, "Regrettable").await;
    age_post(&s.f, reply).await;
    s.f.submit(
        &format!("/p/{reply}/edit"),
        &[("body", "Still regrettable")],
        Some(&s.ruth_cookie),
    )
    .await;
    assert!(
        s.f.page(&format!("/p/{reply}/revisions"))
            .await
            .contains("Regrettable")
    );

    let confirm =
        s.f.page_as(&format!("/p/{reply}/delete"), &s.ruth_cookie)
            .await;
    assert!(confirm.contains(&format!("action=\"/p/{reply}/delete\"")));
    let response =
        s.f.submit(&format!("/p/{reply}/delete"), &[], Some(&s.ruth_cookie))
            .await;
    assert_status(&response, StatusCode::SEE_OTHER);
    let page = s.f.page("/t/1/oops").await;
    assert!(!page.contains("regrettable"));
    assert!(post_html(&page, reply).contains("Post deleted"));
    assert_status(
        &s.f.get(&format!("/p/{reply}/revisions")).await,
        StatusCode::NOT_FOUND,
    );
    assert_status(
        &s.f.get_as(&format!("/p/{reply}/edit"), Some(&s.ruth_cookie))
            .await,
        StatusCode::NOT_FOUND,
    );
}

#[sqlx::test]
async fn editing_a_quoted_post_updates_the_threads_that_embed_it(pool: PgPool) {
    let s = setup(pool).await;
    let t =
        s.f.thread(s.games, s.ruth, "Weekly", "Solo is a hobby")
            .await;
    age_post(&s.f, t.first_post_id).await;
    let fork =
        s.f.new_thread(NewThread {
            category_id: s.games,
            title: "Is solo a hobby".into(),
            author_id: s.jonah,
            body_md: "Discuss".into(),
            forked_from_post_id: Some(t.first_post_id),
            pinned: false,
            locked: false,
            created_at: None,
        })
        .await;
    let fork_path = format!("/t/{}/is-solo-a-hobby", fork.thread_id);
    assert!(s.f.page(&fork_path).await.contains("Solo is a hobby"));
    s.f.submit(
        &format!("/p/{}/edit", t.first_post_id),
        &[("body", "Solo is a separate hobby")],
        Some(&s.ruth_cookie),
    )
    .await;
    let page = s.f.page(&fork_path).await;
    assert!(page.contains("Solo is a separate hobby"), "{page}");
    assert!(page.contains(&format!("/p/{}/revisions", t.first_post_id)));
    s.f.submit(
        &format!("/p/{}/delete", t.first_post_id),
        &[],
        Some(&s.ruth_cookie),
    )
    .await;
    let page = s.f.page(&fork_path).await;
    assert!(page.contains("This post is unavailable"));
    assert!(!page.contains("separate hobby"));
}

// ---------------------------------------------------------------------------
// Reactions

#[sqlx::test]
async fn members_react_with_the_enabled_emoji(pool: PgPool) {
    let s = setup(pool).await;
    let t = s.f.thread(s.games, s.ruth, "Heat", "Great game").await;
    let path = "/t/1/heat";
    let react = format!("/p/{}/reactions", t.first_post_id);
    // A cached anonymous copy, to be cleared.
    s.f.page(path).await;
    for (cookie, emoji) in [
        (&s.jonah_cookie, "👍"),
        (&s.ruth_cookie, "👍"),
        (&s.jonah_cookie, "👍"),
        (&s.jonah_cookie, "😂"),
    ] {
        let response =
            s.f.submit(&react, &[("emoji", emoji), ("action", "add")], Some(cookie))
                .await;
        assert_status(&response, StatusCode::SEE_OTHER);
        assert_eq!(anchored_post(response.location()), t.first_post_id);
    }
    assert_eq!(count(&s.f, "SELECT count(*) FROM reactions").await, 3);
    let page = s.f.page(path).await;
    let post = post_html(&page, t.first_post_id);
    assert!(post.contains("title=\"Jonah, Ruth\""), "{post}");
    assert!(post.contains("👍"));
    assert!(post.contains(">2<"), "{post}");
    // Visitors see counts but no buttons.
    assert!(!post.contains("<button"));

    // A member sees which they've used, and can take one back.
    let page = s.f.page_as(path, &s.jonah_cookie).await;
    let post = post_html(&page, t.first_post_id);
    assert!(post.contains("aria-pressed=\"true\""), "{post}");
    s.f.submit(
        &react,
        &[("emoji", "😂"), ("action", "remove")],
        Some(&s.jonah_cookie),
    )
    .await;
    assert!(!post_html(&s.f.page(path).await, t.first_post_id).contains("😂"));

    // Who reacted, without needing to hover.
    let who = s.f.page(&react).await;
    assert!(who.contains("Jonah"), "{who}");
    assert!(who.contains("Ruth"));

    // Only enabled emoji are accepted.
    s.f.exec("UPDATE reaction_types SET enabled = false WHERE emoji = '😢'")
        .await;
    for emoji in ["😢", "🦀", ""] {
        let response =
            s.f.submit(
                &react,
                &[("emoji", emoji), ("action", "add")],
                Some(&s.ruth_cookie),
            )
            .await;
        assert_status(&response, StatusCode::BAD_REQUEST);
    }
    assert_eq!(count(&s.f, "SELECT count(*) FROM reactions").await, 2);
}

#[sqlx::test]
async fn deleted_posts_cant_be_reacted_to(pool: PgPool) {
    let s = setup(pool).await;
    let t = s.f.thread(s.games, s.ruth, "Heat", "Great game").await;
    s.f.exec(&format!(
        "UPDATE posts SET deleted_at = now() WHERE id = {}",
        t.first_post_id
    ))
    .await;
    let response =
        s.f.submit(
            &format!("/p/{}/reactions", t.first_post_id),
            &[("emoji", "👍"), ("action", "add")],
            Some(&s.jonah_cookie),
        )
        .await;
    assert_status(&response, StatusCode::NOT_FOUND);
}

#[sqlx::test]
async fn admins_choose_the_reactions(pool: PgPool) {
    let s = setup(pool).await;
    let admin = s.f.member("sam", "admin", None).await;
    let admin_cookie = s.f.session(admin).await;
    assert_status(
        &s.f.get_as("/admin/reactions", Some(&s.ruth_cookie)).await,
        StatusCode::FORBIDDEN,
    );
    assert_status(
        &s.f.submit(
            "/admin/reactions",
            &[("action", "add"), ("emoji", "🎲")],
            Some(&s.ruth_cookie),
        )
        .await,
        StatusCode::FORBIDDEN,
    );
    let page = s.f.page_as("/admin/reactions", &admin_cookie).await;
    assert!(page.contains("👍"));
    let response =
        s.f.submit(
            "/admin/reactions",
            &[("action", "add"), ("emoji", "🎲")],
            Some(&admin_cookie),
        )
        .await;
    assert_status(&response, StatusCode::SEE_OTHER);
    // Seven enabled now; one more is allowed, a ninth is not.
    let response =
        s.f.submit(
            "/admin/reactions",
            &[("action", "add"), ("emoji", "🎉")],
            Some(&admin_cookie),
        )
        .await;
    assert_status(&response, StatusCode::SEE_OTHER);
    let response =
        s.f.submit(
            "/admin/reactions",
            &[("action", "add"), ("emoji", "🔥")],
            Some(&admin_cookie),
        )
        .await;
    assert_status(&response, StatusCode::BAD_REQUEST);
    for bad in ["", "abc", "<b>", "👍"] {
        let response =
            s.f.submit(
                "/admin/reactions",
                &[("action", "add"), ("emoji", bad)],
                Some(&admin_cookie),
            )
            .await;
        assert_status(&response, StatusCode::BAD_REQUEST);
    }
    s.f.submit(
        "/admin/reactions",
        &[("action", "disable"), ("emoji", "😢")],
        Some(&admin_cookie),
    )
    .await;
    s.f.submit(
        "/admin/reactions",
        &[("action", "up"), ("emoji", "🎲")],
        Some(&admin_cookie),
    )
    .await;
    let order: Vec<String> = sqlx::query_scalar(
        "SELECT emoji FROM reaction_types WHERE enabled ORDER BY position, emoji",
    )
    .fetch_all(&s.f.db)
    .await
    .unwrap();
    // 🎲 moved up past 🙏 (the turned-off 😢 still has its place).
    assert_eq!(order, vec!["👍", "❤️", "😂", "😮", "🎲", "🙏", "🎉"]);

    // The thread page offers the new set.
    let t = s.f.thread(s.games, s.ruth, "Heat", "Great game").await;
    let page = s.f.page_as("/t/1/heat", &s.jonah_cookie).await;
    let post = post_html(&page, t.first_post_id);
    assert!(post.contains("🎲"), "{post}");
    assert!(!post.contains("😢"));
}

// ---------------------------------------------------------------------------
// Read tracking

#[sqlx::test]
async fn read_markers_follow_what_members_have_seen(pool: PgPool) {
    let s = setup(pool).await;
    let t = s.f.thread(s.games, s.ruth, "Long one", "post 1").await;
    let mut ids = vec![t.first_post_id];
    for i in 2..=60 {
        ids.push(s.f.post(t.thread_id, s.ruth, &format!("post {i}")).await);
    }
    let path = format!("/t/{}/long-one", t.thread_id);
    // Never opened: everything is new, and "unread" starts at the top.
    let list = s.f.page_as("/c/games", &s.jonah_cookie).await;
    assert!(list.contains("60 new"), "{list}");
    let response =
        s.f.get_as(&format!("/t/{}/unread", t.thread_id), Some(&s.jonah_cookie))
            .await;
    assert_eq!(response.location(), format!("{path}#p{}", ids[0]));

    // Reading page 1 moves the marker to post 50.
    let page = s.f.page_as(&path, &s.jonah_cookie).await;
    assert!(!page.contains("New since your last visit"));
    let list = s.f.page_as("/c/games", &s.jonah_cookie).await;
    assert!(list.contains("10 new"), "{list}");
    let response =
        s.f.get_as(&format!("/t/{}/unread", t.thread_id), Some(&s.jonah_cookie))
            .await;
    assert_eq!(response.location(), format!("{path}?page=2#p{}", ids[50]));
    // The marker doesn't go back when an earlier page is read again.
    s.f.page_as(&format!("{path}?page=2"), &s.jonah_cookie)
        .await;
    s.f.page_as(&path, &s.jonah_cookie).await;
    let list = s.f.page_as("/c/games", &s.jonah_cookie).await;
    assert!(!list.contains(" new<"), "{list}");
    let response =
        s.f.get_as(&format!("/t/{}/unread", t.thread_id), Some(&s.jonah_cookie))
            .await;
    assert_eq!(response.location(), format!("{path}?page=2#p{}", ids[59]));

    // New posts after the last visit are marked off.
    let newer = s.f.post(t.thread_id, s.ruth, "post 61").await;
    let page =
        s.f.page_as(&format!("{path}?page=2"), &s.jonah_cookie)
            .await;
    let divider = page.find("New since your last visit").expect("divider");
    assert!(divider < page.find(&format!("id=\"p{newer}\"")).unwrap());
    assert!(divider > page.find(&format!("id=\"p{}\"", ids[59])).unwrap());

    // Anonymous pages never show any of this.
    let anon = s.f.page("/c/games").await;
    assert!(!anon.contains(" new<"));
    let response = s.f.get(&format!("/t/{}/unread", t.thread_id)).await;
    assert_eq!(response.location(), path);
}

#[sqlx::test]
async fn whole_categories_can_be_marked_read(pool: PgPool) {
    let s = setup(pool).await;
    let a = s.f.thread(s.games, s.ruth, "A", "x").await;
    s.f.post(a.thread_id, s.ruth, "y").await;
    s.f.thread(s.games, s.ruth, "B", "x").await;
    let list = s.f.page_as("/c/games", &s.jonah_cookie).await;
    assert!(list.contains("2 new") && list.contains("1 new"), "{list}");
    assert!(list.contains("action=\"/c/games/read\""));
    let response =
        s.f.submit("/c/games/read", &[], Some(&s.jonah_cookie))
            .await;
    assert_status(&response, StatusCode::SEE_OTHER);
    assert_eq!(response.location(), "/c/games");
    let list = s.f.page_as("/c/games", &s.jonah_cookie).await;
    assert!(!list.contains(" new<"), "{list}");
    // Only for jonah.
    assert!(
        s.f.page_as("/c/games", &s.ruth_cookie)
            .await
            .contains("2 new")
    );
}

// ---------------------------------------------------------------------------
// Quote-forks

#[sqlx::test]
async fn quote_forks_start_a_thread_that_embeds_the_post(pool: PgPool) {
    let s = setup(pool).await;
    let books = s.f.category("books", "public", None, 1).await;
    let lounge = s.f.category("lounge", "members", None, 2).await;
    let t =
        s.f.thread(s.games, s.ruth, "Weekly", "Solo play is its own hobby")
            .await;
    let form =
        s.f.page_as(&format!("/p/{}/fork", t.first_post_id), &s.jonah_cookie)
            .await;
    assert!(form.contains("Solo play is its own hobby"), "{form}");
    for slug in ["games", "books", "lounge"] {
        assert!(form.contains(&format!("value=\"{slug}\"")), "{slug}");
    }
    // The page the post is on is cached for visitors, to be cleared.
    s.f.page("/t/1/weekly").await;

    let response =
        s.f.submit(
            &format!("/p/{}/fork", t.first_post_id),
            &[
                ("title", "Is solo a hobby?"),
                ("category", "books"),
                ("body", "Strong disagree."),
            ],
            Some(&s.jonah_cookie),
        )
        .await;
    assert_status(&response, StatusCode::SEE_OTHER);
    let location = response.location().to_owned();
    let fork = s.f.page(location.split('#').next().unwrap()).await;
    assert!(fork.contains("Strong disagree."));
    assert!(fork.contains("class=\"embed\""), "{fork}");
    assert!(fork.contains("Solo play is its own hobby"));
    let source = s.f.page("/t/1/weekly").await;
    assert!(source.contains("Continued in"), "{source}");
    assert!(source.contains("Is solo a hobby?"));
    let _ = (books, lounge);
}

#[sqlx::test]
async fn quote_forks_never_make_a_post_more_visible(pool: PgPool) {
    let s = setup(pool).await;
    s.f.category("books", "public", None, 1).await;
    let lounge = s.f.category("lounge", "members", None, 2).await;
    s.f.category("lounge-two", "members", None, 3).await;
    let t =
        s.f.thread(lounge, s.ruth, "Members chat", "Members-only words")
            .await;
    let path = format!("/p/{}/fork", t.first_post_id);
    let form = s.f.page_as(&path, &s.jonah_cookie).await;
    assert!(form.contains("value=\"lounge\""));
    assert!(form.contains("value=\"lounge-two\""));
    assert!(!form.contains("value=\"games\""), "{form}");
    assert!(!form.contains("value=\"books\""));
    // A hand-made form can't get round it.
    for slug in ["games", "books", "no-such"] {
        let response =
            s.f.submit(
                &path,
                &[("title", "Leak"), ("category", slug), ("body", "x")],
                Some(&s.jonah_cookie),
            )
            .await;
        assert_status(&response, StatusCode::BAD_REQUEST);
    }
    assert_eq!(count(&s.f, "SELECT count(*) FROM threads").await, 1);
    let response =
        s.f.submit(
            &path,
            &[("title", "Fine"), ("category", "lounge-two"), ("body", "x")],
            Some(&s.jonah_cookie),
        )
        .await;
    assert_status(&response, StatusCode::SEE_OTHER);
}

#[sqlx::test]
async fn deleted_hidden_and_invisible_posts_cant_be_quoted(pool: PgPool) {
    let s = setup(pool).await;
    let t = s.f.thread(s.games, s.ruth, "Weekly", "first").await;
    let hidden = s.f.post(t.thread_id, s.ruth, "hidden").await;
    let deleted = s.f.post(t.thread_id, s.ruth, "deleted").await;
    s.f.exec(&format!(
        "UPDATE posts SET hidden_at = now() WHERE id = {hidden}"
    ))
    .await;
    s.f.exec(&format!(
        "UPDATE posts SET deleted_at = now() WHERE id = {deleted}"
    ))
    .await;
    let organisers = s.f.category("organisers", "groups", None, 1).await;
    let secret = s.f.thread(organisers, s.ruth, "Secret", "secret").await;
    for post in [hidden, deleted] {
        assert_status(
            &s.f.get_as(&format!("/p/{post}/fork"), Some(&s.jonah_cookie))
                .await,
            StatusCode::NOT_FOUND,
        );
        assert_status(
            &s.f.get_as(
                &format!("/t/{}/reply?quote={post}", t.thread_id),
                Some(&s.jonah_cookie),
            )
            .await,
            StatusCode::NOT_FOUND,
        );
    }
    assert_status(
        &s.f.get_as(
            &format!("/p/{}/fork", secret.first_post_id),
            Some(&s.jonah_cookie),
        )
        .await,
        StatusCode::NOT_FOUND,
    );
    assert_status(
        &s.f.submit(
            &format!("/p/{}/fork", secret.first_post_id),
            &[("title", "Leak"), ("category", "games"), ("body", "x")],
            Some(&s.jonah_cookie),
        )
        .await,
        StatusCode::NOT_FOUND,
    );
}

// ---------------------------------------------------------------------------
// The pages themselves

#[sqlx::test]
async fn writing_pages_have_no_inline_styles_or_scripts(pool: PgPool) {
    let s = setup(pool).await;
    let admin = s.f.member("sam", "admin", None).await;
    let admin_cookie = s.f.session(admin).await;
    let t = s.f.thread(s.games, s.ruth, "Heat", "Great game").await;
    age_post(&s.f, t.first_post_id).await;
    s.f.submit(
        &format!("/p/{}/edit", t.first_post_id),
        &[("body", "Great game, really")],
        Some(&s.ruth_cookie),
    )
    .await;
    let p = t.first_post_id;
    let mut pages = vec![
        (String::from("/c/games/new"), Some(s.ruth_cookie.clone())),
        (
            format!("/t/{}/reply", t.thread_id),
            Some(s.ruth_cookie.clone()),
        ),
        (
            format!("/t/{}/reply?quote={p}", t.thread_id),
            Some(s.ruth_cookie.clone()),
        ),
        (format!("/p/{p}/edit"), Some(s.ruth_cookie.clone())),
        (format!("/p/{p}/delete"), Some(s.ruth_cookie.clone())),
        (format!("/p/{p}/fork"), Some(s.jonah_cookie.clone())),
        (format!("/p/{p}/revisions"), None),
        (format!("/p/{p}/revisions?v=1"), None),
        (format!("/p/{p}/reactions"), None),
        (format!("/p/{p}/reactions"), Some(s.jonah_cookie.clone())),
        ("/t/1/heat".to_owned(), Some(s.jonah_cookie.clone())),
        ("/c/games".to_owned(), Some(s.jonah_cookie.clone())),
        ("/admin/reactions".to_owned(), Some(admin_cookie)),
    ];
    pages.push(("/t/1/heat".to_owned(), None));
    for (path, cookie) in pages {
        let response = s.f.get_as(&path, cookie.as_deref()).await;
        assert_status(&response, StatusCode::OK);
        assert!(!response.body.contains("style="), "{path}");
        assert!(!response.body.contains("<script"), "{path}");
        if cookie.is_none() {
            assert!(response.header(header::SET_COOKIE).is_none(), "{path}");
        }
    }
}

#[sqlx::test]
async fn posting_is_rate_limited(pool: PgPool) {
    let s = setup(pool).await;
    let t = s.f.thread(s.games, s.ruth, "Busy", "x").await;
    let mut refused = false;
    for i in 0..40 {
        let response =
            s.f.submit(
                &format!("/t/{}/reply", t.thread_id),
                &[("body", &format!("reply {i}"))],
                Some(&s.jonah_cookie),
            )
            .await;
        if response.status == StatusCode::TOO_MANY_REQUESTS {
            refused = true;
            assert!(
                response.body.contains(&format!("reply {i}")),
                "the text is kept"
            );
            break;
        }
        assert_status(&response, StatusCode::SEE_OTHER);
    }
    assert!(refused);
    // Others can still post.
    let response =
        s.f.submit(
            &format!("/t/{}/reply", t.thread_id),
            &[("body", "ruth's turn")],
            Some(&s.ruth_cookie),
        )
        .await;
    assert_status(&response, StatusCode::SEE_OTHER);
}

// ---------------------------------------------------------------------------
// The "at most as visible" rule, over random forums

/// Who can see something at a level, for the reference rule below. No
/// member is in a group yet (M5), so only staff see group categories.
fn sees(who: &str, level: &str) -> bool {
    match level {
        "public" => true,
        "members" => who != "anonymous",
        _ => who == "staff",
    }
}

/// Everyone who could see `dest` could already see `source`.
fn nobody_sees_more(source: &str, dest: &str) -> bool {
    ["anonymous", "member", "staff"]
        .iter()
        .all(|who| !sees(who, dest) || sees(who, source))
}

#[hegel::standalone_function(test_cases = 60, suppress_health_check = [hegel::HealthCheck::TooSlow])]
fn forks_follow_the_rule(tc: hegel::TestCase, rt: &tokio::runtime::Handle, db: &PgPool) {
    use hegel::generators as gs;
    rt.block_on(async {
        sqlx::raw_sql(
            "TRUNCATE settings, posts, threads, categories, sections, users RESTART IDENTITY CASCADE",
        )
        .execute(db)
        .await
        .unwrap();
    });
    let f = Forum::new(db.clone());
    let levels = vec!["public", "members", "groups"];
    let writes = vec!["members", "groups", "staff"];
    let count = tc.draw(gs::integers::<usize>().min_value(1).max_value(5));
    let mut categories = Vec::new();
    for i in 0..count {
        let read = tc.draw(gs::sampled_from(levels.clone()));
        let write = tc.draw(gs::sampled_from(writes.clone()));
        let slug = format!("c{i}");
        rt.block_on(async {
            f.category(&slug, read, None, i as i32).await;
            f.exec(&format!(
                "UPDATE categories SET write_level = '{write}' WHERE slug = '{slug}'"
            ))
            .await;
        });
        categories.push((slug, read, write));
    }
    let role = tc.draw(gs::sampled_from(vec!["user", "moderator"]));
    let who = if role == "user" { "member" } else { "staff" };
    let source_category = tc.draw(gs::integers::<usize>().max_value(count - 1));
    let removal = tc.draw(gs::sampled_from(vec!["", "deleted", "hidden"]));
    let (cookie, post_id) = rt.block_on(async {
        let author = f.user("author").await;
        let me = f.member("forker", role, None).await;
        let t = f
            .thread(
                (source_category + 1) as i64,
                author,
                "Source",
                "Secret source text",
            )
            .await;
        if !removal.is_empty() {
            f.exec(&format!(
                "UPDATE posts SET {removal}_at = now() WHERE id = {}",
                t.first_post_id
            ))
            .await;
        }
        (f.session(me).await, t.first_post_id)
    });
    let source_level = categories[source_category].1;
    let source_visible = removal.is_empty() && sees(who, source_level);
    for (slug, read, write) in &categories {
        let response = rt.block_on(f.submit(
            &format!("/p/{post_id}/fork"),
            &[("title", "Fork"), ("category", slug), ("body", "Opening")],
            Some(&cookie),
        ));
        let may_write = sees(who, read) && (*write == "members" || who == "staff");
        let expected = if !source_visible {
            StatusCode::NOT_FOUND
        } else if may_write && nobody_sees_more(source_level, read) {
            StatusCode::SEE_OTHER
        } else {
            StatusCode::BAD_REQUEST
        };
        assert_eq!(
            response.status, expected,
            "{who} forking from {source_level} into {read} (write {write})"
        );
        if response.status == StatusCode::SEE_OTHER {
            // Nobody sees the quoted post through the fork who couldn't
            // see it where it is.
            let path = response.location().split('#').next().unwrap().to_owned();
            let anonymous = rt.block_on(f.get(&path));
            if anonymous.status == StatusCode::OK {
                assert_eq!(
                    anonymous.body.contains("Secret source text"),
                    sees("anonymous", source_level),
                    "{path}"
                );
            }
        }
    }
}

#[sqlx::test]
async fn quote_forks_follow_the_visibility_rule_property(pool: PgPool) {
    let rt = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || forks_follow_the_rule(&rt, &pool))
        .await
        .unwrap();
}
