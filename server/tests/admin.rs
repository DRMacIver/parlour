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
