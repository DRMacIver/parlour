//! Admin pages for members: giving people roles (DESIGN.md §3.1, §4).

mod common;

use axum::http::StatusCode;
use common::Forum;
use parlour::layouts;
use parlour::permissions::Grant;
use sqlx::PgPool;

async fn role_of(forum: &Forum, user_id: i64) -> String {
    sqlx::query_scalar("SELECT role FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_one(&forum.db)
        .await
        .unwrap()
}

#[sqlx::test]
async fn admins_give_members_roles(db: PgPool) {
    let forum = Forum::new(db);
    let sam = forum.member("sam", "admin", None).await;
    let ruth = forum.member("ruth", "user", None).await;
    let sam_cookie = forum.session(sam).await;
    let ruth_cookie = forum.session(ruth).await;
    // Cache the pages that show roles.
    assert!(!forum.page("/u/ruth").await.contains("Moderator"));
    assert!(!forum.page("/users/tree").await.contains("Moderator"));

    let page = forum.page_as("/admin/users", &sam_cookie).await;
    assert!(page.contains("href=\"/u/ruth\""));
    assert!(page.contains(&format!("action=\"/admin/users/{ruth}/role\"")));
    // The admin pages link to each other.
    assert!(page.contains("href=\"/admin/settings\""));
    assert!(
        forum
            .page_as("/admin/settings", &sam_cookie)
            .await
            .contains("href=\"/admin/users\"")
    );

    let response = forum
        .submit(
            &format!("/admin/users/{ruth}/role"),
            &[("role", "moderator")],
            Some(&sam_cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    assert_eq!(response.location(), "/admin/users?done=role");
    assert_eq!(role_of(&forum, ruth).await, "moderator");
    // Like every other action by staff, it goes in the moderation log.
    let log = forum.page("/moderation").await;
    assert!(
        log.contains("made <a href=\"/u/ruth\">ruth</a> a moderator"),
        "{log}"
    );
    assert!(
        forum
            .page_as("/admin/users?done=role", &sam_cookie)
            .await
            .contains("Role changed")
    );
    // It takes effect at once, and the cached pages show it.
    assert!(
        forum
            .page_as("/invites", &ruth_cookie)
            .await
            .contains("Number of uses")
    );
    assert!(forum.page("/u/ruth").await.contains("Moderator"));
    assert!(forum.page("/users/tree").await.contains("Moderator"));

    let response = forum
        .submit(
            &format!("/admin/users/{ruth}/role"),
            &[("role", "user")],
            Some(&sam_cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert_eq!(role_of(&forum, ruth).await, "user");
    assert!(!forum.page("/u/ruth").await.contains("Moderator"));
    // Ruth is told each time (DESIGN.md §9: a notice about you), and
    // emailed under the usual rules.
    let notices: Vec<String> = sqlx::query_scalar(
        "SELECT message FROM notifications WHERE user_id = $1 AND kind = 'moderation' ORDER BY id",
    )
    .bind(ruth)
    .fetch_all(&forum.db)
    .await
    .unwrap();
    assert_eq!(
        notices,
        [
            "An admin made you a moderator.",
            "An admin made you a member again: you are no longer a moderator.",
        ]
    );
    let page = forum.page_as("/notifications", &ruth_cookie).await;
    assert!(page.contains("An admin made you a moderator."), "{page}");
    // Choosing the role someone already has tells them nothing.
    forum
        .submit(
            &format!("/admin/users/{ruth}/role"),
            &[("role", "user")],
            Some(&sam_cookie),
        )
        .await;
    let notices: i64 = sqlx::query_scalar("SELECT count(*) FROM notifications WHERE user_id = $1")
        .bind(ruth)
        .fetch_one(&forum.db)
        .await
        .unwrap();
    assert_eq!(notices, 2);
}

#[sqlx::test]
async fn only_admins_give_roles(db: PgPool) {
    let forum = Forum::new(db);
    forum.member("sam", "admin", None).await;
    let ruth = forum.member("ruth", "moderator", None).await;
    let jonah = forum.member("jonah", "user", None).await;
    let ruth_cookie = forum.session(ruth).await;
    let path = format!("/admin/users/{jonah}/role");
    assert_eq!(
        forum
            .get_as("/admin/users", Some(&ruth_cookie))
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        forum
            .submit(&path, &[("role", "admin")], Some(&ruth_cookie))
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    let response = forum.get("/admin/users").await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert_eq!(response.location(), "/login?next=/admin/users");
    assert_eq!(
        forum.submit(&path, &[("role", "admin")], None).await.status,
        StatusCode::SEE_OTHER
    );
    assert_eq!(role_of(&forum, jonah).await, "user");
    // The nav bar doesn't offer moderators the admin pages.
    assert!(
        !forum
            .page_as("/", &ruth_cookie)
            .await
            .contains("href=\"/admin/users\"")
    );
}

#[sqlx::test]
async fn there_is_always_an_admin(db: PgPool) {
    let forum = Forum::new(db);
    let sam = forum.member("sam", "admin", None).await;
    let cookie = forum.session(sam).await;
    let path = format!("/admin/users/{sam}/role");
    let response = forum
        .submit(&path, &[("role", "moderator")], Some(&cookie))
        .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(response.body.contains("at least one admin"));
    assert_eq!(role_of(&forum, sam).await, "admin");

    // With another admin, sam can step down, and is then no longer
    // allowed on the admin pages.
    let ruth = forum.member("ruth", "user", None).await;
    let response = forum
        .submit(
            &format!("/admin/users/{ruth}/role"),
            &[("role", "admin")],
            Some(&cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    let response = forum
        .submit(&path, &[("role", "user")], Some(&cookie))
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert_eq!(response.location(), "/");
    assert_eq!(role_of(&forum, sam).await, "user");
    assert_eq!(
        forum.get_as("/admin/users", Some(&cookie)).await.status,
        StatusCode::FORBIDDEN
    );
}

#[sqlx::test]
async fn role_changes_are_checked(db: PgPool) {
    let forum = Forum::new(db);
    let sam = forum.member("sam", "admin", None).await;
    let cookie = forum.session(sam).await;
    let ruth = forum.member("ruth", "user", None).await;
    let response = forum
        .submit(
            &format!("/admin/users/{ruth}/role"),
            &[("role", "emperor")],
            Some(&cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert_eq!(role_of(&forum, ruth).await, "user");
    for path in ["/admin/users/999999/role", "/admin/users/x/role"] {
        let response = forum
            .submit(path, &[("role", "moderator")], Some(&cookie))
            .await;
        assert_eq!(response.status, StatusCode::NOT_FOUND, "{path}");
    }
}

/// The member list is paged.
#[sqlx::test]
async fn the_member_list_has_pages(db: PgPool) {
    let forum = Forum::new(db);
    let sam = forum.member("sam", "admin", None).await;
    let cookie = forum.session(sam).await;
    forum
        .exec(
            "INSERT INTO users (username, display_name, email, role)
             SELECT 'm' || n, 'Member ' || n, 'm' || n || '@example.com', 'user'
             FROM generate_series(1, 120) AS n",
        )
        .await;
    let first = forum.page_as("/admin/users", &cookie).await;
    assert!(first.contains("Page 1 of 2"));
    let second = forum.page_as("/admin/users?page=2", &cookie).await;
    assert!(second.contains("Page 2 of 2"));
    assert_eq!(
        forum
            .get_as("/admin/users?page=3", Some(&cookie))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

// ---------------------------------------------------------------------------
// Sections, spaces and groups

struct Admin {
    forum: Forum,
    cookie: String,
}

async fn admin(db: PgPool) -> Admin {
    let forum = Forum::new(db);
    let sam = forum.member("sam", "admin", None).await;
    let cookie = forum.session(sam).await;
    Admin { forum, cookie }
}

impl Admin {
    async fn submit(&self, path: &str, fields: &[(&str, &str)]) -> common::Response {
        self.forum.submit(path, fields, Some(&self.cookie)).await
    }

    async fn page(&self, path: &str) -> String {
        self.forum.page_as(path, &self.cookie).await
    }

    async fn id(&self, sql: &str) -> i64 {
        sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_owned()))
            .fetch_one(&self.forum.db)
            .await
            .unwrap()
    }
}

/// A space's settings form as staff see it: a read level, one level for
/// both replying and starting threads, and staff alone administering.
fn space_fields<'a>(
    slug: &'a str,
    name: &'a str,
    section: &'a str,
    read: &'a str,
    write: &'a str,
) -> Vec<(&'a str, &'a str)> {
    vec![
        ("name", name),
        ("slug", slug),
        ("description", "All about it."),
        ("section", section),
        ("position", "1"),
        ("read_level", read),
        ("reply_level", write),
        ("start_level", write),
        ("administer_level", "staff"),
    ]
}

/// The section the space is in on the default front page, as a form value.
async fn default_section(a: &Admin, space_id: i64) -> String {
    let default = layouts::default_id(&a.forum.db).await.unwrap();
    layouts::section_of(&a.forum.db, default, space_id)
        .await
        .unwrap()
        .map(|id| id.to_string())
        .unwrap_or_default()
}

#[sqlx::test]
async fn admins_arrange_the_default_front_page(db: PgPool) {
    let a = admin(db).await;
    let page = a.page("/admin/spaces").await;
    assert!(page.contains("href=\"/admin/groups\""));
    assert!(page.contains("No spaces yet"));

    assert!(page.contains("href=\"/admin/layout\""));
    let response = a
        .submit("/admin/layout/sections", &[("name", " Hobbies ")])
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    let hobbies = a
        .id("SELECT id FROM layout_sections WHERE name = 'Hobbies'")
        .await;
    let section = hobbies.to_string();
    // The section is the default layout's, not anyone's copy.
    let default = layouts::default_id(&a.forum.db).await.unwrap();
    let layout_of: i64 = a
        .id(&format!(
            "SELECT layout_id FROM layout_sections WHERE id = {hobbies}"
        ))
        .await;
    assert_eq!(layout_of, default);

    // Spaces are made the way any member makes them, and staff place them
    // on the default front page from the space's settings.
    let response = a
        .submit(
            "/spaces/new",
            &[
                ("name", "Board games"),
                ("slug", "board-games"),
                ("description", "All about it."),
            ],
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    let games = a
        .id("SELECT id FROM spaces WHERE slug = 'board-games'")
        .await;
    // Not on the front page until it is put there.
    assert!(!a.forum.page("/").await.contains("Board games"));
    let form = a.page("/c/board-games/settings").await;
    assert!(form.contains("Hobbies"));
    let response = a
        .submit(
            "/c/board-games/settings",
            &space_fields("board-games", "Board games", &section, "public", "members"),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    let index = a.forum.page("/").await;
    assert!(index.contains("Hobbies") && index.contains("Board games"));
    assert!(index.contains("All about it."));
    assert_eq!(default_section(&a, games).await, section);
    let admin_page = a.page("/admin/spaces").await;
    assert!(admin_page.contains("Board games"));
    let editor = a.page("/admin/layout").await;
    assert!(editor.contains("Hobbies") && editor.contains("Board games"));

    // A clash or a bad address is refused, keeping what was typed.
    for slug in ["board-games", "Board Games!"] {
        let response = a
            .submit(
                "/spaces/new",
                &[("name", "Again"), ("slug", slug), ("description", "")],
            )
            .await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "{slug}");
        assert!(response.body.contains("value=\"Again\""));
    }

    // Renaming, and taking it off the default front page.
    let response = a
        .submit(
            "/c/board-games/settings",
            &space_fields("games", "Games", "", "public", "members"),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    assert_eq!(response.location(), "/c/games/settings?done=saved");
    let index = a.forum.page("/").await;
    assert!(!index.contains("href=\"/c/games\"") && !index.contains("Board games"));
    assert!(a.forum.page("/spaces").await.contains("href=\"/c/games\""));
    assert_eq!(
        a.forum.get("/c/board-games").await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(default_section(&a, games).await, "");
    let admin_page = a.page("/admin/spaces").await;
    assert!(admin_page.contains("href=\"/c/games/settings\""));
    let editor = a.page("/admin/layout").await;
    assert!(editor.contains("Not on the default front page"), "{editor}");
    assert!(editor.contains("href=\"/c/games\""));
    // The old admin addresses lead to the space's own pages.
    let response = a
        .forum
        .get_as(&format!("/admin/spaces/{games}"), Some(&a.cookie))
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert_eq!(response.location(), "/c/games/settings");
    let response = a.forum.get_as("/admin/spaces/new", Some(&a.cookie)).await;
    assert_eq!(response.location(), "/spaces/new");

    // Sections are renamed and deleted from the layout editor; their
    // spaces stay.
    let response = a
        .submit(
            &format!("/admin/layout/sections/{hobbies}"),
            &[("name", "Pastimes")],
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    a.submit(
        "/c/games/settings",
        &space_fields("games", "Games", &section, "public", "members"),
    )
    .await;
    assert!(a.forum.page("/").await.contains("Pastimes"));
    let response = a
        .submit(&format!("/admin/layout/sections/{hobbies}/delete"), &[])
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    let index = a.forum.page("/").await;
    assert!(!index.contains("Pastimes") && !index.contains("href=\"/c/games\""));
    assert_eq!(a.forum.get("/c/games").await.status, StatusCode::OK);

    // Only empty spaces can be deleted.
    let sam = a.id("SELECT id FROM users WHERE username = 'sam'").await;
    a.forum.thread(games, sam, "Heat", "Fast").await;
    let response = a.submit("/c/games/delete", &[]).await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(response.body.contains("Only an empty space can be deleted"));
    a.forum.space("empty", "public", None, 5).await;
    let response = a.submit("/c/empty/delete", &[]).await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert_eq!(a.forum.get("/c/empty").await.status, StatusCode::NOT_FOUND);
}

#[sqlx::test]
async fn admins_mark_spaces_essential_and_moderators_dont(db: PgPool) {
    let a = admin(db).await;
    let games = a.forum.space("games", "public", None, 0).await;
    let section = default_section(&a, games).await;
    let kit = a.forum.member("kit", "moderator", None).await;
    let kit_cookie = a.forum.session(kit).await;
    // Ruth runs Games: staff act on it as staff, which is logged.
    let ruth = a.forum.member("ruth", "user", None).await;
    a.forum
        .set_grant(games, "administer", Grant::listed([], [ruth]))
        .await;
    let fields_run_by_ruth = || {
        let mut fields = space_fields("games", "Games", &section, "public", "members");
        fields.retain(|(k, _)| *k != "administer_level");
        fields.push(("administer_level", "listed"));
        fields.push(("administer_users", "ruth"));
        fields
    };

    // Moderators place spaces on the default front page but can't mark
    // them essential.
    let settings = a.forum.page_as("/c/games/settings", &kit_cookie).await;
    assert!(settings.contains("name=\"section\""));
    assert!(!settings.contains("name=\"essential\""));
    let mut fields = fields_run_by_ruth();
    fields.push(("essential", "on"));
    let response = a
        .forum
        .submit("/c/games/settings", &fields, Some(&kit_cookie))
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    let essential = |a: &Admin| {
        let db = a.forum.db.clone();
        async move {
            sqlx::query_scalar::<_, bool>("SELECT essential FROM spaces WHERE id = $1")
                .bind(games)
                .fetch_one(&db)
                .await
                .unwrap()
        }
    };
    assert!(!essential(&a).await);

    let settings = a.page("/c/games/settings").await;
    assert!(settings.contains("name=\"essential\""));
    let response = a.submit("/c/games/settings", &fields).await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    assert!(essential(&a).await);
    assert!(
        a.page("/c/games/settings")
            .await
            .contains("name=\"essential\" value=\"on\" checked")
    );
    assert!(a.page("/admin/spaces").await.contains("Essential"));
    // Leaving the box unticked clears it.
    let response = a.submit("/c/games/settings", &fields_run_by_ruth()).await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    assert!(!essential(&a).await);

    // Kit and Sam administer Games as staff, not by name, so each save is
    // logged, and marking it essential (or not) is logged in its own right.
    let actions: Vec<String> = sqlx::query_scalar(
        "SELECT action FROM mod_actions WHERE target_kind = 'space' AND target_id = $1 ORDER BY id",
    )
    .bind(games)
    .fetch_all(&a.forum.db)
    .await
    .unwrap();
    assert_eq!(
        actions,
        [
            "edit_space",
            "mark_space_essential",
            "edit_space",
            "unmark_space_essential",
            "edit_space"
        ]
    );
    let log = a.forum.page("/moderation").await;
    assert!(
        log.contains("made <a href=\"/c/games\">Games</a> essential"),
        "{log}"
    );
    assert!(
        log.contains("made <a href=\"/c/games\">Games</a> no longer essential"),
        "{log}"
    );
    assert!(!log.contains("mark_space_essential"), "{log}");
}

#[sqlx::test]
async fn changing_who_can_read_a_space_takes_effect_for_visitors_at_once(db: PgPool) {
    let a = admin(db).await;
    let sam = a.id("SELECT id FROM users WHERE username = 'sam'").await;
    let games = a.forum.space("games", "public", None, 0).await;
    let t = a.forum.thread(games, sam, "Heat", "Fast cars").await;
    let thread_path = format!("/t/{}/heat", t.thread_id);
    // Cached for visitors.
    assert!(a.forum.page(&thread_path).await.contains("Fast cars"));
    assert!(a.forum.page("/").await.contains("Heat"));
    let path = "/c/games/settings";
    let section = default_section(&a, games).await;
    let form = a.page(path).await;
    assert!(form.contains("value=\"games\""));
    let response = a
        .submit(
            path,
            &space_fields("games", "Games", &section, "members", "members"),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    assert_eq!(
        a.forum.get(&thread_path).await.status,
        StatusCode::NOT_FOUND
    );
    let index = a.forum.page("/").await;
    assert!(!index.contains("Heat") && !index.contains("Games"));
    let visibility: String = sqlx::query_scalar("SELECT visibility FROM threads")
        .fetch_one(&a.forum.db)
        .await
        .unwrap();
    assert_eq!(visibility, "members");
    // And back again.
    a.submit(
        path,
        &space_fields("games", "Games", &section, "public", "members"),
    )
    .await;
    assert!(a.forum.page(&thread_path).await.contains("Fast cars"));
}

#[sqlx::test]
async fn admins_manage_groups_and_who_is_in_them(db: PgPool) {
    let a = admin(db).await;
    let ruth = a.forum.member("ruth", "user", None).await;
    let ruth_cookie = a.forum.session(ruth).await;
    let page = a.page("/admin/groups").await;
    assert!(page.contains("No groups yet"));
    let response = a
        .submit(
            "/admin/groups",
            &[
                ("name", "Organisers"),
                ("slug", "organisers"),
                ("description", "Run the club."),
            ],
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    let organisers = a
        .id("SELECT id FROM groups WHERE slug = 'organisers'")
        .await;
    assert_eq!(response.location(), format!("/admin/groups/{organisers}"));
    // Names and short names are unique.
    for (name, slug) in [("Organisers", "other"), ("Other", "organisers")] {
        let response = a
            .submit(
                "/admin/groups",
                &[("name", name), ("slug", slug), ("description", "")],
            )
            .await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "{name} {slug}");
    }

    // A space only the organisers read.
    let planning = a.forum.space("planning", "public", None, 0).await;
    let group = organisers.to_string();
    let section = default_section(&a, planning).await;
    let mut fields = space_fields("planning", "Planning", &section, "listed", "listed");
    fields.push(("read_group", &group));
    fields.push(("reply_group", &group));
    fields.push(("start_group", &group));
    let response = a.submit("/c/planning/settings", &fields).await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    assert_eq!(
        a.forum
            .get_as("/c/planning", Some(&ruth_cookie))
            .await
            .status,
        StatusCode::NOT_FOUND
    );

    let path = format!("/admin/groups/{organisers}");
    let response = a
        .submit(&format!("{path}/members"), &[("username", "nobody")])
        .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(response.body.contains("There is no member called"));
    let response = a
        .submit(&format!("{path}/members"), &[("username", "@Ruth")])
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    let page = a.page(&path).await;
    assert!(page.contains("href=\"/u/ruth\""));
    assert!(page.contains("Planning"));
    assert!(a.page("/admin/groups").await.contains("1 member"));
    // Ruth can read and post there now.
    assert_eq!(
        a.forum
            .get_as("/c/planning/new", Some(&ruth_cookie))
            .await
            .status,
        StatusCode::OK
    );

    let response = a
        .submit(&format!("{path}/members/{ruth}/remove"), &[])
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert_eq!(
        a.forum
            .get_as("/c/planning", Some(&ruth_cookie))
            .await
            .status,
        StatusCode::NOT_FOUND
    );

    let response = a
        .submit(
            &path,
            &[
                ("name", "Committee"),
                ("slug", "committee"),
                ("description", ""),
            ],
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    assert!(a.page("/admin/groups").await.contains("Committee"));
    assert!(a.page("/").await.contains("Committee group"));

    // A group that a space's settings name can't be deleted.
    let response = a.submit(&format!("{path}/delete"), &[]).await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(response.body.contains("can’t be deleted while"));
    for verb in ["read", "reply", "start"] {
        a.forum.set_grant(planning, verb, Grant::members()).await;
    }
    let response = a.submit(&format!("{path}/delete"), &[]).await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    assert_eq!(
        a.forum.get_as(&path, Some(&a.cookie)).await.status,
        StatusCode::NOT_FOUND
    );
    assert!(a.page("/").await.contains("Members only"));
}

#[sqlx::test]
async fn staff_place_spaces_on_the_default_front_page_and_members_dont(db: PgPool) {
    let a = admin(db).await;
    let hobbies = a.forum.section("Hobbies", 0).await;
    let ruth = a.forum.member("ruth", "user", None).await;
    let ruth_cookie = a.forum.session(ruth).await;
    let games = a.forum.unlisted_space("games", "public").await;
    a.forum
        .set_grant(games, "administer", Grant::listed([], [ruth]))
        .await;
    // Ruth runs the space, but the default front page is the admin's to
    // arrange.
    let settings = a.forum.page_as("/c/games/settings", &ruth_cookie).await;
    assert!(!settings.contains("name=\"section\""));
    let section = hobbies.to_string();
    let mut fields = space_fields("games", "Games", &section, "public", "members");
    fields.push(("administer_users", "ruth"));
    fields.retain(|(k, _)| *k != "administer_level");
    fields.push(("administer_level", "listed"));
    let response = a
        .forum
        .submit("/c/games/settings", &fields, Some(&ruth_cookie))
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    assert_eq!(default_section(&a, games).await, "");
    let settings = a.page("/c/games/settings").await;
    assert!(settings.contains("name=\"section\""));
    let response = a.submit("/c/games/settings", &fields).await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    assert_eq!(default_section(&a, games).await, section);
    // Ruth's own copy of the front page follows the default, since she
    // hasn't rearranged it.
    assert!(
        a.forum
            .page_as("/", &ruth_cookie)
            .await
            .contains("href=\"/c/games\"")
    );
    // A section of someone's own layout isn't on offer.
    let ruth_layout = layouts::member_id(&a.forum.db, ruth)
        .await
        .unwrap()
        .unwrap();
    let hers = layouts::add_section(&a.forum.db, ruth_layout, "Hers")
        .await
        .unwrap();
    for (key, value) in [("section", "999"), ("section", &hers.to_string())] {
        let mut fields = fields.clone();
        fields.retain(|(k, _)| *k != key);
        fields.push((key, value));
        let response = a.submit("/c/games/settings", &fields).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "{key}={value}");
    }
}

#[sqlx::test]
async fn only_admins_manage_spaces_and_groups(db: PgPool) {
    let forum = Forum::new(db);
    let ruth = forum.member("ruth", "moderator", None).await;
    let cookie = forum.session(ruth).await;
    let games = forum.space("games", "public", None, 0).await;
    let group = forum.group("organisers", "Organisers").await;
    let section = forum.section("Hobbies", 0).await;
    let pages = [
        "/admin/spaces".to_owned(),
        "/admin/spaces/new".to_owned(),
        format!("/admin/spaces/{games}"),
        "/admin/groups".to_owned(),
        format!("/admin/groups/{group}"),
        format!("/admin/groups/{group}/delete"),
        "/admin/layout".to_owned(),
        format!("/admin/layout/sections/{section}/delete"),
    ];
    for path in &pages {
        assert_eq!(
            forum.get_as(path, Some(&cookie)).await.status,
            StatusCode::FORBIDDEN,
            "{path}"
        );
        let response = forum.get(path).await;
        assert_eq!(response.status, StatusCode::SEE_OTHER, "{path}");
        assert!(response.location().starts_with("/login?next="));
    }
    let posts = [
        "/admin/layout/sections".to_owned(),
        format!("/admin/layout/sections/{section}"),
        format!("/admin/layout/sections/{section}/delete"),
        "/admin/groups".to_owned(),
        format!("/admin/groups/{group}"),
        format!("/admin/groups/{group}/members"),
        format!("/admin/groups/{group}/members/{ruth}/remove"),
        format!("/admin/groups/{group}/delete"),
    ];
    for path in &posts {
        let response = forum
            .submit(path, &[("name", "x"), ("username", "ruth")], Some(&cookie))
            .await;
        assert_eq!(response.status, StatusCode::FORBIDDEN, "{path}");
    }
    let groups: i64 = sqlx::query_scalar("SELECT count(*) FROM groups WHERE owner_user_id IS NULL")
        .fetch_one(&forum.db)
        .await
        .unwrap();
    assert_eq!(groups, 1);
    assert!(forum.get("/c/games").await.status == StatusCode::OK);
}

#[sqlx::test]
async fn unknown_spaces_and_groups_are_not_found(db: PgPool) {
    let a = admin(db).await;
    for path in [
        "/admin/spaces/999",
        "/admin/spaces/x",
        "/admin/groups/999",
        "/admin/groups/x",
        "/admin/groups/999/delete",
        "/admin/groups/x/delete",
        "/admin/layout/sections/999/delete",
        "/admin/layout/sections/x/delete",
    ] {
        assert_eq!(
            a.forum.get_as(path, Some(&a.cookie)).await.status,
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }
    for path in [
        "/admin/layout/sections/999",
        "/admin/layout/sections/999/delete",
        "/admin/groups/999",
        "/admin/groups/999/members",
        "/admin/groups/999/delete",
    ] {
        let response = a
            .submit(
                path,
                &[
                    ("name", "Name"),
                    ("slug", "slug"),
                    ("position", "1"),
                    ("username", "sam"),
                ],
            )
            .await;
        assert_eq!(response.status, StatusCode::NOT_FOUND, "{path}");
    }
}

#[sqlx::test]
async fn deleting_a_group_asks_first_and_says_what_it_will_change(db: PgPool) {
    let a = admin(db).await;
    let sam = a.id("SELECT id FROM users WHERE username = 'sam'").await;
    let ruth = a.forum.member("ruth", "user", None).await;
    let organisers = a.forum.group("organisers", "Organisers").await;
    let board = a.forum.group("board", "Board").await;
    a.forum.join_group(organisers, ruth).await;
    // Only the organisers read Planning; the board reads Accounts too.
    let planning = a.forum.space("planning", "public", None, 0).await;
    a.forum
        .restrict(planning, &[organisers], Some(&[organisers]))
        .await;
    let accounts = a.forum.space("accounts", "public", None, 1).await;
    a.forum
        .restrict(accounts, &[organisers, board], Some(&[board]))
        .await;
    a.forum
        .exec(&format!(
            "INSERT INTO invites (code_hash, created_by, max_uses, expires_at, grant_group_ids)
             VALUES ('\\x01', {sam}, 5, now() + interval '7 days', ARRAY[{organisers}]::bigint[])"
        ))
        .await;

    // The group's page links to a page that asks, rather than deleting.
    let path = format!("/admin/groups/{organisers}");
    let delete_path = format!("{path}/delete");
    let page = a.page(&path).await;
    assert!(page.contains(&format!("href=\"{delete_path}\"")));
    assert!(!page.contains(&format!("action=\"{delete_path}\"")));

    // While spaces name the group, it can't be deleted, and the page says
    // which spaces those are.
    let confirm = a.page(&delete_path).await;
    assert!(!confirm.contains(&format!("action=\"{delete_path}\"")));
    assert!(confirm.contains("can’t be deleted"));
    assert!(confirm.contains("href=\"/c/planning\""));
    assert!(confirm.contains("href=\"/c/accounts\""));
    let response = a.submit(&delete_path, &[]).await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert_eq!(
        a.id("SELECT count(*) FROM groups WHERE owner_user_id IS NULL")
            .await,
        2
    );

    // Taken out of both spaces' settings, it can go.
    a.forum.restrict(planning, &[board], Some(&[board])).await;
    a.forum.restrict(accounts, &[board], Some(&[board])).await;
    let confirm = a.page(&delete_path).await;
    assert!(confirm.contains(&format!("action=\"{delete_path}\"")));
    assert!(confirm.contains(&format!("href=\"{path}\"")));
    assert!(confirm.contains("can’t be undone"));
    assert!(confirm.contains("1 member"));
    assert!(confirm.contains("1 invite"));
    // Asking changes nothing.
    assert_eq!(
        a.id("SELECT count(*) FROM groups WHERE owner_user_id IS NULL")
            .await,
        2,
        "the confirmation page deleted the group"
    );

    let response = a.submit(&delete_path, &[]).await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert_eq!(
        a.id("SELECT count(*) FROM groups WHERE owner_user_id IS NULL")
            .await,
        1
    );
}

#[sqlx::test]
async fn deleting_a_section_asks_first_and_names_its_spaces(db: PgPool) {
    let a = admin(db).await;
    let hobbies = a.forum.section("Hobbies", 0).await;
    a.forum
        .space("board-games", "public", Some(hobbies), 0)
        .await;
    let delete_path = format!("/admin/layout/sections/{hobbies}/delete");

    let page = a.page("/admin/layout").await;
    assert!(page.contains(&format!("href=\"{delete_path}\"")));
    assert!(!page.contains(&format!("action=\"{delete_path}\"")));

    let confirm = a.page(&delete_path).await;
    assert!(confirm.contains("Hobbies"));
    assert!(confirm.contains("Board games"));
    assert!(confirm.contains(&format!("action=\"{delete_path}\"")));
    assert!(confirm.contains("href=\"/admin/layout\""));
    let default = layouts::default_id(&a.forum.db).await.unwrap();
    let sections = |a: &Admin| {
        let db = a.forum.db.clone();
        async move {
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM layout_sections WHERE layout_id = $1",
            )
            .bind(default)
            .fetch_one(&db)
            .await
            .unwrap()
        }
    };
    assert_eq!(sections(&a).await, 1);

    let response = a.submit(&delete_path, &[]).await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert_eq!(sections(&a).await, 0);
    assert_eq!(a.id("SELECT count(*) FROM spaces").await, 1);

    // A section of a member's own layout isn't an admin's to delete.
    let ruth = a.forum.member("ruth", "user", None).await;
    let ruth_layout = layouts::member_id(&a.forum.db, ruth)
        .await
        .unwrap()
        .unwrap();
    let hers = layouts::add_section(&a.forum.db, ruth_layout, "Hers")
        .await
        .unwrap();
    let delete_hers = format!("/admin/layout/sections/{hers}/delete");
    assert_eq!(
        a.forum.get_as(&delete_hers, Some(&a.cookie)).await.status,
        StatusCode::NOT_FOUND
    );
    for path in [delete_hers, format!("/admin/layout/sections/{hers}")] {
        let response = a.submit(&path, &[("name", "Theirs")]).await;
        assert_eq!(response.status, StatusCode::NOT_FOUND, "{path}");
    }
    assert!(
        layouts::read(&a.forum.db, ruth_layout)
            .await
            .unwrap()
            .unwrap()
            .sections
            .iter()
            .any(|s| s.id == hers && s.name == "Hers")
    );
}
