//! Search (DESIGN.md §9): Postgres full-text search over posts and thread
//! titles, filtered by the same visibility condition as everything else,
//! with conversations found only by the people in them.

mod common;

use axum::http::{StatusCode, header};
use common::Forum;
use parlour::http_cache::PRIVATE_CACHE_CONTROL;
use parlour::store::{self, NewConversation};
use sqlx::PgPool;

struct Setup {
    f: Forum,
    public: i64,
    members: i64,
    ruth: i64,
    ruth_cookie: String,
    jonah: i64,
    jonah_cookie: String,
}

async fn setup(pool: PgPool) -> Setup {
    let f = Forum::new(pool).without_rate_limits();
    f.no_new_members().await;
    let public = f.category("games", "public", None, 0).await;
    let members = f.category("lounge", "members", None, 1).await;
    let ruth = f.member("ruth", "user", None).await;
    let jonah = f.member("jonah", "user", None).await;
    Setup {
        ruth_cookie: f.session(ruth).await,
        jonah_cookie: f.session(jonah).await,
        f,
        public,
        members,
        ruth,
        jonah,
    }
}

/// Whether the page links to `href`, however its `&`s are escaped.
fn links_to(page: &str, href: &str) -> bool {
    ["&amp;", "&#38;"]
        .iter()
        .any(|amp| page.contains(&format!("href=\"{}\"", href.replace('&', amp))))
}

/// The post ids a search page links to, in order.
fn hits(page: &str) -> Vec<i64> {
    page.match_indices("class=\"search-hit\" id=\"hit-")
        .map(|(i, m)| {
            let rest = &page[i + m.len()..];
            rest[..rest.find('"').unwrap()].parse().unwrap()
        })
        .collect()
}

#[sqlx::test]
async fn members_find_posts_by_their_words(pool: PgPool) {
    let s = setup(pool).await;
    let heat =
        s.f.thread(
            s.public,
            s.ruth,
            "Racing games",
            "We played Heat with five players.",
        )
        .await;
    let other =
        s.f.post(heat.thread_id, s.jonah, "Cascadia is about animals.")
            .await;
    let page = s.f.page_as("/search?q=heat", &s.ruth_cookie).await;
    assert_eq!(hits(&page), vec![heat.first_post_id]);
    // The words found are marked, and the result links to the post.
    assert!(page.contains("<mark>Heat</mark>"), "{page}");
    assert!(page.contains(&format!("href=\"/p/{}\"", heat.first_post_id)));
    assert!(page.contains("Racing games"));
    // English stemming: "animal" finds "animals".
    let page = s.f.page_as("/search?q=animal", &s.ruth_cookie).await;
    assert_eq!(hits(&page), vec![other]);
    // Web-search syntax: quotes, OR and minus.
    let page =
        s.f.page_as("/search?q=heat+OR+cascadia", &s.ruth_cookie)
            .await;
    assert_eq!(hits(&page).len(), 2);
    let page =
        s.f.page_as("/search?q=%22five+players%22+-cascadia", &s.ruth_cookie)
            .await;
    assert_eq!(hits(&page), vec![heat.first_post_id]);
}

#[sqlx::test]
async fn thread_titles_find_their_first_post(pool: PgPool) {
    let s = setup(pool).await;
    let thread =
        s.f.thread(
            s.public,
            s.ruth,
            "Spirit Island strategy",
            "Where to start?",
        )
        .await;
    s.f.post(thread.thread_id, s.jonah, "Play the easy spirits first.")
        .await;
    let page = s.f.page_as("/search?q=island", &s.ruth_cookie).await;
    assert_eq!(hits(&page), vec![thread.first_post_id]);
}

