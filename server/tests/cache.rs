//! The anonymous page cache over HTTP: headers, conditional requests, who
//! gets cached pages, and invalidation after each kind of change.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::Forum;
use parlour::changes::Change;
use parlour::http_cache::{PRIVATE_CACHE_CONTROL, PUBLIC_CACHE_CONTROL};
use parlour::permissions::Grant;
use parlour::store::NewThread;
use sqlx::PgPool;

struct Setup {
    f: Forum,
    ruth: i64,
    jonah: i64,
    games: i64,
    books: i64,
    thread: parlour::store::CreatedThread,
    path: String,
}

async fn setup(pool: PgPool) -> Setup {
    let f = Forum::new(pool);
    let ruth = f.user("ruth").await;
    let jonah = f.user("jonah").await;
    let games = f.space("games", "public", None, 0).await;
    let books = f.space("books", "public", None, 1).await;
    let thread = f
        .thread(games, ruth, "Spirit Island", "Original words")
        .await;
    let path = format!("/t/{}/spirit-island", thread.thread_id);
    Setup {
        f,
        ruth,
        jonah,
        games,
        books,
        thread,
        path,
    }
}

#[sqlx::test]
async fn anonymous_pages_are_publicly_cacheable_with_an_etag(pool: PgPool) {
    let s = setup(pool).await;
    for path in ["/", "/latest", "/c/games", &s.path] {
        let response = s.f.get(path).await;
        assert_eq!(response.status, StatusCode::OK, "{path}");
        assert_eq!(
            response.header(header::CACHE_CONTROL),
            Some(PUBLIC_CACHE_CONTROL),
            "{path}"
        );
        let etag = response.header(header::ETAG).expect("etag");
        assert!(etag.starts_with('"') && etag.ends_with('"'));
        assert!(
            response
                .header(header::CONTENT_TYPE)
                .unwrap()
                .starts_with("text/html")
        );
        assert!(!response.headers.contains_key(header::SET_COOKIE));
        assert!(!response.headers.contains_key(header::VARY));
    }
}

#[sqlx::test]
async fn repeated_requests_are_served_from_memory(pool: PgPool) {
    let s = setup(pool).await;
    let first = s.f.get(&s.path).await;
    assert_eq!(s.f.state.cache().len(), 1);
    // Change the database behind the cache's back: the cached page is
    // served, proving it didn't touch the database.
    s.f.exec(&format!(
        "UPDATE posts SET body_html = '<p>Sneaky</p>' WHERE id = {}",
        s.thread.first_post_id
    ))
    .await;
    let second = s.f.get(&s.path).await;
    assert_eq!(first.body, second.body);
    assert_eq!(first.header(header::ETAG), second.header(header::ETAG));
    assert!(second.body.contains("Original words"));
}

#[sqlx::test]
async fn query_strings_are_part_of_the_key(pool: PgPool) {
    let s = setup(pool).await;
    for _ in 0..60 {
        s.f.post(s.thread.thread_id, s.ruth, "more").await;
    }
    let page1 = s.f.page(&s.path).await;
    let page2 = s.f.page(&format!("{}?page=2", s.path)).await;
    assert_ne!(page1, page2);
    assert_eq!(s.f.state.cache().len(), 2);
}

