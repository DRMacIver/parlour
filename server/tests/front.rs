//! The front page and `/latest` as layouts shape them (DESIGN.md §3.5):
//! the All, Front page and per-section tabs; unread counts per space and
//! "Mark all read" per section; mutes leaving All, the space index and
//! search; and the Follow, Unfollow, Mute and Unmute forms on a space's
//! page.

mod common;

use axum::http::{StatusCode, header};
use common::Forum;
use parlour::changes::Change;
use parlour::content::{self, Deps, LatestScope};
use parlour::http_cache::{PRIVATE_CACHE_CONTROL, PUBLIC_CACHE_CONTROL};
use parlour::layouts::{self, MuteOutcome};
use parlour::permissions::Grant;
use parlour::viewer::Viewer;
use sqlx::PgPool;

async fn viewer(f: &Forum, user_id: i64) -> Viewer {
    Viewer::User(
        parlour::sessions::user_ctx(f.state.db(), user_id)
            .await
            .unwrap()
            .unwrap(),
    )
}

async fn member_layout(f: &Forum, user_id: i64) -> i64 {
    layouts::member_id(&f.db, user_id)
        .await
        .unwrap()
        .expect("the member has a layout")
}

/// The id of the section called `name` in a layout.
async fn section_named(f: &Forum, layout_id: i64, name: &str) -> i64 {
    layouts::read(&f.db, layout_id)
        .await
        .unwrap()
        .unwrap()
        .sections
        .into_iter()
        .find(|s| s.name == name)
        .unwrap_or_else(|| panic!("no section {name:?}"))
        .id
}

/// The thread titles a `/latest` page lists, in order.
fn titles(page: &str) -> Vec<String> {
    let Some(start) = page.find("<table class=\"list thread-list\">") else {
        return Vec::new();
    };
    page[start..]
        .split("<div class=\"row-title\">")
        .skip(1)
        .map(|row| {
            let link = row.find("</a>").unwrap();
            let before = &row[..link];
            before[before.rfind('>').unwrap() + 1..].to_owned()
        })
        .collect()
}

/// The hrefs of the tabs on a `/latest` page, with the current one marked.
fn tabs(page: &str) -> Vec<(String, bool)> {
    let start = page.find("<nav class=\"tabs\"").expect("no tabs");
    let end = page[start..].find("</nav>").unwrap();
    page[start..start + end]
        .split("<a ")
        .skip(1)
        .map(|a| {
            let href = a.split("href=\"").nth(1).unwrap();
            (
                href[..href.find('"').unwrap()].to_owned(),
                a.contains("aria-current=\"page\""),
            )
        })
        .collect()
}

async fn latest_titles(f: &Forum, v: &Viewer, scope: LatestScope) -> Vec<String> {
    let mut deps = Deps::new();
    content::latest(f.state.db(), v, scope, 1, 100, &mut deps)
        .await
        .unwrap()
        .threads
        .into_iter()
        .map(|row| row.thread.title)
        .collect()
}

// ---------------------------------------------------------------------------
// The tabs

