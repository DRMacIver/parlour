//! Members-only and group-restricted spaces, end to end (DESIGN.md
//! §3.1): who can read and post in them, what the index says about them,
//! and that membership changes take effect straight away.

mod common;

use axum::http::StatusCode;
use common::Forum;
use parlour::permissions::Grant;
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
    f.space("games", "public", None, 0).await;
    f.space("lounge", "members", None, 1).await;
    let planning = f.space("planning", "public", None, 2).await;
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

    /// The pages about the restricted thread and its space.
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
async fn only_group_members_and_staff_read_a_group_space(pool: PgPool) {
    let club = club(pool).await;
    // Visitors and members outside the group get a 404, not a 403, so the
    // space's existence isn't revealed.
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
async fn the_index_says_who_can_read_each_space(pool: PgPool) {
    let club = club(pool).await;
    let index = club.f.page_as("/", &club.ruth_cookie).await;
    // As in the mockup, a padlock marks members-only spaces and a
    // pair of people group ones (Main.dc.html).
    // (The third is Ruth's own feed, which members read.)
    let flags = access_flags(&index);
    assert_eq!(flags.len(), 3, "{index}");
    assert!(
        flags[0].ends_with(">Members only") && flags[0].contains(LOCK),
        "{}",
        flags[0]
    );
    assert!(
        flags[1].ends_with(">Organisers group") && flags[1].contains(PEOPLE),
        "{}",
        flags[1]
    );
    let space = club.f.page_as("/c/planning", &club.ruth_cookie).await;
    assert!(space.contains("Organisers group"));
    let lounge = club.f.page_as("/c/lounge", &club.ruth_cookie).await;
    assert!(lounge.contains("Members only"));
    // A space whose read grant names nobody is for "chosen members" (none
    // yet); one for staff says so.
    club.f.restrict(club.planning, &[], None).await;
    let index = club.f.page_as("/", &club.staff_cookie).await;
    assert!(
        access_flags(&index)
            .iter()
            .any(|flag| flag.ends_with(">Chosen members") && flag.contains(LOCK))
    );
    club.f
        .set_grant(club.planning, "read", Grant::staff())
        .await;
    let index = club.f.page_as("/", &club.staff_cookie).await;
    assert!(
        access_flags(&index)
            .iter()
            .any(|flag| flag.ends_with(">Staff only") && flag.contains(LOCK))
    );
}

/// Part of the padlock icon.
const LOCK: &str = "M8 11V7a4 4 0 0 1 8 0v4";
/// Part of the people icon.
const PEOPLE: &str = "M2.5 20a6.5 6.5 0 0 1 13 0";

/// The contents of each access label on a page, in order.
fn access_flags(page: &str) -> Vec<&str> {
    page.split(r#"<span class="flag flag-access">"#)
        .skip(1)
        .map(|rest| rest.split("</span>").next().unwrap())
        .collect()
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
    // Opening the space to members lets everyone logged in read it.
    club.f
        .set_grant(club.planning, "read", Grant::members())
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
    let notices = club.f.space("notices", "public", None, 3).await;
    for verb in ["reply", "start"] {
        club.f
            .set_grant(notices, verb, Grant::listed([club.organisers], []))
            .await;
    }
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
    // Staff post only where a grant lets them (DESIGN.md §3.1).
    assert_eq!(
        post(&club.f, &club.staff_cookie).await,
        StatusCode::FORBIDDEN
    );
    // Group members also post in a group space their group may read.
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
    let both = club.f.space("both", "public", None, 4).await;
    club.f.restrict(both, &[club.organisers, board], None).await;
    let minutes = club.f.space("minutes", "public", None, 5).await;
    club.f.restrict(minutes, &[club.organisers], None).await;
    let path = format!("/p/{}/fork", club.thread.first_post_id);
    let form = club.f.page_as(&path, &club.ruth_cookie).await;
    // Only spaces read by no one outside the organisers.
    assert!(form.contains("value=\"minutes\""), "{form}");
    assert!(form.contains("Minutes (Organisers group)"), "{form}");
    for slug in ["games", "lounge", "both"] {
        assert!(!form.contains(&format!("value=\"{slug}\"")), "{slug}");
        let response = club
            .f
            .submit(
                &path,
                &[("title", "Leak"), ("space", slug), ("body", "x")],
                Some(&club.ruth_cookie),
            )
            .await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "{slug}");
    }
    let response = club
        .f
        .submit(
            &path,
            &[("title", "Minuted"), ("space", "minutes"), ("body", "x")],
            Some(&club.ruth_cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
}
