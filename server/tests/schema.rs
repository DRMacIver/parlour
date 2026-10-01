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
async fn thread_visibility_follows_its_category(pool: PgPool) {
    let f = Forum::new(pool);
    let ruth = f.user("ruth").await;
    let public = f.category("games", "public", None, 0).await;
    let lounge = f.category("lounge", "members", None, 1).await;
    let t = f.thread(public, ruth, "Thread", "x").await;
    assert_eq!(visibility(&f, t.thread_id).await, "public");

    f.exec(&format!("UPDATE threads SET category_id = {lounge} WHERE id = {}", t.thread_id)).await;
    assert_eq!(visibility(&f, t.thread_id).await, "members");

    f.exec(&format!("UPDATE categories SET read_level = 'groups' WHERE id = {lounge}")).await;
    assert_eq!(visibility(&f, t.thread_id).await, "groups");

    // The copy can't be set by hand.
    f.exec(&format!("UPDATE threads SET visibility = 'public' WHERE id = {}", t.thread_id)).await;
    assert_eq!(visibility(&f, t.thread_id).await, "groups");
}

#[sqlx::test]
async fn replies_must_be_in_the_same_thread(pool: PgPool) {
    let f = Forum::new(pool);
    let ruth = f.user("ruth").await;
    let games = f.category("games", "public", None, 0).await;
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
    let last_seq: i32 = sqlx::query_scalar!("SELECT last_seq FROM threads WHERE id = $1", b.thread_id)
        .fetch_one(&f.db)
        .await
        .unwrap();
    assert_eq!(last_seq, 1);
}

#[sqlx::test]
async fn posts_are_numbered_in_order(pool: PgPool) {
    let f = Forum::new(pool);
    let ruth = f.user("ruth").await;
    let games = f.category("games", "public", None, 0).await;
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
    let seqs: Vec<i32> = sqlx::query_scalar!("SELECT seq FROM posts WHERE thread_id = $1 ORDER BY seq", t.thread_id)
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
