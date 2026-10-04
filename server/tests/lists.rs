//! Lists (DESIGN.md §3.3, milestone 12): groups that members own and
//! manage from `/settings/lists`, the Friends list every member starts
//! with, lists in grants, the rule that nobody who isn't on a list learns
//! it exists, and what happens to a list as people come and go.

mod common;

use axum::http::StatusCode;
use common::Forum;
use parlour::permissions::Grant;
use sqlx::PgPool;

const CREATION_NOTE: &str = "People on this list can see its name and who else is on it.";

struct Club {
    f: Forum,
    ruth: i64,
    jonah: i64,
    maya: i64,
    ruth_cookie: String,
    jonah_cookie: String,
    maya_cookie: String,
    admin_cookie: String,
}

/// Ruth, Jonah and Maya are members; Sam is an admin.
async fn club(pool: PgPool) -> Club {
    let f = Forum::new(pool);
    let ruth = f.member("ruth", "user", None).await;
    let jonah = f.member("jonah", "user", None).await;
    let maya = f.member("maya", "user", None).await;
    let sam = f.member("sam", "admin", None).await;
    Club {
        ruth_cookie: f.session(ruth).await,
        jonah_cookie: f.session(jonah).await,
        maya_cookie: f.session(maya).await,
        admin_cookie: f.session(sam).await,
        f,
        ruth,
        jonah,
        maya,
    }
}

impl Club {
    /// Makes a list of Ruth's through the form, returning its id.
    async fn ruths_list(&self, name: &str) -> i64 {
        let response = self
            .f
            .submit(
                "/settings/lists",
                &[("name", name)],
                Some(&self.ruth_cookie),
            )
            .await;
        assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
        response
            .location()
            .strip_prefix("/settings/lists/")
            .and_then(|rest| rest.split('?').next())
            .and_then(|id| id.parse().ok())
            .unwrap_or_else(|| panic!("made a list at {}", response.location()))
    }

    /// Adds a member to one of Ruth's lists through the form.
    async fn add(&self, list: i64, username: &str) -> common::Response {
        self.f
            .submit(
                &format!("/settings/lists/{list}/members"),
                &[("username", username)],
                Some(&self.ruth_cookie),
            )
            .await
    }

    async fn members_of(&self, list: i64) -> Vec<String> {
        sqlx::query_scalar(
            "SELECT u.username::text FROM group_members m JOIN users u ON u.id = m.user_id
             WHERE m.group_id = $1 ORDER BY 1",
        )
        .bind(list)
        .fetch_all(&self.f.db)
        .await
        .unwrap()
    }
}

async fn friends_of(f: &Forum, user: i64) -> i64 {
    sqlx::query_scalar("SELECT id FROM groups WHERE owner_user_id = $1 AND name = 'Friends'")
        .bind(user)
        .fetch_one(&f.db)
        .await
        .unwrap()
}

#[sqlx::test]
async fn every_member_starts_with_an_empty_friends_list(pool: PgPool) {
    let club = club(pool).await;
    for user in [club.ruth, club.jonah, club.maya] {
        let friends = friends_of(&club.f, user).await;
        let members: i64 =
            sqlx::query_scalar("SELECT count(*) FROM group_members WHERE group_id = $1")
                .bind(friends)
                .fetch_one(&club.f.db)
                .await
                .unwrap();
        assert_eq!(members, 0);
    }
    // Accounts made any other way get one too: here, joining through an
    // invite.
    let kit = club.f.invitee("kit", club.ruth).await;
    friends_of(&club.f, kit).await;
    let page = club.f.page_as("/settings/lists", &club.ruth_cookie).await;
    assert!(page.contains("Friends"), "{page}");
    assert!(page.contains("Nobody is on it yet"), "{page}");
    // The account settings lead there.
    let settings = club.f.page_as("/settings", &club.ruth_cookie).await;
    assert!(settings.contains("href=\"/settings/lists\""));
}

#[sqlx::test]
async fn visitors_are_sent_to_log_in(pool: PgPool) {
    let club = club(pool).await;
    let friends = friends_of(&club.f, club.ruth).await;
    for path in [
        "/settings/lists".to_owned(),
        format!("/settings/lists/{friends}"),
    ] {
        let response = club.f.get(&path).await;
        assert_eq!(response.status, StatusCode::SEE_OTHER, "{path}");
        assert!(response.location().starts_with("/login"), "{path}");
    }
}

