//! The layout editors (DESIGN.md §3.5, §5): a member arranging their own
//! front page at `/settings/layout`, and admins arranging the default
//! front page at `/admin/layout`, each as plain forms and as the island's
//! JSON endpoints. Also the essential flag and "apply to members who
//! haven't edited theirs" on the admin's editor, and the mutes on the
//! member's.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::Forum;
use parlour::islands::IslandAssets;
use parlour::layouts::{self, MuteOutcome};
use parlour::permissions::Grant;
use serde_json::{Value, json};
use sqlx::PgPool;

/// The sections of a layout as `(name, [slugs])`.
async fn shape(f: &Forum, layout_id: i64) -> Vec<(String, Vec<String>)> {
    layouts::read(&f.db, layout_id)
        .await
        .unwrap()
        .expect("the layout exists")
        .sections
        .into_iter()
        .map(|s| {
            (
                s.name,
                s.spaces.into_iter().map(|c| c.slug).collect::<Vec<_>>(),
            )
        })
        .collect()
}

fn section(name: &str, slugs: &[&str]) -> (String, Vec<String>) {
    (
        name.to_owned(),
        slugs.iter().map(|s| (*s).to_owned()).collect(),
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

async fn edited(f: &Forum, layout_id: i64) -> bool {
    layouts::read(&f.db, layout_id)
        .await
        .unwrap()
        .unwrap()
        .edited_at
        .is_some()
}

/// The text of the "add a space" select of one section's form.
fn add_select(page: &str, prefix: &str, section_id: i64) -> String {
    let marker = format!("action=\"{prefix}/sections/{section_id}/spaces\"");
    let start = page
        .find(&marker)
        .unwrap_or_else(|| panic!("no add form for section {section_id} in {page}"));
    let rest = &page[start..];
    let end = rest.find("</select>").unwrap();
    rest[..end].to_owned()
}

/// Sends JSON to an island endpoint as the browser holding `cookie`.
async fn post_json(
    f: &Forum,
    path: &str,
    body: Value,
    cookie: Option<&str>,
) -> (StatusCode, Value) {
    let mut request = Request::post(path)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT, "application/json")
        .header(header::ORIGIN, common::ORIGIN);
    if let Some(cookie) = cookie {
        request = request.header(header::COOKIE, cookie);
    }
    let response = f
        .request(request.body(Body::from(body.to_string())).unwrap())
        .await;
    let json = serde_json::from_str(&response.body)
        .unwrap_or_else(|_| panic!("not JSON ({}): {}", response.status, response.body));
    (response.status, json)
}

/// The props of the first island of this kind on a page.
fn props(page: &str, island: &str) -> Value {
    let start = page
        .find(&format!("data-island=\"{island}\""))
        .unwrap_or_else(|| panic!("no {island} island"));
    let marker = "<script type=\"application/json\" data-island-props>";
    let at = start + page[start..].find(marker).unwrap() + marker.len();
    let end = at + page[at..].find("</script>").unwrap();
    serde_json::from_str(&page[at..end]).unwrap()
}

/// `(name, [slugs])` for each section of a layout as the island has it.
fn island_shape(layout: &Value) -> Vec<(String, Vec<String>)> {
    layout["sections"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["name"].as_str().unwrap().to_owned(),
                s["spaces"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|c| c["slug"].as_str().unwrap().to_owned())
                    .collect(),
            )
        })
        .collect()
}

fn slugs(spaces: &Value) -> Vec<String> {
    spaces
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["slug"].as_str().unwrap().to_owned())
        .collect()
}

const MINE: &str = "/settings/layout";
const DEFAULT: &str = "/admin/layout";

// ---------------------------------------------------------------------------
// A member's own front page