#[sqlx::test]
async fn latest_has_a_tab_for_every_section_and_one_for_the_front_page(pool: PgPool) {
    let f = Forum::new(pool);
    let forum = f.section("Forum", 1).await;
    let general = f.space("general", "public", Some(forum), 0).await;
    let hobbies = f.section("Hobbies", 2).await;
    let games = f.space("games", "public", Some(hobbies), 0).await;
    let lounge = f.space("lounge", "members", Some(hobbies), 1).await;
    let books = f.unlisted_space("books", "public").await;
    let ruth = f.user("ruth").await;
    f.thread(general, ruth, "Welcome", "x").await;
    f.thread(games, ruth, "Spirit Island", "x").await;
    f.thread(lounge, ruth, "Members chat", "x").await;
    f.thread(books, ruth, "Le Guin", "x").await;
    let cookie = f.session(ruth).await;

    // Visitors: All and the default layout's sections.
    let page = f.page("/latest").await;
    assert_eq!(
        tabs(&page),
        vec![
            ("/latest".to_owned(), true),
            (format!("/latest/s/{forum}"), false),
            (format!("/latest/s/{hobbies}"), false),
        ]
    );
    assert_eq!(titles(&page), ["Le Guin", "Spirit Island", "Welcome"]);
    let page = f.page(&format!("/latest/s/{hobbies}")).await;
    assert_eq!(titles(&page), ["Spirit Island"]);
    assert_eq!(
        tabs(&page),
        vec![
            ("/latest".to_owned(), false),
            (format!("/latest/s/{forum}"), false),
            (format!("/latest/s/{hobbies}"), true),
        ]
    );
    assert!(page.contains(">Hobbies<"), "{page}");
    // There is no "your front page" for a visitor: the tab isn't offered,
    // and its address leads to All.
    let response = f.get("/latest/front").await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert_eq!(response.location(), "/latest");

    // Members: All, Front page and the sections of their own layout, as
    // long as there is something in them to see (Ruth's Feeds holds her
    // own feed).
    let ruth_layout = member_layout(&f, ruth).await;
    let my_forum = section_named(&f, ruth_layout, "Forum").await;
    let my_hobbies = section_named(&f, ruth_layout, "Hobbies").await;
    let my_feeds = section_named(&f, ruth_layout, "Feeds").await;
    assert_ne!(my_hobbies, hobbies);
    let page = f.page_as("/latest", &cookie).await;
    assert_eq!(
        tabs(&page),
        vec![
            ("/latest".to_owned(), true),
            ("/latest/front".to_owned(), false),
            (format!("/latest/s/{my_forum}"), false),
            (format!("/latest/s/{my_hobbies}"), false),
            (format!("/latest/s/{my_feeds}"), false),
        ]
    );
    assert_eq!(
        titles(&page),
        ["Le Guin", "Members chat", "Spirit Island", "Welcome"]
    );
    let page = f.page_as("/latest/front", &cookie).await;
    assert_eq!(titles(&page), ["Members chat", "Spirit Island", "Welcome"]);
    assert!(tabs(&page).contains(&("/latest/front".to_owned(), true)));
    let page = f.page_as(&format!("/latest/s/{my_hobbies}"), &cookie).await;
    assert_eq!(titles(&page), ["Members chat", "Spirit Island"]);
    // Another layout's sections aren't theirs to look at.
    assert_eq!(
        f.get_as(&format!("/latest/s/{hobbies}"), Some(&cookie))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    // The headings on the front page lead to the tabs.
    assert!(
        f.page("/")
            .await
            .contains(&format!("href=\"/latest/s/{hobbies}\"")),
    );
    let page = f.page_as("/", &cookie).await;
    assert!(page.contains(&format!("href=\"/latest/s/{my_hobbies}\"")));
    assert!(!page.contains(&format!("href=\"/latest/s/{hobbies}\"")));

    // A section of their own.
    let mine = layouts::add_section(&f.db, ruth_layout, "Mine")
        .await
        .unwrap();
    assert!(
        layouts::follow(&f.db, ruth_layout, mine, books)
            .await
            .unwrap()
    );
    let page = f.page_as("/latest/front", &cookie).await;
    assert_eq!(
        titles(&page),
        ["Le Guin", "Members chat", "Spirit Island", "Welcome"]
    );
    assert!(tabs(&page).contains(&(format!("/latest/s/{mine}"), false)));
    let page = f.page_as(&format!("/latest/s/{mine}"), &cookie).await;
    assert_eq!(titles(&page), ["Le Guin"]);
    assert!(page.contains(">Mine<"), "{page}");

    // An essential space is on the front page, and so on its tab, whether
    // or not it is in a section.
    assert!(
        layouts::unfollow(&f.db, ruth_layout, general)
            .await
            .unwrap()
    );
    assert_eq!(
        latest_titles(&f, &viewer(&f, ruth).await, LatestScope::Front).await,
        ["Le Guin", "Members chat", "Spirit Island"]
    );
    assert!(layouts::set_essential(&f.db, general, true).await.unwrap());
    f.changed(Change::SpaceChanged { space_id: general }).await;
    assert_eq!(
        latest_titles(&f, &viewer(&f, ruth).await, LatestScope::Front).await,
        ["Le Guin", "Members chat", "Spirit Island", "Welcome"]
    );
    // An archived space leaves the front page and its tabs, but not All.
    assert!(
        parlour::store::set_archived(f.state.db(), books, true)
            .await
            .unwrap()
    );
    f.changed(Change::SpaceChanged { space_id: books }).await;
    let v = viewer(&f, ruth).await;
    assert_eq!(
        latest_titles(&f, &v, LatestScope::Front).await,
        ["Members chat", "Spirit Island", "Welcome"]
    );
    assert_eq!(
        latest_titles(&f, &v, LatestScope::Section(mine)).await,
        Vec::<String>::new()
    );
    assert_eq!(
        latest_titles(&f, &v, LatestScope::All).await,
        ["Le Guin", "Members chat", "Spirit Island", "Welcome"]
    );
    // A tab with nothing to show isn't offered, but its page still works.
    let page = f.page_as(&format!("/latest/s/{mine}"), &cookie).await;
    assert!(page.contains("Nothing has been posted"), "{page}");
    assert!(
        !f.page_as("/latest", &cookie)
            .await
            .contains(&format!("/latest/s/{mine}\""))
    );

    // Addresses: one spelling each, and nonsense is not found.
    let response = f.get(&format!("/latest/s/0{hobbies}")).await;
    assert_eq!(response.status, StatusCode::PERMANENT_REDIRECT);
    assert_eq!(response.location(), format!("/latest/s/{hobbies}"));
    let response = f.get(&format!("/latest/s/{hobbies}?page=1")).await;
    assert_eq!(response.status, StatusCode::PERMANENT_REDIRECT);
    assert_eq!(response.location(), format!("/latest/s/{hobbies}"));
    for path in [
        "/latest/s/abc".to_owned(),
        "/latest/s/0".to_owned(),
        format!("/latest/s/{hobbies}?page=2"),
        format!("/latest/s/{}", hobbies + 1000),
    ] {
        assert_eq!(f.get(&path).await.status, StatusCode::NOT_FOUND, "{path}");
    }
    let response = f.get_as("/latest/front?page=1", Some(&cookie)).await;
    assert_eq!(response.status, StatusCode::PERMANENT_REDIRECT);
    assert_eq!(response.location(), "/latest/front");
    assert_eq!(
        f.get_as("/latest/front?page=2", Some(&cookie)).await.status,
        StatusCode::NOT_FOUND
    );
}

#[sqlx::test]
async fn section_tabs_paginate(pool: PgPool) {
    let f = Forum::new(pool);
    let forum = f.section("Forum", 1).await;
    let games = f.space("games", "public", Some(forum), 0).await;
    let ruth = f.user("ruth").await;
    for i in 0..31 {
        f.thread(games, ruth, &format!("Thread number {i:02}"), "x")
            .await;
    }
    let page1 = f.page(&format!("/latest/s/{forum}")).await;
    assert!(page1.contains("Thread number 30"));
    assert!(page1.contains("Thread number 01"));
    assert!(!page1.contains("Thread number 00"));
    assert!(page1.contains(&format!("href=\"/latest/s/{forum}?page=2\"")));
    let page2 = f.page(&format!("/latest/s/{forum}?page=2")).await;
    assert!(page2.contains("Thread number 00"));
    assert!(!page2.contains("Thread number 01"));
    assert_eq!(
        f.get(&format!("/latest/s/{forum}?page=3")).await.status,
        StatusCode::NOT_FOUND
    );
    let cookie = f.session(ruth).await;
    let page2 = f.page_as("/latest/front?page=2", &cookie).await;
    assert!(page2.contains("Thread number 00"));
    assert!(page2.contains("href=\"/latest/front\""));
}

// ---------------------------------------------------------------------------
// Mutes

#[sqlx::test]
async fn muted_spaces_leave_all_the_space_index_and_search_but_keep_working(pool: PgPool) {
    let f = Forum::new(pool).without_rate_limits();
    f.no_new_members().await;
    let forum = f.section("Forum", 1).await;
    let games = f.space("games", "public", Some(forum), 0).await;
    let books = f.space("books", "public", Some(forum), 1).await;
    let ruth = f.member("ruth", "user", None).await;
    let jonah = f.member("jonah", "user", None).await;
    f.thread(games, jonah, "Spirit Island", "A cooperative game.")
        .await;
    f.thread(books, jonah, "Le Guin", "Earthsea again.").await;
    let cookie = f.session(ruth).await;
    assert_eq!(
        layouts::mute(&f.db, ruth, games).await.unwrap(),
        MuteOutcome::Muted
    );

    // All, and the index's list of the latest threads.
    let page = f.page_as("/latest", &cookie).await;
    assert_eq!(titles(&page), ["Le Guin"]);
    assert!(page.contains("muted"), "{page}");
    assert!(!f.page_as("/", &cookie).await.contains("Spirit Island"));
    assert_eq!(
        titles(&f.page("/latest").await),
        ["Le Guin", "Spirit Island"]
    );
    assert!(
        !f.page_as("/latest", &f.session(jonah).await)
            .await
            .contains("muted")
    );

    // The space index, unless asked for them.
    let page = f.page_as("/spaces", &cookie).await;
    assert!(!page.contains("href=\"/c/games\""), "{page}");
    assert!(page.contains("href=\"/spaces?muted=1\""), "{page}");
    let page = f.page_as("/spaces?muted=1", &cookie).await;
    assert!(page.contains("href=\"/c/games\""), "{page}");
    assert!(page.contains(">Muted<"), "{page}");
    assert!(f.page("/spaces").await.contains("href=\"/c/games\""));
    // Visitors have nothing muted, so for them that is just another
    // spelling of the index.
    let response = f.get("/spaces?muted=1").await;
    assert_eq!(response.status, StatusCode::PERMANENT_REDIRECT);
    assert_eq!(response.location(), "/spaces");

    // Search, unless the search is within the space.
    let page = f.page_as("/search?q=cooperative", &cookie).await;
    assert!(!page.contains("Spirit Island"), "{page}");
    assert!(
        f.page("/search?q=cooperative")
            .await
            .contains("Spirit Island")
    );
    let page = f.page_as("/search?q=cooperative&in=games", &cookie).await;
    assert!(page.contains("Spirit Island"), "{page}");

    // The space's own page still works, and they can still post there.
    let page = f.page_as("/c/games", &cookie).await;
    assert!(page.contains("Spirit Island"));
    assert!(page.contains("muted this space"), "{page}");
    let response = f
        .submit(
            "/c/games/new",
            &[("title", "Still here"), ("body", "Posting anyway.")],
            Some(&cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    assert!(f.page_as("/c/games", &cookie).await.contains("Still here"));
    assert_eq!(titles(&f.page_as("/latest", &cookie).await), ["Le Guin"]);
}

// ---------------------------------------------------------------------------
// Following and muting from the space page

#[sqlx::test]
async fn members_follow_unfollow_mute_and_unmute_from_the_space_page(pool: PgPool) {
    let f = Forum::new(pool);
    let forum = f.section("Forum", 1).await;
    let general = f.space("general", "public", Some(forum), 0).await;
    let books = f.unlisted_space("books", "public").await;
    let ruth = f.user("ruth").await;
    let cookie = f.session(ruth).await;
    let ruth_layout = member_layout(&f, ruth).await;

    // Visitors are offered nothing of the sort.
    let page = f.page("/c/books").await;
    assert!(!page.contains("action=\"/c/books/follow\""));
    assert!(!page.contains("action=\"/c/books/mute\""));
    let response = f.submit("/c/books/follow", &[], None).await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert!(response.location().starts_with("/login?next="));

    // Follow puts the space in the first section of the member's layout.
    let page = f.page_as("/c/books", &cookie).await;
    assert!(page.contains("action=\"/c/books/follow\""), "{page}");
    assert!(page.contains("action=\"/c/books/mute\""), "{page}");
    assert!(!page.contains("action=\"/c/books/unfollow\""));
    let response = f.submit("/c/books/follow", &[], Some(&cookie)).await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert_eq!(response.location(), "/c/books");
    let my_forum = section_named(&f, ruth_layout, "Forum").await;
    assert_eq!(
        layouts::section_of(&f.db, ruth_layout, books)
            .await
            .unwrap(),
        Some(my_forum)
    );
    let page = f.page_as("/c/books", &cookie).await;
    assert!(page.contains("action=\"/c/books/unfollow\""), "{page}");
    assert!(!page.contains("action=\"/c/books/follow\""));
    assert!(page.contains("Forum"), "{page}");
    let response = f.submit("/c/books/unfollow", &[], Some(&cookie)).await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert_eq!(
        layouts::section_of(&f.db, ruth_layout, books)
            .await
            .unwrap(),
        None
    );

    // With no sections at all, following makes one called "Following".
    let jonah = f.user("jonah").await;
    let jonah_cookie = f.session(jonah).await;
    let jonah_layout = member_layout(&f, jonah).await;
    for section in layouts::read(&f.db, jonah_layout)
        .await
        .unwrap()
        .unwrap()
        .sections
    {
        assert!(
            layouts::delete_section(&f.db, jonah_layout, section.id)
                .await
                .unwrap()
        );
    }
    let response = f.submit("/c/books/follow", &[], Some(&jonah_cookie)).await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    let layout = layouts::read(&f.db, jonah_layout).await.unwrap().unwrap();
    assert_eq!(layout.sections.len(), 1);
    assert_eq!(layout.sections[0].name, "Following");
    assert_eq!(layout.sections[0].spaces[0].slug, "books");
    assert!(f.page_as("/", &jonah_cookie).await.contains(">Following<"));

    // Mute and unmute.
    let response = f.submit("/c/books/mute", &[], Some(&cookie)).await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert_eq!(response.location(), "/c/books");
    let page = f.page_as("/c/books", &cookie).await;
    assert!(page.contains("action=\"/c/books/unmute\""), "{page}");
    assert!(!page.contains("action=\"/c/books/mute\""));
    assert!(page.contains("muted this space"), "{page}");
    assert!(
        !f.page_as("/spaces", &cookie)
            .await
            .contains("href=\"/c/books\"")
    );
    let response = f.submit("/c/books/unmute", &[], Some(&cookie)).await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    let page = f.page_as("/c/books", &cookie).await;
    assert!(page.contains("action=\"/c/books/mute\""), "{page}");
    assert!(!page.contains("muted this space"));
    // Following a muted space unmutes it.
    assert!(f.submit("/c/books/mute", &[], Some(&cookie)).await.status == StatusCode::SEE_OTHER);
    assert!(f.submit("/c/books/follow", &[], Some(&cookie)).await.status == StatusCode::SEE_OTHER);
    let page = f.page_as("/c/books", &cookie).await;
    assert!(page.contains("action=\"/c/books/mute\""), "{page}");
    assert!(page.contains("action=\"/c/books/unfollow\""), "{page}");

    // Essential spaces can't be muted, and aren't offered it.
    assert!(layouts::set_essential(&f.db, general, true).await.unwrap());
    f.changed(Change::SpaceChanged { space_id: general }).await;
    let page = f.page_as("/c/general", &cookie).await;
    assert!(!page.contains("action=\"/c/general/mute\""), "{page}");
    assert!(page.contains("action=\"/c/general/unfollow\""), "{page}");
    let response = f.submit("/c/general/mute", &[], Some(&cookie)).await;
    assert_eq!(response.status, StatusCode::FORBIDDEN);
    assert!(
        response.body.contains("can’t be muted"),
        "{}",
        response.body
    );

    // Spaces that aren't there, for this member, aren't followed either.
    let secret = f.unlisted_space("secret", "listed").await;
    f.set_grant(secret, "read", Grant::listed([], [jonah]))
        .await;
    for action in ["follow", "unfollow", "mute", "unmute"] {
        let response = f
            .submit(&format!("/c/secret/{action}"), &[], Some(&cookie))
            .await;
        assert_eq!(response.status, StatusCode::NOT_FOUND, "{action}");
        let response = f
            .submit(&format!("/c/nowhere/{action}"), &[], Some(&cookie))
            .await;
        assert_eq!(response.status, StatusCode::NOT_FOUND, "{action}");
    }
    let response = f.submit("/c/secret/follow", &[], Some(&jonah_cookie)).await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
}

/// A suspended member reads what visitors read, but arranges and mutes
/// nothing (decision 20): each form is refused and changes nothing.
#[sqlx::test]
async fn suspended_members_cant_follow_unfollow_mute_or_unmute(pool: PgPool) {
    let f = Forum::new(pool);
    let forum = f.section("Forum", 1).await;
    let general = f.space("general", "public", Some(forum), 0).await;
    let books = f.unlisted_space("books", "public").await;
    let ruth = f.user("ruth").await;
    let cookie = f.session(ruth).await;
    let ruth_layout = member_layout(&f, ruth).await;

    // In good standing: General followed (from the default), Books muted.
    let response = f.submit("/c/books/mute", &[], Some(&cookie)).await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    let my_forum = section_named(&f, ruth_layout, "Forum").await;
    // Where General and Books are in Ruth's layout, and whether each is muted.
    async fn arrangement(
        db: &PgPool,
        layout: i64,
        user: i64,
        spaces: (i64, i64),
    ) -> (Option<i64>, Option<i64>, bool, bool) {
        (
            layouts::section_of(db, layout, spaces.0).await.unwrap(),
            layouts::section_of(db, layout, spaces.1).await.unwrap(),
            layouts::is_muted(db, user, spaces.0).await.unwrap(),
            layouts::is_muted(db, user, spaces.1).await.unwrap(),
        )
    }
    let before = (Some(my_forum), None, false, true);
    assert_eq!(
        arrangement(&f.db, ruth_layout, ruth, (general, books)).await,
        before
    );

    f.exec(&format!(
        "UPDATE users SET suspended_until = now() + interval '1 day' WHERE id = {ruth}"
    ))
    .await;
    // The pages still render for them.
    f.page_as("/c/general", &cookie).await;
    f.page_as("/c/books", &cookie).await;
    for path in [
        "/c/books/follow",
        "/c/general/unfollow",
        "/c/general/mute",
        "/c/books/unmute",
    ] {
        let response = f.submit(path, &[], Some(&cookie)).await;
        assert_eq!(response.status, StatusCode::FORBIDDEN, "{path}");
        assert_eq!(
            arrangement(&f.db, ruth_layout, ruth, (general, books)).await,
            before,
            "{path} changed something"
        );
    }

    // Once the suspension is over, the forms work again.
    f.exec(&format!(
        "UPDATE users SET suspended_until = now() - interval '1 second' WHERE id = {ruth}"
    ))
    .await;
    let response = f.submit("/c/books/follow", &[], Some(&cookie)).await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert_eq!(
        arrangement(&f.db, ruth_layout, ruth, (general, books)).await,
        (Some(my_forum), Some(my_forum), false, false)
    );
}

/// Following and unfollowing write to the member's own layout and never
/// to the default: a member who somehow has no layout (every account is
/// given one, so this is belt and braces) is given a copy first, and the
/// default, and everyone following it, are untouched.
#[sqlx::test]
async fn following_without_a_layout_makes_one_rather_than_editing_the_default(pool: PgPool) {
    let f = Forum::new(pool);
    let forum = f.section("Forum", 1).await;
    let general = f.space("general", "public", Some(forum), 0).await;
    let books = f.unlisted_space("books", "public").await;
    let default = layouts::default_id(&f.db).await.unwrap();
    let ruth = f.user("ruth").await;
    let cookie = f.session(ruth).await;
    let jonah = f.user("jonah").await;
    let jonah_layout = member_layout(&f, jonah).await;

    f.exec(&format!("DELETE FROM layouts WHERE user_id = {ruth}"))
        .await;
    assert_eq!(layouts::member_id(&f.db, ruth).await.unwrap(), None);
    let response = f.submit("/c/general/unfollow", &[], Some(&cookie)).await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    assert_eq!(
        layouts::section_of(&f.db, default, general).await.unwrap(),
        Some(forum),
        "the default lost General"
    );
    let ruth_layout = member_layout(&f, ruth).await;
    assert_eq!(
        layouts::section_of(&f.db, ruth_layout, general)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        layouts::section_of(&f.db, jonah_layout, general)
            .await
            .unwrap(),
        Some(section_named(&f, jonah_layout, "Forum").await)
    );
    assert!(
        f.page_as("/", &f.session(jonah).await)
            .await
            .contains("href=\"/c/general\"")
    );

    f.exec(&format!("DELETE FROM layouts WHERE user_id = {ruth}"))
        .await;
    let response = f.submit("/c/books/follow", &[], Some(&cookie)).await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    assert_eq!(
        layouts::section_of(&f.db, default, books).await.unwrap(),
        None,
        "the default gained Books"
    );
    let ruth_layout = member_layout(&f, ruth).await;
    assert_eq!(
        layouts::section_of(&f.db, ruth_layout, books)
            .await
            .unwrap(),
        Some(section_named(&f, ruth_layout, "Forum").await)
    );
    assert!(f.page_as("/", &cookie).await.contains("href=\"/c/books\""));
    assert!(
        !f.page_as("/", &f.session(jonah).await)
            .await
            .contains("href=\"/c/books\"")
    );
}

// ---------------------------------------------------------------------------
// Unread counts and marking a section read

#[sqlx::test]
async fn the_front_page_counts_unread_threads_and_sections_can_be_marked_read(pool: PgPool) {
    let f = Forum::new(pool);
    let forum = f.section("Forum", 1).await;
    let games = f.space("games", "public", Some(forum), 0).await;
    let books = f.space("books", "public", Some(forum), 1).await;
    let ruth = f.user("ruth").await;
    let jonah = f.user("jonah").await;
    f.thread(games, jonah, "Spirit Island", "x").await;
    f.thread(games, jonah, "Heat", "x").await;
    f.thread(books, jonah, "Le Guin", "x").await;
    let mine = f.thread(games, ruth, "Mine", "x").await;
    let cookie = f.session(ruth).await;

    async fn unread(f: &Forum, v: &Viewer) -> Vec<(String, Option<i64>)> {
        let mut deps = Deps::new();
        content::front_page(f.state.db(), v, &mut deps)
            .await
            .unwrap()
            .sections
            .into_iter()
            .flat_map(|s| s.spaces)
            .map(|s| (s.space.slug, s.unread))
            .collect()
    }
    assert_eq!(
        unread(&f, &Viewer::Anonymous).await,
        [("games".to_owned(), None), ("books".to_owned(), None)]
    );
    assert_eq!(
        unread(&f, &viewer(&f, ruth).await).await,
        [
            ("games".to_owned(), Some(2)),
            ("books".to_owned(), Some(1)),
            ("ruth".to_owned(), Some(0))
        ]
    );
    let page = f.page_as("/", &cookie).await;
    assert!(page.contains("2 unread"), "{page}");
    assert!(page.contains("1 unread"), "{page}");
    assert!(!f.page("/").await.contains("unread"));

    // Each section offers "Mark all read", on the front page and its tab.
    let ruth_layout = member_layout(&f, ruth).await;
    let my_forum = section_named(&f, ruth_layout, "Forum").await;
    let action = format!("action=\"/latest/s/{my_forum}/read\"");
    assert!(page.contains(&action), "{page}");
    let page = f.page_as(&format!("/latest/s/{my_forum}"), &cookie).await;
    assert!(page.contains(&action), "{page}");
    assert!(!f.page("/").await.contains("/read\""));
    let response = f
        .submit(&format!("/latest/s/{my_forum}/read"), &[], Some(&cookie))
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert_eq!(response.location(), format!("/latest/s/{my_forum}"));
    assert_eq!(
        unread(&f, &viewer(&f, ruth).await).await,
        [
            ("games".to_owned(), Some(0)),
            ("books".to_owned(), Some(0)),
            ("ruth".to_owned(), Some(0))
        ]
    );
    assert!(!f.page_as("/", &cookie).await.contains("unread"));

    // A reply makes its thread unread again; the form from the front page
    // comes back there.
    f.post(mine.thread_id, jonah, "reply").await;
    assert_eq!(
        unread(&f, &viewer(&f, ruth).await).await,
        [
            ("games".to_owned(), Some(1)),
            ("books".to_owned(), Some(0)),
            ("ruth".to_owned(), Some(0))
        ]
    );
    let response = f
        .submit(
            &format!("/latest/s/{my_forum}/read"),
            &[("next", "/")],
            Some(&cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert_eq!(response.location(), "/");
    assert_eq!(
        unread(&f, &viewer(&f, ruth).await).await,
        [
            ("games".to_owned(), Some(0)),
            ("books".to_owned(), Some(0)),
            ("ruth".to_owned(), Some(0))
        ]
    );

    // Only the member's own sections, and only members.
    let response = f
        .submit(&format!("/latest/s/{forum}/read"), &[], Some(&cookie))
        .await;
    assert_eq!(response.status, StatusCode::NOT_FOUND);
    let response = f
        .submit(&format!("/latest/s/{forum}/read"), &[], None)
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert!(response.location().starts_with("/login?next="));
}

// ---------------------------------------------------------------------------
// The cache

#[sqlx::test]
async fn visitors_tab_pages_come_from_the_cache_and_clear_with_what_they_show(pool: PgPool) {
    let f = Forum::new(pool);
    let forum = f.section("Forum", 1).await;
    let games = f.space("games", "public", Some(forum), 0).await;
    let ruth = f.user("ruth").await;
    let t = f.thread(games, ruth, "Spirit Island", "x").await;
    let path = format!("/latest/s/{forum}");

    let response = f.get(&path).await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(
        response.header(header::CACHE_CONTROL),
        Some(PUBLIC_CACHE_CONTROL)
    );
    assert_eq!(f.state.cache().len(), 1);
    f.page(&path).await;
    assert_eq!(f.state.cache().len(), 1);

    // A new post in a space on the tab clears it, once reported.
    let jonah = f.user("jonah").await;
    let post = f.post(t.thread_id, jonah, "A reply").await;
    assert!(!f.page(&path).await.contains(">Jonah</a>"));
    f.changed(Change::PostCreated { post_id: post }).await;
    assert!(f.page(&path).await.contains(">Jonah</a>"));
    // A new thread in a space off the tab doesn't appear on it.
    let books = f.unlisted_space("books", "public").await;
    let other = f.thread(books, ruth, "Le Guin", "x").await;
    f.changed(Change::PostCreated {
        post_id: other.first_post_id,
    })
    .await;
    assert!(!f.page(&path).await.contains("Le Guin"));
    // Changing the default layout reaches the tab.
    assert_eq!(
        layouts::rename_section(
            &f.db,
            layouts::default_id(&f.db).await.unwrap(),
            forum,
            "The forum"
        )
        .await
        .unwrap(),
        layouts::Step::Changed
    );
    f.changed(Change::SiteChanged).await;
    assert!(f.page(&path).await.contains(">The forum<"));
    assert!(
        layouts::follow(
            &f.db,
            layouts::default_id(&f.db).await.unwrap(),
            forum,
            books
        )
        .await
        .unwrap()
    );
    f.changed(Change::SiteChanged).await;
    assert!(f.page(&path).await.contains("Le Guin"));
    assert!(
        layouts::delete_section(&f.db, layouts::default_id(&f.db).await.unwrap(), forum)
            .await
            .unwrap()
    );
    f.changed(Change::SiteChanged).await;
    assert_eq!(f.get(&path).await.status, StatusCode::NOT_FOUND);

    // Members' pages are their own.
    let cookie = f.session(ruth).await;
    let response = f.get_as("/latest/front", Some(&cookie)).await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(
        response.header(header::CACHE_CONTROL),
        Some(PRIVATE_CACHE_CONTROL)
    );
}