#[sqlx::test]
async fn visitors_find_only_public_posts(pool: PgPool) {
    let s = setup(pool).await;
    let public =
        s.f.thread(s.public, s.ruth, "Open", "The marmalade question.")
            .await;
    let private =
        s.f.thread(s.members, s.ruth, "Closed", "The marmalade answer.")
            .await;
    let response = s.f.get("/search?q=marmalade").await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(hits(&response.body), vec![public.first_post_id]);
    assert!(!response.body.contains("marmalade answer"));
    // Results depend on who is asking and change all the time, so they
    // aren't cached, and visitors still get no cookie.
    assert_eq!(
        response.header(header::CACHE_CONTROL),
        Some(PRIVATE_CACHE_CONTROL)
    );
    assert!(response.header(header::SET_COOKIE).is_none());
    let page = s.f.page_as("/search?q=marmalade", &s.ruth_cookie).await;
    let mut found = hits(&page);
    found.sort();
    assert_eq!(found, vec![public.first_post_id, private.first_post_id]);
}

#[sqlx::test]
async fn removed_posts_and_threads_are_not_found(pool: PgPool) {
    let s = setup(pool).await;
    let thread =
        s.f.thread(s.public, s.ruth, "Snacks", "Biscuits first.")
            .await;
    let deleted =
        s.f.post(thread.thread_id, s.ruth, "Biscuits deleted.")
            .await;
    let hidden =
        s.f.post(thread.thread_id, s.jonah, "Biscuits hidden.")
            .await;
    let gone =
        s.f.thread(s.public, s.ruth, "Gone", "Biscuits in a deleted thread.")
            .await;
    s.f.exec(&format!(
        "UPDATE posts SET deleted_at = now() WHERE id = {deleted}"
    ))
    .await;
    s.f.exec(&format!(
        "UPDATE posts SET hidden_at = now() WHERE id = {hidden}"
    ))
    .await;
    s.f.exec(&format!(
        "UPDATE threads SET deleted_at = now() WHERE id = {}",
        gone.thread_id
    ))
    .await;
    let page = s.f.page_as("/search?q=biscuits", &s.ruth_cookie).await;
    assert_eq!(hits(&page), vec![thread.first_post_id]);
    // Staff, who read hidden posts where they are, find them too.
    let ruth_mod = s.f.member("moddy", "moderator", None).await;
    let cookie = s.f.session(ruth_mod).await;
    let page = s.f.page_as("/search?q=biscuits", &cookie).await;
    let mut found = hits(&page);
    found.sort();
    assert_eq!(found, vec![thread.first_post_id, hidden]);
    assert!(page.contains("Hidden from members"));
}

#[sqlx::test]
async fn conversations_are_found_only_by_the_people_in_them(pool: PgPool) {
    let s = setup(pool).await;
    let kit = s.f.member("kit", "user", None).await;
    let moderator = s.f.member("moddy", "moderator", None).await;
    let created = store::create_conversation(
        &s.f.db,
        &NewConversation {
            author_id: s.ruth,
            title: "Gift ideas".into(),
            recipient_ids: vec![s.jonah],
            body_md: "A secret about the teapot.".into(),
            created_at: None,
        },
    )
    .await
    .unwrap();
    let page = s.f.page_as("/search?q=teapot", &s.ruth_cookie).await;
    assert_eq!(hits(&page), vec![created.first_post_id]);
    assert!(page.contains(&format!("href=\"/p/{}\"", created.first_post_id)));
    let page = s.f.page_as("/search?q=teapot", &s.jonah_cookie).await;
    assert_eq!(hits(&page), vec![created.first_post_id]);
    // Nobody else finds it: not other members, not visitors, and not
    // moderators, even once a message in it has been reported (which lets
    // them read the conversation, but not search it).
    s.f.exec(&format!(
        "INSERT INTO reports (kind, reporter_id, post_id, user_id, thread_id, reason)
         VALUES ('post', {}, {}, {}, {}, 'spam')",
        s.jonah, created.first_post_id, s.ruth, created.thread_id
    ))
    .await;
    for cookie in [
        Some(s.f.session(kit).await),
        Some(s.f.session(moderator).await),
        None,
    ] {
        let response = s.f.get_as("/search?q=teapot", cookie.as_deref()).await;
        assert_eq!(response.status, StatusCode::OK);
        assert!(hits(&response.body).is_empty(), "{cookie:?}");
        assert!(!response.body.contains("A secret"), "{cookie:?}");
    }
    // Someone who leaves no longer finds it.
    store::leave_conversation(&s.f.db, created.thread_id, s.jonah)
        .await
        .unwrap();
    let page = s.f.page_as("/search?q=teapot", &s.jonah_cookie).await;
    assert!(hits(&page).is_empty());
}

