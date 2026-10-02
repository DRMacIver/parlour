//! The post-info column (DESIGN.md §2): each member's title, pronouns,
//! status line and location, which they edit in /settings and which show
//! beside their posts and on their profile.

mod common;

use axum::http::StatusCode;
use common::{Forum, post_html};
use sqlx::PgPool;

struct Setup {
    f: Forum,
    ruth: i64,
    cookie: String,
    thread_href: String,
    post_id: i64,
}

async fn setup(pool: PgPool) -> Setup {
    let f = Forum::new(pool);
    f.no_new_members().await;
    let games = f.category("games", "public", None, 0).await;
    let ruth = f.member("ruth", "user", None).await;
    let cookie = f.session(ruth).await;
    let thread = f.thread(games, ruth, "Heat", "We played Heat.").await;
    Setup {
        thread_href: format!("/t/{}/heat", thread.thread_id),
        post_id: thread.first_post_id,
        f,
        ruth,
        cookie,
    }
}

fn profile_fields<'a>(
    name: &'a str,
    title: &'a str,
    pronouns: &'a str,
    status: &'a str,
    location: &'a str,
) -> Vec<(&'a str, &'a str)> {
    vec![
        ("display_name", name),
        ("title", title),
        ("pronouns", pronouns),
        ("status_line", status),
        ("location", location),
    ]
}

