//! Members-only and group-restricted categories, end to end (DESIGN.md
//! §3.1): who can read and post in them, what the index says about them,
//! and that membership changes take effect straight away.

mod common;

use axum::http::StatusCode;
use common::Forum;
use sqlx::PgPool;

struct Club {
    f: Forum,
    ruth: i64,
    ruth_cookie: String,
    jonah_cookie: String,
    staff_cookie: String,
    organisers: i64,
    planning: i64,
    thread: parlour::store::CreatedThread,
}

/// Ruth is an organiser; Jonah is a member who isn't; Sam is a moderator.
async fn club(pool: PgPool) -> Club {
    let f = Forum::new(pool);
    let ruth = f.member("ruth", "user", None).await;
    let jonah = f.member("jonah", "user", None).await;
    let sam = f.member("sam", "moderator", None).await;
    f.category("games", "public", None, 0).await;
    f.category("lounge", "members", None, 1).await;
    let planning = f.category("planning", "public", None, 2).await;
    let organisers = f.group("organisers", "Organisers").await;
    f.join_group(organisers, ruth).await;
    f.restrict(planning, &[organisers], None).await;
    let thread = f
        .thread(
            planning,
            ruth,
            "Budget for 2027",
            "Secret plans for the budget",
        )
        .await;
    Club {
        ruth_cookie: f.session(ruth).await,
        jonah_cookie: f.session(jonah).await,
        staff_cookie: f.session(sam).await,
        f,
        ruth,
        organisers,
        planning,
        thread,
    }
}

impl Club {
    fn thread_path(&self) -> String {
        format!("/t/{}/budget-for-2027", self.thread.thread_id)
    }

    /// The pages about the restricted thread and its category.
    fn private_paths(&self) -> Vec<String> {
        vec![
            "/c/planning".to_owned(),
            self.thread_path(),
            format!("/t/{}", self.thread.thread_id),
            format!("/t/{}/unread", self.thread.thread_id),
            format!("/p/{}", self.thread.first_post_id),
            format!("/p/{}/revisions", self.thread.first_post_id),
            format!("/p/{}/reactions", self.thread.first_post_id),
            format!("/p/{}/fork", self.thread.first_post_id),
            format!("/t/{}/reply", self.thread.thread_id),
            "/c/planning/new".to_owned(),
        ]
    }
}

/// Pages for writing, which ask visitors to log in before anything else.
const WRITE_PAGES: [&str; 3] = ["/fork", "/reply", "/new"];

#[sqlx::test]
async fn only_group_members_and_staff_read_a_group_category(pool: PgPool) {
    let club = club(pool).await;
    // Visitors and members outside the group get a 404, not a 403, so the
    // category's existence isn't revealed.
    for cookie in [None, Some(club.jonah_cookie.as_str())] {
        for path in club.private_paths() {
            let response = club.f.get_as(&path, cookie).await;
            if cookie.is_none() && WRITE_PAGES.iter().any(|end| path.ends_with(end)) {
                // Visitors are sent to log in first, whatever the page is
                // about, so this says nothing either.
                assert_eq!(response.status, StatusCode::SEE_OTHER, "{path}");
                let elsewhere = club
                    .f
                    .get(
                        &path
                            .replace("planning", "nowhere")
                            .replace(&format!("/{}/", club.thread.thread_id), "/999999/")
                            .replace(&format!("/{}/", club.thread.first_post_id), "/999999/"),
                    )
                    .await;
                assert_eq!(elsewhere.status, StatusCode::SEE_OTHER, "{path}");
                continue;
            }
            assert_eq!(response.status, StatusCode::NOT_FOUND, "{path} {cookie:?}");
            assert!(!response.body.contains("Secret plans"), "{path}");
            assert!(!response.body.contains("Budget for 2027"), "{path}");
        }
        for path in ["/", "/latest", "/u/ruth"] {
            let page = club.f.get_as(path, cookie).await.body;
            assert!(!page.contains("Planning"), "{path} {cookie:?}");
            assert!(!page.contains("Budget for 2027"), "{path} {cookie:?}");
        }
    }
    for cookie in [&club.ruth_cookie, &club.staff_cookie] {
        let page = club.f.page_as(&club.thread_path(), cookie).await;
        assert!(page.contains("Secret plans"));
        let index = club.f.page_as("/", cookie).await;
        assert!(index.contains("Planning") && index.contains("Budget for 2027"));
        assert!(
            club.f
                .page_as("/latest", cookie)
                .await
                .contains("Budget for 2027")
        );
    }
}

