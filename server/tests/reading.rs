//! The reading path: index, latest, category and thread pages, permalinks.

mod common;

use axum::http::StatusCode;
use common::{Forum, post_html};
use parlour::changes::Change;
use parlour::store::NewThread;
use sqlx::PgPool;

fn position(page: &str, needle: &str) -> usize {
    page.find(needle)
        .unwrap_or_else(|| panic!("{needle:?} not found in page"))
}

fn assert_in_order(page: &str, needles: &[&str]) {
    let positions: Vec<usize> = needles.iter().map(|n| position(page, n)).collect();
    assert!(
        positions.windows(2).all(|w| w[0] < w[1]),
        "{needles:?} out of order: {positions:?}"
    );
}

// ---------------------------------------------------------------------------
// Index

#[sqlx::test]
async fn index_shows_sections_and_categories_in_order(pool: PgPool) {
    let f = Forum::new(pool);
    let hobbies = f.section("Hobbies", 2).await;
    let community = f.section("Community", 1).await;
    f.category("board-games", "public", Some(hobbies), 1).await;
    f.category("books", "public", Some(hobbies), 0).await;
    f.category("general", "public", Some(community), 0).await;
    f.category("meta", "public", None, 0).await;

    let page = f.page("/").await;
    assert_in_order(
        &page,
        &[
            "href=\"/c/meta\"",
            "Community",
            "href=\"/c/general\"",
            "Hobbies",
            "href=\"/c/books\"",
            "href=\"/c/board-games\"",
        ],
    );
    assert!(page.contains("All about books."));
}

#[sqlx::test]
async fn index_shows_counts_and_last_post(pool: PgPool) {
    let f = Forum::new(pool);
    let ruth = f.user("ruth").await;
    let jonah = f.user("jonah").await;
    let games = f.category("games", "public", None, 0).await;
    let t1 = f.thread(games, ruth, "Spirit Island", "First").await;
    f.post(t1.thread_id, jonah, "Second").await;
    f.post(t1.thread_id, ruth, "Third").await;
    let t2 = f.thread(games, jonah, "Cascadia variants", "Hello").await;
    let last = f.post(t2.thread_id, jonah, "Latest word").await;

    let page = f.page("/").await;
    let row = &page[position(&page, "href=\"/c/games\"")..];
    let row = &row[..position(row, "</tr>")];
    assert!(row.contains(">2<"), "thread count in {row}");
    assert!(row.contains(">5<"), "post count in {row}");
    assert!(row.contains("Cascadia variants"), "{row}");
    assert!(row.contains(&format!("href=\"/p/{last}\"")), "{row}");
    assert!(row.contains("Jonah"), "{row}");
}

#[sqlx::test]
async fn index_latest_list_shows_recent_threads(pool: PgPool) {
    let f = Forum::new(pool);
    let ruth = f.user("ruth").await;
    let games = f.category("games", "public", None, 0).await;
    let old = f.thread(games, ruth, "Older thread", "x").await;
    f.thread(games, ruth, "Newer thread", "x").await;
    f.post(old.thread_id, ruth, "bump").await;

    let page = f.page("/").await;
    let latest = &page[position(&page, "id=\"latest-heading\"")..];
    assert_in_order(latest, &["Older thread", "Newer thread"]);
    assert!(latest.contains("href=\"/latest\""));
}

#[sqlx::test]
async fn index_with_no_categories_says_so(pool: PgPool) {
    let f = Forum::new(pool);
    let page = f.page("/").await;
    assert!(page.contains("There are no categories yet."));
}

