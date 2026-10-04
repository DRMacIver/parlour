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
    assert!(summary.spaces >= 6, "{summary:?}");
    assert!(summary.threads >= 40, "{summary:?}");
    assert!(summary.posts >= 500, "{summary:?}");

    let f = Forum::new(pool.clone());
    let index = f.page("/").await;
    assert!(index.contains("The Tuesday Club"));
    assert!(index.contains("Community") && index.contains("Hobbies"));
    // The members-only section has no spaces an anonymous visitor can
    // see, so it isn't shown at all.
    assert!(!index.contains("Members only"));

    // Every public space has threads and every thread page renders.
    let spaces: Vec<(String, String)> = sqlx::query_as("SELECT slug, visibility FROM spaces")
        .fetch_all(&pool)
        .await
        .unwrap();
    for (slug, level) in &spaces {
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
    let count = |sql: &str| {
        let pool = pool.clone();
        let sql = sqlx::AssertSqlSafe(sql.to_owned());
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
    assert!(count("SELECT count(*) FROM spaces c WHERE (SELECT count(*) FROM threads t WHERE t.space_id = c.id) > 30").await >= 1, "a space with several pages");
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
        count("SELECT count(*) FROM spaces WHERE visibility <> 'public'").await >= 2,
        "private spaces"
    );
    assert!(
        count("SELECT count(*) FROM users WHERE status_line IS NOT NULL").await >= 5,
        "post-info fields"
    );
    assert!(
        count("SELECT count(*) FROM post_revisions").await >= 3,
        "edit history"
    );
    // Every post marked as edited has an earlier version to show.
    assert_eq!(
        count(
            "SELECT count(*) FROM posts p WHERE p.edited_at IS NOT NULL
             AND NOT EXISTS (SELECT 1 FROM post_revisions r WHERE r.post_id = p.id)"
        )
        .await,
        0,
        "edited posts without history"
    );
    assert!(
        count("SELECT count(DISTINCT post_id) FROM reactions").await >= 100,
        "reactions"
    );
    assert!(
        count("SELECT count(DISTINCT emoji) FROM reactions").await >= 4,
        "kinds of reaction"
    );
    // Nobody reacts to deleted or hidden posts.
    assert_eq!(
        count(
            "SELECT count(*) FROM reactions r JOIN posts p ON p.id = r.post_id
             WHERE p.deleted_at IS NOT NULL OR p.hidden_at IS NOT NULL"
        )
        .await,
        0
    );
    assert!(
        count("SELECT count(*) FROM thread_reads").await >= 100,
        "read markers"
    );
    // Read markers never run past the end of a thread.
    assert_eq!(
        count(
            "SELECT count(*) FROM thread_reads r JOIN threads t ON t.id = r.thread_id
             WHERE r.last_read_seq > t.last_seq"
        )
        .await,
        0
    );
    // The organisers' space is read by its group, which has ordinary
    // members in it as well as staff.
    assert_eq!(
        count(
            "SELECT count(*) FROM group_members m JOIN groups g ON g.id = m.group_id
             JOIN users u ON u.id = m.user_id
             WHERE g.slug = 'organisers' AND u.role = 'user'"
        )
        .await,
        2
    );
    // Members only post in, react in and have read markers for threads
    // they can read.
    const CANT_READ: &str = "t.visibility = 'restricted' AND u.role = 'user' AND NOT EXISTS (
        SELECT 1 FROM space_grants g
        WHERE g.space_id = t.space_id AND g.verb = 'read'
          AND (g.user_ids @> ARRAY[u.id]
               OR g.group_ids && array(SELECT m.group_id FROM group_members m WHERE m.user_id = u.id)))";
    for sql in [
        "SELECT count(*) FROM thread_reads r JOIN threads t ON t.id = r.thread_id
         JOIN users u ON u.id = r.user_id",
        "SELECT count(*) FROM reactions r JOIN posts p ON p.id = r.post_id
         JOIN threads t ON t.id = p.thread_id JOIN users u ON u.id = r.user_id",
        "SELECT count(*) FROM posts p JOIN threads t ON t.id = p.thread_id
         JOIN users u ON u.id = p.author_id",
    ] {
        assert_eq!(count(&format!("{sql} WHERE {CANT_READ}")).await, 0, "{sql}");
    }
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
    for table in [
        "users",
        "layout_sections",
        "spaces",
        "groups",
        "threads",
        "posts",
    ] {
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
    let games = f.space("games", "public", None, 0).await;
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
        "layout_sections",
        "spaces",
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

/// The conversations from the Messages mockup, with an unread message and
/// notifications waiting for Priya, seen only by the people in them.
#[sqlx::test]
async fn seed_has_conversations_and_notifications(pool: PgPool) {
    let summary = seed::seed(&pool).await.unwrap();
    // The four from the mockup, and one that was reported.
    assert_eq!(summary.conversations, 5);
    let f = Forum::new(pool.clone());
    let id = |name: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, i64>("SELECT id FROM users WHERE username = $1")
                .bind(name)
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    let priya = f.session(id("priya").await).await;
    let list = f.page_as("/messages", &priya).await;
    assert!(list.contains("Tuesday 7th: Brass"), "{list}");
    assert!(list.contains("From the organisers"), "{list}");
    assert!(list.contains("1 new"), "{list}");
    assert!(list.contains("data-count=\"notifications\""), "{list}");
    let notifications = f.page_as("/notifications", &priya).await;
    assert!(
        notifications.contains("Tuesday 7th: Brass"),
        "{notifications}"
    );
    // She subscribed to the September thread, and hears of its new posts.
    let subscribed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM thread_subscriptions s JOIN threads t ON t.id = s.thread_id
         JOIN users u ON u.id = s.user_id
         WHERE u.username = 'priya' AND t.title = 'What did you play this week? (September)'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(subscribed, 1);
    assert!(
        notifications.contains("posted in <a href=\"/notifications/"),
        "{notifications}"
    );
    // Jonah is in none of them.
    let jonah = f.session(id("jonah").await).await;
    let list = f.page_as("/messages", &jonah).await;
    assert!(list.contains("No conversations yet"), "{list}");
}

/// The moderation queue from ModQueue.dc.html: a spam report, a reported
/// message, personal information and a new member's held post, with a log
/// of earlier actions.
#[sqlx::test]
async fn seed_has_things_for_the_moderators(pool: PgPool) {
    seed::seed(&pool).await.unwrap();
    let f = Forum::new(pool.clone());
    let id = |name: &'static str| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, i64>("SELECT id FROM users WHERE username = $1")
                .bind(name)
                .fetch_one(&pool)
                .await
                .unwrap()
        }
    };
    let sam = f.session(id("sam").await).await;
    let queue = f.page_as("/mod/queue", &sam).await;
    for text in [
        "Best replica watches, cheap",
        "reported by maya and kit",
        "Report on a message",
        "A conversation you aren’t in",
        "Personal information",
        "Held for approval",
        "Hello from Otley",
        "4 items need attention",
    ] {
        assert!(
            queue.contains(text),
            "{text} missing from the queue:\n{queue}"
        );
    }
    // The reported conversation stays closed to staff, who aren't in it
    // (DESIGN.md §3.9, §8), and so does every other.
    assert!(!queue.contains("why are you ignoring me"), "{queue}");
    let ruth = f.session(id("ruth").await).await;
    let reported: i64 = sqlx::query_scalar(
        "SELECT DISTINCT thread_id FROM reports r JOIN threads t ON t.id = r.thread_id
         WHERE t.visibility = 'private'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        f.get_as(&format!("/messages/{reported}"), Some(&ruth))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    let brass: i64 =
        sqlx::query_scalar("SELECT id FROM threads WHERE title = 'Tuesday 7th: Brass'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        f.get_as(&format!("/messages/{brass}"), Some(&ruth))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    // The public log shows the public actions only.
    let log = f.page("/moderation").await;
    for text in [
        "approved a post by new member",
        "as a spammer",
        "silenced",
        "moved 4 posts",
    ] {
        assert!(log.contains(text), "{text} missing from the log:\n{log}");
    }
    assert!(!log.contains("staff note"), "{log}");
    let staff_log = f.page_as("/mod/log", &sam).await;
    assert!(
        staff_log.contains("added a staff note about"),
        "{staff_log}"
    );
    // The held post isn't anywhere public.
    assert!(
        !f.page("/c/introductions")
            .await
            .contains("Hello from Otley")
    );
}

/// Deleting a seeded account retires its username; starting again from a
/// fresh seed brings the name back.
#[sqlx::test]
async fn reset_forgets_retired_usernames(pool: PgPool) {
    seed::seed(&pool).await.unwrap();
    let otto: i64 =
        sqlx::query_scalar("SELECT id FROM users WHERE role = 'user' ORDER BY id LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    let db = parlour::db::Db::new(pool.clone());
    parlour::db::users::delete_account(&db, otto).await.unwrap();
    seed::reset(&pool).await.unwrap();
    seed::seed(&pool).await.unwrap();
}