#[sqlx::test]
async fn members_make_rename_and_delete_lists(pool: PgPool) {
    let club = club(pool).await;
    let page = club.f.page_as("/settings/lists", &club.ruth_cookie).await;
    assert!(page.contains(CREATION_NOTE), "{page}");
    let list = club.ruths_list("  Book   club ").await;
    let page = club
        .f
        .page_as(&format!("/settings/lists/{list}"), &club.ruth_cookie)
        .await;
    assert!(page.contains("Book club"), "{page}");
    // Names are the owner's own: two lists of theirs can't share one, but
    // anyone else may use it.
    for name in ["book CLUB", "Friends", "", &"x".repeat(61)] {
        let response = club
            .f
            .submit(
                "/settings/lists",
                &[("name", name)],
                Some(&club.ruth_cookie),
            )
            .await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "{name}");
    }
    let response = club
        .f
        .submit(
            "/settings/lists",
            &[("name", "Book club")],
            Some(&club.jonah_cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    // Renaming.
    let response = club
        .f
        .submit(
            &format!("/settings/lists/{list}"),
            &[("name", "Readers")],
            Some(&club.ruth_cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert_eq!(
        response.location(),
        format!("/settings/lists/{list}?done=saved")
    );
    let page = club.f.page_as("/settings/lists", &club.ruth_cookie).await;
    assert!(page.contains("Readers") && !page.contains("Book club"));
    let response = club
        .f
        .submit(
            &format!("/settings/lists/{list}"),
            &[("name", "friends")],
            Some(&club.ruth_cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(
        response
            .body
            .contains("You already have a list called “friends”."),
        "{}",
        response.body
    );
    // Deleting one nothing names.
    club.add(list, "jonah").await;
    let response = club
        .f
        .submit(
            &format!("/settings/lists/{list}/delete"),
            &[],
            Some(&club.ruth_cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert_eq!(response.location(), "/settings/lists?done=deleted");
    let gone: i64 = sqlx::query_scalar("SELECT count(*) FROM groups WHERE id = $1")
        .bind(list)
        .fetch_one(&club.f.db)
        .await
        .unwrap();
    assert_eq!(gone, 0);
    assert_eq!(
        club.f
            .get_as(&format!("/settings/lists/{list}"), Some(&club.ruth_cookie))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

#[sqlx::test]
async fn making_lists_is_rate_limited(pool: PgPool) {
    let club = club(pool).await;
    let mut refused = None;
    for n in 0..40 {
        let response = club
            .f
            .submit(
                "/settings/lists",
                &[("name", &format!("List {n}"))],
                Some(&club.ruth_cookie),
            )
            .await;
        if response.status != StatusCode::SEE_OTHER {
            refused = Some((n, response.status));
            break;
        }
    }
    let (n, status) = refused.expect("some list was refused");
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert!(n >= 10, "refused after {n}");
}

#[sqlx::test]
async fn owners_add_and_remove_members_by_username(pool: PgPool) {
    let club = club(pool).await;
    let friends = friends_of(&club.f, club.ruth).await;
    let response = club.add(friends, "jonah").await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert_eq!(
        response.location(),
        format!("/settings/lists/{friends}?done=added")
    );
    assert_eq!(
        club.add(friends, " @Maya ").await.status,
        StatusCode::SEE_OTHER
    );
    // Adding someone twice is the same as once.
    assert_eq!(
        club.add(friends, "jonah").await.status,
        StatusCode::SEE_OTHER
    );
    assert_eq!(club.members_of(friends).await, ["jonah", "maya"]);
    for (username, why) in [
        ("nobody", "There is no member called “nobody”."),
        (
            "ruth",
            "You don’t need to be on your own list: wherever it lets people in, it lets you in too.",
        ),
    ] {
        let response = club.add(friends, username).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "{username}");
        assert!(response.body.contains(why), "{}", response.body);
    }
    // A deleted account can't be added.
    let gone = club.f.member("gone", "user", None).await;
    parlour::users::delete_account(club.f.state.db(), gone)
        .await
        .unwrap();
    let response = club.add(friends, &format!("deleted_{gone}")).await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    let page = club
        .f
        .page_as(&format!("/settings/lists/{friends}"), &club.ruth_cookie)
        .await;
    assert!(page.contains("@jonah") && page.contains("@maya"), "{page}");
    let response = club
        .f
        .submit(
            &format!("/settings/lists/{friends}/members/{}/remove", club.jonah),
            &[],
            Some(&club.ruth_cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert_eq!(
        response.location(),
        format!("/settings/lists/{friends}?done=removed")
    );
    assert_eq!(club.members_of(friends).await, ["maya"]);
    // Someone not on it can't be taken off it.
    let response = club
        .f
        .submit(
            &format!("/settings/lists/{friends}/members/{}/remove", club.jonah),
            &[],
            Some(&club.ruth_cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::NOT_FOUND);
}

#[sqlx::test]
async fn nobody_is_told_they_were_added(pool: PgPool) {
    let club = club(pool).await;
    let friends = friends_of(&club.f, club.ruth).await;
    club.add(friends, "jonah").await;
    let notifications: i64 = sqlx::query_scalar("SELECT count(*) FROM notifications")
        .fetch_one(&club.f.db)
        .await
        .unwrap();
    assert_eq!(notifications, 0);
    assert!(club.f.emails().await.is_empty());
}

#[sqlx::test]
async fn people_on_a_list_see_its_name_its_owner_and_each_other(pool: PgPool) {
    let club = club(pool).await;
    let list = club.ruths_list("Book club").await;
    club.add(list, "jonah").await;
    club.add(list, "maya").await;
    let page = club.f.page_as("/settings/lists", &club.jonah_cookie).await;
    assert!(page.contains("Lists you are on"), "{page}");
    assert!(page.contains("Ruth’s Book club"), "{page}");
    assert!(
        page.contains(&format!("href=\"/settings/lists/{list}\"")),
        "{page}"
    );
    // Who else is on it.
    assert!(page.contains("@maya"), "{page}");
    let detail = club
        .f
        .page_as(&format!("/settings/lists/{list}"), &club.jonah_cookie)
        .await;
    assert!(detail.contains("Ruth’s Book club"));
    assert!(detail.contains("@maya") && detail.contains("@jonah"));
    // They can't change it.
    assert!(!detail.contains(&format!("action=\"/settings/lists/{list}")));
    for (path, fields) in [
        (format!("/settings/lists/{list}"), vec![("name", "Mine")]),
        (
            format!("/settings/lists/{list}/members"),
            vec![("username", "sam")],
        ),
        (
            format!("/settings/lists/{list}/members/{}/remove", club.maya),
            vec![],
        ),
        (format!("/settings/lists/{list}/delete"), vec![]),
    ] {
        let response = club
            .f
            .submit(&path, &fields, Some(&club.jonah_cookie))
            .await;
        assert_eq!(response.status, StatusCode::FORBIDDEN, "{path}");
    }
    assert_eq!(club.members_of(list).await, ["jonah", "maya"]);
}

#[sqlx::test]
async fn nobody_else_learns_a_list_exists(pool: PgPool) {
    let club = club(pool).await;
    let list = club.ruths_list("Secret garden").await;
    club.add(list, "jonah").await;
    // Not on it: Maya and the admin.
    for cookie in [&club.maya_cookie, &club.admin_cookie] {
        let page = club.f.page_as("/settings/lists", cookie).await;
        assert!(!page.contains("Secret garden"));
        let response = club
            .f
            .get_as(&format!("/settings/lists/{list}"), Some(cookie))
            .await;
        assert_eq!(response.status, StatusCode::NOT_FOUND);
        assert!(!response.body.contains("Secret garden"));
        for (path, fields) in [
            (format!("/settings/lists/{list}"), vec![("name", "Mine")]),
            (
                format!("/settings/lists/{list}/members"),
                vec![("username", "sam")],
            ),
            (
                format!("/settings/lists/{list}/members/{}/remove", club.jonah),
                vec![],
            ),
            (format!("/settings/lists/{list}/delete"), vec![]),
        ] {
            let response = club.f.submit(&path, &fields, Some(cookie)).await;
            assert_eq!(response.status, StatusCode::NOT_FOUND, "{path}");
        }
    }
    // Admins' pages about groups know nothing of lists.
    let groups = club.f.page_as("/admin/groups", &club.admin_cookie).await;
    assert!(!groups.contains("Secret garden") && !groups.contains("Friends"));
    for path in [
        format!("/admin/groups/{list}"),
        format!("/admin/groups/{list}/delete"),
    ] {
        assert_eq!(
            club.f.get_as(&path, Some(&club.admin_cookie)).await.status,
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }
    for (path, fields) in [
        (
            format!("/admin/groups/{list}/members"),
            vec![("username", "sam")],
        ),
        (
            format!("/admin/groups/{list}/members/{}/remove", club.jonah),
            vec![],
        ),
        (format!("/admin/groups/{list}/delete"), vec![]),
        (
            format!("/admin/groups/{list}"),
            vec![("name", "Mine"), ("slug", "mine")],
        ),
    ] {
        let response = club
            .f
            .submit(&path, &fields, Some(&club.admin_cookie))
            .await;
        assert_eq!(response.status, StatusCode::NOT_FOUND, "{path}");
    }
    assert_eq!(club.members_of(list).await, ["jonah"]);
    // Nor do invites, which add people to site groups only.
    let invites = club.f.page_as("/invites", &club.admin_cookie).await;
    assert!(!invites.contains(&format!("value=\"{list}\"")), "{invites}");
    let response = club
        .f
        .submit(
            "/invites",
            &[("uses", "5"), ("days", "7"), ("group", &list.to_string())],
            Some(&club.admin_cookie),
        )
        .await;
    assert_eq!(
        response.status,
        StatusCode::BAD_REQUEST,
        "{}",
        response.body
    );
}

/// A space only Ruth's Book club may read.
async fn book_space(club: &Club, list: i64) -> i64 {
    let space = club.f.space("books", "members", None, 0).await;
    club.f
        .set_grant(space, "read", Grant::listed([list], []))
        .await;
    space
}

#[sqlx::test]
async fn a_list_in_a_grant_lets_its_owner_and_members_in(pool: PgPool) {
    let club = club(pool).await;
    let list = club.ruths_list("Book club").await;
    club.add(list, "jonah").await;
    let space = book_space(&club, list).await;
    let thread = club
        .f
        .thread(space, club.jonah, "Next month", "Middlemarch, I think")
        .await;
    let path = format!("/t/{}/next-month", thread.thread_id);
    for cookie in [&club.ruth_cookie, &club.jonah_cookie] {
        assert!(club.f.page_as(&path, cookie).await.contains("Middlemarch"));
    }
    assert_eq!(
        club.f.get_as(&path, Some(&club.maya_cookie)).await.status,
        StatusCode::NOT_FOUND
    );
    // People on the list see its name, and how many are on it.
    for cookie in [&club.ruth_cookie, &club.jonah_cookie] {
        let page = club.f.page_as("/c/books", cookie).await;
        assert!(page.contains("Ruth’s Book club (1)"), "{page}");
    }
    // Anyone else who can read the space sees whose choice it is, not the
    // list: here, Maya because the grant names her too, and the admin.
    club.f
        .set_grant(space, "read", Grant::listed([list], [club.maya]))
        .await;
    for cookie in [&club.maya_cookie, &club.admin_cookie] {
        for path in ["/c/books", "/", "/spaces"] {
            let page = club.f.page_as(path, cookie).await;
            assert!(!page.contains("Book club"), "{path}: {page}");
            assert!(page.contains("People Ruth has chosen"), "{path}: {page}");
        }
    }
}

#[sqlx::test]
async fn removing_someone_ends_their_access_but_keeps_their_posts(pool: PgPool) {
    let club = club(pool).await;
    let list = club.ruths_list("Book club").await;
    club.add(list, "jonah").await;
    let space = book_space(&club, list).await;
    let thread = club
        .f
        .thread(space, club.ruth, "Next month", "Suggestions?")
        .await;
    let reply = club
        .f
        .post(thread.thread_id, club.jonah, "Middlemarch, I think")
        .await;
    let path = format!("/t/{}/next-month", thread.thread_id);
    club.f
        .submit(
            &format!("/settings/lists/{list}/members/{}/remove", club.jonah),
            &[],
            Some(&club.ruth_cookie),
        )
        .await;
    assert_eq!(
        club.f.get_as(&path, Some(&club.jonah_cookie)).await.status,
        StatusCode::NOT_FOUND
    );
    let page = club.f.page_as(&path, &club.ruth_cookie).await;
    let post = common::post_html(&page, reply);
    assert!(
        post.contains("Middlemarch") && post.contains("@jonah"),
        "{post}"
    );
}

#[sqlx::test]
async fn a_list_a_grant_names_cant_be_deleted_only_renamed_or_emptied(pool: PgPool) {
    let club = club(pool).await;
    let list = club.ruths_list("Book club").await;
    club.add(list, "jonah").await;
    book_space(&club, list).await;
    let page = club
        .f
        .page_as(&format!("/settings/lists/{list}"), &club.ruth_cookie)
        .await;
    assert!(
        !page.contains(&format!("action=\"/settings/lists/{list}/delete\"")),
        "{page}"
    );
    assert!(page.contains("Books"), "{page}");
    let response = club
        .f
        .submit(
            &format!("/settings/lists/{list}/delete"),
            &[],
            Some(&club.ruth_cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(
        response
            .body
            .contains("The list can’t be deleted while 1 space names it"),
        "{}",
        response.body
    );
    let response = club
        .f
        .submit(
            &format!("/settings/lists/{list}"),
            &[("name", "Readers")],
            Some(&club.ruth_cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    club.f
        .submit(
            &format!("/settings/lists/{list}/members/{}/remove", club.jonah),
            &[],
            Some(&club.ruth_cookie),
        )
        .await;
    assert!(club.members_of(list).await.is_empty());
}

#[sqlx::test]
async fn deleting_the_owner_freezes_the_list(pool: PgPool) {
    let club = club(pool).await;
    let list = club.ruths_list("Book club").await;
    club.add(list, "jonah").await;
    let space = book_space(&club, list).await;
    let thread = club
        .f
        .thread(space, club.jonah, "Next month", "Middlemarch, I think")
        .await;
    parlour::users::delete_account(club.f.state.db(), club.ruth)
        .await
        .unwrap();
    let frozen: bool = sqlx::query_scalar("SELECT frozen_at IS NOT NULL FROM groups WHERE id = $1")
        .bind(list)
        .fetch_one(&club.f.db)
        .await
        .unwrap();
    assert!(frozen);
    // The grant naming it still lets Jonah in.
    let path = format!("/t/{}/next-month", thread.thread_id);
    assert!(
        club.f
            .page_as(&path, &club.jonah_cookie)
            .await
            .contains("Middlemarch")
    );
    let page = club
        .f
        .page_as(&format!("/settings/lists/{list}"), &club.jonah_cookie)
        .await;
    assert!(page.contains("nobody can change who is on it"), "{page}");
    // Its membership can't change, even from below the application.
    for sql in [
        format!(
            "INSERT INTO group_members (group_id, user_id) VALUES ({list}, {})",
            club.maya
        ),
        format!(
            "DELETE FROM group_members WHERE group_id = {list} AND user_id = {}",
            club.jonah
        ),
    ] {
        assert!(
            sqlx::raw_sql(sqlx::AssertSqlSafe(sql.clone()))
                .execute(&club.f.db)
                .await
                .is_err(),
            "{sql}"
        );
    }
    // But someone on it who deletes their own account still leaves it.
    parlour::users::delete_account(club.f.state.db(), club.jonah)
        .await
        .unwrap();
    assert!(club.members_of(list).await.is_empty());
}

#[sqlx::test]
async fn deleting_a_spammer_freezes_their_lists(pool: PgPool) {
    let club = club(pool).await;
    let friends = friends_of(&club.f, club.maya).await;
    let sam = club.f.user_id("sam").await;
    parlour::mod_records::delete_spammer(&club.f.db, club.maya, sam)
        .await
        .unwrap()
        .unwrap();
    let frozen: bool = sqlx::query_scalar("SELECT frozen_at IS NOT NULL FROM groups WHERE id = $1")
        .bind(friends)
        .fetch_one(&club.f.db)
        .await
        .unwrap();
    assert!(frozen);
}

/// The settings form's checkboxes for groups and lists, as (value, label).
fn group_choices(page: &str, verb: &str) -> Vec<(String, String)> {
    let marker = format!("name=\"{verb}_group\" value=\"");
    page.split(&marker)
        .skip(1)
        .map(|rest| {
            let value = rest.split('"').next().unwrap().to_owned();
            let label = rest
                .split_once('>')
                .unwrap()
                .1
                .split("</label>")
                .next()
                .unwrap()
                .trim()
                .to_owned();
            (value, label)
        })
        .collect()
}

#[sqlx::test]
async fn grant_forms_offer_the_lists_the_editor_knows(pool: PgPool) {
    let club = club(pool).await;
    let organisers = club.f.group("organisers", "Organisers").await;
    let list = club.ruths_list("Book club").await;
    let ruths_friends = friends_of(&club.f, club.ruth).await;
    let jonahs_friends = friends_of(&club.f, club.jonah).await;
    club.add(list, "jonah").await;
    // Jonah makes a space, and can name the site's groups, his own lists
    // and the lists he is on, but nobody else's.
    let response = club
        .f
        .submit(
            "/spaces/new",
            &[
                ("name", "Reading"),
                ("slug", "reading"),
                ("description", ""),
            ],
            Some(&club.jonah_cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    let page = club
        .f
        .page_as("/c/reading/settings", &club.jonah_cookie)
        .await;
    let choices = group_choices(&page, "read");
    assert_eq!(
        choices,
        [
            (organisers.to_string(), "Organisers".to_owned()),
            (jonahs_friends.to_string(), "Your Friends".to_owned()),
            (list.to_string(), "Ruth’s Book club".to_owned()),
        ]
    );
    let save = |groups: Vec<String>| {
        let mut fields = vec![
            ("name".to_owned(), "Reading".to_owned()),
            ("slug".to_owned(), "reading".to_owned()),
            ("description".to_owned(), String::new()),
            ("read_level".to_owned(), "listed".to_owned()),
            ("read_users".to_owned(), String::new()),
            ("reply_level".to_owned(), "members".to_owned()),
            ("start_level".to_owned(), "members".to_owned()),
            ("administer_level".to_owned(), "listed".to_owned()),
            ("administer_users".to_owned(), "jonah".to_owned()),
        ];
        fields.extend(groups.into_iter().map(|g| ("read_group".to_owned(), g)));
        fields
    };
    let submit = |fields: Vec<(String, String)>, cookie: String| {
        let f = &club.f;
        async move {
            let fields: Vec<(&str, &str)> = fields
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            f.submit("/c/reading/settings", &fields, Some(&cookie))
                .await
        }
    };
    let response = submit(
        save(vec![ruths_friends.to_string()]),
        club.jonah_cookie.clone(),
    )
    .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(!response.body.contains("Ruth’s Friends"));
    let response = submit(
        save(vec![list.to_string(), jonahs_friends.to_string()]),
        club.jonah_cookie.clone(),
    )
    .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    // An admin, on neither list, sees that the grant names lists without
    // learning what they are, and saving keeps them.
    let page = club
        .f
        .page_as("/c/reading/settings", &club.admin_cookie)
        .await;
    assert!(
        !page.contains("Book club") && !page.contains("Jonah’s Friends"),
        "{page}"
    );
    let sams_friends = friends_of(&club.f, club.f.user_id("sam").await).await;
    let choices = group_choices(&page, "read");
    assert_eq!(
        choices,
        [
            (organisers.to_string(), "Organisers".to_owned()),
            (sams_friends.to_string(), "Your Friends".to_owned()),
            (
                jonahs_friends.to_string(),
                "People Jonah has chosen".to_owned()
            ),
            (list.to_string(), "People Ruth has chosen".to_owned()),
        ]
    );
    let response = submit(
        save(vec![list.to_string(), jonahs_friends.to_string()]),
        club.admin_cookie.clone(),
    )
    .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    let named: Vec<i64> = sqlx::query_scalar(
        "SELECT unnest(group_ids) FROM space_grants g JOIN spaces s ON s.id = g.space_id
         WHERE s.slug = 'reading' AND g.verb = 'read' ORDER BY 1",
    )
    .fetch_all(&club.f.db)
    .await
    .unwrap();
    let mut expected = vec![list, jonahs_friends];
    expected.sort();
    assert_eq!(named, expected);
}

#[sqlx::test]
async fn the_data_export_lists_the_members_lists(pool: PgPool) {
    let club = club(pool).await;
    let list = club.ruths_list("Book club").await;
    club.add(list, "jonah").await;
    let friends = friends_of(&club.f, club.jonah).await;
    club.f.join_group(friends, club.maya).await;
    let jonah = parlour::sessions::user_ctx(club.f.state.db(), club.jonah)
        .await
        .unwrap()
        .unwrap();
    let export =
        parlour::db::export::export(club.f.state.db(), &parlour::viewer::Viewer::User(jonah))
            .await
            .unwrap()
            .unwrap();
    assert_eq!(
        export["lists"],
        serde_json::json!([{ "name": "Friends", "members": ["maya"] }])
    );
    assert_eq!(
        export["lists_you_are_on"],
        serde_json::json!([{ "name": "Book club", "owner": "ruth" }])
    );
    assert_eq!(export["groups"], serde_json::json!([]));
}