#[sqlx::test]
async fn members_edit_their_post_info_column(pool: PgPool) {
    let s = setup(pool).await;
    // Visitors have the thread page cached before the change.
    let before = s.f.page(&s.thread_href).await;
    assert!(!before.contains("Keeper of the spreadsheet"));

    let settings = s.f.page_as("/settings", &s.cookie).await;
    assert!(settings.contains("action=\"/settings/profile\""));
    let response =
        s.f.submit(
            "/settings/profile",
            &profile_fields(
                "Ruth Okafor",
                "Organiser",
                "they/them",
                "Keeper of the spreadsheet",
                "Sheffield",
            ),
            Some(&s.cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    assert_eq!(response.location(), "/settings?done=profile");
    let settings = s.f.page_as("/settings?done=profile", &s.cookie).await;
    assert!(settings.contains("Your profile has been saved."));
    assert!(settings.contains("value=\"Keeper of the spreadsheet\""));

    // The column beside their posts, served to visitors from the cache,
    // shows the new details straight away.
    let page = s.f.page(&s.thread_href).await;
    let post = post_html(&page, s.post_id);
    for text in [
        "Ruth Okafor",
        "Organiser",
        "they/them",
        "Keeper of the spreadsheet",
        "Sheffield",
    ] {
        assert!(post.contains(text), "{text} missing from {post}");
    }
    let profile = s.f.page("/u/ruth").await;
    assert!(profile.contains("Keeper of the spreadsheet"));
    assert!(profile.contains("Sheffield"));
}

#[sqlx::test]
async fn blank_fields_are_removed(pool: PgPool) {
    let s = setup(pool).await;
    for (status, title) in [("Busy", "Organiser"), ("  ", "")] {
        let response =
            s.f.submit(
                "/settings/profile",
                &profile_fields("Ruth", title, "", status, ""),
                Some(&s.cookie),
            )
            .await;
        assert_eq!(response.status, StatusCode::SEE_OTHER);
    }
    let row: (Option<String>, Option<String>, Option<String>) =
        sqlx::query_as("SELECT title, status_line, pronouns FROM users WHERE id = $1")
            .bind(s.ruth)
            .fetch_one(&s.f.db)
            .await
            .unwrap();
    assert_eq!(row, (None, None, None));
    let page = s.f.page(&s.thread_href).await;
    assert!(!post_html(&page, s.post_id).contains("author-status"));
}

#[sqlx::test]
async fn status_lines_are_short_plain_text(pool: PgPool) {
    let s = setup(pool).await;
    let long = "x".repeat(61);
    let response =
        s.f.submit(
            "/settings/profile",
            &profile_fields("Ruth", "", "", &long, "Leeds"),
            Some(&s.cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(response.body.contains("60 characters"));
    // What was typed is kept, to fix.
    assert!(response.body.contains(&format!("value=\"{long}\"")));
    assert!(response.body.contains("value=\"Leeds\""));

    // Exactly 60 is fine, and markup and links are shown as typed text.
    let status = "<a href=\"https://spam.example\">cheap</a> https://x.example";
    assert!(status.chars().count() <= 60);
    let response =
        s.f.submit(
            "/settings/profile",
            &profile_fields("Ruth", "", "", status, ""),
            Some(&s.cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    let page = s.f.page(&s.thread_href).await;
    let post = post_html(&page, s.post_id);
    assert!(!post.contains("href=\"https://spam.example\""));
    assert!(!post.contains("href=\"https://x.example\""));
    // The tag is shown as text, escaped.
    assert!(!post.contains("<a href=\"https://spam"));
    assert!(post.contains("cheap&#60;/a&#62;") || post.contains("cheap&lt;/a&gt;"));
}

#[sqlx::test]
async fn names_and_fields_are_checked(pool: PgPool) {
    let s = setup(pool).await;
    for (fields, problem) in [
        (profile_fields("", "", "", "", ""), "Enter the name"),
        (
            profile_fields("Ruth", &"t".repeat(41), "", "", ""),
            "Titles are at most 40",
        ),
        (
            profile_fields("Ruth", "", &"p".repeat(31), "", ""),
            "Pronouns are at most 30",
        ),
        (
            profile_fields("Ruth", "", "", "", &"l".repeat(41)),
            "Locations are at most 40",
        ),
    ] {
        let response =
            s.f.submit("/settings/profile", &fields, Some(&s.cookie))
                .await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "{problem}");
        assert!(response.body.contains(problem), "{problem}");
    }
}

#[sqlx::test]
async fn visitors_and_silenced_members_cannot_edit_profiles(pool: PgPool) {
    let s = setup(pool).await;
    let fields = profile_fields("Spammer", "", "", "buy now", "");
    let response = s.f.submit("/settings/profile", &fields, None).await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert!(response.location().starts_with("/login"));

    s.f.exec(&format!(
        "UPDATE users SET silenced_until = now() + interval '1 day' WHERE id = {}",
        s.ruth
    ))
    .await;
    let response =
        s.f.submit("/settings/profile", &fields, Some(&s.cookie))
            .await;
    assert_eq!(response.status, StatusCode::FORBIDDEN);
    let status: Option<String> = sqlx::query_scalar("SELECT status_line FROM users WHERE id = $1")
        .bind(s.ruth)
        .fetch_one(&s.f.db)
        .await
        .unwrap();
    assert_eq!(status, None);
}

#[sqlx::test]
async fn the_database_refuses_long_status_lines(pool: PgPool) {
    let s = setup(pool).await;
    let result = sqlx::query("UPDATE users SET status_line = $2 WHERE id = $1")
        .bind(s.ruth)
        .bind("x".repeat(61))
        .execute(&s.f.db)
        .await;
    assert!(result.is_err());
    let result = sqlx::query("UPDATE users SET status_line = $2 WHERE id = $1")
        .bind(s.ruth)
        .bind("two\nlines")
        .execute(&s.f.db)
        .await;
    assert!(result.is_err());
}

#[hegel::standalone_function(test_cases = 200)]
fn accepted_profiles_fit_the_database(
    tc: hegel::TestCase,
    rt: &tokio::runtime::Handle,
    pool: &PgPool,
    user_id: i64,
) {
    use hegel::generators as gs;
    let raw: Vec<String> = (0..5).map(|_| tc.draw(gs::text().max_size(70))).collect();
    let (fields, problems) =
        parlour::accounts::ProfileFields::clean(&raw[0], &raw[1], &raw[2], &raw[3], &raw[4]);
    if problems.is_empty() {
        rt.block_on(parlour::users::update_profile(pool, user_id, &fields))
            .expect("the database accepts every profile the form accepts");
        let saved = rt
            .block_on(parlour::users::profile_fields(pool, user_id))
            .unwrap();
        assert_eq!(saved, fields);
    }
}

/// Whatever the form accepts, the database's checks accept too, and it
/// reads back unchanged.
#[sqlx::test]
async fn every_accepted_profile_can_be_saved(pool: PgPool) {
    let f = Forum::new(pool.clone());
    let ruth = f.user("ruth").await;
    let rt = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || accepted_profiles_fit_the_database(&rt, &pool, ruth))
        .await
        .unwrap();
}

/// The settings page's title is plain text: the browser shows it in tabs,
/// history and bookmarks, and screen readers announce it.
#[sqlx::test]
async fn the_settings_page_title_is_plain_text(pool: PgPool) {
    let s = setup(pool).await;
    let settings = s.f.page_as("/settings", &s.cookie).await;
    let start = settings.find("<title>").unwrap() + "<title>".len();
    let end = start + settings[start..].find("</title>").unwrap();
    assert_eq!(&settings[start..end], "Your account · Parlour");
}
