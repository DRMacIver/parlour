//! Work done per request: how many database queries each kind of page
//! makes, and that per-author post counts don't grow with how much the
//! authors have posted.

mod common;

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::http::StatusCode;
use common::Forum;
use sqlx::PgPool;
use tracing_subscriber::prelude::*;

/// Counts the queries sqlx runs on this thread, other than its lookups of
/// type details in the catalog, which it makes once per connection and
/// caches: those depend on which pooled connection a request gets, not on
/// what the page does.
struct CountQueries(Arc<AtomicUsize>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CountQueries {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        if event.metadata().target() != "sqlx::query" {
            return;
        }
        struct Statement(String);
        impl tracing::field::Visit for Statement {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "db.statement" {
                    self.0 = format!("{value:?}");
                }
            }
        }
        let mut statement = Statement(String::new());
        event.record(&mut statement);
        if !statement.0.contains("FROM pg_catalog.pg_type") && !statement.0.contains("pg_type.oid")
        {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
}

/// How many queries `f` runs. `#[sqlx::test]` runs on a single-threaded
/// runtime, so a subscriber for this thread sees every query the request
/// makes and none from other tests.
async fn queries<T>(f: impl Future<Output = T>) -> (usize, T) {
    let count = Arc::new(AtomicUsize::new(0));
    let subscriber = tracing_subscriber::registry().with(CountQueries(count.clone()));
    let _guard = tracing::subscriber::set_default(subscriber);
    let out = f.await;
    (count.load(Ordering::SeqCst), out)
}

struct Seeded {
    forum: Forum,
    /// The longest thread in a public space.
    thread_path: String,
}

async fn seeded(pool: PgPool) -> Seeded {
    parlour::seed::seed(&pool).await.unwrap();
    let (id, slug): (i64, String) = sqlx::query_as(
        "SELECT t.id, t.slug FROM threads t JOIN spaces c ON c.id = t.space_id
         WHERE c.read_level = 'public' AND t.deleted_at IS NULL
         ORDER BY t.last_seq DESC LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    Seeded {
        forum: Forum::uncached(pool),
        thread_path: format!("/t/{id}/{slug}"),
    }
}

/// The number of queries a page makes for `username` (or a visitor), once
/// any first-visit work (such as recording when a session was last seen)
/// is out of the way.
async fn page_queries(forum: &Forum, path: &str, username: Option<&str>) -> usize {
    let cookie = match username {
        Some(name) => Some(forum.session(forum.user_id(name).await).await),
        None => None,
    };
    let first = forum.get_as(path, cookie.as_deref()).await;
    assert_eq!(first.status, StatusCode::OK, "{path}");
    let (n, response) = queries(forum.get_as(path, cookie.as_deref())).await;
    assert_eq!(response.status, StatusCode::OK, "{path}");
    n
}

#[sqlx::test]
async fn visitors_pages_make_few_queries(pool: PgPool) {
    let s = seeded(pool).await;
    // Site settings, then the page's content.
    assert!(page_queries(&s.forum, "/", None).await <= 4);
    assert!(page_queries(&s.forum, "/latest", None).await <= 3);
    assert!(page_queries(&s.forum, &s.thread_path, None).await <= 8);
}

#[sqlx::test]
async fn members_pages_make_few_queries(pool: PgPool) {
    let s = seeded(pool).await;
    for who in ["priya", "sam"] {
        // One query for the session and the member, one for everything in
        // the page header, then the page's content.
        let index = page_queries(&s.forum, "/", Some(who)).await;
        assert!(index <= 5, "{who}: the index made {index} queries");
        let latest = page_queries(&s.forum, "/latest", Some(who)).await;
        assert!(latest <= 5, "{who}: /latest made {latest} queries");
        // The thread page also advances the read marker and marks the
        // member's notifications about it read, in one statement.
        let thread = page_queries(&s.forum, &s.thread_path, Some(who)).await;
        assert!(thread <= 11, "{who}: the thread page made {thread} queries");
    }
}

/// Thread pages and profiles show how many posts each author has that the
/// viewer can see. That number comes from per-space counts kept up to
/// date as posts change, not from counting the author's posts on every
/// page, which would cost more the more they have posted.
#[sqlx::test]
async fn author_post_counts_come_from_maintained_counts(pool: PgPool) {
    let s = seeded(pool).await;
    let author: (i64, String) = sqlx::query_as(
        "SELECT u.id, u.username::text FROM posts p JOIN users u ON u.id = p.author_id
         WHERE p.thread_id = $1 ORDER BY p.seq LIMIT 1",
    )
    .bind(
        s.thread_path
            .split('/')
            .nth(2)
            .unwrap()
            .parse::<i64>()
            .unwrap(),
    )
    .fetch_one(&s.forum.db)
    .await
    .unwrap();
    sqlx::query("DELETE FROM user_space_post_counts WHERE user_id = $1")
        .bind(author.0)
        .execute(&s.forum.db)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO user_space_post_counts (user_id, space_id, posts)
         SELECT $1, min(c.id), 98765 FROM spaces c WHERE c.read_level = 'public'",
    )
    .bind(author.0)
    .execute(&s.forum.db)
    .await
    .unwrap();
    let thread = s.forum.get(&s.thread_path).await;
    assert!(thread.body.contains("98,765 posts"), "{}", thread.body);
    let profile = s.forum.get(&format!("/u/{}", author.1)).await;
    assert!(profile.body.contains("98,765"), "{}", profile.body);
}
