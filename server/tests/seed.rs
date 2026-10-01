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
            f.page(&format!("/t/{id}/{slug}?page={page}")).await;
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
