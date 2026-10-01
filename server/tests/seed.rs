//! The `parlour seed` development tool.

mod common;

use axum::http::StatusCode;
use common::Forum;
use parlour::seed::{self, SeedError};
use sqlx::PgPool;

#[sqlx::test]
async fn seed_fills_an_empty_forum_with_browsable_content(pool: PgPool) {
    let summary = seed::seed(&pool).await.unwrap();
    assert!(summary.users >= 10, "{summary:?}");
    assert!(summary.categories >= 6, "{summary:?}");
    assert!(summary.threads >= 40, "{summary:?}");
    assert!(summary.posts >= 500, "{summary:?}");

    let f = Forum::new(pool.clone());
    let index = f.page("/").await;
    assert!(index.contains("The Tuesday Club"));
    assert!(index.contains("Community") && index.contains("Hobbies"));
    // The members-only section has no categories an anonymous visitor can
    // see, so it isn't shown at all.
    assert!(!index.contains("Members only"));

    // Every public category has threads and every thread page renders.
    let categories: Vec<(String, String)> =
        sqlx::query_as("SELECT slug, read_level FROM categories")
            .fetch_all(&pool)
            .await
            .unwrap();
    for (slug, level) in &categories {
        let response = f.get(&format!("/c/{slug}")).await;
        let expected = if level == "public" {
            StatusCode::OK
        } else {
            StatusCode::NOT_FOUND
        };
        assert_eq!(response.status, expected, "/c/{slug}");
        if level == "public" {
            assert!(
                !response.body.contains("No threads yet."),
                "/c/{slug} is empty"
            );
        }
    }
    let threads: Vec<(i64, String, i32)> = sqlx::query_as(
        "SELECT id, slug, last_seq FROM threads WHERE visibility = 'public' AND deleted_at IS NULL",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    for (id, slug, last_seq) in &threads {
        let pages = (i64::from(*last_seq) + 49) / 50;
        for page in 1..=pages {
            f.page(&parlour::pagination::page_href(
                &format!("/t/{id}/{slug}"),
                page,
            ))
            .await;
        }
    }
}

#[sqlx::test]
async fn seed_includes_the_interesting_cases(pool: PgPool) {
    seed::seed(&pool).await.unwrap();
    let count = |sql: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, i64>(sql)
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    assert!(
        count("SELECT count(*) FROM threads WHERE last_seq > 100").await >= 2,
        "long threads"
    );
    assert!(count("SELECT count(*) FROM categories c WHERE (SELECT count(*) FROM threads t WHERE t.category_id = c.id) > 30").await >= 1, "a category with several pages");
    assert!(
        count("SELECT count(*) FROM threads WHERE pinned").await >= 2,
        "pinned threads"
    );
    assert!(
        count("SELECT count(*) FROM threads WHERE locked").await >= 1,
        "locked threads"
    );
    assert!(
        count("SELECT count(*) FROM threads WHERE forked_from_post_id IS NOT NULL").await >= 1,
        "quote-forks"
    );
    assert!(
        count("SELECT count(*) FROM posts WHERE reply_to_post_id IS NOT NULL").await >= 20,
        "replies"
    );
    assert!(
        count("SELECT count(*) FROM posts WHERE deleted_at IS NOT NULL").await >= 1,
        "deleted posts"
    );
    assert!(
        count("SELECT count(*) FROM posts WHERE hidden_at IS NOT NULL").await >= 1,
        "hidden posts"
    );
    assert!(
        count("SELECT count(*) FROM posts WHERE edited_at IS NOT NULL").await >= 1,
        "edited posts"
    );
    assert!(
        count("SELECT count(*) FROM categories WHERE read_level <> 'public'").await >= 2,
        "private categories"
    );
    assert!(
        count("SELECT count(*) FROM users WHERE status_line IS NOT NULL").await >= 5,
        "post-info fields"
    );
}

#[sqlx::test]
async fn seed_is_deterministic(pool: PgPool) {
    seed::seed(&pool).await.unwrap();
    let snapshot = |pool: PgPool| async move {
        sqlx::query_scalar::<_, String>("SELECT string_agg(body_md, '|' ORDER BY id) FROM posts")
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    let first = snapshot(pool.clone()).await;
    seed::reset(&pool).await.unwrap();
    seed::seed(&pool).await.unwrap();
    assert_eq!(first, snapshot(pool).await);
}

#[sqlx::test]
async fn seed_refuses_to_touch_a_forum_with_content(pool: PgPool) {
    seed::seed(&pool).await.unwrap();
    assert!(matches!(seed::seed(&pool).await, Err(SeedError::NotEmpty)));
}

#[sqlx::test]
async fn reset_empties_the_content_tables(pool: PgPool) {
    seed::seed(&pool).await.unwrap();
    seed::reset(&pool).await.unwrap();
    for table in ["users", "sections", "categories", "threads", "posts"] {
        let n: i64 =
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(n, 0, "{table}");
    }
}

async fn count(pool: &PgPool, table: &str) -> i64 {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
        .fetch_one(pool)
        .await
        .unwrap()
}

#[sqlx::test]
async fn reset_refuses_a_database_seed_did_not_create(pool: PgPool) {
    let f = Forum::new(pool.clone());
    let ruth = f.user("ruth").await;
    let games = f.category("games", "public", None, 0).await;
    f.thread(games, ruth, "Real content", "x").await;
    assert!(matches!(
        seed::reset(&pool).await,
        Err(SeedError::NotSeeded)
    ));
    assert_eq!(count(&pool, "users").await, 1);
    assert_eq!(count(&pool, "posts").await, 1);
}

#[sqlx::test]
async fn reset_refuses_a_seeded_database_that_has_gained_content(pool: PgPool) {
    seed::seed(&pool).await.unwrap();
    let posts = count(&pool, "posts").await;
    let f = Forum::new(pool.clone());
    let ruth = f.user("newcomer").await;
    let thread: i64 = sqlx::query_scalar("SELECT min(id) FROM threads")
        .fetch_one(&pool)
        .await
        .unwrap();
    f.post(thread, ruth, "A real post").await;
    assert!(matches!(
        seed::reset(&pool).await,
        Err(SeedError::ChangedSinceSeed)
    ));
    assert_eq!(count(&pool, "posts").await, posts + 1);
}

#[sqlx::test]
async fn reset_accepts_an_empty_database(pool: PgPool) {
    seed::reset(&pool).await.unwrap();
    seed::seed(&pool).await.unwrap();
}

#[sqlx::test]
async fn a_failed_seed_leaves_nothing_behind(pool: PgPool) {
    // Make the seed fail part-way through, once the long threads are being
    // written.
    sqlx::raw_sql("ALTER TABLE posts ADD CONSTRAINT boom CHECK (seq < 30)")
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        seed::seed(&pool).await,
        Err(SeedError::Database(_))
    ));
    for table in [
        "users",
        "sections",
        "categories",
        "threads",
        "posts",
        "settings",
    ] {
        assert_eq!(count(&pool, table).await, 0, "{table}");
    }
    sqlx::raw_sql("ALTER TABLE posts DROP CONSTRAINT boom")
        .execute(&pool)
        .await
        .unwrap();
    seed::seed(&pool).await.unwrap();
}

