//! Migrations that change what is already stored, run over data written
//! as the schema before them had it.

use parlour::MIGRATOR;
use sqlx::PgPool;

async fn run_to(db: &PgPool, version: i64) {
    MIGRATOR.run_to(version, db).await.unwrap();
}

async fn exec(db: &PgPool, sql: &str) {
    sqlx::raw_sql(sqlx::AssertSqlSafe(sql.to_owned()))
        .execute(db)
        .await
        .unwrap();
}

async fn pairs(db: &PgPool, sql: &str) -> Vec<(String, String)> {
    sqlx::query_as::<_, (String, String)>(sqlx::AssertSqlSafe(sql.to_owned()))
        .fetch_all(db)
        .await
        .unwrap()
}

/// Migration 0025 (DESIGN.md §9, milestone 11): watches on threads become
/// thread subscriptions, the members in a conversation are subscribed to
/// it unless they had muted it, watches on spaces go (and everyone who had
/// one is told), stored notifications take the new kinds, and the email
/// preference for watching becomes the one for subscriptions.
#[sqlx::test(migrations = false)]
async fn watches_become_thread_subscriptions(db: PgPool) {
    run_to(&db, 24).await;
    exec(
        &db,
        "INSERT INTO users (id, username, display_name, email, email_prefs, deleted_at) OVERRIDING SYSTEM VALUE VALUES
             (1, 'ruth', 'Ruth', 'ruth@example.com', '{\"replies\": false, \"watching\": false}', NULL),
             (2, 'jonah', 'Jonah', 'jonah@example.com', '{}', NULL),
             (3, 'maya', 'Maya', 'maya@example.com', '{\"watching\": true, \"mentions\": false}', NULL),
             (4, 'kit', 'Kit', 'kit@example.com', '{\"replies\": true}', NULL),
             (5, 'deleted_5', 'Deleted member', 'deleted-5@deleted.invalid', '{}', now());
         INSERT INTO spaces (id, slug, name, visibility) OVERRIDING SYSTEM VALUE VALUES
             (1, 'games', 'Games', 'public'), (2, 'lounge', 'Lounge', 'members');
         INSERT INTO threads (id, space_id, visibility, title, slug, author_id) OVERRIDING SYSTEM VALUE VALUES
             (1, 1, 'public', 'Brass', 'brass', 1),
             (2, 1, 'public', 'Heat', 'heat', 2),
             (3, NULL, 'private', 'Tuesday', 'message', 1),
             (4, 1, 'public', 'Subscribe', 'subscribe', 1);
         INSERT INTO private_thread_participants (thread_id, user_id, left_at) VALUES
             (3, 1, NULL), (3, 2, NULL), (3, 3, NULL), (3, 4, now());
         INSERT INTO posts (id, thread_id, seq, author_id, body_md, body_html) OVERRIDING SYSTEM VALUE VALUES
             (1, 1, 1, 1, 'a', 'a'), (2, 1, 2, 2, 'b', 'b'), (3, 1, 3, 3, 'c', 'c'),
             (4, 2, 1, 2, 'd', 'd'), (5, 3, 1, 1, 'e', 'e'), (6, 1, 4, 2, 'f', 'f');
         INSERT INTO watches (user_id, thread_id, space_id, level) VALUES
             (1, 1, NULL, 'watching'),
             (2, 1, NULL, 'muted'),
             (3, 2, NULL, 'normal'),
             (4, 2, NULL, 'watching'),
             (3, 3, NULL, 'muted'),
             (2, NULL, 1, 'watching'),
             (3, NULL, 2, 'muted'),
             (5, NULL, 1, 'watching');
         INSERT INTO notifications (user_id, kind, actor_id, thread_id, post_id, message) VALUES
             (1, 'reply', 2, 1, 2, NULL),
             (1, 'watched_thread', 3, 1, 3, NULL),
             (3, 'watched_space', 2, 2, 4, NULL),
             (3, 'mention', 2, 1, 6, NULL),
             (2, 'message', 1, 3, 5, NULL),
             (4, 'moderation', NULL, NULL, NULL, 'Please keep it civil.');",
    )
    .await;

    run_to(&db, 25).await;

    // Watching a thread is being subscribed to it; muting or leaving it
    // alone is not. Everyone still in a conversation is subscribed to it,
    // unless they had muted it.
    assert_eq!(
        pairs(
            &db,
            "SELECT user_id::text, thread_id::text FROM thread_subscriptions ORDER BY 1, 2"
        )
        .await,
        [("1", "1"), ("1", "3"), ("2", "3"), ("4", "2")].map(|(u, t)| (u.to_owned(), t.to_owned()))
    );
    // Watches are gone, spaces' with them.
    let watches: Option<String> = sqlx::query_scalar("SELECT to_regclass('watches')::text")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(watches, None);
    // The stored notifications are of the kinds there are now.
    assert_eq!(
        pairs(
            &db,
            "SELECT user_id::text, kind FROM notifications WHERE post_id IS NOT NULL ORDER BY post_id"
        )
        .await,
        [
            ("1", "subscription"),
            ("1", "subscription"),
            ("3", "subscription"),
            ("2", "message"),
            ("3", "mention"),
        ]
        .map(|(u, k)| (u.to_owned(), k.to_owned()))
    );
    // Everyone who watched or muted a space, and is still here, is told
    // once what has changed.
    let notices = pairs(
        &db,
        "SELECT user_id::text, message FROM notifications
         WHERE kind = 'moderation' AND user_id <> 4 ORDER BY user_id",
    )
    .await;
    assert_eq!(
        notices.iter().map(|(u, _)| u.as_str()).collect::<Vec<_>>(),
        ["2", "3"]
    );
    for (_, message) in &notices {
        for words in ["spaces", "subscribed", "threads you start"] {
            assert!(message.contains(words), "{message}");
        }
    }
    // The preference for emails about watched threads and spaces is the
    // one for subscribed threads now; replies to your posts have no
    // preference of their own any more.
    assert_eq!(
        pairs(
            &db,
            "SELECT id::text, email_prefs::text FROM users ORDER BY id"
        )
        .await,
        [
            ("1", r#"{"subscriptions": false}"#),
            ("2", "{}"),
            ("3", r#"{"mentions": false, "subscriptions": true}"#),
            ("4", "{}"),
            ("5", "{}"),
        ]
        .map(|(u, p)| (u.to_owned(), p.to_owned()))
    );
    // A thread's slug can't be one of its routes.
    let slug: String = sqlx::query_scalar("SELECT slug FROM threads WHERE id = 4")
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(slug, "subscribe-thread");
    // And the old kinds can't come back.
    let refused = sqlx::raw_sql(
        "INSERT INTO notifications (user_id, kind, actor_id, thread_id, post_id)
         VALUES (2, 'reply', 1, 1, 1)",
    )
    .execute(&db)
    .await;
    assert!(refused.is_err());
}

/// Migration 0026 (DESIGN.md §3.3, milestone 12): every member who has an
/// account gets an empty list called Friends, site groups stay as they
/// were, and from then on every new account gets one too.
#[sqlx::test(migrations = false)]
async fn every_member_gets_a_friends_list(db: PgPool) {
    run_to(&db, 25).await;
    exec(
        &db,
        "INSERT INTO users (id, username, display_name, email, deleted_at) OVERRIDING SYSTEM VALUE VALUES
             (1, 'ruth', 'Ruth', 'ruth@example.com', NULL),
             (2, 'jonah', 'Jonah', 'jonah@example.com', NULL),
             (3, 'deleted_3', 'Deleted member', 'deleted-3@deleted.invalid', now());
         INSERT INTO groups (slug, name) VALUES ('organisers', 'Organisers');
         INSERT INTO group_members (group_id, user_id) VALUES (1, 2);",
    )
    .await;

    run_to(&db, 26).await;

    assert_eq!(
        pairs(
            &db,
            "SELECT coalesce(owner_user_id::text, '-'), name FROM groups ORDER BY id"
        )
        .await,
        [
            ("-".to_owned(), "Organisers".to_owned()),
            ("1".to_owned(), "Friends".to_owned()),
            ("2".to_owned(), "Friends".to_owned()),
        ]
    );
    assert_eq!(
        pairs(
            &db,
            "SELECT group_id::text, user_id::text FROM group_members ORDER BY 1, 2"
        )
        .await,
        [("1".to_owned(), "2".to_owned())]
    );
    exec(
        &db,
        "INSERT INTO users (id, username, display_name, email) OVERRIDING SYSTEM VALUE VALUES
             (4, 'maya', 'Maya', 'maya@example.com')",
    )
    .await;
    assert_eq!(
        pairs(
            &db,
            "SELECT owner_user_id::text, name FROM groups WHERE owner_user_id = 4"
        )
        .await,
        [("4".to_owned(), "Friends".to_owned())]
    );
}