#[sqlx::test]
async fn conditional_requests_get_304(pool: PgPool) {
    let s = setup(pool).await;
    // Both on a miss and on a hit.
    for _ in 0..2 {
        let etag =
            s.f.get(&s.path)
                .await
                .header(header::ETAG)
                .unwrap()
                .to_owned();
        for candidate in [
            etag.clone(),
            format!("W/{etag}"),
            format!("\"nope\", {etag}"),
            "*".to_owned(),
        ] {
            let response =
                s.f.request(
                    Request::get(&s.path)
                        .header(header::IF_NONE_MATCH, &candidate)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await;
            assert_eq!(response.status, StatusCode::NOT_MODIFIED, "{candidate}");
            assert!(response.body.is_empty());
            assert_eq!(response.header(header::ETAG), Some(etag.as_str()));
            assert_eq!(
                response.header(header::CACHE_CONTROL),
                Some(PUBLIC_CACHE_CONTROL)
            );
        }
        let response =
            s.f.request(
                Request::get(&s.path)
                    .header(header::IF_NONE_MATCH, "\"stale\"")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
        assert_eq!(response.status, StatusCode::OK);
        assert!(response.body.contains("Original words"));
    }
}

#[sqlx::test]
async fn conditional_request_on_a_cold_cache_gets_304(pool: PgPool) {
    let s = setup(pool).await;
    let etag =
        s.f.get(&s.path)
            .await
            .header(header::ETAG)
            .unwrap()
            .to_owned();
    s.f.state.cache().invalidate(&[parlour::cache::Tag::Site]);
    let response =
        s.f.request(
            Request::get(&s.path)
                .header(header::IF_NONE_MATCH, &etag)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(response.status, StatusCode::NOT_MODIFIED);
}

#[sqlx::test]
async fn etag_changes_when_the_page_does(pool: PgPool) {
    let s = setup(pool).await;
    let before =
        s.f.get(&s.path)
            .await
            .header(header::ETAG)
            .unwrap()
            .to_owned();
    let post = s.f.post(s.thread.thread_id, s.jonah, "A reply").await;
    s.f.changed(Change::PostCreated { post_id: post }).await;
    let after = s.f.get(&s.path).await;
    assert_ne!(after.header(header::ETAG).unwrap(), before);
    let response =
        s.f.request(
            Request::get(&s.path)
                .header(header::IF_NONE_MATCH, &before)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(response.status, StatusCode::OK);
}

#[sqlx::test]
async fn head_requests_are_answered_from_the_cache(pool: PgPool) {
    let s = setup(pool).await;
    let head =
        s.f.request(Request::head(&s.path).body(Body::empty()).unwrap())
            .await;
    assert_eq!(head.status, StatusCode::OK);
    assert!(head.body.is_empty());
    let get = s.f.get(&s.path).await;
    let head =
        s.f.request(Request::head(&s.path).body(Body::empty()).unwrap())
            .await;
    assert_eq!(head.status, StatusCode::OK);
    assert!(head.body.is_empty());
    assert_eq!(head.header(header::ETAG), get.header(header::ETAG));
    assert_eq!(
        head.header(header::CACHE_CONTROL),
        Some(PUBLIC_CACHE_CONTROL)
    );
}

/// RFC 9110 §9.3.2: a HEAD response's Content-Length, if sent, is the length
/// the GET response would have. Checked on a cache miss and a hit.
#[sqlx::test]
async fn head_responses_carry_the_get_content_length(pool: PgPool) {
    let s = setup(pool).await;
    let head = || {
        s.f.request(Request::head(&s.path).body(Body::empty()).unwrap())
    };
    let miss = head().await;
    let get = s.f.get(&s.path).await;
    let hit = head().await;
    let length = get.body.len().to_string();
    assert_eq!(get.header(header::CONTENT_LENGTH), Some(length.as_str()));
    for response in [miss, hit] {
        assert!(response.body.is_empty());
        assert_eq!(
            response.header(header::CONTENT_LENGTH),
            Some(length.as_str())
        );
    }
}

#[sqlx::test]
async fn requests_with_a_session_cookie_bypass_the_cache(pool: PgPool) {
    let s = setup(pool).await;
    s.f.get(&s.path).await;
    s.f.exec(&format!(
        "UPDATE posts SET body_html = '<p>Fresh words</p>' WHERE id = {}",
        s.thread.first_post_id
    ))
    .await;
    let response =
        s.f.request(
            Request::get(&s.path)
                .header(header::COOKIE, "theme=dark; parlour_session=whatever")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(response.status, StatusCode::OK);
    assert!(response.body.contains("Fresh words"));
    assert_eq!(
        response.header(header::CACHE_CONTROL),
        Some(PRIVATE_CACHE_CONTROL)
    );
    assert!(!response.headers.contains_key(header::ETAG));
    // Other cookies don't matter.
    let response =
        s.f.request(
            Request::get(&s.path)
                .header(header::COOKIE, "theme=dark")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert!(response.body.contains("Original words"));
    assert_eq!(
        response.header(header::CACHE_CONTROL),
        Some(PUBLIC_CACHE_CONTROL)
    );
}

#[sqlx::test]
async fn session_requests_never_fill_the_cache(pool: PgPool) {
    let s = setup(pool).await;
    s.f.request(
        Request::get(&s.path)
            .header(header::COOKIE, "parlour_session=x")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert!(s.f.state.cache().is_empty());
}

#[sqlx::test]
async fn other_methods_and_responses_are_private(pool: PgPool) {
    let s = setup(pool).await;
    for (request, status) in [
        (
            Request::post(&s.path)
                .header(header::ORIGIN, "http://localhost:8080")
                .body(Body::empty())
                .unwrap(),
            StatusCode::METHOD_NOT_ALLOWED,
        ),
        // From another site, any form post is refused.
        (
            Request::post(&s.path).body(Body::empty()).unwrap(),
            StatusCode::FORBIDDEN,
        ),
        (
            Request::get("/no-such-page").body(Body::empty()).unwrap(),
            StatusCode::NOT_FOUND,
        ),
        (
            Request::get("/c/nope").body(Body::empty()).unwrap(),
            StatusCode::NOT_FOUND,
        ),
        (
            Request::get(format!("/t/{}/wrong-slug", s.thread.thread_id))
                .body(Body::empty())
                .unwrap(),
            StatusCode::PERMANENT_REDIRECT,
        ),
        (
            Request::get(format!("/p/{}", s.thread.first_post_id))
                .body(Body::empty())
                .unwrap(),
            StatusCode::TEMPORARY_REDIRECT,
        ),
        (
            Request::get("/healthz").body(Body::empty()).unwrap(),
            StatusCode::OK,
        ),
    ] {
        let uri = request.uri().clone();
        let response = s.f.request(request).await;
        assert_eq!(response.status, status, "{uri}");
        assert_eq!(
            response.header(header::CACHE_CONTROL),
            Some(PRIVATE_CACHE_CONTROL),
            "{uri}"
        );
        assert!(!response.headers.contains_key(header::ETAG), "{uri}");
        assert!(!response.headers.contains_key(header::SET_COOKIE), "{uri}");
    }
    assert!(s.f.state.cache().is_empty());
}

#[sqlx::test]
async fn static_assets_keep_their_own_caching(pool: PgPool) {
    let s = setup(pool).await;
    let css = s.f.get("/static/parlour.css").await;
    assert_eq!(
        css.header(header::CACHE_CONTROL),
        Some("public, max-age=3600")
    );
    assert!(s.f.state.cache().is_empty());
}

// ---------------------------------------------------------------------------
// Invalidation. Each test warms the cache with every page, makes a change
// directly in the database, reports it, and checks which pages now show it.

const PAGES: [&str; 5] = ["/", "/latest", "/c/games", "/c/books", "THREAD"];

impl Setup {
    async fn warm(&self, extra: &[&str]) {
        for path in PAGES.iter().chain(extra) {
            self.f.page(&self.resolve(path)).await;
        }
    }

    fn resolve(&self, path: &str) -> String {
        if path == "THREAD" {
            self.path.clone()
        } else {
            path.to_owned()
        }
    }

    /// Asserts which of `paths` currently contain `needle`.
    async fn assert_shown(&self, needle: &str, shown: &[&str], not_shown: &[&str]) {
        for path in shown {
            let path = self.resolve(path);
            assert!(
                self.f.page(&path).await.contains(needle),
                "{needle:?} not on {path}"
            );
        }
        for path in not_shown {
            let path = self.resolve(path);
            assert!(
                !self.f.page(&path).await.contains(needle),
                "{needle:?} on {path}"
            );
        }
    }
}

#[sqlx::test]
async fn new_post_clears_its_thread_space_and_lists(pool: PgPool) {
    let s = setup(pool).await;
    s.f.thread(s.books, s.ruth, "Unrelated", "x").await;
    s.warm(&[]).await;
    sqlx::query!(
        "UPDATE users SET display_name = 'Jonah the Bold' WHERE id = $1",
        s.jonah
    )
    .execute(&s.f.db)
    .await
    .unwrap();
    let post =
        s.f.post(s.thread.thread_id, s.jonah, "A brand new reply")
            .await;
    // Not yet reported: everything is still cached.
    s.assert_shown("A brand new reply", &[], &["THREAD"]).await;
    s.f.changed(Change::PostCreated { post_id: post }).await;
    s.assert_shown("A brand new reply", &["THREAD"], &[]).await;
    // The last poster shows up in lists of the thread's space.
    s.assert_shown(
        "Jonah the Bold",
        &["/", "/latest", "/c/games", "THREAD"],
        &["/c/books"],
    )
    .await;
}

#[sqlx::test]
async fn new_post_updates_the_authors_post_count_on_other_threads(pool: PgPool) {
    let s = setup(pool).await;
    let other = s.f.thread(s.books, s.ruth, "Other", "x").await;
    s.warm(&[]).await;
    assert!(s.f.page(&s.path).await.contains("2 posts"));
    let post = s.f.post(other.thread_id, s.ruth, "and another").await;
    s.f.changed(Change::PostCreated { post_id: post }).await;
    assert!(s.f.page(&s.path).await.contains("3 posts"));
}

#[sqlx::test]
async fn new_thread_appears_in_lists(pool: PgPool) {
    let s = setup(pool).await;
    s.warm(&[]).await;
    let t = s.f.thread(s.books, s.jonah, "Le Guin reread", "x").await;
    s.f.changed(Change::PostCreated {
        post_id: t.first_post_id,
    })
    .await;
    s.assert_shown(
        "Le Guin reread",
        &["/", "/latest", "/c/books"],
        &["/c/games", "THREAD"],
    )
    .await;
}

#[sqlx::test]
async fn edit_clears_the_thread_and_threads_that_embed_the_post(pool: PgPool) {
    let s = setup(pool).await;
    let fork =
        s.f.new_thread(NewThread {
            space_id: s.books,
            title: "A fork".into(),
            author_id: s.jonah,
            body_md: "Forking".into(),
            forked_from_post_id: Some(s.thread.first_post_id),
            pinned: false,
            locked: false,
            created_at: None,
        })
        .await;
    let fork_path = format!("/t/{}/a-fork", fork.thread_id);
    s.warm(&[&fork_path]).await;
    s.f.exec(&format!(
        "UPDATE posts SET body_html = '<p>Amended words</p>', edited_at = now() WHERE id = {}",
        s.thread.first_post_id
    ))
    .await;
    s.f.changed(Change::PostEdited {
        post_id: s.thread.first_post_id,
    })
    .await;
    s.assert_shown("Amended words", &["THREAD", &fork_path], &[])
        .await;
}

#[sqlx::test]
async fn creating_a_fork_adds_a_backlink_to_the_source(pool: PgPool) {
    let s = setup(pool).await;
    s.warm(&[]).await;
    let fork =
        s.f.new_thread(NewThread {
            space_id: s.books,
            title: "A fork".into(),
            author_id: s.jonah,
            body_md: "Forking".into(),
            forked_from_post_id: Some(s.thread.first_post_id),
            pinned: false,
            locked: false,
            created_at: None,
        })
        .await;
    s.f.changed(Change::PostCreated {
        post_id: fork.first_post_id,
    })
    .await;
    s.assert_shown("Continued in", &["THREAD"], &[]).await;
    // Replies to the fork update the post count on the backlink.
    let reply = s.f.post(fork.thread_id, s.ruth, "reply").await;
    s.f.changed(Change::PostCreated { post_id: reply }).await;
    s.assert_shown("(2 posts, by Jonah)", &["THREAD"], &[])
        .await;
}

#[sqlx::test]
async fn removing_a_post_clears_pages_that_show_it(pool: PgPool) {
    let s = setup(pool).await;
    let reply = s.f.post(s.thread.thread_id, s.jonah, "Regrettable").await;
    let fork =
        s.f.new_thread(NewThread {
            space_id: s.books,
            title: "A fork".into(),
            author_id: s.ruth,
            body_md: "Forking".into(),
            forked_from_post_id: Some(reply),
            pinned: false,
            locked: false,
            created_at: None,
        })
        .await;
    let fork_path = format!("/t/{}/a-fork", fork.thread_id);
    s.warm(&[&fork_path]).await;
    s.f.exec(&format!(
        "UPDATE posts SET deleted_at = now() WHERE id = {reply}"
    ))
    .await;
    s.f.changed(Change::PostRemoved { post_id: reply }).await;
    s.assert_shown("Regrettable", &[], &["THREAD", &fork_path])
        .await;
    // The last visible post is now Ruth's.
    let row = s.f.page("/c/games").await;
    assert!(!row.contains("by Jonah <a"), "{row}");
    assert!(row.contains("by Ruth <a"), "{row}");
}

#[sqlx::test]
async fn moving_a_thread_clears_both_spaces(pool: PgPool) {
    let s = setup(pool).await;
    s.warm(&[]).await;
    s.f.exec(&format!(
        "UPDATE threads SET space_id = {} WHERE id = {}",
        s.books, s.thread.thread_id
    ))
    .await;
    s.f.changed(Change::ThreadChanged {
        thread_id: s.thread.thread_id,
        previous_space_id: Some(s.games),
    })
    .await;
    s.assert_shown(
        "Spirit Island",
        &["/c/books", "THREAD", "/", "/latest"],
        &["/c/games"],
    )
    .await;
    assert!(s.f.page(&s.path).await.contains("href=\"/c/books\""));
}

#[sqlx::test]
async fn moving_a_thread_somewhere_private_hides_it_everywhere(pool: PgPool) {
    let s = setup(pool).await;
    let lounge = s.f.space("lounge", "members", None, 2).await;
    let other = s.f.thread(s.books, s.ruth, "Other", "x").await;
    let other_path = format!("/t/{}/other", other.thread_id);
    s.warm(&[&other_path]).await;
    assert!(s.f.page(&other_path).await.contains("2 posts"));
    s.f.exec(&format!(
        "UPDATE threads SET space_id = {lounge} WHERE id = {}",
        s.thread.thread_id
    ))
    .await;
    s.f.changed(Change::ThreadChanged {
        thread_id: s.thread.thread_id,
        previous_space_id: Some(s.games),
    })
    .await;
    s.assert_shown(
        "Spirit Island",
        &[],
        &["/", "/latest", "/c/games", "/c/books"],
    )
    .await;
    assert_eq!(s.f.get(&s.path).await.status, StatusCode::NOT_FOUND);
    // Ruth's visible post count on other threads drops.
    assert!(s.f.page(&other_path).await.contains("1 post<"));
}

#[sqlx::test]
async fn renaming_pinning_and_deleting_a_thread(pool: PgPool) {
    let s = setup(pool).await;
    s.warm(&[]).await;
    let change = Change::ThreadChanged {
        thread_id: s.thread.thread_id,
        previous_space_id: None,
    };
    s.f.exec(&format!(
        "UPDATE threads SET title = 'Spirit Island, renamed' WHERE id = {}",
        s.thread.thread_id
    ))
    .await;
    s.f.changed(change).await;
    s.assert_shown(
        "Spirit Island, renamed",
        &["/", "/latest", "/c/games", "THREAD"],
        &[],
    )
    .await;

    s.f.exec(&format!(
        "UPDATE threads SET pinned = true, locked = true WHERE id = {}",
        s.thread.thread_id
    ))
    .await;
    s.f.changed(change).await;
    s.assert_shown("Locked", &["/c/games", "THREAD"], &[]).await;

    s.f.exec(&format!(
        "UPDATE threads SET deleted_at = now() WHERE id = {}",
        s.thread.thread_id
    ))
    .await;
    s.f.changed(change).await;
    s.assert_shown("Spirit Island", &[], &["/", "/latest", "/c/games"])
        .await;
}

#[sqlx::test]
async fn space_changes_clear_the_site(pool: PgPool) {
    let s = setup(pool).await;
    s.warm(&[]).await;
    s.f.exec(&format!(
        "UPDATE spaces SET name = 'Tabletop' WHERE id = {}",
        s.games
    ))
    .await;
    s.f.changed(Change::SpaceChanged { space_id: s.games })
        .await;
    s.assert_shown(
        "Tabletop",
        &["/", "/latest", "/c/games", "THREAD"],
        &["/c/books"],
    )
    .await;

    // Making a space members-only hides it and its threads.
    s.f.set_grant(s.games, "read", Grant::members()).await;
    s.assert_shown("Spirit Island", &[], &["/", "/latest"])
        .await;
    assert_eq!(s.f.get(&s.path).await.status, StatusCode::NOT_FOUND);
    assert_eq!(s.f.get("/c/games").await.status, StatusCode::NOT_FOUND);

    // New spaces appear on the index.
    let new = s.f.space("making-things", "public", None, 3).await;
    s.f.changed(Change::SpaceChanged { space_id: new }).await;
    s.assert_shown("Making things", &["/"], &[]).await;
}

#[sqlx::test]
async fn user_and_site_changes_clear_everything(pool: PgPool) {
    let s = setup(pool).await;
    s.warm(&[]).await;
    sqlx::query!(
        "UPDATE users SET display_name = 'Ruth Renamed' WHERE id = $1",
        s.ruth
    )
    .execute(&s.f.db)
    .await
    .unwrap();
    s.f.changed(Change::UserChanged { user_id: s.ruth }).await;
    s.assert_shown("Ruth Renamed", &["/", "/latest", "/c/games", "THREAD"], &[])
        .await;

    s.f.exec("INSERT INTO settings (key, value) VALUES ('site_name', '\"The Tuesday Club\"')")
        .await;
    s.f.changed(Change::SiteChanged).await;
    s.assert_shown("The Tuesday Club", &PAGES, &[]).await;
}

#[sqlx::test]
async fn changes_dont_clear_unrelated_pages(pool: PgPool) {
    let s = setup(pool).await;
    let other = s.f.thread(s.books, s.jonah, "Other", "x").await;
    let other_path = format!("/t/{}/other", other.thread_id);
    s.warm(&[&other_path]).await;
    let before = s.f.state.cache().len();
    let post = s.f.post(s.thread.thread_id, s.ruth, "reply").await;
    s.f.changed(Change::PostCreated { post_id: post }).await;
    // /c/books and Jonah's thread survive.
    assert_eq!(s.f.state.cache().len(), 2, "had {before}");
    s.f.page("/c/books").await;
    s.f.page(&other_path).await;
    assert_eq!(s.f.state.cache().len(), 2);
}
