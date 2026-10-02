//! Members' own data (DESIGN.md §2): downloading everything held about
//! them, deleting their account (their posts stay, anonymised), and the
//! site's terms and privacy pages.

mod common;

use axum::http::{StatusCode, header};
use common::{Forum, cookie_set_by, post_html};
use parlour::http_cache::PRIVATE_CACHE_CONTROL;
use parlour::store::{self, NewConversation};
use serde_json::Value;
use sqlx::PgPool;

const PASSWORD: &str = "correct horse battery";

struct Setup {
    f: Forum,
    public: i64,
    ruth: i64,
    ruth_cookie: String,
    jonah: i64,
}

async fn setup(pool: PgPool) -> Setup {
    let f = Forum::new(pool).without_rate_limits();
    f.no_new_members().await;
    let public = f.category("games", "public", None, 0).await;
    let ruth = f.member("ruth", "user", Some(PASSWORD)).await;
    let jonah = f.member("jonah", "user", None).await;
    Setup {
        ruth_cookie: f.session(ruth).await,
        f,
        public,
        ruth,
        jonah,
    }
}

#[sqlx::test]
async fn members_download_their_data(pool: PgPool) {
    let s = setup(pool).await;
    s.f.exec(&format!(
        "UPDATE users SET status_line = 'Keeper of the spreadsheet' WHERE id = {}",
        s.ruth
    ))
    .await;
    let thread =
        s.f.thread(s.public, s.ruth, "Heat", "We played Heat.")
            .await;
    let reply =
        s.f.post(thread.thread_id, s.jonah, "Jonah's own words.")
            .await;
    s.f.exec(&format!(
        "UPDATE posts SET created_at = now() - interval '1 hour' WHERE id = {}",
        thread.first_post_id
    ))
    .await;
    store::edit_post(
        &s.f.db,
        thread.first_post_id,
        s.ruth,
        "We played Heat, twice.",
    )
    .await
    .unwrap();
    let conversation = store::create_conversation(
        &s.f.db,
        &NewConversation {
            author_id: s.jonah,
            title: "Gift ideas".into(),
            recipient_ids: vec![s.ruth],
            body_md: "Jonah's secret message.".into(),
            created_at: None,
        },
    )
    .await
    .unwrap();
    s.f.post(conversation.thread_id, s.ruth, "Ruth's reply in private.")
        .await;
    s.f.exec(&format!(
        "INSERT INTO reactions (post_id, user_id, emoji) VALUES ({reply}, {}, '👍')",
        s.ruth
    ))
    .await;
    // A thread she wrote in that she can no longer read.
    let hidden = s.f.category("secret", "members", None, 2).await;
    let group = s.f.group("org", "Organisers").await;
    let secret =
        s.f.thread(hidden, s.ruth, "Secret plans title", "Ruth's plan.")
            .await;
    s.f.restrict(hidden, &[group], None).await;

    let page = s.f.page_as("/settings", &s.ruth_cookie).await;
    assert!(page.contains("action=\"/settings/export\""));
    let response =
        s.f.submit("/settings/export", &[], Some(&s.ruth_cookie))
            .await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(
        response.header(header::CONTENT_TYPE),
        Some("application/json")
    );
    assert!(
        response
            .header(header::CONTENT_DISPOSITION)
            .unwrap()
            .starts_with("attachment; filename=\"ruth-")
    );
    assert_eq!(
        response.header(header::CACHE_CONTROL),
        Some(PRIVATE_CACHE_CONTROL)
    );
    let data: Value = serde_json::from_str(&response.body).unwrap();
    assert_eq!(data["account"]["username"], "ruth");
    assert_eq!(data["account"]["email"], "ruth@example.com");
    assert_eq!(data["account"]["status_line"], "Keeper of the spreadsheet");
    let posts = data["posts"].as_array().unwrap();
    let bodies: Vec<&str> = posts
        .iter()
        .map(|p| p["markdown"].as_str().unwrap())
        .collect();
    assert!(bodies.contains(&"We played Heat, twice."));
    assert!(bodies.contains(&"Ruth's reply in private."));
    assert!(bodies.contains(&"Ruth's plan."));
    // Earlier versions of her posts are hers too.
    assert!(response.body.contains("We played Heat."));
    // Other people's words are not.
    assert!(!response.body.contains("Jonah's own words"));
    assert!(!response.body.contains("Jonah's secret message"));
    // Nor the title of a thread she can't read any more.
    assert!(!response.body.contains("Secret plans title"));
    let secret_post = posts
        .iter()
        .find(|p| p["id"] == secret.first_post_id)
        .unwrap();
    assert_eq!(secret_post["thread_title"], Value::Null);
    assert_eq!(data["conversations"][0]["title"], "Gift ideas");
    assert_eq!(data["reactions"][0]["emoji"], "👍");
    assert!(data["sessions"].as_array().is_some_and(|s| !s.is_empty()));
}