#[sqlx::test]
async fn anonymous_visitors_dont_see_private_categories(pool: PgPool) {
    let f = Forum::new(pool);
    let ruth = f.user("ruth").await;
    let lounge = f.category("lounge", "members", None, 0).await;
    let organisers = f.category("organisers", "groups", None, 1).await;
    let public = f.category("general", "public", None, 2).await;
    f.thread(lounge, ruth, "Secret lounge thread", "lounge body")
        .await;
    f.thread(
        organisers,
        ruth,
        "Secret organisers thread",
        "organiser body",
    )
    .await;
    f.thread(public, ruth, "Public thread", "public body").await;

    for path in ["/", "/latest"] {
        let page = f.page(path).await;
        assert!(page.contains("Public thread"), "{path}");
        for secret in ["lounge", "organisers", "Secret"] {
            assert!(!page.contains(secret), "{path} mentions {secret}");
        }
    }
    assert_eq!(f.get("/c/lounge").await.status, StatusCode::NOT_FOUND);
    assert_eq!(f.get("/c/organisers").await.status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Latest

#[sqlx::test]
async fn latest_lists_threads_by_activity_with_categories(pool: PgPool) {
    let f = Forum::new(pool);
    let ruth = f.user("ruth").await;
    let games = f.category("games", "public", None, 0).await;
    let books = f.category("books", "public", None, 1).await;
    let a = f.thread(games, ruth, "Thread A", "x").await;
    f.thread(books, ruth, "Thread B", "x").await;
    f.new_thread(NewThread {
        category_id: games,
        title: "Pinned C".into(),
        author_id: ruth,
        body_md: "x".into(),
        forked_from_post_id: None,
        pinned: true,
        locked: false,
        created_at: Some(chrono::Utc::now() - chrono::Duration::days(30)),
    })
    .await;
    f.post(a.thread_id, ruth, "bump").await;

    let page = f.page("/latest").await;
    // Pins don't count on /latest.
    assert_in_order(&page, &["Thread A", "Thread B", "Pinned C"]);
    assert!(page.contains("href=\"/c/books\""));
}

#[sqlx::test]
async fn latest_paginates(pool: PgPool) {
    let f = Forum::new(pool);
    let ruth = f.user("ruth").await;
    let games = f.category("games", "public", None, 0).await;
    for i in 0..31 {
        f.thread(games, ruth, &format!("Thread number {i:02}"), "x")
            .await;
    }
    let page1 = f.page("/latest").await;
    assert!(page1.contains("Thread number 30"));
    assert!(page1.contains("Thread number 01"));
    assert!(!page1.contains("Thread number 00"));
    assert!(page1.contains("href=\"/latest?page=2\""));
    let page2 = f.page("/latest?page=2").await;
    assert!(page2.contains("Thread number 00"));
    assert!(!page2.contains("Thread number 01"));
    assert_eq!(f.get("/latest?page=3").await.status, StatusCode::NOT_FOUND);
}

// ---------------------------------------------------------------------------
// Category

#[sqlx::test]
async fn category_lists_pinned_threads_first_then_by_activity(pool: PgPool) {
    let f = Forum::new(pool);
    let ruth = f.user("ruth").await;
    let section = f.section("Hobbies", 0).await;
    let games = f.category("games", "public", Some(section), 0).await;
    let old = f.thread(games, ruth, "Old but bumped", "x").await;
    f.thread(games, ruth, "Newer thread", "x").await;
    f.new_thread(NewThread {
        category_id: games,
        title: "Read me first".into(),
        author_id: ruth,
        body_md: "x".into(),
        forked_from_post_id: None,
        pinned: true,
        locked: true,
        created_at: Some(chrono::Utc::now() - chrono::Duration::days(300)),
    })
    .await;
    f.post(old.thread_id, ruth, "bump").await;

    let page = f.page("/c/games").await;
    assert_in_order(&page, &["Read me first", "Old but bumped", "Newer thread"]);
    assert!(page.contains("Pinned"));
    assert!(page.contains("Locked"));
    // Breadcrumbs and heading.
    assert_in_order(&page, &["href=\"/\"", "Hobbies", "<h1"]);
    assert!(page.contains("All about games."));
}

#[sqlx::test]
async fn category_paginates_thirty_threads_per_page(pool: PgPool) {
    let f = Forum::new(pool);
    let ruth = f.user("ruth").await;
    let games = f.category("games", "public", None, 0).await;
    for i in 0..65 {
        f.thread(games, ruth, &format!("Thread number {i:02}"), "x")
            .await;
    }
    let page1 = f.page("/c/games").await;
    assert!(page1.contains("Thread number 64") && page1.contains("Thread number 35"));
    assert!(!page1.contains("Thread number 34"));
    assert!(
        page1.contains("href=\"/c/games?page=2\"") && page1.contains("href=\"/c/games?page=3\"")
    );
    let page3 = f.page("/c/games?page=3").await;
    assert!(page3.contains("Thread number 00") && page3.contains("Thread number 04"));
    assert!(!page3.contains("Thread number 05"));
    assert!(page3.contains("href=\"/c/games?page=2\""));

    for bad in [
        "/c/games?page=4",
        "/c/games?page=0",
        "/c/games?page=-1",
        "/c/games?page=two",
        "/c/nope",
    ] {
        assert_eq!(f.get(bad).await.status, StatusCode::NOT_FOUND, "{bad}");
    }
}

#[sqlx::test]
async fn category_rows_show_reply_counts_last_poster_and_page_links(pool: PgPool) {
    let f = Forum::new(pool);
    let ruth = f.user("ruth").await;
    let jonah = f.user("jonah").await;
    let games = f.category("games", "public", None, 0).await;
    let t = f.thread(games, ruth, "Long thread", "x").await;
    let mut last = 0;
    for _ in 0..120 {
        last = f.post(t.thread_id, jonah, "more").await;
    }
    let page = f.page("/c/games").await;
    assert!(page.contains(">120<"), "reply count");
    assert!(page.contains(&format!("href=\"/p/{last}\"")));
    let id = t.thread_id;
    for p in [2, 3] {
        assert!(
            page.contains(&format!("href=\"/t/{id}/long-thread?page={p}\"")),
            "page {p} link"
        );
    }
}

#[sqlx::test]
async fn empty_category_says_so(pool: PgPool) {
    let f = Forum::new(pool);
    f.category("games", "public", None, 0).await;
    let page = f.page("/c/games").await;
    assert!(page.contains("No threads yet."));
}

// ---------------------------------------------------------------------------
// Threads

#[sqlx::test]
async fn thread_shows_posts_with_author_column(pool: PgPool) {
    let f = Forum::new(pool);
    let ruth = f.user("ruth").await;
    sqlx::query!(
        "UPDATE users SET title = 'Book club host', pronouns = 'she/her', status_line = 'Reading: Le Guin, again', location = 'Leeds' WHERE id = $1",
        ruth
    )
    .execute(&f.db)
    .await
    .unwrap();
    let games = f.category("games", "public", None, 0).await;
    let t = f
        .thread(games, ruth, "Spirit Island", "We **lost** on turn six.")
        .await;
    f.post(t.thread_id, ruth, "Again!").await;

    let page = f.page(&format!("/t/{}/spirit-island", t.thread_id)).await;
    assert!(page.contains("<h1"));
    assert!(page.contains("Spirit Island"));
    let first = post_html(&page, t.first_post_id);
    assert!(first.contains("№ 1"), "{first}");
    assert!(
        first.contains("We <strong>lost</strong> on turn six."),
        "{first}"
    );
    for detail in [
        "Ruth",
        "@ruth",
        "Book club host",
        "she/her",
        "Reading: Le Guin, again",
        "Leeds",
        "2 posts",
    ] {
        assert!(first.contains(detail), "{detail} missing from {first}");
    }
    assert!(page.contains("№ 2"));
}

#[sqlx::test]
async fn thread_pages_hold_fifty_posts(pool: PgPool) {
    let f = Forum::new(pool);
    let ruth = f.user("ruth").await;
    let games = f.category("games", "public", None, 0).await;
    let t = f.thread(games, ruth, "Long", "post 1 body").await;
    let mut ids = vec![t.first_post_id];
    for i in 2..=107 {
        ids.push(f.post(t.thread_id, ruth, &format!("post {i} body")).await);
    }
    let base = format!("/t/{}/long", t.thread_id);
    let page1 = f.page(&base).await;
    assert!(page1.contains("post 1 body") && page1.contains("post 50 body"));
    assert!(!page1.contains("post 51 body"));
    assert!(page1.contains(&format!("href=\"{base}?page=3\"")));
    let page3 = f.page(&format!("{base}?page=3")).await;
    assert!(page3.contains("post 101 body") && page3.contains("post 107 body"));
    assert!(!page3.contains("post 100 body"));
    assert!(page3.contains("№ 107"));
    assert_eq!(
        f.get(&format!("{base}?page=4")).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        f.get(&format!("{base}?page=0")).await.status,
        StatusCode::NOT_FOUND
    );
}

#[sqlx::test]
async fn deleted_and_hidden_posts_are_tombstones(pool: PgPool) {
    let f = Forum::new(pool);
    let ruth = f.user("ruth").await;
    let games = f.category("games", "public", None, 0).await;
    let t = f.thread(games, ruth, "Thread", "first").await;
    let deleted = f.post(t.thread_id, ruth, "regrettable words").await;
    let hidden = f.post(t.thread_id, ruth, "rule-breaking words").await;
    f.post(t.thread_id, ruth, "after").await;
    f.exec(&format!(
        "UPDATE posts SET deleted_at = now() WHERE id = {deleted}"
    ))
    .await;
    f.exec(&format!(
        "UPDATE posts SET hidden_at = now() WHERE id = {hidden}"
    ))
    .await;

    let page = f.page(&format!("/t/{}/thread", t.thread_id)).await;
    assert!(!page.contains("regrettable"));
    assert!(!page.contains("rule-breaking"));
    let d = post_html(&page, deleted);
    assert!(d.contains("№ 2") && d.contains("Post deleted"), "{d}");
    let h = post_html(&page, hidden);
    assert!(
        h.contains("№ 3") && h.contains("hidden by a moderator"),
        "{h}"
    );
    assert!(page.contains("№ 4"));
    // Removed posts don't count towards the author's total.
    assert!(post_html(&page, t.first_post_id).contains("2 posts"));
}

#[sqlx::test]
async fn replies_link_to_the_post_they_reply_to(pool: PgPool) {
    let f = Forum::new(pool);
    let ruth = f.user("ruth").await;
    let jonah = f.user("jonah").await;
    let games = f.category("games", "public", None, 0).await;
    let t = f
        .thread(games, jonah, "Thread", "We lost on turn six")
        .await;
    let same_page = f
        .reply(
            t.thread_id,
            ruth,
            "> We lost\n\nClassic.",
            Some(t.first_post_id),
        )
        .await;
    for _ in 0..60 {
        f.post(t.thread_id, ruth, "filler").await;
    }
    let other_page = f
        .reply(
            t.thread_id,
            ruth,
            "Back to the start",
            Some(t.first_post_id),
        )
        .await;

    let base = format!("/t/{}/thread", t.thread_id);
    let page1 = f.page(&base).await;
    let reply = post_html(&page1, same_page);
    assert!(
        reply.contains(&format!("href=\"#p{}\"", t.first_post_id)),
        "{reply}"
    );
    assert!(reply.contains("Jonah"), "{reply}");
    assert!(reply.contains("№ 1"), "{reply}");
    assert!(reply.contains("<blockquote>"), "{reply}");

    let page2 = f.page(&format!("{base}?page=2")).await;
    let reply = post_html(&page2, other_page);
    assert!(
        reply.contains(&format!("href=\"/p/{}\"", t.first_post_id)),
        "{reply}"
    );

    // Replying to a removed post doesn't reveal who wrote it.
    f.exec(&format!(
        "UPDATE posts SET deleted_at = now() WHERE id = {}",
        t.first_post_id
    ))
    .await;
    f.changed(Change::PostRemoved {
        post_id: t.first_post_id,
    })
    .await;
    let page1 = f.page(&base).await;
    let reply = post_html(&page1, same_page);
    assert!(reply.contains("№ 1") && !reply.contains("Jonah"), "{reply}");
}

#[sqlx::test]
async fn quote_forks_embed_their_source_and_the_source_links_back(pool: PgPool) {
    let f = Forum::new(pool);
    let maya = f.user("maya").await;
    let ollie = f.user("ollie").await;
    let games = f.category("games", "public", None, 0).await;
    let source = f
        .thread(
            games,
            maya,
            "What did you play?",
            "Solo play is its own hobby.",
        )
        .await;
    let fork = f
        .new_thread(NewThread {
            category_id: games,
            title: "Do solo modes count?".into(),
            author_id: ollie,
            body_md: "Strong disagree.".into(),
            forked_from_post_id: Some(source.first_post_id),
            pinned: false,
            locked: false,
            created_at: None,
        })
        .await;

    let fork_page = f
        .page(&format!("/t/{}/do-solo-modes-count", fork.thread_id))
        .await;
    assert_in_order(
        &fork_page,
        &["Solo play is its own hobby.", "Strong disagree."],
    );
    assert!(fork_page.contains(&format!("href=\"/p/{}\"", source.first_post_id)));
    assert!(fork_page.contains("What did you play?"));

    let source_page = f
        .page(&format!("/t/{}/what-did-you-play", source.thread_id))
        .await;
    let post = post_html(&source_page, source.first_post_id);
    assert!(post.contains("Continued in"), "{post}");
    assert!(
        post.contains(&format!(
            "href=\"/t/{}/do-solo-modes-count\"",
            fork.thread_id
        )),
        "{post}"
    );

    let category = f.page("/c/games").await;
    assert!(
        category.contains("from <a href=\"/t/"),
        "fork origin on category page"
    );
}

#[sqlx::test]
async fn embeds_and_backlinks_respect_visibility(pool: PgPool) {
    let f = Forum::new(pool);
    let maya = f.user("maya").await;
    let public = f.category("games", "public", None, 0).await;
    let lounge = f.category("lounge", "members", None, 1).await;
    let source = f
        .thread(public, maya, "Public source", "Public words")
        .await;
    // A fork into a members-only category: legal, but anonymous visitors
    // mustn't learn it exists.
    let hidden_fork = f
        .new_thread(NewThread {
            category_id: lounge,
            title: "Members-only fork".into(),
            author_id: maya,
            body_md: "Private discussion".into(),
            forked_from_post_id: Some(source.first_post_id),
            pinned: false,
            locked: false,
            created_at: None,
        })
        .await;
    let page = f
        .page(&format!("/t/{}/public-source", source.thread_id))
        .await;
    assert!(!page.contains("Members-only fork"));
    assert!(!page.contains("Continued in"));

    // A public fork whose source is later moved somewhere private shows the
    // embed as unavailable.
    let fork = f
        .new_thread(NewThread {
            category_id: public,
            title: "Public fork".into(),
            author_id: maya,
            body_md: "Fork words".into(),
            forked_from_post_id: Some(source.first_post_id),
            pinned: false,
            locked: false,
            created_at: None,
        })
        .await;
    f.exec(&format!(
        "UPDATE threads SET category_id = {lounge} WHERE id = {}",
        source.thread_id
    ))
    .await;
    let page = f.page(&format!("/t/{}/public-fork", fork.thread_id)).await;
    assert!(page.contains("This post is unavailable"));
    assert!(!page.contains("Public words"));
    assert!(!page.contains("Public source"));
    let _ = hidden_fork;
}

#[sqlx::test]
async fn thread_urls_are_canonicalised(pool: PgPool) {
    let f = Forum::new(pool);
    let ruth = f.user("ruth").await;
    let games = f.category("games", "public", None, 0).await;
    let t = f.thread(games, ruth, "Spirit Island", "x").await;
    for _ in 0..60 {
        f.post(t.thread_id, ruth, "x").await;
    }
    let id = t.thread_id;
    let wrong = f.get(&format!("/t/{id}/old-title?page=2")).await;
    assert_eq!(wrong.status, StatusCode::PERMANENT_REDIRECT);
    assert_eq!(wrong.location(), format!("/t/{id}/spirit-island?page=2"));
    let bare = f.get(&format!("/t/{id}")).await;
    assert_eq!(bare.status, StatusCode::PERMANENT_REDIRECT);
    assert_eq!(bare.location(), format!("/t/{id}/spirit-island"));
    assert_eq!(f.get("/t/999999/x").await.status, StatusCode::NOT_FOUND);
    assert_eq!(f.get("/t/notanumber/x").await.status, StatusCode::NOT_FOUND);
}

#[sqlx::test]
async fn private_and_deleted_threads_are_not_found(pool: PgPool) {
    let f = Forum::new(pool);
    let ruth = f.user("ruth").await;
    let lounge = f.category("lounge", "members", None, 0).await;
    let games = f.category("games", "public", None, 1).await;
    let private = f.thread(lounge, ruth, "Secret", "secret body").await;
    let deleted = f.thread(games, ruth, "Deleted", "deleted body").await;
    f.exec(&format!(
        "UPDATE threads SET deleted_at = now() WHERE id = {}",
        deleted.thread_id
    ))
    .await;
    for t in [private, deleted] {
        for path in [
            format!("/t/{}/x", t.thread_id),
            format!("/t/{}", t.thread_id),
            format!("/t/{}/unread", t.thread_id),
            format!("/p/{}", t.first_post_id),
        ] {
            let response = f.get(&path).await;
            assert_eq!(response.status, StatusCode::NOT_FOUND, "{path}");
            assert!(!response.body.contains("secret body"), "{path}");
            assert!(!response.body.contains("deleted body"), "{path}");
        }
    }
    assert!(!f.page("/c/games").await.contains("Deleted"));
}

// ---------------------------------------------------------------------------
// Permalinks

#[sqlx::test]
async fn post_permalinks_redirect_to_the_right_page(pool: PgPool) {
    let f = Forum::new(pool);
    let ruth = f.user("ruth").await;
    let games = f.category("games", "public", None, 0).await;
    let t = f.thread(games, ruth, "Long", "x").await;
    let mut ids = vec![t.first_post_id];
    for _ in 2..=101 {
        ids.push(f.post(t.thread_id, ruth, "x").await);
    }
    let id = t.thread_id;
    for (seq, expected) in [
        (1, format!("/t/{id}/long")),
        (50, format!("/t/{id}/long")),
        (51, format!("/t/{id}/long?page=2")),
        (101, format!("/t/{id}/long?page=3")),
    ] {
        let post = ids[seq - 1];
        let response = f.get(&format!("/p/{post}")).await;
        assert_eq!(response.status, StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(response.location(), format!("{expected}#p{post}"));
    }
    assert_eq!(f.get("/p/999999").await.status, StatusCode::NOT_FOUND);
    assert_eq!(f.get("/p/abc").await.status, StatusCode::NOT_FOUND);
}

#[sqlx::test]
async fn unread_redirects_to_the_first_page_for_now(pool: PgPool) {
    let f = Forum::new(pool);
    let ruth = f.user("ruth").await;
    let games = f.category("games", "public", None, 0).await;
    let t = f.thread(games, ruth, "Spirit Island", "x").await;
    let response = f.get(&format!("/t/{}/unread", t.thread_id)).await;
    assert_eq!(response.status, StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(
        response.location(),
        format!("/t/{}/spirit-island", t.thread_id)
    );
}

#[sqlx::test]
async fn a_thread_called_unread_still_works(pool: PgPool) {
    let f = Forum::new(pool);
    let ruth = f.user("ruth").await;
    let games = f.category("games", "public", None, 0).await;
    let t = f.thread(games, ruth, "Unread", "the body").await;
    let response = f.get(&format!("/t/{}/unread", t.thread_id)).await;
    assert_eq!(response.status, StatusCode::TEMPORARY_REDIRECT);
    let page = f.page(response.location()).await;
    assert!(page.contains("the body"));
}

// ---------------------------------------------------------------------------
// Every page

#[sqlx::test]
async fn pages_have_no_inline_styles_or_scripts_and_set_no_cookies(pool: PgPool) {
    let f = Forum::new(pool);
    let ruth = f.user("ruth").await;
    let section = f.section("Hobbies", 0).await;
    let games = f.category("games", "public", Some(section), 0).await;
    let t = f.thread(games, ruth, "Spirit Island", "x").await;
    let reply = f.reply(t.thread_id, ruth, "y", Some(t.first_post_id)).await;
    f.exec(&format!(
        "UPDATE posts SET deleted_at = now() WHERE id = {reply}"
    ))
    .await;
    for path in [
        "/",
        "/latest",
        "/c/games",
        &format!("/t/{}/spirit-island", t.thread_id),
        "/nope",
    ] {
        let response = f.get(path).await;
        assert!(
            !response.body.contains("style="),
            "{path} has inline styles"
        );
        assert!(!response.body.contains("<script"), "{path} has scripts");
        assert!(!response.headers.contains_key("set-cookie"), "{path}");
        assert!(response.body.contains("name=\"viewport\""), "{path}");
    }
}

#[sqlx::test]
async fn huge_page_numbers_are_not_found(pool: PgPool) {
    let f = Forum::new(pool);
    let ruth = f.user("ruth").await;
    let games = f.category("games", "public", None, 0).await;
    let t = f.thread(games, ruth, "Spirit Island", "x").await;
    let id = t.thread_id;
    for page in [
        i64::MAX.to_string(),
        (i64::MAX / 30 + 1).to_string(),
        (i64::MAX / 50 + 1).to_string(),
        (i64::from(i32::MAX) / 50 + 2).to_string(),
        // Wraps to page 1 when truncated to 32 bits.
        ((1i64 << 32) + 1).to_string(),
        "99999999999999999999999".to_owned(),
    ] {
        for path in [
            format!("/latest?page={page}"),
            format!("/c/games?page={page}"),
            format!("/t/{id}/spirit-island?page={page}"),
        ] {
            let response = f.get(&path).await;
            assert_eq!(response.status, StatusCode::NOT_FOUND, "{path}");
        }
    }
}

#[sqlx::test]
async fn reply_links_never_reveal_posts_in_other_threads(pool: PgPool) {
    let f = Forum::new(pool);
    let ruth = f.user("ruth").await;
    let maya = f.user("maya").await;
    let games = f.category("games", "public", None, 0).await;
    let lounge = f.category("lounge", "groups", None, 1).await;
    let public = f.thread(games, ruth, "Public", "x").await;
    let secret = f.thread(lounge, maya, "Secret", "x").await;
    let target = f.post(public.thread_id, maya, "target").await;
    let reply = f.reply(public.thread_id, ruth, "reply", Some(target)).await;
    // The schema refuses this; if it ever failed to, the page still mustn't
    // show who wrote the post or where it is now.
    f.exec(&format!(
        "ALTER TABLE posts DISABLE TRIGGER USER;
         UPDATE posts SET thread_id = {}, seq = 999 WHERE id = {target};
         ALTER TABLE posts ENABLE TRIGGER USER;",
        secret.thread_id
    ))
    .await;
    let page = f.page(&format!("/t/{}/public", public.thread_id)).await;
    let html = post_html(&page, reply);
    assert!(!html.contains("Maya"), "{html}");
    assert!(!html.contains("999"), "{html}");
    assert!(!html.contains(&format!("/p/{target}")), "{html}");
}

// ---------------------------------------------------------------------------
// Canonical URLs: every page has exactly one URL that serves it, so junk
// query strings and spellings of numbers can't fill the anonymous cache.

struct Canonical {
    f: Forum,
    thread_id: i64,
    /// Every URL that should serve a page.
    pages: Vec<String>,
}

async fn canonical_setup(pool: PgPool) -> Canonical {
    let f = Forum::new(pool);
    let ruth = f.user("ruth").await;
    let games = f.category("games", "public", None, 0).await;
    let t = f.thread(games, ruth, "Spirit Island", "x").await;
    for _ in 0..60 {
        f.post(t.thread_id, ruth, "x").await;
    }
    for i in 0..30 {
        f.thread(games, ruth, &format!("Filler {i}"), "x").await;
    }
    let id = t.thread_id;
    let pages = vec![
        "/".to_owned(),
        "/latest".to_owned(),
        "/latest?page=2".to_owned(),
        "/c/games".to_owned(),
        "/c/games?page=2".to_owned(),
        format!("/t/{id}/spirit-island"),
        format!("/t/{id}/spirit-island?page=2"),
    ];
    Canonical {
        f,
        thread_id: id,
        pages,
    }
}

#[sqlx::test]
async fn non_canonical_urls_redirect_to_the_canonical_one(pool: PgPool) {
    let c = canonical_setup(pool).await;
    let id = c.thread_id;
    let thread = format!("/t/{id}/spirit-island");
    for (from, to) in [
        ("/?x=1".to_owned(), "/".to_owned()),
        ("/latest?page=1".to_owned(), "/latest".to_owned()),
        ("/latest?x=1&page=2".to_owned(), "/latest?page=2".to_owned()),
        ("/c/games?x=1".to_owned(), "/c/games".to_owned()),
        ("/c/games?x=2".to_owned(), "/c/games".to_owned()),
        ("/c/games?page=01".to_owned(), "/c/games".to_owned()),
        ("/c/games?page=%2B1".to_owned(), "/c/games".to_owned()),
        ("/c/games?page=02".to_owned(), "/c/games?page=2".to_owned()),
        ("/c/games?page=%32".to_owned(), "/c/games?page=2".to_owned()),
        ("/c/game%73".to_owned(), "/c/games".to_owned()),
        ("/c/games?".to_owned(), "/c/games".to_owned()),
        (format!("/t/0{id}/spirit-island"), thread.clone()),
        (
            format!("/t/0{id}/spirit-island?page=2"),
            format!("{thread}?page=2"),
        ),
        (format!("{thread}?page=1"), thread.clone()),
        (format!("{thread}?utm_source=x"), thread.clone()),
        (format!("/t/{id}/spirit-islan%64"), thread.clone()),
        (
            format!("/t/{id}/Spirit%20Island?page=2"),
            format!("{thread}?page=2"),
        ),
    ] {
        let response = c.f.get(&from).await;
        assert_eq!(response.status, StatusCode::PERMANENT_REDIRECT, "{from}");
        assert_eq!(response.location(), to, "{from}");
    }
    assert!(c.f.state.cache().is_empty(), "redirects aren't cached");
}

#[sqlx::test]
async fn ambiguous_page_numbers_are_not_found(pool: PgPool) {
    let c = canonical_setup(pool).await;
    let id = c.thread_id;
    for path in [
        "/latest?page=1&page=2".to_owned(),
        "/c/games?page=2&page=2".to_owned(),
        format!("/t/{id}/spirit-island?page=1&page=1"),
        "/c/games?page=".to_owned(),
        "/c/games?page=%FF".to_owned(),
    ] {
        let response = c.f.get(&path).await;
        assert_eq!(response.status, StatusCode::NOT_FOUND, "{path}");
        assert!(response.body.contains("<html"), "{path}: {}", response.body);
    }
}

/// Percent-encodes `s`, encoding each byte that doesn't need it only when
/// `encode_anyway` says so.
fn percent_encode(s: &str, mut encode_anyway: impl FnMut() -> bool) -> String {
    let mut out = String::new();
    for &b in s.as_bytes() {
        if (b.is_ascii_alphanumeric() || b == b'-') && !encode_anyway() {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[hegel::standalone_function(test_cases = 200)]
fn only_canonical_urls_are_served_or_cached(
    tc: hegel::TestCase,
    rt: &tokio::runtime::Handle,
    c: &Canonical,
) {
    use hegel::generators::{self as gs, Generator};
    let target = tc.draw(gs::sampled_from(c.pages.clone()));
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    // Respell the path: percent-encode some characters, pad numbers.
    let mut new_path = String::new();
    for segment in path.split('/').skip(1) {
        new_path.push('/');
        let zeros = if segment.bytes().all(|b| b.is_ascii_digit()) && !segment.is_empty() {
            tc.draw(gs::integers::<usize>().max_value(3))
        } else {
            0
        };
        new_path.push_str(&"0".repeat(zeros));
        new_path.push_str(&percent_encode(segment, || tc.draw(gs::booleans())));
    }
    if new_path.is_empty() {
        new_path.push('/');
    }
    // Rebuild the query with extra parameters and respelled values.
    let mut params: Vec<(String, String)> = query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap();
            (k.to_owned(), v.to_owned())
        })
        .collect();
    let extras = tc.draw(gs::integers::<usize>().max_value(3));
    for _ in 0..extras {
        let key = tc.draw(gs::sampled_from(vec![
            "page".to_owned(),
            "x".to_owned(),
            tc.draw(gs::text()),
        ]));
        let value = tc.draw(hegel::one_of!(
            gs::text(),
            gs::integers::<i64>().map(|n| n.to_string()),
            gs::integers::<u8>().map(|n| format!("+{n}")),
            gs::integers::<u8>().map(|n| format!("0{n}"))
        ));
        let at = tc.draw(gs::integers::<usize>().max_value(params.len()));
        params.insert(at, (key, value));
    }
    let mut url = new_path;
    if !params.is_empty() || tc.draw(gs::booleans()) {
        url.push('?');
        let encoded: Vec<String> = params
            .iter()
            .map(|(k, v)| {
                format!(
                    "{}={}",
                    percent_encode(k, || tc.draw(gs::booleans())),
                    percent_encode(v, || tc.draw(gs::booleans()))
                )
            })
            .collect();
        url.push_str(&encoded.join("&"));
    }
    tc.note(&url);

    let response = rt.block_on(c.f.get(&url));
    match response.status {
        StatusCode::OK => assert!(c.pages.contains(&url), "{url} served a page"),
        StatusCode::PERMANENT_REDIRECT => {
            assert!(!c.pages.contains(&url), "{url} redirected");
            let location = response.location().to_owned();
            let next = rt.block_on(c.f.get(&location));
            assert!(
                next.status == StatusCode::OK || next.status == StatusCode::NOT_FOUND,
                "{url} -> {location}: {}",
                next.status
            );
            if next.status == StatusCode::OK {
                assert!(c.pages.contains(&location), "{url} -> {location}");
            }
        }
        StatusCode::NOT_FOUND => assert!(!c.pages.contains(&url)),
        other => panic!("{url}: {other}"),
    }
    assert!(c.f.state.cache().len() <= c.pages.len());
    assert!(rt.block_on(async { c.f.state.cache().get(&url).is_none() }) || c.pages.contains(&url));
}

#[sqlx::test]
async fn only_canonical_urls_are_served_or_cached_property(pool: PgPool) {
    let c = canonical_setup(pool).await;
    let rt = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || only_canonical_urls_are_served_or_cached(&rt, &c))
        .await
        .unwrap();
}
