//! The guard against starting on the wrong database (DESIGN.md §10): once
//! a forum has started, the uploads directory holds its id, and the server
//! refuses to start on a database that doesn't have that id, such as an
//! empty one left by a Postgres volume mix-up. Otherwise the first person
//! to see the log could take the forum over through `/setup`.

use std::path::PathBuf;

use parlour::{MIGRATOR, startup};
use sqlx::PgPool;

/// A new empty directory for this test's uploads.
fn uploads_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "parlour-startup-test-{}",
        parlour::tokens::random_token(12)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

async fn start(db: &PgPool, dir: &std::path::Path) -> anyhow::Result<()> {
    startup::check(db, dir).await?;
    MIGRATOR.run(db).await?;
    startup::record(db, dir).await
}

#[sqlx::test(migrations = false)]
async fn a_forum_starts_again_on_its_own_database(db: PgPool) {
    let dir = uploads_dir();
    start(&db, &dir).await.unwrap();
    let marker = std::fs::read_to_string(dir.join(startup::MARKER_FILE)).unwrap();
    assert!(!marker.trim().is_empty());
    start(&db, &dir).await.unwrap();
    start(&db, &dir).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.join(startup::MARKER_FILE)).unwrap(),
        marker
    );
}

#[sqlx::test(migrations = false)]
async fn a_forum_refuses_to_start_on_an_empty_database(db: PgPool) {
    let dir = uploads_dir();
    std::fs::write(dir.join(startup::MARKER_FILE), "an-earlier-forum\n").unwrap();
    let err = start(&db, &dir).await.unwrap_err().to_string();
    assert!(err.contains("empty"), "{err}");
    assert!(
        err.contains(&dir.join(startup::MARKER_FILE).display().to_string()),
        "{err}"
    );
    // Nothing was done to the database: it is still empty, so a backup
    // can be restored into it.
    let tables: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM information_schema.tables WHERE table_schema = 'public'",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(tables, 0);
}

#[sqlx::test(migrations = false)]
async fn a_forum_refuses_to_start_on_another_forums_database(db: PgPool) {
    let ours = uploads_dir();
    start(&db, &ours).await.unwrap();
    let theirs = uploads_dir();
    std::fs::write(theirs.join(startup::MARKER_FILE), "another-forum\n").unwrap();
    let err = start(&db, &theirs).await.unwrap_err().to_string();
    assert!(err.contains("different forum"), "{err}");
    // Ours still starts.
    start(&db, &ours).await.unwrap();
}

#[sqlx::test(migrations = false)]
async fn removing_the_marker_allows_a_fresh_start(db: PgPool) {
    let dir = uploads_dir();
    std::fs::write(dir.join(startup::MARKER_FILE), "an-earlier-forum\n").unwrap();
    assert!(start(&db, &dir).await.is_err());
    std::fs::remove_file(dir.join(startup::MARKER_FILE)).unwrap();
    start(&db, &dir).await.unwrap();
    assert_ne!(
        std::fs::read_to_string(dir.join(startup::MARKER_FILE)).unwrap(),
        "an-earlier-forum\n"
    );
    start(&db, &dir).await.unwrap();
}

/// A database with data but no id isn't the one the uploads name.
#[sqlx::test]
async fn a_forum_refuses_to_start_on_a_database_without_an_id(db: PgPool) {
    let dir = uploads_dir();
    std::fs::write(dir.join(startup::MARKER_FILE), "an-earlier-forum\n").unwrap();
    let err = start(&db, &dir).await.unwrap_err().to_string();
    assert!(err.contains("different forum"), "{err}");
}

/// A forum that ran before the guard existed has data but no id: it
/// starts, and from then on is guarded.
#[sqlx::test]
async fn existing_forums_are_guarded_from_their_next_start(db: PgPool) {
    let dir = uploads_dir();
    start(&db, &dir).await.unwrap();
    assert!(dir.join(startup::MARKER_FILE).exists());
}

/// The uploads directory needn't exist yet on the very first start.
#[sqlx::test(migrations = false)]
async fn the_uploads_directory_is_made_if_missing(db: PgPool) {
    let dir = uploads_dir().join("not-yet");
    start(&db, &dir).await.unwrap();
    assert!(dir.join(startup::MARKER_FILE).exists());
}
