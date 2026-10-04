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

/// Migration 0027 (DESIGN.md §3.2, milestone 13): every member whose
/// account exists gets a feed at their username, in their Feeds section
/// (made at the end of their front page if they had taken it away), and a
/// topic space already at a member's name moves aside, with a word to the
/// people who run it and to the admins. From then on every new account
/// gets a feed too.
#[sqlx::test(migrations = false)]
async fn every_member_gets_a_feed(db: PgPool) {
    run_to(&db, 26).await;
    exec(
        &db,
        "INSERT INTO users (id, username, display_name, email, role, deleted_at) OVERRIDING SYSTEM VALUE VALUES
             (1, 'ruth', 'Ruth', 'ruth@example.com', 'admin', NULL),
             (2, 'Jonah', 'Jonah', 'jonah@example.com', 'user', NULL),
             (3, 'maya', 'Maya', 'maya@example.com', 'user', NULL),
             (4, 'deleted_4', 'Deleted member', 'deleted-4@deleted.invalid', 'user', now());
         INSERT INTO spaces (id, slug, name, visibility, created_by) OVERRIDING SYSTEM VALUE VALUES
             (1, 'jonah', 'Jonah’s fans', 'members', 3),
             (2, 'maya', 'Maya', 'members', NULL),
             (3, 'maya-space', 'Maya space', 'members', NULL),
             (4, 'games', 'Games', 'public', NULL);
         INSERT INTO space_grants (space_id, verb, level, user_ids) VALUES
             (1, 'read', 'members', '{}'), (1, 'reply', 'members', '{}'),
             (1, 'start', 'members', '{}'), (1, 'administer', 'listed', '{3}'),
             (2, 'read', 'members', '{}'), (2, 'reply', 'members', '{}'),
             (2, 'start', 'members', '{}'), (2, 'administer', 'listed', '{2}'),
             (3, 'read', 'members', '{}'), (3, 'reply', 'members', '{}'),
             (3, 'start', 'members', '{}'), (3, 'administer', 'staff', '{}'),
             (4, 'read', 'public', '{}'), (4, 'reply', 'members', '{}'),
             (4, 'start', 'members', '{}'), (4, 'administer', 'staff', '{}');
         INSERT INTO layouts (id, user_id, edited_at) OVERRIDING SYSTEM VALUE VALUES
             (2, 1, NULL), (3, 2, now()), (4, 3, now());
         INSERT INTO layout_sections (id, layout_id, name, position) OVERRIDING SYSTEM VALUE VALUES
             (1, 1, 'Forum', 0), (2, 2, 'Forum', 0), (3, 2, 'Feeds', 1),
             (4, 3, 'Mine', 0), (5, 4, 'Feeds', 0), (6, 4, 'Forum', 1);
         INSERT INTO layout_section_spaces (section_id, space_id, position) VALUES
             (1, 4, 0), (2, 4, 0), (4, 4, 0), (5, 2, 0);
         SELECT setval(pg_get_serial_sequence('spaces', 'id'), 4);
         SELECT setval(pg_get_serial_sequence('layout_sections', 'id'), 6);",
    )
    .await;

    run_to(&db, 27).await;

    // A feed for each member whose account exists, at their username, in
    // lower case, named as they are, which they run and start threads in.
    assert_eq!(
        pairs(
            &db,
            "SELECT owner_user_id::text, slug || ' ' || name FROM spaces WHERE kind = 'feed' ORDER BY 1"
        )
        .await,
        [("1", "ruth Ruth"), ("2", "jonah Jonah"), ("3", "maya Maya")]
            .map(|(u, s)| (u.to_owned(), s.to_owned()))
    );
    assert_eq!(
        pairs(
            &db,
            "SELECT c.owner_user_id::text, string_agg(g.verb || ' ' || g.level || ' ' || g.user_ids::text, ', '
                                                      ORDER BY g.verb)
             FROM spaces c JOIN space_grants g ON g.space_id = c.id
             WHERE c.kind = 'feed' GROUP BY 1 ORDER BY 1"
        )
        .await,
        [1, 2, 3].map(|u| (
            u.to_string(),
            format!(
                "administer listed {{{u}}}, read members {{}}, reply members {{}}, start listed {{{u}}}"
            )
        ))
    );
    // Topic spaces at a member's name moved aside, to the first free
    // address.
    assert_eq!(
        pairs(
            &db,
            "SELECT id::text, slug FROM spaces WHERE kind = 'topic' ORDER BY id"
        )
        .await,
        [
            ("1", "jonah-space"),
            ("2", "maya-space-2"),
            ("3", "maya-space"),
            ("4", "games")
        ]
        .map(|(i, s)| (i.to_owned(), s.to_owned()))
    );
    // Those who run them, and the admins, are told where they went.
    let notices = pairs(
        &db,
        "SELECT user_id::text, message FROM notifications WHERE kind = 'moderation' ORDER BY user_id, id",
    )
    .await;
    assert_eq!(
        notices.iter().map(|(u, _)| u.as_str()).collect::<Vec<_>>(),
        ["1", "1", "2", "3"]
    );
    for (user, message) in &notices {
        assert!(message.contains("own feed"), "{message}");
        match user.as_str() {
            "2" => assert!(message.contains("/c/maya-space-2"), "{message}"),
            "3" => assert!(message.contains("/c/jonah-space"), "{message}"),
            _ => {}
        }
    }
    // Each feed is in its owner's Feeds section, which a member who had
    // taken it away gets back at the end of their front page.
    assert_eq!(
        pairs(
            &db,
            "SELECT l.user_id::text, string_agg(s.name || ':' || coalesce(x.space_id::text, '-'), ' '
                                                ORDER BY s.position, s.id, x.position)
             FROM layouts l JOIN layout_sections s ON s.layout_id = l.id
             LEFT JOIN layout_section_spaces x ON x.section_id = s.id
             WHERE l.user_id IS NOT NULL GROUP BY 1 ORDER BY 1"
        )
        .await,
        [
            ("1", "Forum:4 Feeds:5"),
            ("2", "Mine:4 Feeds:6"),
            ("3", "Feeds:2 Feeds:7 Forum:-"),
        ]
        .map(|(u, s)| (u.to_owned(), s.to_owned()))
    );
    exec(
        &db,
        "INSERT INTO users (id, username, display_name, email) OVERRIDING SYSTEM VALUE VALUES
             (5, 'kit', 'Kit', 'kit@example.com')",
    )
    .await;
    assert_eq!(
        pairs(
            &db,
            "SELECT owner_user_id::text, slug FROM spaces WHERE owner_user_id = 5"
        )
        .await,
        [("5".to_owned(), "kit".to_owned())]
    );
}