#[sqlx::test]
async fn seeded_members_log_in_with_the_development_password(pool: PgPool) {
    seed::seed(&pool).await.unwrap();
    let f = Forum::new(pool);
    for (username, role_badge) in [("sam", Some("Admin")), ("priya", None)] {
        let response = f
            .submit(
                "/login",
                &[("identifier", username), ("password", seed::DEV_PASSWORD)],
                None,
            )
            .await;
        assert_eq!(response.status, StatusCode::SEE_OTHER, "{username}");
        let cookie = common::cookie_set_by(&response).unwrap();
        let settings = f.page_as("/settings", &cookie).await;
        assert!(settings.contains("Confirmed"), "{username}");
        if let Some(badge) = role_badge {
            assert!(settings.contains(badge));
        }
        // Members see the members-only section.
        assert!(f.page_as("/", &cookie).await.contains("Members only"));
    }
}

#[sqlx::test]
async fn the_seeded_invite_tree_has_everyone(pool: PgPool) {
    let summary = seed::seed(&pool).await.unwrap();
    let f = Forum::new(pool);
    let tree = f.page("/users/tree").await;
    assert!(tree.contains(&format!("{} members.", summary.users)));
    let profile = f.page("/u/priya").await;
    assert!(profile.contains("href=\"/u/ruth\">Ruth</a>"));
}