#[sqlx::test]
async fn the_index_says_who_can_read_each_category(pool: PgPool) {
    let club = club(pool).await;
    let index = club.f.page_as("/", &club.ruth_cookie).await;
    assert!(index.contains(r#"<span class="flag flag-access">Members only</span>"#));
    assert!(index.contains(r#"<span class="flag flag-access">Organisers group</span>"#));
    assert_eq!(index.matches("flag-access").count(), 2, "{index}");
    let category = club.f.page_as("/c/planning", &club.ruth_cookie).await;
    assert!(category.contains("Organisers group"));
    let lounge = club.f.page_as("/c/lounge", &club.ruth_cookie).await;
    assert!(lounge.contains("Members only"));
    // Staff see group categories that no group may read, as staff only.
    club.f.restrict(club.planning, &[], None).await;
    let index = club.f.page_as("/", &club.staff_cookie).await;
    assert!(index.contains(r#"<span class="flag flag-access">Staff only</span>"#));
}

#[sqlx::test]
async fn joining_and_leaving_a_group_takes_effect_at_once(pool: PgPool) {
    let club = club(pool).await;
    let jonah: i64 = sqlx::query_scalar("SELECT id FROM users WHERE username = 'jonah'")
        .fetch_one(&club.f.db)
        .await
        .unwrap();
    let path = club.thread_path();
    assert_eq!(
        club.f.get_as(&path, Some(&club.jonah_cookie)).await.status,
        StatusCode::NOT_FOUND
    );
    club.f.join_group(club.organisers, jonah).await;
    assert_eq!(
        club.f.get_as(&path, Some(&club.jonah_cookie)).await.status,
        StatusCode::OK
    );
    assert!(
        parlour::groups::remove_member(club.f.state.db(), club.organisers, club.ruth)
            .await
            .unwrap()
    );
    assert_eq!(
        club.f.get_as(&path, Some(&club.ruth_cookie)).await.status,
        StatusCode::NOT_FOUND
    );
    // Opening the category to members lets everyone logged in read it.
    club.f
        .exec(&format!(
            "UPDATE categories SET read_level = 'members' WHERE id = {}",
            club.planning
        ))
        .await;
    assert_eq!(
        club.f.get_as(&path, Some(&club.ruth_cookie)).await.status,
        StatusCode::OK
    );
    assert_eq!(club.f.get(&path).await.status, StatusCode::NOT_FOUND);
}

async fn post(f: &Forum, cookie: &str) -> StatusCode {
    f.submit(
        "/c/notices/new",
        &[("title", "Meeting moved"), ("body", "To Thursday")],
        Some(cookie),
    )
    .await
    .status
}

#[sqlx::test]
async fn group_members_post_where_their_group_may_write(pool: PgPool) {
    let club = club(pool).await;
    // Anyone can read the notices; only organisers post them.
    let notices = club.f.category("notices", "public", None, 3).await;
    let db = club.f.state.db();
    let viewer = parlour::viewer::Viewer::User(
        parlour::sessions::user_ctx(db, club.ruth)
            .await
            .unwrap()
            .unwrap(),
    );
    let mut settings = parlour::content::category_by_id(db, &viewer, notices)
        .await
        .unwrap()
        .unwrap()
        .settings();
    settings.write_level = "groups".into();
    settings.write_groups = vec![club.organisers];
    parlour::store::update_category(db, notices, &settings.clean().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        post(&club.f, &club.jonah_cookie).await,
        StatusCode::FORBIDDEN
    );
    assert!(
        !club
            .f
            .page_as("/c/notices", &club.jonah_cookie)
            .await
            .contains("href=\"/c/notices/new\"")
    );
    assert_eq!(
        post(&club.f, &club.ruth_cookie).await,
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        post(&club.f, &club.staff_cookie).await,
        StatusCode::SEE_OTHER
    );
    // Group members also post in a group category their group may read.
    let response = club
        .f
        .submit(
            &format!("/t/{}/reply", club.thread.thread_id),
            &[("body", "Agreed")],
            Some(&club.ruth_cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
}

#[sqlx::test]
async fn group_posts_can_only_be_quoted_somewhere_as_private(pool: PgPool) {
    let club = club(pool).await;
    let board = club.f.group("board", "Board").await;
    let both = club.f.category("both", "public", None, 4).await;
    club.f.restrict(both, &[club.organisers, board], None).await;
    let minutes = club.f.category("minutes", "public", None, 5).await;
    club.f.restrict(minutes, &[club.organisers], None).await;
    let path = format!("/p/{}/fork", club.thread.first_post_id);
    let form = club.f.page_as(&path, &club.ruth_cookie).await;
    // Only categories read by no one outside the organisers.
    assert!(form.contains("value=\"minutes\""), "{form}");
    assert!(form.contains("Minutes (Organisers group)"), "{form}");
    for slug in ["games", "lounge", "both"] {
        assert!(!form.contains(&format!("value=\"{slug}\"")), "{slug}");
        let response = club
            .f
            .submit(
                &path,
                &[("title", "Leak"), ("category", slug), ("body", "x")],
                Some(&club.ruth_cookie),
            )
            .await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "{slug}");
    }
    let response = club
        .f
        .submit(
            &path,
            &[("title", "Minuted"), ("category", "minutes"), ("body", "x")],
            Some(&club.ruth_cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
}
