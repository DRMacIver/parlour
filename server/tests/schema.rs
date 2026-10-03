//! Invariants the database itself enforces.

mod common;

use common::Forum;
use parlour::store::NewPost;
use sqlx::PgPool;

async fn visibility(f: &Forum, thread_id: i64) -> String {
    sqlx::query_scalar!("SELECT visibility FROM threads WHERE id = $1", thread_id)
        .fetch_one(&f.db)
        .await
        .unwrap()
}

#[sqlx::test]
async fn thread_visibility_follows_its_space(pool: PgPool) {
    let f = Forum::new(pool);
    let ruth = f.user("ruth").await;
    let public = f.space("games", "public", None, 0).await;
    let lounge = f.space("lounge", "members", None, 1).await;
    let t = f.thread(public, ruth, "Thread", "x").await;
    assert_eq!(visibility(&f, t.thread_id).await, "public");

    f.exec(&format!(
        "UPDATE threads SET space_id = {lounge} WHERE id = {}",
        t.thread_id
    ))
    .await;
    assert_eq!(visibility(&f, t.thread_id).await, "members");

    f.exec(&format!(
        "UPDATE spaces SET read_level = 'groups' WHERE id = {lounge}"
    ))
    .await;
    assert_eq!(visibility(&f, t.thread_id).await, "groups");

    // The copy can't be set by hand.
    f.exec(&format!(
        "UPDATE threads SET visibility = 'public' WHERE id = {}",
        t.thread_id
    ))
    .await;
    assert_eq!(visibility(&f, t.thread_id).await, "groups");
}

#[sqlx::test]
async fn replies_must_be_in_the_same_thread(pool: PgPool) {
    let f = Forum::new(pool);
    let ruth = f.user("ruth").await;
    let games = f.space("games", "public", None, 0).await;
    let a = f.thread(games, ruth, "A", "x").await;
    let b = f.thread(games, ruth, "B", "x").await;
    let result = parlour::store::create_post(
        &f.db,
        &NewPost {
            thread_id: b.thread_id,
            author_id: ruth,
            body_md: "x".into(),
            reply_to_post_id: Some(a.first_post_id),
            created_at: None,
        },
    )
    .await;
    assert!(result.is_err());
    // Nothing was half-written.
    let last_seq: i32 =
        sqlx::query_scalar!("SELECT last_seq FROM threads WHERE id = $1", b.thread_id)
            .fetch_one(&f.db)
            .await
            .unwrap();
    assert_eq!(last_seq, 1);
}

#[sqlx::test]
async fn posts_are_numbered_in_order(pool: PgPool) {
    let f = Forum::new(pool);
    let ruth = f.user("ruth").await;
    let games = f.space("games", "public", None, 0).await;
    let t = f.thread(games, ruth, "A", "x").await;
    let mut handles = Vec::new();
    for _ in 0..20 {
        let db = f.db.clone();
        handles.push(tokio::spawn(async move {
            parlour::store::create_post(
                &db,
                &NewPost {
                    thread_id: t.thread_id,
                    author_id: ruth,
                    body_md: "x".into(),
                    reply_to_post_id: None,
                    created_at: None,
                },
            )
            .await
            .unwrap()
        }));
    }
    for handle in handles {
        handle.await.unwrap();
    }
    let seqs: Vec<i32> = sqlx::query_scalar!(
        "SELECT seq FROM posts WHERE thread_id = $1 ORDER BY seq",
        t.thread_id
    )
    .fetch_all(&f.db)
    .await
    .unwrap();
    assert_eq!(seqs, (1..=21).collect::<Vec<i32>>());
}

#[sqlx::test]
async fn usernames_are_case_insensitively_unique(pool: PgPool) {
    let f = Forum::new(pool);
    f.user("ruth").await;
    let result = parlour::store::create_user(
        &f.db,
        &parlour::store::NewUser {
            username: "RUTH".into(),
            display_name: "Ruth".into(),
            email: "other@example.com".into(),
            ..Default::default()
        },
    )
    .await;
    assert!(result.is_err());
}

async fn read_level(f: &Forum, space_id: i64) -> String {
    sqlx::query_scalar!("SELECT read_level FROM spaces WHERE id = $1", space_id)
        .fetch_one(&f.db)
        .await
        .unwrap()
}

/// Runs `first` in an open transaction, then starts `second` on another
/// connection while that transaction is still open, then commits `first` and
/// waits for `second`. Whatever order the database serialises them in, the
/// thread's visibility must end up matching its space's read level.
async fn race(f: &Forum, first: String, second: String) {
    let mut a = f.db.begin().await.unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(first))
        .execute(&mut *a)
        .await
        .unwrap();
    let db = f.db.clone();
    let b = tokio::spawn(async move {
        sqlx::raw_sql(sqlx::AssertSqlSafe(second))
            .execute(&db)
            .await
            .unwrap();
    });
    // Give the second statement time to either finish or block on a lock.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    a.commit().await.unwrap();
    b.await.unwrap();
}

