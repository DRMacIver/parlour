//! Member-created spaces and their grants (DESIGN.md v0.2 §3.1): any
//! member makes a space, its administrators say who may read, reply and
//! start threads in it, spaces can be archived, and the space index lists
//! what the viewer can read.

mod common;

use axum::http::StatusCode;
use common::Forum;
use parlour::permissions::Grant;
use sqlx::PgPool;

struct Setup {
    f: Forum,
    ruth: i64,
    ruth_cookie: String,
    jonah_cookie: String,
    staff_cookie: String,
}

async fn setup(pool: PgPool) -> Setup {
    let f = Forum::new(pool);
    f.no_new_members().await;
    let ruth = f.member("ruth", "user", None).await;
    let jonah = f.member("jonah", "user", None).await;
    let sam = f.member("sam", "moderator", None).await;
    Setup {
        ruth_cookie: f.session(ruth).await,
        jonah_cookie: f.session(jonah).await,
        staff_cookie: f.session(sam).await,
        f,
        ruth,
    }
}

/// The settings form, with every grant at `members` unless given, and
/// staff alone administering.
fn settings_fields<'a>(
    name: &'a str,
    slug: &'a str,
    grants: &[(&'a str, &'a str)],
) -> Vec<(&'a str, &'a str)> {
    let mut fields = vec![
        ("name", name),
        ("slug", slug),
        ("description", "All about it."),
    ];
    for verb in ["read", "reply", "start", "administer"] {
        let level = grants.iter().find(|(v, _)| *v == verb).map_or(
            if verb == "administer" {
                "staff"
            } else {
                "members"
            },
            |(_, level)| *level,
        );
        fields.push((
            match verb {
                "read" => "read_level",
                "reply" => "reply_level",
                "start" => "start_level",
                _ => "administer_level",
            },
            level,
        ));
    }
    fields
}

async fn count(f: &Forum, sql: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(sql.to_owned()))
        .fetch_one(&f.db)
        .await
        .unwrap()
}