/// Migration 0028 (DESIGN.md §2, §3.2): 0027's notice that a topic space
/// moved aside named the space, so it is kept only for the people who can
/// read that space; anyone else it went to (its maker, or someone named to
/// run it, since taken off its read grant) loses it. And a feed's owner
/// holds every permission on it: grants saved before that are mended.
#[sqlx::test(migrations = false)]
async fn notices_of_moved_spaces_go_only_to_their_readers(db: PgPool) {
    run_to(&db, 26).await;
    exec(
        &db,
        "INSERT INTO users (id, username, display_name, email, role) OVERRIDING SYSTEM VALUE VALUES
             (1, 'ruth', 'Ruth', 'ruth@example.com', 'admin'),
             (2, 'plans', 'Plans', 'plans@example.com', 'user'),
             (3, 'maya', 'Maya', 'maya@example.com', 'user'),
             (4, 'jonah', 'Jonah', 'jonah@example.com', 'user'),
             (5, 'kit', 'Kit', 'kit@example.com', 'user'),
             (6, 'games', 'Games', 'games@example.com', 'user');
         INSERT INTO spaces (id, slug, name, visibility, created_by) OVERRIDING SYSTEM VALUE VALUES
             (1, 'plans', 'Secret Plans', 'restricted', 3),
             (2, 'games', 'Games', 'members', 5);
         INSERT INTO space_grants (space_id, verb, level, user_ids) VALUES
             (1, 'read', 'listed', '{4}'), (1, 'reply', 'members', '{}'),
             (1, 'start', 'members', '{}'), (1, 'administer', 'listed', '{4,5}'),
             (2, 'read', 'members', '{}'), (2, 'reply', 'members', '{}'),
             (2, 'start', 'members', '{}'), (2, 'administer', 'staff', '{}');
         SELECT setval(pg_get_serial_sequence('spaces', 'id'), 2);",
    )
    .await;
    run_to(&db, 27).await;
    // A feed whose owner left themselves out of starting threads, as the
    // store let them then, and one only staff could read.
    exec(
        &db,
        "UPDATE space_grants SET level = 'listed', user_ids = '{4}'
         WHERE verb = 'start' AND space_id = (SELECT id FROM spaces WHERE owner_user_id = 3);
         UPDATE space_grants SET level = 'staff', user_ids = '{}'
         WHERE verb = 'read' AND space_id = (SELECT id FROM spaces WHERE owner_user_id = 4);",
    )
    .await;

    run_to(&db, 28).await;

    // Maya made Secret Plans and Kit helps run it, but neither can read
    // it; Jonah can, and so can Ruth, who as an admin reads every space
    // until milestone 15. Everyone who runs Games reads it.
    let notices = pairs(
        &db,
        "SELECT user_id::text, message FROM notifications WHERE kind = 'moderation' ORDER BY user_id, id",
    )
    .await;
    assert_eq!(
        notices
            .iter()
            .map(|(u, m)| (u.as_str(), m.contains("Secret Plans")))
            .collect::<Vec<_>>(),
        [("1", true), ("1", false), ("4", true), ("5", false)]
    );
    // The owners are back in their feeds' grants.
    assert_eq!(
        pairs(
            &db,
            "SELECT c.owner_user_id::text, g.verb || ' ' || g.level || ' ' || g.user_ids::text
             FROM spaces c JOIN space_grants g ON g.space_id = c.id
             WHERE c.owner_user_id IN (3, 4) AND g.verb IN ('read', 'start') ORDER BY 1, g.verb"
        )
        .await,
        [
            ("3", "read members {}"),
            ("3", "start listed {4,3}"),
            ("4", "read listed {4}"),
            ("4", "start listed {4}"),
        ]
        .map(|(u, s)| (u.to_owned(), s.to_owned()))
    );
    // And from now on nothing can leave them out.
    let refused = sqlx::raw_sql(
        "UPDATE space_grants SET level = 'listed', user_ids = '{}'
         WHERE verb = 'reply' AND space_id = (SELECT id FROM spaces WHERE owner_user_id = 3)",
    )
    .execute(&db)
    .await;
    assert!(refused.is_err());
}
