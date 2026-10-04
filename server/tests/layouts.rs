//! Layouts (DESIGN.md §3.5): the default front page and each member's
//! copy of it, essential spaces, and mutes.

mod common;

use axum::http::StatusCode;
use common::Forum;
use parlour::content::{self, Deps};
use parlour::layouts::{self, MuteOutcome};
use parlour::viewer::Viewer;
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

/// The slugs on the front page as the viewer sees it: the essential block
/// and then each section.
async fn front_page(f: &Forum, viewer: &Viewer) -> (Vec<String>, Vec<(String, Vec<String>)>) {
    let mut deps = Deps::new();
    let page = content::front_page(f.state.db(), viewer, &mut deps)
        .await
        .unwrap();
    (
        page.essential
            .iter()
            .map(|s| s.space.slug.clone())
            .collect(),
        page.sections
            .into_iter()
            .map(|s| {
                (
                    s.name,
                    s.spaces
                        .into_iter()
                        .map(|c| c.space.slug)
                        .collect::<Vec<_>>(),
                )
            })
            .collect(),
    )
}

async fn viewer(f: &Forum, user_id: i64) -> Viewer {
    Viewer::User(
        parlour::sessions::user_ctx(f.state.db(), user_id)
            .await
            .unwrap()
            .unwrap(),
    )
}

// ---------------------------------------------------------------------------
// Copies

