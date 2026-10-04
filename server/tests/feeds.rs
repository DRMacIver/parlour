//! Feeds (DESIGN.md §3.2, milestone 13): every member has a space of their
//! own from the moment their account exists, at their username, which
//! they always run. It stays out of sight (the space index, other
//! members' front pages, search) until its first thread, except on its
//! owner's front page, where it waits in their "Feeds" section.

mod common;

use axum::http::StatusCode;
use common::Forum;
use parlour::changes::Change;
use parlour::layouts;
use parlour::permissions::Grant;
use parlour::store;
use sqlx::PgPool;

struct Setup {
    f: Forum,
    ruth: i64,
    jonah: i64,
    ruth_cookie: String,
    jonah_cookie: String,
    staff_cookie: String,
}

async fn setup(pool: PgPool) -> Setup {
    let f = Forum::new(pool);
    f.no_new_members().await;
    let ruth = f
        .member("ruth", "user", Some("correct horse battery"))
        .await;
    let jonah = f.member("jonah", "user", None).await;
    let sam = f.member("sam", "moderator", None).await;
    Setup {
        ruth_cookie: f.session(ruth).await,
        jonah_cookie: f.session(jonah).await,
        staff_cookie: f.session(sam).await,
        f,
        ruth,
        jonah,
    }
}

async fn feed_id(f: &Forum, owner: i64) -> i64 {
    sqlx::query_scalar("SELECT id FROM spaces WHERE kind = 'feed' AND owner_user_id = $1")
        .bind(owner)
        .fetch_one(&f.db)
        .await
        .unwrap()
}

/// Each grant of a space, as (verb, level, group ids, user ids).
async fn grants(f: &Forum, space_id: i64) -> Vec<(String, String, Vec<i64>, Vec<i64>)> {
    sqlx::query_as(
        "SELECT verb, level, group_ids, user_ids FROM space_grants WHERE space_id = $1
         ORDER BY array_position(ARRAY['read', 'reply', 'start', 'administer'], verb)",
    )
    .bind(space_id)
    .fetch_all(&f.db)
    .await
    .unwrap()
}

fn grant(verb: &str, level: &str, users: &[i64]) -> (String, String, Vec<i64>, Vec<i64>) {
    (verb.into(), level.into(), Vec::new(), users.to_vec())
}