/// A conversation found is named for the people in it, whatever its
/// title, and one without a title still has a link to the post.
#[sqlx::test]
async fn conversations_found_say_who_they_are_with(pool: PgPool) {
    let s = setup(pool).await;
    for (title, body) in [
        ("Secret plans", "The teapot plan."),
        ("", "Another teapot."),
    ] {
        store::create_conversation(
            &s.f.db,
            &NewConversation {
                author_id: s.ruth,
                title: title.into(),
                recipient_ids: vec![s.jonah],
                body_md: body.into(),
                created_at: None,
            },
        )
        .await
        .unwrap();
    }
    let page = s.f.page_as("/search?q=teapot", &s.ruth_cookie).await;
    assert_eq!(hits(&page).len(), 2);
    assert!(!page.contains("conversation with Secret plans"), "{page}");
    assert_eq!(
        page.matches("conversation with Jonah<").count(),
        2,
        "{page}"
    );
    assert!(page.contains(">Secret plans</a>"), "{page}");
    assert!(!page.contains("\"></a>"), "{page}");
}

/// Snippets show a post's text, not its Markdown.
#[sqlx::test]
async fn snippets_leave_out_markdown(pool: PgPool) {
    let s = setup(pool).await;
    s.f.thread(
        s.public,
        s.ruth,
        "Quoting",
        "> First **walrus**\n\nAnd a [link](https://example.com) and `code`.",
    )
    .await;
    let page = s.f.page_as("/search?q=walrus", &s.ruth_cookie).await;
    assert_eq!(hits(&page).len(), 1);
    assert!(
        page.contains("First <mark>walrus</mark> And a link and code"),
        "{page}"
    );
    assert!(!page.contains("**"), "{page}");
    assert!(!page.contains("&gt; First"), "{page}");
}

#[sqlx::test]
async fn snippets_are_plain_text(pool: PgPool) {
    let s = setup(pool).await;
    s.f.thread(
        s.public,
        s.ruth,
        "Markup",
        "<script>alert(1)</script> and <b>bold</b> walrus ⟦not a mark⟧",
    )
    .await;
    let page = s.f.page_as("/search?q=walrus", &s.ruth_cookie).await;
    assert_eq!(hits(&page).len(), 1);
    assert!(!page.contains("<script>alert"));
    assert!(!page.contains("<b>bold"));
    assert!(page.contains("<mark>walrus</mark>"));
    assert_eq!(
        page.matches("<mark>").count(),
        page.matches("</mark>").count()
    );
}