#[sqlx::test]
async fn visitors_have_no_data_to_download(pool: PgPool) {
    let s = setup(pool).await;
    let response = s.f.submit("/settings/export", &[], None).await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert!(response.location().starts_with("/login"));
}

#[sqlx::test]
async fn deleting_an_account_keeps_its_posts_anonymised(pool: PgPool) {
    let s = setup(pool).await;
    let thread =
        s.f.thread(s.public, s.ruth, "Heat", "We played Heat.")
            .await;
    let href = format!("/t/{}/heat", thread.thread_id);
    s.f.exec(&format!(
        "INSERT INTO reactions (post_id, user_id, emoji) VALUES ({}, {}, '👍')",
        thread.first_post_id, s.jonah
    ))
    .await;
    let conversation = store::create_conversation(
        &s.f.db,
        &NewConversation {
            author_id: s.jonah,
            title: "Gifts".into(),
            recipient_ids: vec![s.ruth],
            body_md: "Hello.".into(),
            created_at: None,
        },
    )
    .await
    .unwrap();
    // Visitors have the page cached before the change.
    assert!(s.f.page(&href).await.contains("Ruth"));
    assert!(s.f.page("/users/tree").await.contains("/u/ruth"));

    // The wrong password, or no confirmation, changes nothing.
    let response =
        s.f.submit(
            "/settings/delete",
            &[("password", "wrong"), ("confirm", "ruth")],
            Some(&s.ruth_cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    let response =
        s.f.submit(
            "/settings/delete",
            &[("password", PASSWORD), ("confirm", "")],
            Some(&s.ruth_cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        s.f.get_as("/settings", Some(&s.ruth_cookie)).await.status,
        StatusCode::OK
    );

    let response =
        s.f.submit(
            "/settings/delete",
            &[("password", PASSWORD), ("confirm", "ruth")],
            Some(&s.ruth_cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    assert!(response.body.contains("Your account has been deleted"));
    // The browser is logged out, and the session is gone.
    assert!(
        response
            .header(header::SET_COOKIE)
            .unwrap()
            .contains("Max-Age=0")
    );
    let settings = s.f.get_as("/settings", Some(&s.ruth_cookie)).await;
    assert_eq!(settings.status, StatusCode::SEE_OTHER);

    // Her post stays, by nobody in particular, on the cached page too.
    let page = s.f.page(&href).await;
    let post = post_html(&page, thread.first_post_id);
    assert!(post.contains("We played Heat."));
    assert!(post.contains("Deleted member"));
    assert!(!post.contains("Ruth"));
    assert!(!post.contains("@ruth"));
    // Her profile and her place in the tree are gone.
    assert_eq!(s.f.get("/u/ruth").await.status, StatusCode::NOT_FOUND);
    assert!(!s.f.page("/users/tree").await.contains("/u/ruth"));

    // Nothing that identifies her is left in her account.
    let row: (String, String, String, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT username::text, display_name, email::text, password_hash, status_line FROM users WHERE id = $1",
    )
    .bind(s.ruth)
    .fetch_one(&s.f.db)
    .await
    .unwrap();
    assert_eq!(row.0, format!("deleted_{}", s.ruth));
    assert_eq!(row.1, "Deleted member");
    assert!(!row.2.contains("ruth"));
    assert_eq!((row.3, row.4), (None, None));
    for (table, column) in [
        ("sessions", "user_id"),
        ("user_ips", "user_id"),
        ("reactions", "user_id"),
        ("group_members", "user_id"),
        ("login_tokens", "user_id"),
    ] {
        let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT count(*) FROM {table} WHERE {column} = $1"
        )))
        .bind(s.ruth)
        .fetch_one(&s.f.db)
        .await
        .unwrap();
        assert_eq!(n, 0, "{table}");
    }
    let left: bool = sqlx::query_scalar(
        "SELECT left_at IS NOT NULL FROM private_thread_participants WHERE thread_id = $1 AND user_id = $2",
    )
    .bind(conversation.thread_id)
    .bind(s.ruth)
    .fetch_one(&s.f.db)
    .await
    .unwrap();
    assert!(left);
    // She can't log in, and her address can be used again.
    let response =
        s.f.submit(
            "/login",
            &[
                ("identifier", "ruth@example.com"),
                ("password", PASSWORD),
                ("next", ""),
            ],
            None,
        )
        .await;
    assert!(cookie_set_by(&response).is_none());
    s.f.member("newruth", "user", None).await;
    s.f.exec("UPDATE users SET email = 'ruth@example.com' WHERE username = 'newruth'")
        .await;
}

#[sqlx::test]
async fn the_last_admin_cannot_delete_their_account(pool: PgPool) {
    let f = Forum::new(pool);
    let sam = f.member("sam", "admin", Some(PASSWORD)).await;
    let cookie = f.session(sam).await;
    let response = f
        .submit(
            "/settings/delete",
            &[("password", PASSWORD), ("confirm", "sam")],
            Some(&cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(response.body.contains("only admin"));
    assert_eq!(
        f.get_as("/settings", Some(&cookie)).await.status,
        StatusCode::OK
    );
}

#[sqlx::test]
async fn members_without_passwords_confirm_with_their_username(pool: PgPool) {
    let s = setup(pool).await;
    let cookie = s.f.session(s.jonah).await;
    let page = s.f.page_as("/settings", &cookie).await;
    assert!(page.contains("action=\"/settings/delete\""));
    let response =
        s.f.submit("/settings/delete", &[("confirm", "JONAH")], Some(&cookie))
            .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    let deleted: bool =
        sqlx::query_scalar("SELECT deleted_at IS NOT NULL FROM users WHERE id = $1")
            .bind(s.jonah)
            .fetch_one(&s.f.db)
            .await
            .unwrap();
    assert!(deleted);
}

#[sqlx::test]
async fn admins_edit_the_terms_and_privacy_pages(pool: PgPool) {
    let f = Forum::new(pool);
    let sam = f.member("sam", "admin", None).await;
    let ruth = f.member("ruth", "user", None).await;
    let admin = f.session(sam).await;
    let member = f.session(ruth).await;
    let index = f.page("/").await;
    assert!(index.contains("href=\"/terms\""));
    assert!(index.contains("href=\"/privacy\""));
    for path in ["/terms", "/privacy"] {
        let page = f.page(path).await;
        assert!(page.contains("hasn’t written"), "{path}");
    }
    assert_eq!(
        f.get_as("/admin/pages", Some(&member)).await.status,
        StatusCode::FORBIDDEN
    );
    let response = f
        .submit(
            "/admin/pages",
            &[
                ("terms", "Be **kind**."),
                ("privacy", "We keep <script>x</script> little."),
            ],
            Some(&member),
        )
        .await;
    assert_eq!(response.status, StatusCode::FORBIDDEN);

    let page = f.page_as("/admin/pages", &admin).await;
    assert!(page.contains("name=\"terms\""));
    let response = f
        .submit(
            "/admin/pages",
            &[
                ("terms", "Be **kind**."),
                ("privacy", "We keep <script>x</script> little."),
            ],
            Some(&admin),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    // The cached pages change straight away.
    let terms = f.page("/terms").await;
    assert!(terms.contains("Be <strong>kind</strong>."));
    let privacy = f.page("/privacy").await;
    assert!(!privacy.contains("<script>x"));
    assert!(privacy.contains("little."));
    assert!(
        f.page_as("/admin/pages", &admin)
            .await
            .contains("Be **kind**.")
    );
}

/// After deletion, nothing that identified the member is anywhere in the
/// database except in what they posted.
#[sqlx::test]
async fn deletion_leaves_nothing_identifying(pool: PgPool) {
    let s = setup(pool).await;
    s.f.exec(&format!(
        "UPDATE users SET username = 'zqruth', display_name = 'Zq Display', email = 'zqmail@example.com',
                title = 'Zq title', pronouns = 'zq/zq', status_line = 'Zq status', location = 'Zq town'
         WHERE id = {}",
        s.ruth
    ))
    .await;
    s.f.exec(&format!(
        "INSERT INTO user_ips (user_id, ip) VALUES ({0}, '10.9.9.9');
         INSERT INTO blocks (user_id, blocked_user_id) VALUES ({0}, {1});
         INSERT INTO user_notes (user_id, author_id, body) VALUES ({0}, {1}, 'Zq note');
         INSERT INTO login_tokens (token_hash, user_id, purpose, email, expires_at)
             VALUES ('\\x01', {0}, 'verify_email', 'zqnew@example.com', now() + interval '1 day')",
        s.ruth, s.jonah
    ))
    .await;
    s.f.thread(s.public, s.ruth, "A title", "Post text stays.")
        .await;
    let response =
        s.f.submit(
            "/settings/delete",
            &[("password", PASSWORD), ("confirm", "zqruth")],
            Some(&s.ruth_cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT table_name::text FROM information_schema.tables
         WHERE table_schema = 'public' AND table_type = 'BASE TABLE'
           AND table_name NOT IN ('posts', 'post_revisions', '_sqlx_migrations')",
    )
    .fetch_all(&s.f.db)
    .await
    .unwrap();
    for table in tables {
        let rows: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT t::text FROM {table} t"
        )))
        .fetch_all(&s.f.db)
        .await
        .unwrap();
        for row in rows {
            assert!(!row.to_lowercase().contains("zq"), "{table}: {row}");
            assert!(!row.contains("10.9.9.9"), "{table}: {row}");
        }
    }
}