#[sqlx::test]
async fn members_arrange_their_own_front_page_with_plain_forms(pool: PgPool) {
    let f = Forum::new(pool);
    let forum = f.section("Forum", 1).await;
    let general = f.space("general", "public", Some(forum), 0).await;
    let games = f.space("games", "public", Some(forum), 1).await;
    let reading = f.section("Reading", 2).await;
    let books = f.space("books", "public", Some(reading), 0).await;
    let films = f.unlisted_space("films", "public").await;
    let ruth = f.user("ruth").await;
    let cookie = f.session(ruth).await;
    let layout = member_layout(&f, ruth).await;
    let my_forum = section_named(&f, layout, "Forum").await;
    let my_reading = section_named(&f, layout, "Reading").await;

    // The page: every section with its spaces, and the forms for each.
    let page = f.page_as(MINE, &cookie).await;
    assert!(page.contains("is the default one for now"), "{page}");
    assert!(page.contains(&format!("action=\"{MINE}/sections\"")));
    assert!(page.contains(&format!("action=\"{MINE}/sections/{my_forum}\"")));
    assert!(page.contains("value=\"Forum\""));
    assert!(page.contains(&format!("action=\"{MINE}/sections/{my_forum}/move\"")));
    assert!(page.contains(&format!("href=\"{MINE}/sections/{my_forum}/delete\"")));
    assert!(page.contains(&format!("action=\"{MINE}/spaces/{general}/move\"")));
    assert!(page.contains(&format!("action=\"{MINE}/spaces/{general}/remove\"")));
    assert!(page.contains("href=\"/c/general\""));
    // The add select of a section offers what isn't in it: spaces off the
    // front page, and spaces in other sections, saying where they are.
    let select = add_select(&page, MINE, my_forum);
    assert!(select.contains(&format!("value=\"{films}\"")), "{select}");
    assert!(select.contains("Books (in Reading)"), "{select}");
    assert!(
        !select.contains(&format!("value=\"{general}\"")),
        "{select}"
    );
    assert!(page.contains("haven’t muted any"), "{page}");
    // The default's sections aren't offered as the member's own.
    assert!(!page.contains(&format!("sections/{forum}")));
    assert!(!edited(&f, layout).await);

    // A new section, at the end.
    let response = f
        .submit(
            &format!("{MINE}/sections"),
            &[("name", "  Mine ")],
            Some(&cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    assert_eq!(response.location(), format!("{MINE}?done=section"));
    assert_eq!(
        shape(&f, layout).await,
        vec![
            section("Forum", &["general", "games"]),
            section("Reading", &["books"]),
            section("Feeds", &["ruth"]),
            section("Mine", &[]),
        ]
    );
    // The first edit makes the layout the member's own.
    assert!(edited(&f, layout).await);
    let page = f.page_as(&format!("{MINE}?done=section"), &cookie).await;
    assert!(page.contains("Section saved."), "{page}");
    assert!(!page.contains("is the default one for now"), "{page}");
    let mine = section_named(&f, layout, "Mine").await;

    // Renaming.
    let response = f
        .submit(
            &format!("{MINE}/sections/{my_forum}"),
            &[("name", "Home")],
            Some(&cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    assert_eq!(shape(&f, layout).await[0].0, "Home");

    // Moving sections up and down; the top and bottom stay put.
    let move_section = |id: i64, direction: &'static str| {
        let f = &f;
        let cookie = &cookie;
        async move {
            let response = f
                .submit(
                    &format!("{MINE}/sections/{id}/move"),
                    &[("direction", direction)],
                    Some(cookie),
                )
                .await;
            assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
        }
    };
    move_section(mine, "up").await;
    move_section(mine, "up").await;
    assert_eq!(
        shape(&f, layout)
            .await
            .iter()
            .map(|s| s.0.as_str())
            .collect::<Vec<_>>(),
        vec!["Home", "Mine", "Reading", "Feeds"]
    );
    move_section(mine, "down").await;
    assert_eq!(
        shape(&f, layout)
            .await
            .iter()
            .map(|s| s.0.as_str())
            .collect::<Vec<_>>(),
        vec!["Home", "Reading", "Mine", "Feeds"]
    );
    move_section(my_forum, "up").await;
    assert_eq!(shape(&f, layout).await[0].0, "Home");

    // Adding a space to a section: one off the front page, and one from
    // another section, which moves.
    let add = |section_id: i64, space_id: i64| {
        let f = &f;
        let cookie = &cookie;
        async move {
            f.submit(
                &format!("{MINE}/sections/{section_id}/spaces"),
                &[("space", &space_id.to_string())],
                Some(cookie),
            )
            .await
        }
    };
    let response = add(mine, films).await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    let response = add(mine, books).await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    assert_eq!(
        shape(&f, layout).await,
        vec![
            section("Home", &["general", "games"]),
            section("Reading", &[]),
            section("Mine", &["films", "books"]),
            section("Feeds", &["ruth"]),
        ]
    );

    // Moving a space up and down within its section.
    let move_space = |id: i64, direction: &'static str| {
        let f = &f;
        let cookie = &cookie;
        async move {
            let response = f
                .submit(
                    &format!("{MINE}/spaces/{id}/move"),
                    &[("direction", direction)],
                    Some(cookie),
                )
                .await;
            assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
        }
    };
    move_space(books, "up").await;
    assert_eq!(shape(&f, layout).await[2].1, vec!["books", "films"]);
    move_space(books, "up").await;
    assert_eq!(shape(&f, layout).await[2].1, vec!["books", "films"]);
    move_space(books, "down").await;
    move_space(books, "down").await;
    assert_eq!(shape(&f, layout).await[2].1, vec!["films", "books"]);

    // Removing a space from the front page.
    let response = f
        .submit(&format!("{MINE}/spaces/{films}/remove"), &[], Some(&cookie))
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    assert_eq!(shape(&f, layout).await[2].1, vec!["books"]);
    assert_eq!(
        layouts::section_of(&f.db, layout, films).await.unwrap(),
        None
    );

    // Deleting a section asks first, naming its spaces.
    let response = add(my_reading, games).await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    let delete = format!("{MINE}/sections/{my_reading}/delete");
    let confirm = f.page_as(&delete, &cookie).await;
    assert!(confirm.contains("Reading"), "{confirm}");
    assert!(confirm.contains("Games"), "{confirm}");
    assert!(confirm.contains(&format!("action=\"{delete}\"")));
    assert!(confirm.contains(&format!("href=\"{MINE}\"")));
    assert_eq!(shape(&f, layout).await.len(), 4, "asking deleted it");
    let response = f.submit(&delete, &[], Some(&cookie)).await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert_eq!(response.location(), format!("{MINE}?done=section-deleted"));
    assert_eq!(
        shape(&f, layout).await,
        vec![
            section("Home", &["general"]),
            section("Mine", &["books"]),
            section("Feeds", &["ruth"]),
        ]
    );
    assert!(
        f.page_as(&format!("{MINE}?done=section-deleted"), &cookie)
            .await
            .contains("Section deleted.")
    );

    // Bad input is refused, saying what is wrong.
    let response = f
        .submit(
            &format!("{MINE}/sections"),
            &[("name", "   ")],
            Some(&cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(response.body.contains("between 1 and 80 characters"));
    let response = f
        .submit(
            &format!("{MINE}/sections/{mine}/move"),
            &[("direction", "sideways")],
            Some(&cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    let response = f
        .submit(
            &format!("{MINE}/sections/{mine}/spaces"),
            &[("space", "nonsense")],
            Some(&cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::NOT_FOUND);

    // Nobody else's sections, and no space the member can't see.
    let jonah = f.user("jonah").await;
    let jonah_layout = member_layout(&f, jonah).await;
    let his = layouts::add_section(&f.db, jonah_layout, "His")
        .await
        .unwrap();
    let secret = f.unlisted_space("secret", "listed").await;
    f.set_grant(secret, "read", Grant::listed([], [jonah]))
        .await;
    for section_id in [his, forum, 999] {
        for (path, fields) in [
            (
                format!("{MINE}/sections/{section_id}"),
                vec![("name", "Theirs")],
            ),
            (
                format!("{MINE}/sections/{section_id}/move"),
                vec![("direction", "up")],
            ),
            (format!("{MINE}/sections/{section_id}/delete"), vec![]),
            (
                format!("{MINE}/sections/{section_id}/spaces"),
                vec![("space", "1")],
            ),
        ] {
            let response = f.submit(&path, &fields, Some(&cookie)).await;
            assert_eq!(response.status, StatusCode::NOT_FOUND, "{path}");
        }
        assert_eq!(
            f.get_as(
                &format!("{MINE}/sections/{section_id}/delete"),
                Some(&cookie)
            )
            .await
            .status,
            StatusCode::NOT_FOUND
        );
    }
    for space_id in [secret, 999] {
        let response = f
            .submit(
                &format!("{MINE}/sections/{mine}/spaces"),
                &[("space", &space_id.to_string())],
                Some(&cookie),
            )
            .await;
        assert_eq!(response.status, StatusCode::NOT_FOUND, "{space_id}");
        for action in ["move", "remove"] {
            let response = f
                .submit(
                    &format!("{MINE}/spaces/{space_id}/{action}"),
                    &[("direction", "up")],
                    Some(&cookie),
                )
                .await;
            assert_eq!(
                response.status,
                StatusCode::NOT_FOUND,
                "{space_id} {action}"
            );
        }
    }
    assert_eq!(
        shape(&f, jonah_layout).await,
        vec![
            section("Forum", &["general", "games"]),
            section("Reading", &["books"]),
            section("Feeds", &["jonah"]),
            section("His", &[])
        ]
    );

    // The layout is the member's own now: a change to the default doesn't
    // reach it.
    let default = layouts::default_id(&f.db).await.unwrap();
    assert_eq!(
        layouts::rename_section(&f.db, default, forum, "The forum")
            .await
            .unwrap(),
        layouts::Step::Changed
    );
    assert_eq!(shape(&f, layout).await[0].0, "Home");
}

#[sqlx::test]
async fn the_editor_shows_only_what_the_member_can_read_and_keeps_the_rest(pool: PgPool) {
    let f = Forum::new(pool);
    let forum = f.section("Forum", 1).await;
    let secret = f.space("secret", "listed", Some(forum), 0).await;
    let general = f.space("general", "public", Some(forum), 1).await;
    let old = f.space("old", "public", Some(forum), 2).await;
    assert!(
        parlour::store::set_archived(f.state.db(), old, true)
            .await
            .unwrap()
    );
    let films = f.unlisted_space("films", "public").await;
    let ruth = f.user("ruth").await;
    let cookie = f.session(ruth).await;
    let layout = member_layout(&f, ruth).await;
    let my_forum = section_named(&f, layout, "Forum").await;

    let page = f.page_as(MINE, &cookie).await;
    assert!(!page.contains("Secret"), "{page}");
    assert!(!page.contains("/c/secret"), "{page}");
    assert!(page.contains("href=\"/c/general\""));
    // An archived space is shown, flagged, so that it can be tidied away,
    // but isn't offered to add anywhere.
    assert!(page.contains("href=\"/c/old\""), "{page}");
    assert!(page.contains("Archived"), "{page}");
    let select = add_select(&page, MINE, my_forum);
    assert!(select.contains(&format!("value=\"{films}\"")));
    assert!(!select.contains(&format!("value=\"{old}\"")));
    assert!(!select.contains(&format!("value=\"{secret}\"")));

    // Arranging what they can see leaves what they can't where it was.
    let response = f
        .submit(
            &format!("{MINE}/sections/{my_forum}/spaces"),
            &[("space", &films.to_string())],
            Some(&cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    let response = f
        .submit(
            &format!("{MINE}/spaces/{general}/move"),
            &[("direction", "up")],
            Some(&cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    let response = f
        .submit(&format!("{MINE}/spaces/{old}/remove"), &[], Some(&cookie))
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert_eq!(
        shape(&f, layout).await,
        vec![
            section("Forum", &["general", "secret", "films"]),
            section("Feeds", &["ruth"]),
        ]
    );
}

#[sqlx::test]
async fn visitors_log_in_first_and_suspended_members_arrange_nothing(pool: PgPool) {
    let f = Forum::new(pool);
    let forum = f.section("Forum", 1).await;
    let general = f.space("general", "public", Some(forum), 0).await;
    let films = f.unlisted_space("films", "public").await;
    let ruth = f.user("ruth").await;
    let cookie = f.session(ruth).await;
    let layout = member_layout(&f, ruth).await;
    let my_forum = section_named(&f, layout, "Forum").await;

    let response = f.get(MINE).await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert_eq!(response.location(), "/login?next=/settings/layout");
    let films_id = films.to_string();
    let posts = [
        (format!("{MINE}/sections"), vec![("name", "Mine")]),
        (
            format!("{MINE}/sections/{my_forum}"),
            vec![("name", "Home")],
        ),
        (
            format!("{MINE}/sections/{my_forum}/move"),
            vec![("direction", "down")],
        ),
        (format!("{MINE}/sections/{my_forum}/delete"), vec![]),
        (
            format!("{MINE}/sections/{my_forum}/spaces"),
            vec![("space", films_id.as_str())],
        ),
        (
            format!("{MINE}/spaces/{general}/move"),
            vec![("direction", "down")],
        ),
        (format!("{MINE}/spaces/{general}/remove"), vec![]),
    ];
    for (path, fields) in &posts {
        let response = f.submit(path, fields, None).await;
        assert_eq!(response.status, StatusCode::SEE_OTHER, "{path}");
        assert!(response.location().starts_with("/login?next="), "{path}");
    }
    let before = shape(&f, layout).await;

    f.exec(&format!(
        "UPDATE users SET suspended_until = now() + interval '1 day' WHERE id = {ruth}"
    ))
    .await;
    // They can still look.
    f.page_as(MINE, &cookie).await;
    for (path, fields) in &posts {
        let response = f.submit(path, fields, Some(&cookie)).await;
        assert_eq!(response.status, StatusCode::FORBIDDEN, "{path}");
    }
    assert_eq!(shape(&f, layout).await, before);
    assert!(!edited(&f, layout).await);
}

#[sqlx::test]
async fn the_editor_lists_muted_spaces_and_unmutes_them(pool: PgPool) {
    let f = Forum::new(pool);
    let forum = f.section("Forum", 1).await;
    f.space("general", "public", Some(forum), 0).await;
    let games = f.space("games", "public", Some(forum), 1).await;
    let ruth = f.user("ruth").await;
    let cookie = f.session(ruth).await;
    let layout = member_layout(&f, ruth).await;
    let my_forum = section_named(&f, layout, "Forum").await;
    assert_eq!(
        layouts::mute(&f.db, ruth, games).await.unwrap(),
        MuteOutcome::Muted
    );

    let page = f.page_as(MINE, &cookie).await;
    assert!(!page.contains("haven’t muted any"), "{page}");
    assert!(page.contains("action=\"/c/games/unmute\""), "{page}");
    assert!(
        page.contains("name=\"next\" value=\"/settings/layout?done=unmuted\""),
        "{page}"
    );
    // A muted space isn't offered to add to a section: unmute it first.
    let select = add_select(&page, MINE, my_forum);
    assert!(!select.contains(&format!("value=\"{games}\"")), "{select}");

    let response = f
        .submit(
            "/c/games/unmute",
            &[("next", "/settings/layout?done=unmuted")],
            Some(&cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert_eq!(response.location(), "/settings/layout?done=unmuted");
    assert!(!layouts::is_muted(&f.db, ruth, games).await.unwrap());
    let page = f.page_as("/settings/layout?done=unmuted", &cookie).await;
    assert!(page.contains("Unmuted."), "{page}");
    assert!(page.contains("haven’t muted any"), "{page}");
    // Unmuting doesn't put it back; the select now offers it.
    assert_eq!(shape(&f, layout).await[0].1, vec!["general"]);
    let select = add_select(&page, MINE, my_forum);
    assert!(select.contains(&format!("value=\"{games}\"")), "{select}");
    // Muting isn't an edit of the layout.
    assert!(!edited(&f, layout).await);

    // A `next` off this site is ignored.
    assert_eq!(
        layouts::mute(&f.db, ruth, games).await.unwrap(),
        MuteOutcome::Muted
    );
    let response = f
        .submit(
            "/c/games/unmute",
            &[("next", "https://evil.example/")],
            Some(&cookie),
        )
        .await;
    assert_eq!(response.location(), "/c/games");

    // The account settings page leads here.
    assert!(
        f.page_as("/settings", &cookie)
            .await
            .contains("href=\"/settings/layout\"")
    );
    assert!(
        f.page_as("/", &cookie)
            .await
            .contains("href=\"/settings/layout\"")
    );
    assert!(!f.page("/").await.contains("href=\"/settings/layout\""));
}

// ---------------------------------------------------------------------------
// The default front page

struct Admin {
    f: Forum,
    cookie: String,
}

async fn admin(pool: PgPool) -> Admin {
    let f = Forum::new(pool);
    let sam = f.member("sam", "admin", None).await;
    let cookie = f.session(sam).await;
    Admin { f, cookie }
}

impl Admin {
    async fn submit(&self, path: &str, fields: &[(&str, &str)]) -> common::Response {
        self.f.submit(path, fields, Some(&self.cookie)).await
    }

    async fn page(&self, path: &str) -> String {
        self.f.page_as(path, &self.cookie).await
    }
}

#[sqlx::test]
async fn admins_arrange_the_default_front_page_for_everyone_who_follows_it(pool: PgPool) {
    let a = admin(pool).await;
    let f = &a.f;
    let forum = f.section("Forum", 1).await;
    let general = f.space("general", "public", Some(forum), 0).await;
    let games = f.space("games", "public", Some(forum), 1).await;
    let books = f.unlisted_space("books", "public").await;
    let default = layouts::default_id(&f.db).await.unwrap();
    let ruth = f.user("ruth").await;
    let ruth_layout = member_layout(f, ruth).await;
    let jonah = f.user("jonah").await;
    let jonah_layout = member_layout(f, jonah).await;
    assert!(
        layouts::add_section(&f.db, jonah_layout, "His")
            .await
            .unwrap()
            > 0
    );
    let jonah_before = shape(f, jonah_layout).await;

    // Admins only.
    let kit = f.member("kit", "moderator", None).await;
    for cookie in [f.session(kit).await, f.session(ruth).await] {
        assert_eq!(
            f.get_as(DEFAULT, Some(&cookie)).await.status,
            StatusCode::FORBIDDEN
        );
        let response = f
            .submit(
                &format!("{DEFAULT}/sections"),
                &[("name", "Theirs")],
                Some(&cookie),
            )
            .await;
        assert_eq!(response.status, StatusCode::FORBIDDEN);
        let response = f
            .submit(
                &format!("{DEFAULT}/spaces/{books}/essential"),
                &[("essential", "1")],
                Some(&cookie),
            )
            .await;
        assert_eq!(response.status, StatusCode::FORBIDDEN);
        let response = f
            .submit(&format!("{DEFAULT}/apply"), &[], Some(&cookie))
            .await;
        assert_eq!(response.status, StatusCode::FORBIDDEN);
    }
    let response = f.get(DEFAULT).await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert_eq!(response.location(), "/login?next=/admin/layout");

    // The page: the default's sections, the spaces on no section, and how
    // many members follow it. (Sam, Ruth and Kit do; Jonah has his own.)
    let page = a.page(DEFAULT).await;
    assert!(page.contains(&format!("action=\"{DEFAULT}/sections/{forum}\"")));
    assert!(page.contains("href=\"/c/general\""));
    assert!(page.contains("Not on the default front page"), "{page}");
    assert!(page.contains("href=\"/c/books\""), "{page}");
    assert!(page.contains("3 members"), "{page}");
    assert!(
        page.contains(&format!("action=\"{DEFAULT}/apply\"")),
        "{page}"
    );
    assert!(
        page.contains(&format!("action=\"{DEFAULT}/spaces/{books}/essential\"")),
        "{page}"
    );
    assert!(page.contains("href=\"/admin/spaces\""));

    // Visitors' front page is cached; each change below reaches it.
    let index = f.page("/").await;
    assert!(!index.contains("Reading"));

    let response = a
        .submit(&format!("{DEFAULT}/sections"), &[("name", "Reading")])
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    assert_eq!(response.location(), format!("{DEFAULT}?done=section"));
    let reading = section_named(f, default, "Reading").await;
    let response = a
        .submit(
            &format!("{DEFAULT}/sections/{reading}/spaces"),
            &[("space", &books.to_string())],
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    assert_eq!(
        shape(f, default).await,
        vec![
            section("Forum", &["general", "games"]),
            section("Reading", &["books"]),
        ]
    );
    // Ruth's copy followed; Jonah's didn't.
    assert_eq!(
        shape(f, ruth_layout).await,
        vec![
            section("Forum", &["general", "games"]),
            section("Reading", &["books"]),
            section("Feeds", &["ruth"]),
        ]
    );
    assert_eq!(shape(f, jonah_layout).await, jonah_before);
    assert!(!edited(f, ruth_layout).await);
    let index = f.page("/").await;
    assert!(index.contains(">Reading<") && index.contains("href=\"/c/books\""));

    // Renaming, moving, and moving spaces.
    let response = a
        .submit(
            &format!("{DEFAULT}/sections/{reading}"),
            &[("name", "Books and films")],
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    let response = a
        .submit(
            &format!("{DEFAULT}/sections/{reading}/move"),
            &[("direction", "up")],
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    let response = a
        .submit(
            &format!("{DEFAULT}/spaces/{games}/move"),
            &[("direction", "up")],
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    let response = a
        .submit(&format!("{DEFAULT}/spaces/{general}/remove"), &[])
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    let expected = vec![
        section("Books and films", &["books"]),
        section("Forum", &["games"]),
    ];
    assert_eq!(shape(f, default).await, expected);
    let mut ruths = expected.clone();
    ruths.push(section("Feeds", &["ruth"]));
    assert_eq!(shape(f, ruth_layout).await, ruths);
    let index = f.page("/").await;
    assert!(index.contains(">Books and films<"), "{index}");
    assert!(!index.contains("href=\"/c/general\""), "{index}");
    let page = a.page(DEFAULT).await;
    assert!(page.contains("value=\"Books and films\""));

    // Deleting a section asks first.
    let delete = format!("{DEFAULT}/sections/{reading}/delete");
    let confirm = a.page(&delete).await;
    assert!(confirm.contains("Books and films") && confirm.contains("Books"));
    assert!(confirm.contains(&format!("action=\"{delete}\"")));
    assert!(
        confirm.contains("every member who hasn’t arranged their own"),
        "{confirm}"
    );
    let response = a.submit(&delete, &[]).await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert_eq!(
        response.location(),
        format!("{DEFAULT}?done=section-deleted")
    );
    assert_eq!(shape(f, default).await, vec![section("Forum", &["games"])]);
    assert_eq!(
        shape(f, ruth_layout).await,
        vec![section("Forum", &["games"]), section("Feeds", &["ruth"])]
    );
    assert!(!f.page("/").await.contains("Books and films"));

    // A member's own section isn't the admin's to touch.
    let his = section_named(f, jonah_layout, "His").await;
    for (path, fields) in [
        (
            format!("{DEFAULT}/sections/{his}"),
            vec![("name", "Theirs")],
        ),
        (
            format!("{DEFAULT}/sections/{his}/move"),
            vec![("direction", "up")],
        ),
        (format!("{DEFAULT}/sections/{his}/delete"), vec![]),
        (
            format!("{DEFAULT}/sections/{his}/spaces"),
            vec![("space", &*books.to_string())],
        ),
    ] {
        let response = a.submit(&path, &fields).await;
        assert_eq!(response.status, StatusCode::NOT_FOUND, "{path}");
    }
    assert_eq!(
        a.f.get_as(&format!("{DEFAULT}/sections/{his}/delete"), Some(&a.cookie))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(shape(f, jonah_layout).await, jonah_before);

    // Essential, and back, from the editor; both are logged.
    let response = a
        .submit(
            &format!("{DEFAULT}/spaces/{books}/essential"),
            &[("essential", "1")],
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    assert_eq!(response.location(), format!("{DEFAULT}?done=saved"));
    let index = f.page("/").await;
    assert!(index.contains("href=\"/c/books\""), "{index}");
    let log = f.page("/moderation").await;
    assert!(
        log.contains("made <a href=\"/c/books\">Books</a> essential"),
        "{log}"
    );
    let page = a.page(DEFAULT).await;
    assert!(page.contains("Essential"), "{page}");
    let response = a
        .submit(
            &format!("{DEFAULT}/spaces/{books}/essential"),
            &[("essential", "0")],
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert!(!f.page("/").await.contains("href=\"/c/books\""));
    assert!(
        f.page("/moderation")
            .await
            .contains("made <a href=\"/c/books\">Books</a> no longer essential")
    );
    let response = a
        .submit(
            &format!("{DEFAULT}/spaces/999/essential"),
            &[("essential", "1")],
        )
        .await;
    assert_eq!(response.status, StatusCode::NOT_FOUND);

    // "Apply to members who haven't edited theirs" copies the default out
    // again, which puts back what a follower unmuted since the last change.
    assert_eq!(
        layouts::mute(&f.db, ruth, games).await.unwrap(),
        MuteOutcome::Muted
    );
    assert!(layouts::unmute(&f.db, ruth, games).await.unwrap());
    assert_eq!(
        shape(f, ruth_layout).await,
        vec![section("Forum", &[]), section("Feeds", &["ruth"])]
    );
    let response = a.submit(&format!("{DEFAULT}/apply"), &[]).await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    assert_eq!(response.location(), format!("{DEFAULT}?done=applied"));
    assert_eq!(
        shape(f, ruth_layout).await,
        vec![section("Forum", &["games"]), section("Feeds", &["ruth"])]
    );
    assert_eq!(shape(f, jonah_layout).await, jonah_before);
    assert!(
        a.page(&format!("{DEFAULT}?done=applied"))
            .await
            .contains("Applied"),
    );
}

#[sqlx::test]
async fn the_old_section_routes_are_gone_and_the_spaces_page_leads_to_the_editor(pool: PgPool) {
    let a = admin(pool).await;
    let forum = a.f.section("Forum", 1).await;
    a.f.space("general", "public", Some(forum), 0).await;
    let page = a.page("/admin/spaces").await;
    assert!(page.contains("href=\"/admin/layout\""), "{page}");
    assert!(!page.contains("action=\"/admin/sections"), "{page}");
    assert!(!page.contains("New section"), "{page}");
    assert!(page.contains("href=\"/c/general/settings\""));
    for path in [
        "/admin/sections",
        "/admin/sections/1",
        "/admin/sections/1/delete",
    ] {
        assert_eq!(
            a.submit(path, &[("name", "x")]).await.status,
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }
    assert_eq!(
        a.f.get_as("/admin/sections/1/delete", Some(&a.cookie))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    // The admin navigation has the editor.
    assert!(
        a.page("/admin/settings")
            .await
            .contains("href=\"/admin/layout\"")
    );
}

// ---------------------------------------------------------------------------
// The island and its endpoints

fn islands() -> IslandAssets {
    IslandAssets {
        entry: Some("/static/islands/assets/main-test.js".into()),
        preload: Vec::new(),
        css: Vec::new(),
    }
}

#[sqlx::test]
async fn the_layout_editor_island_wraps_the_plain_forms_and_saves_through_the_api(pool: PgPool) {
    let mut f = Forum::new(pool);
    f.state = f.state.clone().with_islands(islands());
    let forum = f.section("Forum", 1).await;
    let general = f.space("general", "public", Some(forum), 0).await;
    f.space("games", "public", Some(forum), 1).await;
    let films = f.unlisted_space("films", "public").await;
    let secret = f.unlisted_space("secret", "listed").await;
    let ruth = f.user("ruth").await;
    let cookie = f.session(ruth).await;
    let layout = member_layout(&f, ruth).await;
    let my_forum = section_named(&f, layout, "Forum").await;
    let feeds = section_named(&f, layout, "Feeds").await;

    // The island wraps the plain forms, and starts from the same layout.
    let page = f.page_as(MINE, &cookie).await;
    assert!(page.contains("data-island=\"layout-editor\""));
    assert!(page.contains(&format!("action=\"{MINE}/sections\"")));
    let props = props(&page, "layout-editor");
    assert_eq!(props["endpoint"], "/api/layout");
    assert_eq!(props["admin"], false);
    assert_eq!(
        island_shape(&props["layout"]),
        vec![
            section("Forum", &["general", "games"]),
            section("Feeds", &["ruth"])
        ]
    );
    assert_eq!(props["layout"]["sections"][0]["id"], my_forum);
    assert_eq!(slugs(&props["layout"]["available"]), vec!["films"]);
    let space = &props["layout"]["sections"][0]["spaces"][0];
    assert_eq!(space["id"], general);
    assert_eq!(space["name"], "General");
    assert_eq!(space["essential"], false);
    assert_eq!(space["archived"], false);

    // Visitors get nothing from the endpoints.
    let (status, _) = post_json(&f, "/api/layout/sections", json!({"name": "Mine"}), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // A new section, a space dragged into it, the sections reordered, the
    // section renamed, the space taken out, the section deleted: each
    // answer is the layout as it now is.
    let (status, body) = post_json(
        &f,
        "/api/layout/sections",
        json!({"name": " Mine "}),
        Some(&cookie),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        island_shape(&body["layout"]),
        vec![
            section("Forum", &["general", "games"]),
            section("Feeds", &["ruth"]),
            section("Mine", &[]),
        ]
    );
    let mine = body["layout"]["sections"][2]["id"].as_i64().unwrap();
    assert!(edited(&f, layout).await);

    let (status, body) = post_json(
        &f,
        &format!("/api/layout/spaces/{films}/move"),
        json!({"section": mine, "index": 0}),
        Some(&cookie),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["layout"]["sections"][2]["spaces"][0]["slug"], "films");
    assert_eq!(slugs(&body["layout"]["available"]), Vec::<String>::new());
    let (status, body) = post_json(
        &f,
        &format!("/api/layout/spaces/{general}/move"),
        json!({"section": mine, "index": 1}),
        Some(&cookie),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        island_shape(&body["layout"]),
        vec![
            section("Forum", &["games"]),
            section("Feeds", &["ruth"]),
            section("Mine", &["films", "general"]),
        ]
    );

    let (status, body) = post_json(
        &f,
        "/api/layout/sections/order",
        json!({"sections": [mine, my_forum, feeds]}),
        Some(&cookie),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        island_shape(&body["layout"])
            .iter()
            .map(|s| s.0.clone())
            .collect::<Vec<_>>(),
        vec!["Mine", "Forum", "Feeds"]
    );
    assert_eq!(
        shape(&f, layout)
            .await
            .iter()
            .map(|s| s.0.clone())
            .collect::<Vec<_>>(),
        vec!["Mine", "Forum", "Feeds"]
    );

    let (status, body) = post_json(
        &f,
        &format!("/api/layout/sections/{mine}"),
        json!({"name": "Ours"}),
        Some(&cookie),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["layout"]["sections"][0]["name"], "Ours");

    let (status, body) = post_json(
        &f,
        &format!("/api/layout/spaces/{films}/remove"),
        json!({}),
        Some(&cookie),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(island_shape(&body["layout"])[0].1, vec!["general"]);
    assert_eq!(slugs(&body["layout"]["available"]), vec!["films"]);

    let (status, body) = post_json(
        &f,
        &format!("/api/layout/sections/{mine}/delete"),
        json!({}),
        Some(&cookie),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        island_shape(&body["layout"]),
        vec![section("Forum", &["games"]), section("Feeds", &["ruth"])]
    );
    assert_eq!(
        slugs(&body["layout"]["available"]),
        vec!["films", "general"]
    );

    // What is refused, and how.
    let (status, body) = post_json(
        &f,
        "/api/layout/sections",
        json!({"name": ""}),
        Some(&cookie),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("between 1 and 80"));
    for (path, body) in [
        (
            format!("/api/layout/sections/{forum}"),
            json!({"name": "Theirs"}),
        ),
        (format!("/api/layout/sections/{forum}/delete"), json!({})),
        (
            format!("/api/layout/spaces/{general}/move"),
            json!({"section": forum, "index": 0}),
        ),
        (
            format!("/api/layout/spaces/{secret}/move"),
            json!({"section": my_forum, "index": 0}),
        ),
        (format!("/api/layout/spaces/{secret}/remove"), json!({})),
        (
            "/api/layout/sections/order".to_owned(),
            json!({"sections": [my_forum, forum]}),
        ),
    ] {
        let (status, _) = post_json(&f, &path, body.clone(), Some(&cookie)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path} {body}");
    }
    assert_eq!(
        shape(&f, layout).await,
        vec![section("Forum", &["games"]), section("Feeds", &["ruth"])]
    );
    let default = layouts::default_id(&f.db).await.unwrap();
    assert_eq!(
        shape(&f, default).await,
        vec![section("Forum", &["general", "games"])]
    );

    // The default's endpoints are for admins, and clear visitors' pages.
    let (status, _) = post_json(
        &f,
        "/api/admin/layout/sections",
        json!({"name": "Reading"}),
        Some(&cookie),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let sam = f.member("sam", "admin", None).await;
    let sam_cookie = f.session(sam).await;
    let page = f.page_as(DEFAULT, &sam_cookie).await;
    let admin_props = self::props(&page, "layout-editor");
    assert_eq!(admin_props["endpoint"], "/api/admin/layout");
    assert_eq!(admin_props["admin"], true);
    assert_eq!(admin_props["layout"]["sections"][0]["id"], forum);
    assert_eq!(
        slugs(&admin_props["layout"]["available"]),
        vec!["films", "secret"]
    );
    assert!(!f.page("/").await.contains("Reading"));
    let (status, body) = post_json(
        &f,
        "/api/admin/layout/sections",
        json!({"name": "Reading"}),
        Some(&sam_cookie),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let reading = body["layout"]["sections"][1]["id"].as_i64().unwrap();
    let (status, body) = post_json(
        &f,
        &format!("/api/admin/layout/spaces/{films}/move"),
        json!({"section": reading, "index": 0}),
        Some(&sam_cookie),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let index = f.page("/").await;
    assert!(
        index.contains(">Reading<") && index.contains("href=\"/c/films\""),
        "{index}"
    );
    // The members who follow the default followed.
    let jonah = f.user("jonah").await;
    assert_eq!(
        shape(&f, member_layout(&f, jonah).await).await,
        vec![
            section("Forum", &["general", "games"]),
            section("Reading", &["films"]),
            section("Feeds", &["jonah"]),
        ]
    );
    // Marking a space essential is logged, like the plain form.
    let (status, body) = post_json(
        &f,
        &format!("/api/admin/layout/spaces/{films}/essential"),
        json!({"essential": true}),
        Some(&sam_cookie),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["layout"]["sections"][1]["spaces"][0]["essential"],
        true
    );
    assert!(
        f.page("/moderation")
            .await
            .contains("made <a href=\"/c/films\">Films</a> essential")
    );
    let (status, _) = post_json(
        &f,
        &format!("/api/layout/spaces/{films}/essential"),
        json!({"essential": false}),
        Some(&sam_cookie),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "only the default has an essential endpoint"
    );
}

#[sqlx::test]
async fn moves_place_a_space_among_the_ones_the_member_can_see(pool: PgPool) {
    let f = Forum::new(pool);
    let forum = f.section("Forum", 1).await;
    let general = f.space("general", "public", Some(forum), 0).await;
    f.space("secret", "listed", Some(forum), 1).await;
    let games = f.space("games", "public", Some(forum), 2).await;
    let films = f.unlisted_space("films", "public").await;
    let ruth = f.user("ruth").await;
    let cookie = f.session(ruth).await;
    let layout = member_layout(&f, ruth).await;
    let my_forum = section_named(&f, layout, "Forum").await;
    let forum_spaces = |f: &Forum| {
        let db = f.db.clone();
        async move {
            layouts::read(&db, layout).await.unwrap().unwrap().sections[0]
                .spaces
                .iter()
                .map(|c| c.slug.clone())
                .collect::<Vec<_>>()
        }
    };
    assert_eq!(forum_spaces(&f).await, ["general", "secret", "games"]);

    // Ruth sees General and Games. Dropping Films before Games (the second
    // space she sees) puts it just before Games, after the space she can't
    // see; the answer shows her only what she can.
    let (status, body) = post_json(
        &f,
        &format!("/api/layout/spaces/{films}/move"),
        json!({"section": my_forum, "index": 1}),
        Some(&cookie),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        forum_spaces(&f).await,
        ["general", "secret", "films", "games"]
    );
    assert_eq!(
        slugs(&body["layout"]["sections"][0]["spaces"]),
        ["general", "films", "games"]
    );

    // Up goes before the previous space she sees; down goes after the next.
    let step = |id: i64, direction: &'static str| {
        let f = &f;
        let cookie = &cookie;
        async move {
            let response = f
                .submit(
                    &format!("{MINE}/spaces/{id}/move"),
                    &[("direction", direction)],
                    Some(cookie),
                )
                .await;
            assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
            assert_eq!(response.location(), format!("{MINE}?done=saved"));
        }
    };
    step(games, "up").await;
    assert_eq!(
        forum_spaces(&f).await,
        ["general", "secret", "games", "films"]
    );
    step(general, "down").await;
    assert_eq!(
        forum_spaces(&f).await,
        ["secret", "games", "general", "films"]
    );
    // An index counts without the space being moved.
    let (status, body) = post_json(
        &f,
        &format!("/api/layout/spaces/{general}/move"),
        json!({"section": my_forum, "index": 0}),
        Some(&cookie),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        forum_spaces(&f).await,
        ["secret", "general", "games", "films"]
    );
    // Without an index, or past the end, it goes last.
    let (status, body) = post_json(
        &f,
        &format!("/api/layout/spaces/{general}/move"),
        json!({"section": my_forum}),
        Some(&cookie),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        forum_spaces(&f).await,
        ["secret", "games", "films", "general"]
    );
}

/// Asserts that an island endpoint answered with the layout.
async fn api_ok(f: &Forum, path: &str, body: Value, cookie: &str) {
    let (status, body) = post_json(f, path, body, Some(cookie)).await;
    assert_eq!(status, StatusCode::OK, "{path}: {body}");
}

#[sqlx::test]
async fn a_step_that_changes_nothing_leaves_the_layout_following_the_default(pool: PgPool) {
    let f = Forum::new(pool);
    let forum = f.section("Forum", 1).await;
    let general = f.space("general", "public", Some(forum), 0).await;
    let games = f.space("games", "public", Some(forum), 1).await;
    let ruth = f.user("ruth").await;
    let cookie = f.session(ruth).await;
    let layout = member_layout(&f, ruth).await;
    let my_forum = section_named(&f, layout, "Forum").await;
    let feeds = section_named(&f, layout, "Feeds").await;
    let before = shape(&f, layout).await;
    assert_eq!(
        before,
        vec![
            section("Forum", &["general", "games"]),
            section("Feeds", &["ruth"])
        ]
    );

    // Renaming a section to the name it has (pressing Enter in its field),
    // through the form and through the island.
    for name in ["Forum", "  Forum "] {
        let response = f
            .submit(
                &format!("{MINE}/sections/{my_forum}"),
                &[("name", name)],
                Some(&cookie),
            )
            .await;
        assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    }
    api_ok(
        &f,
        &format!("/api/layout/sections/{my_forum}"),
        json!({"name": "Forum"}),
        &cookie,
    )
    .await;
    assert!(!edited(&f, layout).await);

    // Dropping a space where it already is: before the space just after
    // it, or at the end of its section when it is last there; adding a
    // space to the section it is last in.
    api_ok(
        &f,
        &format!("/api/layout/spaces/{general}/move"),
        json!({"section": my_forum, "index": 0}),
        &cookie,
    )
    .await;
    api_ok(
        &f,
        &format!("/api/layout/spaces/{games}/move"),
        json!({"section": my_forum}),
        &cookie,
    )
    .await;
    api_ok(
        &f,
        &format!("/api/layout/spaces/{games}/move"),
        json!({"section": my_forum, "index": 1}),
        &cookie,
    )
    .await;
    let response = f
        .submit(
            &format!("{MINE}/sections/{my_forum}/spaces"),
            &[("space", &games.to_string())],
            Some(&cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    assert!(!edited(&f, layout).await);

    // Putting the sections in the order they have.
    api_ok(
        &f,
        "/api/layout/sections/order",
        json!({"sections": [my_forum, feeds]}),
        &cookie,
    )
    .await;
    api_ok(
        &f,
        "/api/layout/sections/order",
        json!({"sections": [my_forum]}),
        &cookie,
    )
    .await;
    assert!(!edited(&f, layout).await);
    assert_eq!(shape(&f, layout).await, before);

    // A real change still counts.
    api_ok(
        &f,
        &format!("/api/layout/spaces/{games}/move"),
        json!({"section": my_forum, "index": 0}),
        &cookie,
    )
    .await;
    assert!(edited(&f, layout).await);
}

#[sqlx::test]
async fn a_step_that_changes_nothing_in_the_default_keeps_the_cached_front_page(pool: PgPool) {
    let a = admin(pool).await;
    let f = &a.f;
    let forum = f.section("Forum", 1).await;
    let general = f.space("general", "public", Some(forum), 0).await;
    let games = f.space("games", "public", Some(forum), 1).await;
    let reading = f.section("Reading", 2).await;
    f.space("books", "public", Some(reading), 0).await;
    let ruth = f.user("ruth").await;
    let ruth_layout = member_layout(f, ruth).await;
    let cached = |f: &Forum| f.state.cache().get("/").is_some();
    f.page("/").await;
    assert!(cached(f));

    let response = a
        .submit(&format!("{DEFAULT}/sections/{forum}"), &[("name", "Forum")])
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    api_ok(
        f,
        &format!("/api/admin/layout/sections/{reading}"),
        json!({"name": "Reading"}),
        &a.cookie,
    )
    .await;
    api_ok(
        f,
        &format!("/api/admin/layout/spaces/{general}/move"),
        json!({"section": forum, "index": 0}),
        &a.cookie,
    )
    .await;
    api_ok(
        f,
        &format!("/api/admin/layout/spaces/{games}/move"),
        json!({"section": forum}),
        &a.cookie,
    )
    .await;
    api_ok(
        f,
        "/api/admin/layout/sections/order",
        json!({"sections": [forum, reading]}),
        &a.cookie,
    )
    .await;
    assert!(cached(f), "a step that changed nothing cleared the cache");
    assert!(!edited(f, ruth_layout).await);

    // A real change clears it.
    api_ok(
        f,
        "/api/admin/layout/sections/order",
        json!({"sections": [reading, forum]}),
        &a.cookie,
    )
    .await;
    assert!(!cached(f));
}

/// The default's editor says how many members use it, without calling
/// that following, which is putting a space in a section (DESIGN.md §3.5).
#[sqlx::test]
async fn the_default_says_who_uses_it(pool: PgPool) {
    let f = Forum::new(pool);
    let admin = f.member("ada", "admin", None).await;
    let cookie = f.session(admin).await;
    f.user("ruth").await;
    let page = f.page_as("/admin/layout", &cookie).await;
    assert!(page.contains("Members using the default"), "{page}");
    assert!(!page.contains("Who follows it"), "{page}");
    assert!(
        page.contains("haven’t arranged their own front page, so it is this one."),
        "{page}"
    );
    assert!(!page.contains("follow this one"), "{page}");
}