#[sqlx::test]
async fn filters_and_pages(pool: PgPool) {
    let s = setup(pool).await;
    let thread = s.f.thread(s.public, s.ruth, "Many", "Zebra one.").await;
    for n in 0..24 {
        let author = if n % 2 == 0 { s.jonah } else { s.ruth };
        s.f.post(thread.thread_id, author, &format!("Zebra number {n}."))
            .await;
    }
    let lounge =
        s.f.thread(s.members, s.ruth, "Lounge", "Zebra in the lounge.")
            .await;
    let page = s.f.page_as("/search?q=zebra", &s.ruth_cookie).await;
    assert_eq!(hits(&page).len(), 20);
    assert!(links_to(&page, "/search?q=zebra&page=2"));
    let page2 = s.f.page_as("/search?q=zebra&page=2", &s.ruth_cookie).await;
    assert_eq!(hits(&page2).len(), 6);
    assert!(!page2.contains("page=3"));
    let page =
        s.f.page_as("/search?q=zebra&in=lounge", &s.ruth_cookie)
            .await;
    assert_eq!(hits(&page), vec![lounge.first_post_id]);
    let page =
        s.f.page_as("/search?q=zebra&by=jonah", &s.ruth_cookie)
            .await;
    assert_eq!(hits(&page).len(), 12);
    // Newest first.
    let page =
        s.f.page_as("/search?q=zebra&sort=new", &s.ruth_cookie)
            .await;
    assert_eq!(hits(&page)[0], lounge.first_post_id);
    // Unknown filters find nothing, rather than everything.
    let page =
        s.f.page_as("/search?q=zebra&by=nobody", &s.ruth_cookie)
            .await;
    assert!(hits(&page).is_empty());
    // Visitors can't use a category they can't read as a filter, and it
    // isn't offered.
    let response = s.f.get("/search?q=zebra&in=lounge").await;
    assert!(hits(&response.body).is_empty());
    assert!(!response.body.contains("value=\"lounge\""));
}

#[sqlx::test]
async fn empty_and_odd_queries(pool: PgPool) {
    let s = setup(pool).await;
    s.f.thread(s.public, s.ruth, "Words", "The and of.").await;
    // No query: just the form.
    let page = s.f.page("/search").await;
    assert!(page.contains("name=\"q\""));
    assert!(hits(&page).is_empty());
    // Only words too common to search for.
    let page = s.f.page("/search?q=the+and").await;
    assert!(hits(&page).is_empty());
    assert!(page.contains("too common"));
    for q in ["%22", "-", "or", "(((", "%00", "a:*", "!!"] {
        let response = s.f.get(&format!("/search?q={q}")).await;
        assert_eq!(response.status, StatusCode::OK, "{q}");
    }
    let long = "x".repeat(300);
    let response = s.f.get(&format!("/search?q={long}")).await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(
        response
            .body
            .contains("Searches can be at most 200 characters long.")
    );
    // Over-long filters are named as what's too long.
    let response =
        s.f.get(&format!("/search?q=words&by={}", "x".repeat(65)))
            .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(
        response
            .body
            .contains("Usernames are at most 64 characters long.")
    );
    assert!(!response.body.contains("Searches can be"));
    let response =
        s.f.get(&format!("/search?q=words&in={}", "x".repeat(101)))
            .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(
        response
            .body
            .contains("Category names are at most 100 characters long.")
    );
    assert!(!response.body.contains("Searches can be"));
    // Pages far past the end aren't searched.
    let response = s.f.get("/search?q=words&page=1000").await;
    assert_eq!(response.status, StatusCode::NOT_FOUND);
    let response = s.f.get("/search?q=words&page=0").await;
    assert_eq!(response.status, StatusCode::NOT_FOUND);
}

#[sqlx::test]
async fn searching_is_rate_limited(pool: PgPool) {
    let f = Forum::new(pool).trusting_proxy();
    let public = f.category("games", "public", None, 0).await;
    let ruth = f.member("ruth", "user", None).await;
    f.thread(public, ruth, "Heat", "Heat.").await;
    let search = |ip: &'static str| {
        let f = &f;
        async move {
            f.request(
                axum::http::Request::get("/search?q=heat")
                    .header("x-forwarded-for", ip)
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .status
        }
    };
    let mut statuses = Vec::new();
    for _ in 0..40 {
        statuses.push(search("10.0.0.1").await);
    }
    assert!(statuses.contains(&StatusCode::TOO_MANY_REQUESTS));
    assert_eq!(statuses[0], StatusCode::OK);
    // Others aren't held up.
    assert_eq!(search("10.0.0.2").await, StatusCode::OK);
}

#[sqlx::test]
async fn the_masthead_has_a_search_box(pool: PgPool) {
    let s = setup(pool).await;
    let page = s.f.page("/").await;
    assert!(page.contains("role=\"search\""));
    assert!(page.contains("action=\"/search\""));
}