/// Ruth starts a thread in her feed through its form.
async fn start_thread(s: &Setup, title: &str) -> i64 {
    let response =
        s.f.submit(
            "/c/ruth/new",
            &[("title", title), ("body", "What I did today.")],
            Some(&s.ruth_cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    s.f.id(&format!(
        "SELECT id FROM threads WHERE title = '{title}' ORDER BY id DESC LIMIT 1"
    ))
    .await
}

#[sqlx::test]
async fn every_member_has_a_feed_from_the_start(pool: PgPool) {
    let s = setup(pool).await;
    let feed = feed_id(&s.f, s.ruth).await;
    let (slug, name, created_by, archived): (String, String, Option<i64>, bool) = sqlx::query_as(
        "SELECT slug, name, created_by, archived_at IS NOT NULL FROM spaces WHERE id = $1",
    )
    .bind(feed)
    .fetch_one(&s.f.db)
    .await
    .unwrap();
    assert_eq!(
        (slug.as_str(), name.as_str(), created_by, archived),
        ("ruth", "Ruth", Some(s.ruth), false)
    );
    // Members read and reply; only Ruth starts threads, and she runs it.
    assert_eq!(
        grants(&s.f, feed).await,
        [
            grant("read", "members", &[]),
            grant("reply", "members", &[]),
            grant("start", "listed", &[s.ruth]),
            grant("administer", "listed", &[s.ruth]),
        ]
    );
    // It waits in her own Feeds section, so she always sees where to
    // post.
    let layout = layouts::member_id(&s.f.db, s.ruth).await.unwrap().unwrap();
    let sections = layouts::read(&s.f.db, layout)
        .await
        .unwrap()
        .unwrap()
        .sections;
    let feeds = sections.iter().find(|s| s.name == "Feeds").unwrap();
    assert_eq!(
        feeds.spaces.iter().map(|c| c.id).collect::<Vec<_>>(),
        [feed]
    );
    let front = s.f.page_as("/", &s.ruth_cookie).await;
    assert!(front.contains("href=\"/c/ruth\""), "{front}");
    let page = s.f.page_as("/c/ruth", &s.ruth_cookie).await;
    assert!(page.contains("href=\"/c/ruth/new\""), "{page}");
    assert!(page.contains("Your feed"), "{page}");
    // Jonah can look at it, but not start threads in it.
    let page = s.f.page_as("/c/ruth", &s.jonah_cookie).await;
    assert!(!page.contains("href=\"/c/ruth/new\""));
    assert!(page.contains("href=\"/u/ruth\""), "{page}");
    let response =
        s.f.submit(
            "/c/ruth/new",
            &[("title", "Guestbook"), ("body", "Hello")],
            Some(&s.jonah_cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::FORBIDDEN, "{}", response.body);
    // Visitors don't see it: members read it.
    assert_eq!(s.f.get("/c/ruth").await.status, StatusCode::NOT_FOUND);

    // Ruth posts; Jonah replies.
    let thread = start_thread(&s, "Tuesday").await;
    let response =
        s.f.submit(
            &format!("/t/{thread}/reply"),
            &[("body", "Sounds good.")],
            Some(&s.jonah_cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);

    // A member who joins later gets one too, and Ruth's profile leads to
    // hers for those who can read it.
    let maya = s.f.user("maya").await;
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT slug FROM spaces WHERE id = $1")
            .bind(feed_id(&s.f, maya).await)
            .fetch_one(&s.f.db)
            .await
            .unwrap(),
        "maya"
    );
    let profile = s.f.page_as("/u/ruth", &s.jonah_cookie).await;
    assert!(profile.contains("href=\"/c/ruth\""), "{profile}");
    let profile = s.f.page("/u/ruth").await;
    assert!(!profile.contains("href=\"/c/ruth\""), "{profile}");
}

#[sqlx::test]
async fn a_feed_without_threads_is_out_of_sight(pool: PgPool) {
    let s = setup(pool).await;
    let feed = feed_id(&s.f, s.ruth).await;
    let general = s.f.space("general", "public", None, 0).await;
    // Ruth opens her feed to visitors, and an admin puts it on the
    // default front page; Jonah follows it too.
    s.f.set_grant(feed, "read", Grant::public()).await;
    let default = layouts::default_id(&s.f.db).await.unwrap();
    let section = layouts::section_of(&s.f.db, default, general)
        .await
        .unwrap()
        .unwrap();
    assert!(
        layouts::follow(&s.f.db, default, section, feed)
            .await
            .unwrap()
    );
    s.f.changed(Change::SiteChanged).await;
    let jonahs = layouts::member_id(&s.f.db, s.jonah).await.unwrap().unwrap();
    let response =
        s.f.submit("/c/ruth/follow", &[], Some(&s.jonah_cookie))
            .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    assert!(
        layouts::placement(&s.f.db, jonahs, feed)
            .await
            .unwrap()
            .is_some()
    );

    let everywhere = async |shown: bool| {
        for (path, cookie) in [
            ("/", None),
            ("/", Some(s.jonah_cookie.as_str())),
            ("/spaces", None),
            ("/spaces", Some(&s.jonah_cookie)),
            ("/spaces", Some(&s.ruth_cookie)),
            ("/spaces?kind=feed", None),
            ("/latest", None),
            ("/settings/layout", Some(&s.jonah_cookie)),
        ] {
            let response = s.f.get_as(path, cookie).await;
            assert_eq!(response.status, StatusCode::OK, "{path}");
            let body = &response.body;
            if path == "/settings/layout" {
                // Jonah put it in his sections himself, so his editor
                // shows it there to take out; it isn't offered to add.
                continue;
            }
            assert_eq!(
                body.contains("href=\"/c/ruth\""),
                shown,
                "{path} as {cookie:?}: {body}"
            );
        }
        // Its owner always has it on her front page.
        let front = s.f.page_as("/", &s.ruth_cookie).await;
        assert!(front.contains("href=\"/c/ruth\""));
    };
    everywhere(false).await;
    // Nor is it offered to members arranging their front page, once it
    // isn't in their sections.
    let maya = s.f.user("maya").await;
    let maya_cookie = s.f.session(maya).await;
    let response =
        s.f.submit("/c/ruth/unfollow", &[], Some(&maya_cookie))
            .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    let editor = s.f.page_as("/settings/layout", &maya_cookie).await;
    assert!(!editor.contains(">Ruth<"), "{editor}");

    // Its first thread brings it into view, the cached pages included.
    let thread = start_thread(&s, "Tuesday").await;
    everywhere(true).await;
    let editor = s.f.page_as("/settings/layout", &maya_cookie).await;
    assert!(editor.contains(">Ruth<"), "{editor}");
    let found = s.f.page("/search?q=today").await;
    assert!(found.contains("Tuesday"), "{found}");

    // Without it, the feed is out of sight again.
    s.f.exec(&format!(
        "UPDATE threads SET deleted_at = now() WHERE id = {thread}"
    ))
    .await;
    s.f.changed(Change::ThreadChanged {
        thread_id: thread,
        previous_space_id: None,
    })
    .await;
    everywhere(false).await;
}

#[sqlx::test]
async fn the_space_index_filters_by_kind(pool: PgPool) {
    let s = setup(pool).await;
    s.f.space("general", "members", None, 0).await;
    start_thread(&s, "Tuesday").await;
    for (path, topics, feeds) in [
        ("/spaces", true, true),
        ("/spaces?kind=topic", true, false),
        ("/spaces?kind=feed", false, true),
        ("/spaces?sort=name&kind=feed", false, true),
    ] {
        let page = s.f.page_as(path, &s.jonah_cookie).await;
        assert_eq!(page.contains("href=\"/c/general\""), topics, "{path}");
        assert_eq!(page.contains("href=\"/c/ruth\""), feeds, "{path}");
    }
    // Feeds are marked as such.
    let page = s.f.page_as("/spaces", &s.jonah_cookie).await;
    assert!(page.contains("class=\"flag\">Feed<"), "{page}");
    // One address for each view.
    for (path, canonical) in [
        ("/spaces?kind=both", "/spaces"),
        ("/spaces?sort=name&kind=feed", "/spaces?sort=name&kind=feed"),
        (
            "/spaces?kind=feed&archived=1",
            "/spaces?kind=feed&archived=1",
        ),
    ] {
        let response = s.f.get(path).await;
        if path == canonical {
            assert_eq!(response.status, StatusCode::OK, "{path}");
        } else {
            assert_eq!(response.status, StatusCode::PERMANENT_REDIRECT, "{path}");
            assert_eq!(response.location(), canonical);
        }
    }
    let response = s.f.get("/spaces?kind=feed&sort=name").await;
    assert_eq!(response.status, StatusCode::PERMANENT_REDIRECT);
    assert_eq!(response.location(), "/spaces?sort=name&kind=feed");
    for path in ["/spaces?kind=journal", "/spaces?kind=feed&kind=topic"] {
        assert_eq!(s.f.get(path).await.status, StatusCode::NOT_FOUND, "{path}");
    }
}

#[sqlx::test]
async fn the_owner_always_runs_their_feed(pool: PgPool) {
    let s = setup(pool).await;
    let feed = feed_id(&s.f, s.ruth).await;
    // Her settings: no address to change, and no way to stop running it.
    let settings = s.f.page_as("/c/ruth/settings", &s.ruth_cookie).await;
    assert!(!settings.contains("name=\"slug\""), "{settings}");
    assert!(!settings.contains("name=\"administer_level\" value=\"staff\""));
    assert!(settings.contains("You always run your feed"), "{settings}");
    let fields = |administer_level: &'static str, administer_users: &'static str| {
        vec![
            ("name", "Ruth’s notebook"),
            ("description", "Days."),
            ("read_level", "members"),
            ("reply_level", "members"),
            ("start_level", "listed"),
            ("start_users", "ruth"),
            ("administer_level", administer_level),
            ("administer_users", administer_users),
        ]
    };
    // Naming Jonah to help her keeps her too, though the form doesn't
    // name her.
    let response =
        s.f.submit(
            "/c/ruth/settings",
            &fields("listed", "jonah"),
            Some(&s.ruth_cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    assert_eq!(response.location(), "/c/ruth/settings?done=saved");
    let saved = grants(&s.f, feed).await;
    assert_eq!(saved[2], grant("start", "listed", &[s.ruth]));
    assert_eq!(saved[3], grant("administer", "listed", &[s.ruth, s.jonah]));
    let name: String = sqlx::query_scalar("SELECT name FROM spaces WHERE id = $1")
        .bind(feed)
        .fetch_one(&s.f.db)
        .await
        .unwrap();
    assert_eq!(name, "Ruth’s notebook");
    // Jonah can't take her off, nor hand it to the moderators.
    let response =
        s.f.submit(
            "/c/ruth/settings",
            &fields("listed", ""),
            Some(&s.jonah_cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    assert_eq!(
        grants(&s.f, feed).await[3],
        grant("administer", "listed", &[s.ruth])
    );
    let response =
        s.f.submit(
            "/c/ruth/settings",
            &fields("staff", ""),
            Some(&s.ruth_cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(
        response
            .body
            .contains("The owner of a feed always runs it."),
        "{}",
        response.body
    );
    // The address is her username, whatever the form says.
    let mut moved = fields("listed", "");
    moved.push(("slug", "notebook"));
    let response =
        s.f.submit("/c/ruth/settings", &moved, Some(&s.ruth_cookie))
            .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(
        response
            .body
            .contains("A feed’s address is its owner’s username."),
        "{}",
        response.body
    );
    // Nor can the store, or anything else, leave her out.
    for administer in [Grant::listed([], [s.jonah]), Grant::staff()] {
        assert!(
            store::set_grant(&s.f.db, feed, "administer", &administer)
                .await
                .is_err()
        );
    }
    let mut settings = store::SpaceSettings {
        slug: "ruth".into(),
        name: "Ruth".into(),
        description: String::new(),
        read: Grant::members(),
        reply: Grant::members(),
        start: Grant::listed([], [s.ruth]),
        administer: Grant::listed([], [s.jonah]),
    };
    assert_eq!(
        store::update_space(&s.f.db, feed, &settings).await.unwrap(),
        Ok(true)
    );
    assert_eq!(
        grants(&s.f, feed).await[3],
        grant("administer", "listed", &[s.ruth, s.jonah])
    );
    settings.administer = Grant::staff();
    assert_eq!(
        store::update_space(&s.f.db, feed, &settings).await.unwrap(),
        Err("The owner of a feed always runs it.".to_owned())
    );
    // A guestbook: any member may start threads, if she likes.
    settings.administer = Grant::members();
    settings.start = Grant::members();
    assert_eq!(
        store::update_space(&s.f.db, feed, &settings).await.unwrap(),
        Ok(true)
    );

    // A feed can't be deleted, even an empty one, but its owner may
    // archive it and open it again, and so may staff.
    let response =
        s.f.submit("/c/ruth/delete", &[], Some(&s.ruth_cookie))
            .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(
        response.body.contains("A feed can’t be deleted."),
        "{}",
        response.body
    );
    assert_eq!(feed_id(&s.f, s.ruth).await, feed);
    let archived = async || -> bool {
        sqlx::query_scalar("SELECT archived_at IS NOT NULL FROM spaces WHERE id = $1")
            .bind(feed)
            .fetch_one(&s.f.db)
            .await
            .unwrap()
    };
    for (action, cookie, expected) in [
        ("archive", &s.ruth_cookie, true),
        ("unarchive", &s.ruth_cookie, false),
        ("archive", &s.staff_cookie, true),
        ("unarchive", &s.staff_cookie, false),
    ] {
        let response =
            s.f.submit(&format!("/c/ruth/{action}"), &[], Some(cookie))
                .await;
        assert_eq!(response.status, StatusCode::SEE_OTHER, "{action}");
        assert_eq!(archived().await, expected, "{action}");
    }
}

#[sqlx::test]
async fn a_feed_follows_its_owners_name(pool: PgPool) {
    let s = setup(pool).await;
    let feed = feed_id(&s.f, s.ruth).await;
    let named = async || -> (String, String) {
        sqlx::query_as("SELECT slug, name FROM spaces WHERE id = $1")
            .bind(feed)
            .fetch_one(&s.f.db)
            .await
            .unwrap()
    };
    // Its address is her username, whatever she is called.
    s.f.exec(&format!(
        "UPDATE users SET username = 'Ruthie' WHERE id = {}",
        s.ruth
    ))
    .await;
    s.f.changed(Change::UserChanged { user_id: s.ruth }).await;
    assert_eq!(named().await, ("ruthie".to_owned(), "Ruth".to_owned()));
    assert_eq!(
        s.f.get_as("/c/ruthie", Some(&s.jonah_cookie)).await.status,
        StatusCode::OK
    );
    assert_eq!(
        s.f.get_as("/c/ruth", Some(&s.jonah_cookie)).await.status,
        StatusCode::NOT_FOUND
    );
    // Its name is her display name until she gives it another.
    let response =
        s.f.submit(
            "/settings/profile",
            &[("display_name", "Ruth M")],
            Some(&s.ruth_cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    assert_eq!(named().await.1, "Ruth M");
    s.f.exec(&format!(
        "UPDATE spaces SET name = 'Field notes' WHERE id = {feed}"
    ))
    .await;
    let response =
        s.f.submit(
            "/settings/profile",
            &[("display_name", "Ruth")],
            Some(&s.ruth_cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    assert_eq!(named().await.1, "Field notes");
}

#[sqlx::test]
async fn a_deleted_members_feed_is_archived(pool: PgPool) {
    let s = setup(pool).await;
    let feed = feed_id(&s.f, s.ruth).await;
    let thread = start_thread(&s, "Tuesday").await;
    let response =
        s.f.submit(
            "/settings/delete",
            &[("password", "correct horse battery"), ("confirm", "ruth")],
            Some(&s.ruth_cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    let (slug, name, archived): (String, String, bool) =
        sqlx::query_as("SELECT slug, name, archived_at IS NOT NULL FROM spaces WHERE id = $1")
            .bind(feed)
            .fetch_one(&s.f.db)
            .await
            .unwrap();
    // It no longer says whose it was, and is closed to new posts, but
    // stays readable by whoever could read it.
    assert_eq!(slug, format!("deleted_{}", s.ruth));
    assert_eq!(name, "Deleted member");
    assert!(archived);
    let page = s.f.page_as(&format!("/c/{slug}"), &s.jonah_cookie).await;
    assert!(page.contains("Tuesday"), "{page}");
    let response =
        s.f.submit(
            &format!("/t/{thread}/reply"),
            &[("body", "Still here?")],
            Some(&s.jonah_cookie),
        )
        .await;
    assert_ne!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    // Not even staff can open it again: nobody owns it now.
    let response =
        s.f.submit(&format!("/c/{slug}/unarchive"), &[], Some(&s.staff_cookie))
            .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    let archived: bool =
        sqlx::query_scalar("SELECT archived_at IS NOT NULL FROM spaces WHERE id = $1")
            .bind(feed)
            .fetch_one(&s.f.db)
            .await
            .unwrap();
    assert!(archived);
    let settings =
        s.f.page_as(&format!("/c/{slug}/settings"), &s.staff_cookie)
            .await;
    assert!(!settings.contains("/unarchive\""), "{settings}");
}

#[sqlx::test]
async fn topic_spaces_cant_take_a_members_name(pool: PgPool) {
    let s = setup(pool).await;
    let response =
        s.f.submit(
            "/spaces/new",
            &[("name", "Jonah"), ("slug", "jonah"), ("description", "")],
            Some(&s.ruth_cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(
        response.body.contains("That address is a member’s name."),
        "{}",
        response.body
    );
}
