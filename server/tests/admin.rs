//! Admin pages for members: giving people roles (DESIGN.md §3.1, §4).

mod common;

use axum::http::StatusCode;
use common::Forum;
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
// Sections, categories and groups

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

fn category_fields<'a>(
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
        ("write_level", write),
    ]
}

#[sqlx::test]
async fn admins_arrange_sections_and_categories(db: PgPool) {
    let a = admin(db).await;
    let page = a.page("/admin/categories").await;
    assert!(page.contains("href=\"/admin/groups\""));
    assert!(page.contains("No categories yet"));

    let response = a
        .submit(
            "/admin/sections",
            &[("name", " Hobbies "), ("position", "2")],
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    let hobbies = a.id("SELECT id FROM sections WHERE name = 'Hobbies'").await;
    let section = hobbies.to_string();

    let form = a.page("/admin/categories/new").await;
    assert!(form.contains("Hobbies"));
    let response = a
        .submit(
            "/admin/categories/new",
            &category_fields("board-games", "Board games", &section, "public", "members"),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    let games = a
        .id("SELECT id FROM categories WHERE slug = 'board-games'")
        .await;
    let index = a.forum.page("/").await;
    assert!(index.contains("Hobbies") && index.contains("Board games"));
    assert!(index.contains("All about it."));

    // A clash or a bad address is refused, keeping what was typed.
    for slug in ["board-games", "Board Games!"] {
        let response = a
            .submit(
                "/admin/categories/new",
                &category_fields(slug, "Again", &section, "public", "members"),
            )
            .await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "{slug}");
        assert!(response.body.contains("value=\"Again\""));
    }

    // Renaming and moving.
    let path = format!("/admin/categories/{games}");
    let response = a
        .submit(
            &path,
            &category_fields("games", "Games", "", "public", "members"),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    let index = a.forum.page("/").await;
    assert!(index.contains("href=\"/c/games\"") && !index.contains("Board games"));
    assert_eq!(
        a.forum.get("/c/board-games").await.status,
        StatusCode::NOT_FOUND
    );

    // Sections are renamed, reordered and deleted; their categories stay.
    let response = a
        .submit(
            &format!("/admin/sections/{hobbies}"),
            &[("name", "Pastimes"), ("position", "3")],
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    a.submit(
        &path,
        &category_fields("games", "Games", &section, "public", "members"),
    )
    .await;
    assert!(a.forum.page("/").await.contains("Pastimes"));
    let response = a
        .submit(&format!("/admin/sections/{hobbies}/delete"), &[])
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    let index = a.forum.page("/").await;
    assert!(!index.contains("Pastimes") && index.contains("href=\"/c/games\""));

    // Only empty categories can be deleted.
    let sam = a.id("SELECT id FROM users WHERE username = 'sam'").await;
    a.forum.thread(games, sam, "Heat", "Fast").await;
    let response = a
        .submit(&format!("/admin/categories/{games}/delete"), &[])
        .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(
        response
            .body
            .contains("Only an empty category can be deleted")
    );
    let empty = a.forum.category("empty", "public", None, 5).await;
    let response = a
        .submit(&format!("/admin/categories/{empty}/delete"), &[])
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert_eq!(a.forum.get("/c/empty").await.status, StatusCode::NOT_FOUND);
}

#[sqlx::test]
async fn changing_who_can_read_a_category_takes_effect_for_visitors_at_once(db: PgPool) {
    let a = admin(db).await;
    let sam = a.id("SELECT id FROM users WHERE username = 'sam'").await;
    let games = a.forum.category("games", "public", None, 0).await;
    let t = a.forum.thread(games, sam, "Heat", "Fast cars").await;
    let thread_path = format!("/t/{}/heat", t.thread_id);
    // Cached for visitors.
    assert!(a.forum.page(&thread_path).await.contains("Fast cars"));
    assert!(a.forum.page("/").await.contains("Heat"));
    let path = format!("/admin/categories/{games}");
    let form = a.page(&path).await;
    assert!(form.contains("value=\"games\""));
    let response = a
        .submit(
            &path,
            &category_fields("games", "Games", "", "members", "members"),
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
        &path,
        &category_fields("games", "Games", "", "public", "members"),
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

    // A category only the organisers read.
    let planning = a.forum.category("planning", "public", None, 0).await;
    let group = organisers.to_string();
    let mut fields = category_fields("planning", "Planning", "", "groups", "groups");
    fields.push(("read_group", &group));
    fields.push(("write_group", &group));
    let response = a
        .submit(&format!("/admin/categories/{planning}"), &fields)
        .await;
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

    let response = a.submit(&format!("{path}/delete"), &[]).await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert_eq!(
        a.forum.get_as(&path, Some(&a.cookie)).await.status,
        StatusCode::NOT_FOUND
    );
    // The category is now for staff only.
    assert!(a.page("/").await.contains("Staff only"));
}

#[sqlx::test]
async fn category_settings_are_checked(db: PgPool) {
    let a = admin(db).await;
    let organisers = a.forum.group("organisers", "Organisers").await.to_string();
    let board = a.forum.group("board", "Board").await.to_string();
    let mut fields = category_fields("planning", "Planning", "", "groups", "groups");
    fields.push(("read_group", &organisers));
    fields.push(("write_group", &board));
    let response = a.submit("/admin/categories/new", &fields).await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(
        response
            .body
            .contains("Groups that can post must also be able to read.")
    );
    for (key, value) in [
        ("read_level", "everyone"),
        ("write_level", "anyone"),
        ("section", "999"),
        ("position", "first"),
        ("name", ""),
    ] {
        let mut fields = category_fields("planning", "Planning", "", "public", "members");
        fields.retain(|(k, _)| *k != key);
        fields.push((key, value));
        let response = a.submit("/admin/categories/new", &fields).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "{key}={value}");
    }
    let mut fields = category_fields("planning", "Planning", "", "groups", "members");
    fields.push(("read_group", "999"));
    let response = a.submit("/admin/categories/new", &fields).await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(response.body.contains("Choose groups from the list."));
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM categories")
        .fetch_one(&a.forum.db)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[sqlx::test]
async fn only_admins_manage_categories_and_groups(db: PgPool) {
    let forum = Forum::new(db);
    let ruth = forum.member("ruth", "moderator", None).await;
    let cookie = forum.session(ruth).await;
    let games = forum.category("games", "public", None, 0).await;
    let group = forum.group("organisers", "Organisers").await;
    let pages = [
        "/admin/categories".to_owned(),
        "/admin/categories/new".to_owned(),
        format!("/admin/categories/{games}"),
        "/admin/groups".to_owned(),
        format!("/admin/groups/{group}"),
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
        "/admin/sections".to_owned(),
        "/admin/sections/1".to_owned(),
        "/admin/sections/1/delete".to_owned(),
        "/admin/categories/new".to_owned(),
        format!("/admin/categories/{games}"),
        format!("/admin/categories/{games}/delete"),
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
    let groups: i64 = sqlx::query_scalar("SELECT count(*) FROM groups")
        .fetch_one(&forum.db)
        .await
        .unwrap();
    assert_eq!(groups, 1);
    assert!(forum.get("/c/games").await.status == StatusCode::OK);
}

#[sqlx::test]
async fn unknown_categories_and_groups_are_not_found(db: PgPool) {
    let a = admin(db).await;
    for path in [
        "/admin/categories/999",
        "/admin/categories/x",
        "/admin/groups/999",
        "/admin/groups/x",
    ] {
        assert_eq!(
            a.forum.get_as(path, Some(&a.cookie)).await.status,
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }
    for path in [
        "/admin/categories/999",
        "/admin/categories/999/delete",
        "/admin/sections/999",
        "/admin/sections/999/delete",
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