#[sqlx::test]
async fn moving_a_thread_while_its_destination_is_tightened_keeps_visibility_in_sync(pool: PgPool) {
    let f = Forum::new(pool);
    let ruth = f.user("ruth").await;
    let games = f.space("games", "public", None, 0).await;
    let dest = f.space("race-test", "public", None, 1).await;
    let t = f.thread(games, ruth, "Thread", "x").await;
    race(
        &f,
        format!(
            "UPDATE threads SET space_id = {dest} WHERE id = {}",
            t.thread_id
        ),
        format!("UPDATE spaces SET read_level = 'members' WHERE id = {dest}"),
    )
    .await;
    assert_eq!(read_level(&f, dest).await, "members");
    assert_eq!(visibility(&f, t.thread_id).await, "members");
}

#[sqlx::test]
async fn tightening_a_space_while_a_thread_moves_into_it_keeps_visibility_in_sync(pool: PgPool) {
    let f = Forum::new(pool);
    let ruth = f.user("ruth").await;
    let games = f.space("games", "public", None, 0).await;
    let dest = f.space("race-test", "public", None, 1).await;
    let t = f.thread(games, ruth, "Thread", "x").await;
    race(
        &f,
        format!("UPDATE spaces SET read_level = 'members' WHERE id = {dest}"),
        format!(
            "UPDATE threads SET space_id = {dest} WHERE id = {}",
            t.thread_id
        ),
    )
    .await;
    assert_eq!(visibility(&f, t.thread_id).await, "members");
}

#[sqlx::test]
async fn creating_a_thread_while_its_space_is_tightened_keeps_visibility_in_sync(pool: PgPool) {
    let f = Forum::new(pool);
    let ruth = f.user("ruth").await;
    let dest = f.space("race-test", "public", None, 1).await;
    race(
        &f,
        format!(
            "INSERT INTO threads (space_id, visibility, title, slug, author_id)
             VALUES ({dest}, 'public', 'Racing', 'racing', {ruth})"
        ),
        format!("UPDATE spaces SET read_level = 'members' WHERE id = {dest}"),
    )
    .await;
    let visibility: String =
        sqlx::query_scalar!("SELECT visibility FROM threads WHERE slug = 'racing'")
            .fetch_one(&f.db)
            .await
            .unwrap();
    assert_eq!(visibility, "members");
}

#[sqlx::test]
async fn a_post_with_replies_cant_move_away_from_them(pool: PgPool) {
    let f = Forum::new(pool);
    let ruth = f.user("ruth").await;
    let games = f.space("games", "public", None, 0).await;
    let a = f.thread(games, ruth, "A", "x").await;
    let b = f.thread(games, ruth, "B", "x").await;
    let target = f.post(a.thread_id, ruth, "target").await;
    let reply = f.reply(a.thread_id, ruth, "reply", Some(target)).await;

    let moved = sqlx::query!(
        "UPDATE posts SET thread_id = $1, seq = 999 WHERE id = $2",
        b.thread_id,
        target
    )
    .execute(&f.db)
    .await;
    assert!(moved.is_err(), "moved a post away from its replies");

    // Moving the reply away from its target is refused too.
    let moved = sqlx::query!(
        "UPDATE posts SET thread_id = $1, seq = 999 WHERE id = $2",
        b.thread_id,
        reply
    )
    .execute(&f.db)
    .await;
    assert!(moved.is_err(), "moved a reply away from its target");

    // Moving both together in one statement is fine (a split).
    sqlx::query!(
        "UPDATE posts SET thread_id = $1, seq = 1000 + seq WHERE id = ANY($2)",
        b.thread_id,
        &[target, reply][..]
    )
    .execute(&f.db)
    .await
    .unwrap();

    // So is clearing the link and then moving, in one transaction.
    let mut tx = f.db.begin().await.unwrap();
    sqlx::query!(
        "UPDATE posts SET reply_to_post_id = NULL WHERE id = $1",
        reply
    )
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query!(
        "UPDATE posts SET thread_id = $1, seq = 2000 WHERE id = $2",
        a.thread_id,
        target
    )
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

#[sqlx::test]
async fn thread_slugs_are_url_safe(pool: PgPool) {
    let f = Forum::new(pool);
    let ruth = f.user("ruth").await;
    let games = f.space("games", "public", None, 0).await;
    let t = f.thread(games, ruth, "Spirit Island", "x").await;
    for bad in [
        "",
        "Spirit",
        "spirit island",
        "spirit--island",
        "-x",
        "x-",
        "é",
    ] {
        let result = sqlx::query!(
            "UPDATE threads SET slug = $1 WHERE id = $2",
            bad,
            t.thread_id
        )
        .execute(&f.db)
        .await;
        assert!(result.is_err(), "{bad:?}");
    }
}