#[sqlx::test]
async fn every_new_member_gets_a_copy_of_the_default_with_a_feeds_section(pool: PgPool) {
    let f = Forum::new(pool);
    let forum = f.section("Forum", 1).await;
    f.space("announcements", "public", Some(forum), 0).await;
    f.space("general", "public", Some(forum), 1).await;
    let default = layouts::default_id(&f.db).await.unwrap();
    assert_eq!(
        shape(&f, default).await,
        vec![section("Forum", &["announcements", "general"])]
    );

    // Members made every way an account is made have a copy: the fixture
    // goes through `store::create_user`, as the seed does.
    let ruth = f.user("ruth").await;
    let ruth_layout = member_layout(&f, ruth).await;
    assert_ne!(ruth_layout, default);
    assert_eq!(
        shape(&f, ruth_layout).await,
        vec![
            section("Forum", &["announcements", "general"]),
            section("Feeds", &["ruth"]),
        ]
    );
    let layout = layouts::read(&f.db, ruth_layout).await.unwrap().unwrap();
    assert_eq!(layout.user_id, Some(ruth));
    assert!(layout.edited_at.is_none());

    // Asking again for a copy changes nothing.
    assert_eq!(
        layouts::create_member_copy(&f.db, ruth).await.unwrap(),
        ruth_layout
    );

    // Someone who joins with an invite has one too.
    let sam = f.member("sam", "admin", None).await;
    let cookie = f.session(sam).await;
    let response = f
        .submit(
            "/invites",
            &[("email", ""), ("note", ""), ("days", "7"), ("uses", "1")],
            Some(&cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    let marker = "class=\"link-box\" type=\"text\" readonly value=\"";
    let start = response.body.find(marker).expect("no invite link shown") + marker.len();
    let link = &response.body[start..start + response.body[start..].find('"').unwrap()];
    let link = link.strip_prefix(common::ORIGIN).unwrap();
    let response = f
        .submit(
            link,
            &[
                ("username", "jonah"),
                ("display_name", "Jonah"),
                ("email", "jonah@example.com"),
                ("password", ""),
            ],
            None,
        )
        .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    assert!(response.body.contains("Check your email"));
    let message = f.last_email_to("jonah@example.com").await;
    let join = common::link_in(&message.body, "/join/");
    let response = f.submit(&join, &[("username", "jonah")], None).await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    let jonah = f.user_id("jonah").await;
    assert_eq!(
        shape(&f, member_layout(&f, jonah).await).await,
        vec![
            section("Forum", &["announcements", "general"]),
            section("Feeds", &["jonah"]),
        ]
    );
}

#[sqlx::test]
async fn changing_the_default_reaches_members_who_havent_edited_theirs(pool: PgPool) {
    let f = Forum::new(pool);
    let forum = f.section("Forum", 1).await;
    f.space("general", "public", Some(forum), 0).await;
    let ruth = f.user("ruth").await;
    let jonah = f.user("jonah").await;
    let default = layouts::default_id(&f.db).await.unwrap();
    let ruth_layout = member_layout(&f, ruth).await;
    let jonah_layout = member_layout(&f, jonah).await;

    // Jonah rearranges his: from now on it is his own.
    let hobbies = f.space("hobbies", "public", None, 0).await;
    let mine = layouts::add_section(&f.db, jonah_layout, "Mine")
        .await
        .unwrap();
    assert!(
        layouts::follow(&f.db, jonah_layout, mine, hobbies)
            .await
            .unwrap()
    );
    assert!(
        layouts::read(&f.db, jonah_layout)
            .await
            .unwrap()
            .unwrap()
            .edited_at
            .is_some()
    );

    // The admin adds a section to the default and a space to Forum.
    let games = f.space("games", "public", None, 0).await;
    let books = f.space("books", "public", None, 0).await;
    let reading = layouts::add_section(&f.db, default, "Reading")
        .await
        .unwrap();
    assert!(
        layouts::follow(&f.db, default, reading, books)
            .await
            .unwrap()
    );
    assert!(layouts::follow(&f.db, default, forum, games).await.unwrap());
    assert_eq!(
        layouts::rename_section(&f.db, default, forum, "The forum")
            .await
            .unwrap(),
        layouts::Step::Changed
    );

    // Ruth's copy follows; Jonah's is left alone, Feeds and all. (`games`
    // and `books` went into the fixture's catch-all "Spaces" section
    // first; a space is in one section of a layout at most, so following
    // them elsewhere moved them.)
    let expected_default = vec![
        section("Spaces", &["hobbies"]),
        section("The forum", &["general", "games"]),
        section("Reading", &["books"]),
    ];
    assert_eq!(shape(&f, default).await, expected_default);
    let mut expected_ruth = expected_default.clone();
    expected_ruth.push(section("Feeds", &["ruth"]));
    assert_eq!(shape(&f, ruth_layout).await, expected_ruth);
    assert_eq!(
        shape(&f, jonah_layout).await,
        vec![
            section("Spaces", &[]),
            section("Forum", &["general"]),
            section("Feeds", &["jonah"]),
            section("Mine", &["hobbies"]),
        ]
    );

    // What Ruth's Feeds section holds stays through a re-copy.
    let feeds = layouts::read(&f.db, ruth_layout)
        .await
        .unwrap()
        .unwrap()
        .sections
        .into_iter()
        .find(|s| s.name == "Feeds")
        .unwrap()
        .id;
    assert!(
        layouts::unfollow(&f.db, ruth_layout, hobbies)
            .await
            .unwrap()
    );
    f.exec(&format!(
        "INSERT INTO layout_section_spaces (section_id, space_id, position) VALUES ({feeds}, {hobbies}, 0)"
    ))
    .await;
    f.exec(&format!(
        "UPDATE layouts SET edited_at = NULL WHERE id = {ruth_layout}"
    ))
    .await;
    assert!(
        layouts::delete_section(&f.db, default, reading)
            .await
            .unwrap()
    );
    assert_eq!(
        shape(&f, ruth_layout).await,
        vec![
            section("Spaces", &[]),
            section("The forum", &["general", "games"]),
            section("Feeds", &["ruth", "hobbies"]),
        ]
    );
}

#[sqlx::test]
async fn sections_and_spaces_are_arranged_in_order(pool: PgPool) {
    let f = Forum::new(pool);
    let ruth = f.user("ruth").await;
    let layout = member_layout(&f, ruth).await;
    let games = f.unlisted_space("games", "public").await;
    let books = f.unlisted_space("books", "public").await;
    let films = f.unlisted_space("films", "public").await;
    let a = layouts::add_section(&f.db, layout, "A").await.unwrap();
    let b = layouts::add_section(&f.db, layout, "B").await.unwrap();
    assert!(layouts::follow(&f.db, layout, a, games).await.unwrap());
    assert!(layouts::follow(&f.db, layout, a, books).await.unwrap());
    assert!(layouts::follow(&f.db, layout, a, films).await.unwrap());
    assert_eq!(
        layouts::section_of(&f.db, layout, films).await.unwrap(),
        Some(a)
    );
    assert_eq!(
        shape(&f, layout).await,
        vec![
            section("Feeds", &["ruth"]),
            section("A", &["games", "books", "films"]),
            section("B", &[]),
        ]
    );

    // Following a space again puts it where asked and nowhere else.
    assert!(layouts::follow(&f.db, layout, b, books).await.unwrap());
    assert_eq!(
        layouts::move_space(&f.db, layout, films, a, 0)
            .await
            .unwrap(),
        layouts::Step::Changed
    );
    assert_eq!(
        layouts::move_section(&f.db, layout, b, -1).await.unwrap(),
        layouts::Step::Changed
    );
    assert_eq!(
        shape(&f, layout).await,
        vec![
            section("B", &["books"]),
            section("Feeds", &["ruth"]),
            section("A", &["films", "games"]),
        ]
    );
    assert!(layouts::unfollow(&f.db, layout, games).await.unwrap());
    assert!(!layouts::unfollow(&f.db, layout, games).await.unwrap());
    assert!(layouts::delete_section(&f.db, layout, b).await.unwrap());
    assert!(!layouts::delete_section(&f.db, layout, b).await.unwrap());
    assert_eq!(
        shape(&f, layout).await,
        vec![section("Feeds", &["ruth"]), section("A", &["films"])]
    );
    assert_eq!(
        layouts::section_of(&f.db, layout, books).await.unwrap(),
        None
    );

    // Another member's section isn't reachable through this layout.
    let jonah = f.user("jonah").await;
    let jonah_layout = member_layout(&f, jonah).await;
    let his = layouts::add_section(&f.db, jonah_layout, "His")
        .await
        .unwrap();
    assert!(!layouts::follow(&f.db, layout, his, games).await.unwrap());
    assert_eq!(
        layouts::rename_section(&f.db, layout, his, "Mine")
            .await
            .unwrap(),
        layouts::Step::NotFound
    );
    assert!(!layouts::delete_section(&f.db, layout, his).await.unwrap());
    assert_eq!(
        layouts::move_space(&f.db, layout, films, his, 0)
            .await
            .unwrap(),
        layouts::Step::NotFound
    );
    assert_eq!(
        shape(&f, jonah_layout).await,
        vec![section("Feeds", &["jonah"]), section("His", &[])]
    );

    // A space can't be in two sections of one layout, however it gets there.
    let a2 = layouts::add_section(&f.db, layout, "A2").await.unwrap();
    let result = sqlx::query(
        "INSERT INTO layout_section_spaces (section_id, space_id, position) VALUES ($1, $2, 0)",
    )
    .bind(a2)
    .bind(films)
    .execute(&f.db)
    .await;
    assert!(result.is_err());
}

// ---------------------------------------------------------------------------
// The front page

#[sqlx::test]
async fn visitors_see_the_default_and_members_their_own(pool: PgPool) {
    let f = Forum::new(pool);
    let forum = f.section("Forum", 1).await;
    f.space("general", "public", Some(forum), 0).await;
    let games = f.space("games", "public", Some(forum), 1).await;
    let ruth = f.user("ruth").await;
    let ruth_layout = member_layout(&f, ruth).await;
    let mine = layouts::add_section(&f.db, ruth_layout, "Mine")
        .await
        .unwrap();
    assert!(
        layouts::follow(&f.db, ruth_layout, mine, games)
            .await
            .unwrap()
    );

    assert_eq!(
        front_page(&f, &Viewer::Anonymous).await,
        (vec![], vec![section("Forum", &["general", "games"])])
    );
    assert_eq!(
        front_page(&f, &viewer(&f, ruth).await).await,
        (
            vec![],
            vec![
                section("Forum", &["general"]),
                section("Feeds", &["ruth"]),
                section("Mine", &["games"]),
            ]
        )
    );
    // Empty sections aren't shown.
    let page = f.page("/").await;
    assert_in_order(
        &page,
        &[">Forum<", "href=\"/c/general\"", "href=\"/c/games\""],
    );
    assert!(!page.contains("Mine"));
    let page = f.page_as("/", &f.session(ruth).await).await;
    assert_in_order(
        &page,
        &[
            ">Forum<",
            "href=\"/c/general\"",
            ">Feeds<",
            "href=\"/c/ruth\"",
            ">Mine<",
            "href=\"/c/games\"",
        ],
    );
}

#[sqlx::test]
async fn essential_spaces_come_first_whatever_the_layout(pool: PgPool) {
    let f = Forum::new(pool);
    let forum = f.section("Forum", 1).await;
    let announcements = f.space("announcements", "public", Some(forum), 0).await;
    f.space("general", "public", Some(forum), 1).await;
    let ruth = f.user("ruth").await;
    let ruth_layout = member_layout(&f, ruth).await;
    assert!(
        layouts::unfollow(&f.db, ruth_layout, announcements)
            .await
            .unwrap()
    );
    assert_eq!(
        front_page(&f, &viewer(&f, ruth).await).await,
        (
            vec![],
            vec![section("Forum", &["general"]), section("Feeds", &["ruth"])]
        )
    );

    assert!(
        layouts::set_essential(&f.db, announcements, true)
            .await
            .unwrap()
    );
    f.changed(parlour::changes::Change::SpaceChanged {
        space_id: announcements,
    })
    .await;
    for v in [Viewer::Anonymous, viewer(&f, ruth).await] {
        let mut sections = vec![section("Forum", &["general"])];
        if v.user_id().is_some() {
            sections.push(section("Feeds", &["ruth"]));
        }
        assert_eq!(
            front_page(&f, &v).await,
            (vec!["announcements".to_owned()], sections),
            "{v:?}"
        );
    }
    let page = f.page("/").await;
    assert_in_order(
        &page,
        &[
            "href=\"/c/announcements\"",
            ">Forum<",
            "href=\"/c/general\"",
        ],
    );

    // An essential space nobody can read isn't shown to them.
    f.set_grant(
        announcements,
        "read",
        parlour::permissions::Grant::members(),
    )
    .await;
    assert_eq!(
        front_page(&f, &Viewer::Anonymous).await,
        (vec![], vec![section("Forum", &["general"])])
    );
    assert_eq!(
        front_page(&f, &viewer(&f, ruth).await).await.0,
        vec!["announcements".to_owned()]
    );

    assert!(
        layouts::set_essential(&f.db, announcements, false)
            .await
            .unwrap()
    );
    assert!(
        !layouts::set_essential(&f.db, announcements, false)
            .await
            .unwrap()
    );
    assert_eq!(
        front_page(&f, &viewer(&f, ruth).await).await,
        (
            vec![],
            vec![section("Forum", &["general"]), section("Feeds", &["ruth"])]
        )
    );
}

#[sqlx::test]
async fn muted_spaces_leave_the_front_page_and_essential_ones_cant_be_muted(pool: PgPool) {
    let f = Forum::new(pool);
    let forum = f.section("Forum", 1).await;
    let announcements = f.space("announcements", "public", Some(forum), 0).await;
    let general = f.space("general", "public", Some(forum), 1).await;
    let games = f.space("games", "public", Some(forum), 2).await;
    assert!(
        layouts::set_essential(&f.db, announcements, true)
            .await
            .unwrap()
    );
    let ruth = f.user("ruth").await;

    assert_eq!(
        layouts::mute(&f.db, ruth, games).await.unwrap(),
        MuteOutcome::Muted
    );
    assert_eq!(
        layouts::mute(&f.db, ruth, games).await.unwrap(),
        MuteOutcome::AlreadyMuted
    );
    assert_eq!(
        layouts::mute(&f.db, ruth, announcements).await.unwrap(),
        MuteOutcome::Essential
    );
    assert_eq!(
        front_page(&f, &viewer(&f, ruth).await).await,
        (
            vec!["announcements".to_owned()],
            vec![section("Forum", &["general"]), section("Feeds", &["ruth"])]
        )
    );
    // Muting left the member's sections, and isn't an edit of the layout:
    // they keep following the default.
    let ruth_layout = member_layout(&f, ruth).await;
    assert_eq!(
        layouts::section_of(&f.db, ruth_layout, games)
            .await
            .unwrap(),
        None
    );
    assert!(
        layouts::read(&f.db, ruth_layout)
            .await
            .unwrap()
            .unwrap()
            .edited_at
            .is_none()
    );
    // Nobody else is affected.
    assert_eq!(
        front_page(&f, &Viewer::Anonymous).await,
        (
            vec!["announcements".to_owned()],
            vec![section("Forum", &["general", "games"])]
        )
    );

    // Making a muted space essential unmutes it for everyone.
    assert!(layouts::set_essential(&f.db, games, true).await.unwrap());
    assert_eq!(
        front_page(&f, &viewer(&f, ruth).await).await.0,
        vec!["announcements".to_owned(), "games".to_owned()]
    );
    assert!(!layouts::unmute(&f.db, ruth, games).await.unwrap());
    assert!(layouts::set_essential(&f.db, games, false).await.unwrap());

    // Unmuting doesn't put a space back in the sections; following does,
    // and following a muted space unmutes it.
    assert_eq!(
        layouts::mute(&f.db, ruth, general).await.unwrap(),
        MuteOutcome::Muted
    );
    assert!(layouts::unmute(&f.db, ruth, general).await.unwrap());
    assert_eq!(
        front_page(&f, &viewer(&f, ruth).await).await.1,
        vec![section("Feeds", &["ruth"])]
    );
    assert_eq!(
        layouts::mute(&f.db, ruth, general).await.unwrap(),
        MuteOutcome::Muted
    );
    let section_id = layouts::read(&f.db, ruth_layout)
        .await
        .unwrap()
        .unwrap()
        .sections[0]
        .id;
    assert!(
        layouts::follow(&f.db, ruth_layout, section_id, general)
            .await
            .unwrap()
    );
    assert_eq!(
        front_page(&f, &viewer(&f, ruth).await).await.1,
        vec![section("Forum", &["general"]), section("Feeds", &["ruth"])]
    );
    let muted: i64 = sqlx::query_scalar("SELECT count(*) FROM space_mutes WHERE user_id = $1")
        .bind(ruth)
        .fetch_one(&f.db)
        .await
        .unwrap();
    assert_eq!(muted, 0);
}

#[sqlx::test]
async fn archived_and_unreadable_spaces_are_left_off_the_front_page(pool: PgPool) {
    let f = Forum::new(pool);
    let forum = f.section("Forum", 1).await;
    f.space("general", "public", Some(forum), 0).await;
    let lounge = f.space("lounge", "members", Some(forum), 1).await;
    let old = f.space("old", "public", Some(forum), 2).await;
    assert!(
        parlour::store::set_archived(f.state.db(), old, true)
            .await
            .unwrap()
    );
    assert!(layouts::set_essential(&f.db, lounge, true).await.unwrap());
    let ruth = f.user("ruth").await;
    assert_eq!(
        front_page(&f, &Viewer::Anonymous).await,
        (vec![], vec![section("Forum", &["general"])])
    );
    assert_eq!(
        front_page(&f, &viewer(&f, ruth).await).await,
        (
            vec!["lounge".to_owned()],
            vec![section("Forum", &["general"]), section("Feeds", &["ruth"])]
        )
    );
    // The layout itself still lists them all.
    assert_eq!(
        shape(&f, layouts::default_id(&f.db).await.unwrap()).await,
        vec![section("Forum", &["general", "lounge", "old"])]
    );
}

#[sqlx::test]
async fn a_front_page_with_nothing_on_it_says_so(pool: PgPool) {
    let f = Forum::new(pool);
    let page = f.page("/").await;
    assert!(page.contains("There are no spaces yet."));
    // Spaces exist, but none is on the front page.
    f.unlisted_space("games", "public").await;
    let page = f.page("/").await;
    assert!(!page.contains("There are no spaces yet."), "{page}");
    assert!(page.contains("href=\"/spaces\""), "{page}");
    let ruth = f.user("ruth").await;
    let page = f.page_as("/", &f.session(ruth).await).await;
    assert!(page.contains("href=\"/spaces\""), "{page}");
    assert!(!page.contains("href=\"/c/games\""));
}

// ---------------------------------------------------------------------------
// Accounts

#[sqlx::test]
async fn deleting_an_account_removes_its_layout_and_mutes(pool: PgPool) {
    let f = Forum::new(pool);
    f.member("sam", "admin", None).await;
    let games = f.space("games", "public", None, 0).await;
    let ruth = f.member("ruth", "user", None).await;
    assert_eq!(
        layouts::mute(&f.db, ruth, games).await.unwrap(),
        MuteOutcome::Muted
    );
    assert!(layouts::member_id(&f.db, ruth).await.unwrap().is_some());
    assert!(matches!(
        parlour::users::delete_account(f.state.db(), ruth)
            .await
            .unwrap(),
        parlour::users::Deletion::Deleted { .. }
    ));
    assert!(layouts::member_id(&f.db, ruth).await.unwrap().is_none());
    let muted: i64 = sqlx::query_scalar("SELECT count(*) FROM space_mutes WHERE user_id = $1")
        .bind(ruth)
        .fetch_one(&f.db)
        .await
        .unwrap();
    assert_eq!(muted, 0);
}

/// A change to the default puts nothing back that the member has muted,
/// so unmuting means the same whether or not they follow the default: the
/// space stays off their front page until they follow it again.
#[sqlx::test]
async fn a_default_change_doesnt_put_a_muted_space_back_in_the_sections(pool: PgPool) {
    let f = Forum::new(pool);
    let forum = f.section("Forum", 1).await;
    let general = f.space("general", "public", Some(forum), 0).await;
    f.space("games", "public", Some(forum), 1).await;
    let ruth = f.user("ruth").await;
    let ruth_layout = member_layout(&f, ruth).await;
    assert_eq!(
        layouts::mute(&f.db, ruth, general).await.unwrap(),
        MuteOutcome::Muted
    );
    let default = layouts::default_id(&f.db).await.unwrap();
    assert_eq!(
        layouts::rename_section(&f.db, default, forum, "The forum")
            .await
            .unwrap(),
        layouts::Step::Changed
    );
    // Ruth still follows the default (the rename reached her), but the
    // space she muted isn't in her copy of it.
    assert_eq!(
        shape(&f, ruth_layout).await,
        vec![
            section("The forum", &["games"]),
            section("Feeds", &["ruth"])
        ]
    );
    assert!(layouts::unmute(&f.db, ruth, general).await.unwrap());
    assert_eq!(
        front_page(&f, &viewer(&f, ruth).await).await.1,
        vec![
            section("The forum", &["games"]),
            section("Feeds", &["ruth"])
        ]
    );
    assert!(
        layouts::read(&f.db, ruth_layout)
            .await
            .unwrap()
            .unwrap()
            .edited_at
            .is_none()
    );
    // Something the default's Feeds section holds is skipped the same way.
    let jonah = f.user("jonah").await;
    let books = f.unlisted_space("books", "public").await;
    assert_eq!(
        layouts::mute(&f.db, jonah, books).await.unwrap(),
        MuteOutcome::Muted
    );
    let default_feeds = layouts::add_section(&f.db, default, "Feeds").await.unwrap();
    assert!(
        layouts::follow(&f.db, default, default_feeds, books)
            .await
            .unwrap()
    );
    assert_eq!(
        shape(&f, member_layout(&f, jonah).await).await,
        vec![
            section("The forum", &["general", "games"]),
            section("Feeds", &["jonah"])
        ]
    );
    // Ruth unmuted General above and still follows the default, so this
    // change brings her the default in full: General is back where the
    // default puts it. (Had she edited her layout, nothing would reach it.)
    assert_eq!(
        shape(&f, ruth_layout).await,
        vec![
            section("The forum", &["general", "games"]),
            section("Feeds", &["ruth", "books"])
        ]
    );
}

/// A member's first edit of their layout, landing while an admin's change
/// to the default is being copied out, is theirs to keep: the copy checks
/// again, once it has the member's row, that they still follow the default.
#[sqlx::test]
async fn a_members_first_edit_survives_a_default_change_in_progress(pool: PgPool) {
    let f = Forum::new(pool);
    let forum = f.section("Forum", 1).await;
    f.space("general", "public", Some(forum), 0).await;
    let ruth = f.user("ruth").await;
    let jonah = f.user("jonah").await;
    let default = layouts::default_id(&f.db).await.unwrap();
    let jonah_layout = member_layout(&f, jonah).await;

    // Jonah's edit has his layout's row, and hasn't committed yet.
    let mut jonahs_edit = f.db.begin().await.unwrap();
    let mine = layouts::add_section(&mut jonahs_edit, jonah_layout, "Mine")
        .await
        .unwrap();

    // The admin adds a section to the default: copying it out waits for
    // Jonah's row.
    let db = f.db.clone();
    let admins_write =
        tokio::spawn(async move { layouts::add_section(&db, default, "Reading").await.unwrap() });
    loop {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity
             WHERE datname = current_database() AND wait_event_type = 'Lock'",
        )
        .fetch_one(&f.db)
        .await
        .unwrap();
        if waiting > 0 {
            break;
        }
        assert!(
            !admins_write.is_finished(),
            "the admin's write went through without waiting for Jonah's row"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    jonahs_edit.commit().await.unwrap();
    admins_write.await.unwrap();

    // Jonah's layout is his own now, section and all; Ruth's followed.
    assert_eq!(
        shape(&f, jonah_layout).await,
        vec![
            section("Forum", &["general"]),
            section("Feeds", &["jonah"]),
            section("Mine", &[]),
        ]
    );
    let layout = layouts::read(&f.db, jonah_layout).await.unwrap().unwrap();
    assert_eq!(layout.sections.last().map(|s| s.id), Some(mine));
    assert!(layout.edited_at.is_some());
    assert_eq!(
        shape(&f, member_layout(&f, ruth).await).await,
        vec![
            section("Forum", &["general"]),
            section("Reading", &[]),
            section("Feeds", &["ruth"]),
        ]
    );
}

/// Two admins changing the default at once (or one admin's save racing a
/// member's) must not trip over each other's copying into the members'
/// layouts: every write serialises on the layout it changes.
#[sqlx::test]
async fn changes_to_the_default_are_serialised(pool: PgPool) {
    let f = Forum::new(pool);
    let forum = f.section("Forum", 1).await;
    for name in ["ruth", "jonah", "maya", "kit", "sam"] {
        f.user(name).await;
    }
    let mut spaces = Vec::new();
    for n in 0..6 {
        spaces.push(f.unlisted_space(&format!("space-{n}"), "public").await);
    }
    let default = layouts::default_id(&f.db).await.unwrap();
    let mut handles = Vec::new();
    for space in spaces.clone() {
        let db = f.db.clone();
        handles.push(tokio::spawn(async move {
            assert!(layouts::follow(&db, default, forum, space).await.unwrap());
        }));
        let db = f.db.clone();
        handles.push(tokio::spawn(async move {
            layouts::add_section(&db, default, &format!("Section {space}"))
                .await
                .unwrap();
        }));
    }
    for handle in handles {
        handle.await.unwrap();
    }
    let expected = shape(&f, default).await;
    assert_eq!(expected[0].1.len(), 6);
    assert_eq!(expected.len(), 7);
    for name in ["ruth", "jonah", "maya", "kit", "sam"] {
        let user = f.user_id(name).await;
        let mut copy = shape(&f, member_layout(&f, user).await).await;
        assert_eq!(copy.pop(), Some(section("Feeds", &[name])), "{name}");
        assert_eq!(copy, expected, "{name}");
    }
}

#[sqlx::test]
async fn steps_that_leave_a_layout_as_it_was_write_nothing(pool: PgPool) {
    let f = Forum::new(pool);
    let forum = f.section("Forum", 1).await;
    let general = f.space("general", "public", Some(forum), 0).await;
    let games = f.space("games", "public", Some(forum), 1).await;
    let ruth = f.user("ruth").await;
    let layout = member_layout(&f, ruth).await;
    let read = |f: &Forum| {
        let db = f.db.clone();
        async move { layouts::read(&db, layout).await.unwrap().unwrap() }
    };
    let before = read(&f).await;
    let mine = before.sections[0].id;
    let feeds = before.sections[1].id;
    let unchanged = layouts::Step::Unchanged;

    assert_eq!(
        layouts::rename_section(&f.db, layout, mine, "Forum")
            .await
            .unwrap(),
        unchanged
    );
    assert_eq!(
        layouts::move_section(&f.db, layout, mine, before.sections[0].position)
            .await
            .unwrap(),
        unchanged
    );
    assert_eq!(
        layouts::reorder_sections(&f.db, layout, &[mine, feeds])
            .await
            .unwrap(),
        unchanged
    );
    assert_eq!(
        layouts::move_space(&f.db, layout, general, mine, 0)
            .await
            .unwrap(),
        unchanged
    );
    assert_eq!(
        layouts::move_space(&f.db, layout, games, mine, 9)
            .await
            .unwrap(),
        unchanged
    );
    assert_eq!(read(&f).await, before);
    assert_eq!(before.edited_at, None);

    // The same steps on the default change nothing either, and a section
    // that isn't the layout's is still not found.
    let default = layouts::default_id(&f.db).await.unwrap();
    assert_eq!(
        layouts::rename_section(&f.db, default, forum, "Forum")
            .await
            .unwrap(),
        unchanged
    );
    assert_eq!(
        layouts::rename_section(&f.db, layout, forum, "Forum")
            .await
            .unwrap(),
        layouts::Step::NotFound
    );
    assert_eq!(
        layouts::move_section(&f.db, layout, forum, 0)
            .await
            .unwrap(),
        layouts::Step::NotFound
    );

    // Moving a space from another section is a change, wherever it goes.
    assert_eq!(
        layouts::move_space(&f.db, layout, games, feeds, 0)
            .await
            .unwrap(),
        layouts::Step::Changed
    );
    assert!(read(&f).await.edited_at.is_some());
}