#[sqlx::test]
async fn any_member_can_make_a_space(pool: PgPool) {
    let s = setup(pool).await;
    let response = s.f.get("/spaces/new").await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert!(response.location().starts_with("/login?next="));
    let form = s.f.page_as("/spaces/new", &s.ruth_cookie).await;
    assert!(form.contains("name=\"name\"") && form.contains("name=\"slug\""));

    let response =
        s.f.submit(
            "/spaces/new",
            &[
                ("name", " Board games "),
                ("slug", "board-games"),
                ("description", "Dice, cards and cardboard."),
            ],
            Some(&s.ruth_cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    assert_eq!(response.location(), "/c/board-games/settings?done=created");
    let created_by: Option<i64> =
        sqlx::query_scalar("SELECT created_by FROM spaces WHERE slug = 'board-games'")
            .fetch_one(&s.f.db)
            .await
            .unwrap();
    assert_eq!(created_by, Some(s.ruth));

    // By default any member reads, replies and starts threads; visitors
    // see nothing; the maker administers it.
    assert_eq!(
        s.f.get("/c/board-games").await.status,
        StatusCode::NOT_FOUND
    );
    let page = s.f.page_as("/c/board-games", &s.jonah_cookie).await;
    assert!(page.contains("Board games") && page.contains("href=\"/c/board-games/new\""));
    assert!(!page.contains("href=\"/c/board-games/settings\""));
    let page = s.f.page_as("/c/board-games", &s.ruth_cookie).await;
    assert!(page.contains("href=\"/c/board-games/settings\""));
    let response =
        s.f.submit(
            "/c/board-games/new",
            &[("title", "Heat"), ("body", "Fast cars")],
            Some(&s.jonah_cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    assert_eq!(
        s.f.get_as("/c/board-games/settings", Some(&s.jonah_cookie))
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    for cookie in [&s.ruth_cookie, &s.staff_cookie] {
        let settings = s.f.page_as("/c/board-games/settings", cookie).await;
        assert!(settings.contains("value=\"Board games\""), "{settings}");
        assert!(settings.contains("name=\"read_level\""));
    }

    // Addresses are checked: taken, malformed, or a member's name.
    for (slug, why) in [
        ("board-games", "Another space has that address."),
        ("Board Games!", "lower-case letters"),
        ("jonah", "a member’s name"),
    ] {
        let response =
            s.f.submit(
                "/spaces/new",
                &[("name", "Again"), ("slug", slug), ("description", "")],
                Some(&s.ruth_cookie),
            )
            .await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "{slug}");
        assert!(response.body.contains(why), "{slug}: {}", response.body);
        assert!(response.body.contains("value=\"Again\""));
    }
    assert_eq!(
        count(&s.f, "SELECT count(*) FROM spaces WHERE kind = 'topic'").await,
        1
    );
}

#[sqlx::test]
async fn administrators_set_who_may_read_reply_and_start(pool: PgPool) {
    let s = setup(pool).await;
    let maya = s.f.member("maya", "user", None).await;
    let maya_cookie = s.f.session(maya).await;
    s.f.submit(
        "/spaces/new",
        &[("name", "Club"), ("slug", "club"), ("description", "")],
        Some(&s.ruth_cookie),
    )
    .await;
    let club = s.f.id("SELECT id FROM spaces WHERE slug = 'club'").await;

    // Only Ruth and Jonah read it; anyone who reads may reply; only Ruth
    // starts threads.
    let mut fields = settings_fields(
        "Club",
        "club",
        &[
            ("read", "listed"),
            ("start", "listed"),
            ("administer", "listed"),
        ],
    );
    fields.push(("read_users", "ruth, @Jonah"));
    fields.push(("start_users", "ruth"));
    fields.push(("administer_users", "ruth"));
    let response =
        s.f.submit("/c/club/settings", &fields, Some(&s.ruth_cookie))
            .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    assert_eq!(
        s.f.get_as("/c/club", Some(&maya_cookie)).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        s.f.get_as("/c/club", Some(&s.jonah_cookie)).await.status,
        StatusCode::OK
    );
    let response =
        s.f.submit(
            "/c/club/new",
            &[("title", "Rules"), ("body", "Read these")],
            Some(&s.jonah_cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::FORBIDDEN);
    let response =
        s.f.submit(
            "/c/club/new",
            &[("title", "Rules"), ("body", "Read these")],
            Some(&s.ruth_cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    let thread = s.f.id("SELECT id FROM threads WHERE slug = 'rules'").await;
    let response =
        s.f.submit(
            &format!("/t/{thread}/reply"),
            &[("body", "Noted")],
            Some(&s.jonah_cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    let visibility: String =
        sqlx::query_scalar("SELECT visibility FROM threads WHERE slug = 'rules'")
            .fetch_one(&s.f.db)
            .await
            .unwrap();
    assert_eq!(visibility, "restricted");
    // The space page and thread say who can read it.
    let page = s.f.page_as("/c/club", &s.jonah_cookie).await;
    assert!(page.contains("Chosen members"), "{page}");

    // Starting threads for staff only: an administrator who isn't staff
    // can't; a moderator can.
    s.f.set_grant(club, "start", Grant::staff()).await;
    let response =
        s.f.submit(
            "/c/club/new",
            &[("title", "Mine"), ("body", "x")],
            Some(&s.ruth_cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::FORBIDDEN);
    // A moderator may start threads but, not being named among those who
    // read it, can't read the space, so can't post in it: staff read only
    // what their memberships let them (DESIGN.md §3.1).
    let response =
        s.f.submit(
            "/c/club/new",
            &[("title", "Notice"), ("body", "x")],
            Some(&s.staff_cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::NOT_FOUND, "{}", response.body);
    // Once the read grant names them, they can.
    let sam = s.f.user_id("sam").await;
    let ruth = s.f.user_id("ruth").await;
    let jonah = s.f.user_id("jonah").await;
    s.f.set_grant(club, "read", Grant::listed([], [ruth, jonah, sam]))
        .await;
    let response =
        s.f.submit(
            "/c/club/new",
            &[("title", "Notice"), ("body", "x")],
            Some(&s.staff_cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);

    // Reading for staff only.
    s.f.set_grant(club, "read", Grant::staff()).await;
    assert_eq!(
        s.f.get_as("/c/club", Some(&s.jonah_cookie)).await.status,
        StatusCode::NOT_FOUND
    );
    let page = s.f.page_as("/c/club", &s.staff_cookie).await;
    assert!(page.contains("Staff only"), "{page}");

    // A group in a grant.
    let organisers = s.f.group("organisers", "Organisers").await;
    s.f.join_group(organisers, maya).await;
    s.f.set_grant(club, "read", Grant::listed([organisers], []))
        .await;
    assert_eq!(
        s.f.get_as("/c/club", Some(&maya_cookie)).await.status,
        StatusCode::OK
    );
    assert_eq!(
        s.f.get_as("/c/club", Some(&s.jonah_cookie)).await.status,
        StatusCode::NOT_FOUND
    );
    let page = s.f.page_as("/c/club", &maya_cookie).await;
    assert!(page.contains("Organisers group"), "{page}");

    // The form refuses what it can't use.
    for (key, value, why) in [
        ("read_users", "nobody", "There is no member called"),
        (
            "reply_level",
            "public",
            "Only reading can be open to everyone.",
        ),
        ("start_level", "sometimes", "Choose who can start threads."),
        ("read_group", "999", "Choose groups from the list."),
    ] {
        let mut fields = settings_fields("Club", "club", &[("read", "listed")]);
        fields.retain(|(k, _)| *k != key);
        fields.push((key, value));
        let response =
            s.f.submit("/c/club/settings", &fields, Some(&s.staff_cookie))
                .await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "{key}={value}");
        assert!(
            response.body.contains(why),
            "{key}={value}: {}",
            response.body
        );
    }
}

#[sqlx::test]
async fn archived_spaces_are_read_only(pool: PgPool) {
    let s = setup(pool).await;
    let games = s.f.space("games", "public", None, 0).await;
    s.f.set_grant(games, "administer", Grant::listed([], [s.ruth]))
        .await;
    let t = s.f.thread(games, s.ruth, "Heat", "Fast cars").await;
    let response =
        s.f.submit("/c/games/archive", &[], Some(&s.jonah_cookie))
            .await;
    assert_eq!(response.status, StatusCode::FORBIDDEN);
    let response =
        s.f.submit("/c/games/archive", &[], Some(&s.ruth_cookie))
            .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);

    // Still readable, by visitors too, but closed.
    let page = s.f.page("/c/games").await;
    assert!(page.contains("Archived") && page.contains("Heat"), "{page}");
    let page = s.f.page_as("/c/games", &s.jonah_cookie).await;
    assert!(!page.contains("href=\"/c/games/new\""));
    for (path, fields) in [
        ("/c/games/new", vec![("title", "New"), ("body", "x")]),
        (&*format!("/t/{}/reply", t.thread_id), vec![("body", "x")]),
    ] {
        let response = s.f.submit(path, &fields, Some(&s.jonah_cookie)).await;
        assert_eq!(response.status, StatusCode::FORBIDDEN, "{path}");
    }
    // Out of the index and the space list, unless asked for.
    assert!(!s.f.page("/").await.contains("href=\"/c/games\""));
    assert!(!s.f.page("/spaces").await.contains("href=\"/c/games\""));
    assert!(
        s.f.page("/spaces?archived=1")
            .await
            .contains("href=\"/c/games\"")
    );
    assert_eq!(
        count(
            &s.f,
            "SELECT count(*) FROM mod_actions WHERE action = 'archive_space'"
        )
        .await,
        0,
        "an administrator's own action isn't moderation"
    );

    // And back.
    let response =
        s.f.submit("/c/games/unarchive", &[], Some(&s.staff_cookie))
            .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    assert!(s.f.page("/").await.contains("href=\"/c/games\""));
    assert_eq!(
        count(
            &s.f,
            "SELECT count(*) FROM mod_actions WHERE action = 'unarchive_space'"
        )
        .await,
        1,
        "staff acting on a space they don't administer is logged"
    );
}

#[sqlx::test]
async fn the_space_index_lists_what_the_viewer_can_read(pool: PgPool) {
    let s = setup(pool).await;
    let games = s.f.space("games", "public", None, 0).await;
    let lounge = s.f.space("lounge", "members", None, 1).await;
    let secret = s.f.space("secret", "listed", None, 2).await;
    s.f.set_grant(secret, "read", Grant::listed([], [s.ruth]))
        .await;
    s.f.thread(games, s.ruth, "Old", "x").await;
    s.f.thread(lounge, s.ruth, "Newer", "x").await;

    fn slugs(page: &str) -> Vec<&str> {
        page.split("href=\"/c/")
            .skip(1)
            .filter_map(|rest| rest.split('"').next())
            .filter(|slug| !slug.contains('/'))
            .collect()
    }
    // Newest first by default.
    assert_eq!(slugs(&s.f.page("/spaces").await), vec!["games"]);
    assert_eq!(
        slugs(&s.f.page_as("/spaces", &s.jonah_cookie).await),
        vec!["lounge", "games"]
    );
    assert_eq!(
        slugs(&s.f.page_as("/spaces", &s.ruth_cookie).await),
        vec!["secret", "lounge", "games"]
    );
    assert_eq!(
        slugs(&s.f.page_as("/spaces?sort=name", &s.ruth_cookie).await),
        vec!["games", "lounge", "secret"]
    );
    assert_eq!(
        slugs(&s.f.page_as("/spaces?sort=activity", &s.ruth_cookie).await),
        vec!["lounge", "games", "secret"]
    );
    // Other spellings redirect to the one address, so the cache stays small.
    let response = s.f.get("/spaces?sort=created").await;
    assert_eq!(response.status, StatusCode::PERMANENT_REDIRECT);
    assert_eq!(response.location(), "/spaces");
    let response = s.f.get("/spaces?utm=1").await;
    assert_eq!(response.location(), "/spaces");
    assert_eq!(
        s.f.get("/spaces?sort=bogus").await.status,
        StatusCode::NOT_FOUND
    );
    // The page links to making a space, for those who may.
    assert!(
        s.f.page_as("/spaces", &s.jonah_cookie)
            .await
            .contains("href=\"/spaces/new\"")
    );
    assert!(!s.f.page("/spaces").await.contains("href=\"/spaces/new\""));
}

#[sqlx::test]
async fn admins_can_keep_making_spaces_to_staff(pool: PgPool) {
    let s = setup(pool).await;
    let admin = s.f.member("boss", "admin", None).await;
    let admin_cookie = s.f.session(admin).await;
    let settings = s.f.page_as("/admin/settings", &admin_cookie).await;
    assert!(settings.contains("name=\"space_creation\""));
    let response =
        s.f.submit(
            "/admin/settings",
            &[
                ("site_name", "Club"),
                ("registration_mode", "invite_only"),
                ("invite_who", "everyone"),
                ("space_creation", "staff"),
            ],
            Some(&admin_cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    assert_eq!(
        s.f.get_as("/spaces/new", Some(&s.ruth_cookie)).await.status,
        StatusCode::FORBIDDEN
    );
    let response =
        s.f.submit(
            "/spaces/new",
            &[("name", "Mine"), ("slug", "mine"), ("description", "")],
            Some(&s.ruth_cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::FORBIDDEN);
    assert!(
        !s.f.page_as("/spaces", &s.ruth_cookie)
            .await
            .contains("href=\"/spaces/new\"")
    );
    assert_eq!(
        s.f.get_as("/spaces/new", Some(&s.staff_cookie))
            .await
            .status,
        StatusCode::OK
    );
}

#[sqlx::test]
async fn groups_named_in_grants_cant_be_deleted(pool: PgPool) {
    let s = setup(pool).await;
    let admin = s.f.member("boss", "admin", None).await;
    let admin_cookie = s.f.session(admin).await;
    let organisers = s.f.group("organisers", "Organisers").await;
    let planning = s.f.space("planning", "public", None, 0).await;
    s.f.restrict(planning, &[organisers], None).await;
    let path = format!("/admin/groups/{organisers}/delete");
    let page = s.f.page_as(&path, &admin_cookie).await;
    assert!(page.contains("href=\"/c/planning\""), "{page}");
    assert!(!page.contains(&format!("action=\"{path}\"")), "{page}");
    let response = s.f.submit(&path, &[], Some(&admin_cookie)).await;
    assert_eq!(
        response.status,
        StatusCode::BAD_REQUEST,
        "{}",
        response.body
    );
    assert_eq!(
        count(
            &s.f,
            "SELECT count(*) FROM groups WHERE owner_user_id IS NULL"
        )
        .await,
        1
    );

    s.f.set_grant(planning, "read", Grant::members()).await;
    let response = s.f.submit(&path, &[], Some(&admin_cookie)).await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    assert_eq!(
        count(
            &s.f,
            "SELECT count(*) FROM groups WHERE owner_user_id IS NULL"
        )
        .await,
        0
    );
}

#[sqlx::test]
async fn staff_edits_to_other_peoples_spaces_are_logged(pool: PgPool) {
    let s = setup(pool).await;
    s.f.submit(
        "/spaces/new",
        &[("name", "Club"), ("slug", "club"), ("description", "")],
        Some(&s.ruth_cookie),
    )
    .await;
    // Ruth's own edit is nobody's business.
    let mut fields = settings_fields("Chess club", "club", &[("administer", "listed")]);
    fields.push(("administer_users", "ruth"));
    let response =
        s.f.submit("/c/club/settings", &fields, Some(&s.ruth_cookie))
            .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    assert_eq!(count(&s.f, "SELECT count(*) FROM mod_actions").await, 0);
    // A moderator's is recorded. The space is members-only, so the entry
    // is for staff, not the public log.
    let mut fields = settings_fields("Chess", "club", &[("administer", "listed")]);
    fields.push(("administer_users", "ruth"));
    let response =
        s.f.submit("/c/club/settings", &fields, Some(&s.staff_cookie))
            .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    assert_eq!(
        count(
            &s.f,
            "SELECT count(*) FROM mod_actions WHERE action = 'edit_space' AND NOT public"
        )
        .await,
        1
    );
    let log = s.f.page_as("/mod/log", &s.staff_cookie).await;
    assert!(log.contains("changed the settings of"), "{log}");
    assert!(log.contains("href=\"/c/club\""), "{log}");
}

/// Every space's audience page (DESIGN.md §3.1, §3.3) says who may read,
/// reply, start threads and run it, level by level.
#[sqlx::test]
async fn the_audience_page_says_who_may_do_what(pool: PgPool) {
    let s = setup(pool).await;
    s.f.space("open", "public", None, 0).await;
    let page = s.f.page_as("/c/open/audience", &s.jonah_cookie).await;
    for text in [
        "Who can take part in Open",
        "Anyone, including visitors who haven’t logged in.",
        "Every member.",
        "The moderators and admins.",
        "Replying and starting threads also need permission to read.",
    ] {
        assert!(page.contains(text), "{text}: {page}");
    }
    let staff = s.f.space("back-room", "staff", None, 0).await;
    s.f.set_grant(staff, "start", Grant::listed([], [])).await;
    let page = s.f.page_as("/c/back-room/audience", &s.staff_cookie).await;
    assert!(page.contains("Nobody."), "{page}");
    assert_eq!(
        s.f.get_as("/c/back-room/audience", Some(&s.jonah_cookie))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    // An archived space says nobody can post.
    s.f.exec("UPDATE spaces SET archived_at = now() WHERE slug = 'open'")
        .await;
    let page = s.f.page_as("/c/open/audience", &s.ruth_cookie).await;
    assert!(
        page.contains("The space is archived, so nobody can post in it."),
        "{page}"
    );
    // A space with no administrators says who runs it meanwhile.
    s.f.set_grant(staff, "administer", Grant::listed([], []))
        .await;
    let page = s.f.page_as("/c/back-room/audience", &s.staff_cookie).await;
    assert!(
        page.contains("Nobody is named, so the moderators and admins run it until someone is."),
        "{page}"
    );
}

/// Staff run every space, and whatever they do to one they don't run in
/// their own right is in the moderation log (DESIGN.md §3.1, §8): a space
/// whose administer grant is "staff", like Announcements and General from
/// setup and every space from before spaces had administrators, is run by
/// their role, so their changes there are logged. One whose grant names
/// them, and which they read, is theirs, and isn't.
#[sqlx::test]
async fn staff_changing_a_space_run_by_staff_are_logged(pool: PgPool) {
    let s = setup(pool).await;
    s.f.space("general", "members", None, 0).await;
    let logged = |action: &'static str| {
        let f = &s.f;
        async move {
            count(
                f,
                &format!("SELECT count(*) FROM mod_actions WHERE action = '{action}'"),
            )
            .await
        }
    };
    let response =
        s.f.submit(
            "/c/general/settings",
            &settings_fields("General", "general", &[("read", "public")]),
            Some(&s.staff_cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    assert_eq!(logged("edit_space").await, 1);
    for path in ["/c/general/archive", "/c/general/unarchive"] {
        let response = s.f.submit(path, &[], Some(&s.staff_cookie)).await;
        assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    }
    assert_eq!(logged("archive_space").await, 1);
    assert_eq!(logged("unarchive_space").await, 1);

    // A space whose grant names the moderator, and which they read, is
    // theirs: archiving it is no moderation.
    let mine = s.f.space("mine", "members", None, 1).await;
    let sam = s.f.user_id("sam").await;
    s.f.set_grant(mine, "administer", Grant::listed([], [sam]))
        .await;
    let response =
        s.f.submit("/c/mine/archive", &[], Some(&s.staff_cookie))
            .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    assert_eq!(logged("archive_space").await, 1);
}

/// Deleting a space asks first, as deleting a front-page section or a
/// group does, and then says it was done.
#[sqlx::test]
async fn deleting_a_space_asks_first(pool: PgPool) {
    let s = setup(pool).await;
    let games = s.f.space("boardgames", "members", None, 0).await;
    s.f.set_grant(games, "administer", Grant::listed([], [s.ruth]))
        .await;
    let settings = s.f.page_as("/c/boardgames/settings", &s.ruth_cookie).await;
    assert!(
        settings.contains("<a class=\"button button-quiet\" href=\"/c/boardgames/delete\">"),
        "{settings}"
    );
    assert!(
        !settings.contains("action=\"/c/boardgames/delete\""),
        "{settings}"
    );
    let page = s.f.page_as("/c/boardgames/delete", &s.ruth_cookie).await;
    assert!(page.contains("Delete Boardgames?"), "{page}");
    assert!(page.contains("action=\"/c/boardgames/delete\""), "{page}");
    assert!(page.contains("href=\"/c/boardgames/settings\""), "{page}");
    // Only those who run it are asked.
    let response =
        s.f.get_as("/c/boardgames/delete", Some(&s.jonah_cookie))
            .await;
    assert_eq!(response.status, StatusCode::FORBIDDEN);
    let response =
        s.f.submit("/c/boardgames/delete", &[], Some(&s.ruth_cookie))
            .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    assert_eq!(response.location(), "/spaces?deleted=1");
    let page = s.f.page_as("/spaces?deleted=1", &s.ruth_cookie).await;
    assert!(page.contains("The space was deleted."), "{page}");
    // Visitors have nothing to be told, and keep one address.
    let response = s.f.get("/spaces?deleted=1").await;
    assert_eq!(response.status, StatusCode::PERMANENT_REDIRECT);
    // A space with threads isn't offered.
    let kept = s.f.space("kept", "members", None, 1).await;
    s.f.thread(kept, s.ruth, "Here", "Yes").await;
    let page = s.f.page_as("/c/kept/delete", &s.staff_cookie).await;
    assert!(page.contains("can’t be deleted"), "{page}");
    assert!(!page.contains("action=\"/c/kept/delete\""), "{page}");
}

/// Each page has one name: `/` is the front page and `/spaces` the space
/// index, in the navigation, the breadcrumbs and what pages say about
/// them.
#[sqlx::test]
async fn the_front_page_and_the_space_index_have_one_name_each(pool: PgPool) {
    let s = setup(pool).await;
    let games = s.f.space("games", "members", None, 0).await;
    s.f.set_grant(games, "administer", Grant::listed([], [s.ruth]))
        .await;
    for path in ["/", "/spaces", "/c/games", "/c/games/settings", "/latest"] {
        let page = s.f.page_as(path, &s.ruth_cookie).await;
        assert!(!page.contains("Forum index"), "{path}: {page}");
        assert!(!page.contains("forum index"), "{path}: {page}");
        assert!(!page.contains("list of spaces"), "{path}: {page}");
        assert!(page.contains(">Front page</a>"), "{path}: {page}");
    }
    let settings = s.f.page_as("/c/games/settings", &s.ruth_cookie).await;
    assert!(!settings.contains("the index"), "{settings}");
    assert!(settings.contains("the space index"), "{settings}");
    let layout = s.f.page_as("/settings/layout", &s.ruth_cookie).await;
    assert!(!layout.contains("list of spaces"), "{layout}");
    let missing = s.f.get("/c/nowhere").await;
    assert!(
        missing.body.contains("Back to the front page"),
        "{}",
        missing.body
    );
}

/// A space nobody runs any more, whose maker hasn't been on the site for
/// 60 days, can be handed to a member by staff (DESIGN.md §3.1): its
/// settings show staff that nobody runs it and when its maker was last
/// seen, and offer to hand it on only once the 60 days have passed. The
/// handing is logged, and the new administrator told.
#[sqlx::test]
async fn staff_hand_on_a_space_its_maker_left(pool: PgPool) {
    let s = setup(pool).await;
    let kit = s.f.member("kit", "user", None).await;
    let jonah = s.f.user_id("jonah").await;
    let games = s.f.space("games", "public", None, 0).await;
    s.f.exec(&format!(
        "UPDATE spaces SET created_by = {kit} WHERE id = {games};
         UPDATE users SET last_seen_at = now() - interval '10 days' WHERE id = {kit}"
    ))
    .await;
    // While someone runs it, there is nothing to hand on.
    s.f.set_grant(games, "administer", Grant::listed([], [kit]))
        .await;
    let page = s.f.page_as("/c/games/settings", &s.staff_cookie).await;
    assert!(!page.contains("Nobody runs this space"), "{page}");
    let hand = |cookie: &str| {
        let f = &s.f;
        let cookie = cookie.to_owned();
        async move {
            f.submit(
                "/c/games/hand",
                &[
                    ("username", "jonah"),
                    ("reason", "Jonah asked to look after it"),
                ],
                Some(&cookie),
            )
            .await
        }
    };
    let response = hand(&s.staff_cookie).await;
    assert_eq!(
        response.status,
        StatusCode::BAD_REQUEST,
        "{}",
        response.body
    );
    assert!(
        response.body.contains("Someone runs this space already"),
        "{}",
        response.body
    );
    // Kit leaves it, ten days after last being seen: not yet.
    s.f.set_grant(games, "administer", Grant::listed([], []))
        .await;
    let page = s.f.page_as("/c/games/settings", &s.staff_cookie).await;
    assert!(page.contains("Nobody runs this space."), "{page}");
    assert!(
        page.contains("Kit, who made it, was last seen 10 days ago"),
        "{page}"
    );
    assert!(page.contains("in 50 days"), "{page}");
    assert!(!page.contains("action=\"/c/games/hand\""), "{page}");
    let response = hand(&s.staff_cookie).await;
    assert_eq!(
        response.status,
        StatusCode::BAD_REQUEST,
        "{}",
        response.body
    );
    // Members aren't offered it, and can't.
    assert_eq!(hand(&s.ruth_cookie).await.status, StatusCode::FORBIDDEN);
    // After 60 days away, it can be handed on.
    s.f.exec(&format!(
        "UPDATE users SET last_seen_at = now() - interval '61 days' WHERE id = {kit}"
    ))
    .await;
    let page = s.f.page_as("/c/games/settings", &s.staff_cookie).await;
    assert!(page.contains("was last seen 61 days ago"), "{page}");
    assert!(page.contains("action=\"/c/games/hand\""), "{page}");
    let response = hand(&s.staff_cookie).await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    let administer: Vec<i64> = sqlx::query_scalar(
        "SELECT user_ids FROM space_grants WHERE space_id = $1 AND verb = 'administer'",
    )
    .bind(games)
    .fetch_one(&s.f.db)
    .await
    .unwrap();
    assert_eq!(administer, vec![jonah]);
    assert_eq!(
        count(
            &s.f,
            &format!(
                "SELECT count(*) FROM mod_actions WHERE action = 'hand_space' AND user_id = {jonah}"
            )
        )
        .await,
        1
    );
    let notes = s.f.page_as("/notifications", &s.jonah_cookie).await;
    assert!(
        notes.contains(
            "A moderator handed you the space Games, which nobody ran any more: you run it now."
        ),
        "{notes}"
    );
    let log = s.f.page("/moderation").await;
    assert!(log.contains("handed"), "{log}");
    assert!(log.contains("Jonah asked to look after it"), "{log}");
    // Jonah runs it now.
    s.f.page_as("/c/games/settings", &s.jonah_cookie).await;
}
