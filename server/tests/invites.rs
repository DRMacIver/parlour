//! Invites, registration, the invite tree and profiles (DESIGN.md §3.6).

mod common;

use axum::http::{StatusCode, header};
use common::{Forum, cookie_set_by, link_in};
use parlour::http_cache::{PRIVATE_CACHE_CONTROL, PUBLIC_CACHE_CONTROL};
use sqlx::PgPool;

/// The invite link shown after making an invite, as a path.
fn shown_link(page: &str) -> String {
    let marker = "class=\"link-box\" type=\"text\" readonly value=\"";
    let start = page.find(marker).expect("no invite link shown") + marker.len();
    let url = &page[start..start + page[start..].find('"').unwrap()];
    url.strip_prefix(common::ORIGIN).unwrap().to_owned()
}

async fn make_invite(forum: &Forum, cookie: &str, fields: &[(&str, &str)]) -> String {
    let response = forum.submit("/invites", fields, Some(cookie)).await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    shown_link(&response.body)
}

async fn join(forum: &Forum, link: &str, username: &str, email: &str) -> common::Response {
    forum
        .submit(
            link,
            &[
                ("username", username),
                ("display_name", &common::capitalise(username)),
                ("email", email),
                ("password", ""),
            ],
            None,
        )
        .await
}

struct Inviter {
    forum: Forum,
    id: i64,
    cookie: String,
}

async fn setup(db: PgPool, role: &str) -> Inviter {
    let forum = Forum::new(db);
    let id = forum.member("ruth", role, None).await;
    let cookie = forum.session(id).await;
    Inviter { forum, id, cookie }
}

#[sqlx::test]
async fn a_member_invites_someone_with_a_link(db: PgPool) {
    let Inviter {
        forum,
        id: ruth,
        cookie,
    } = setup(db, "user").await;
    let page = forum.page_as("/invites", &cookie).await;
    assert!(page.contains("Invite someone"));
    let link = make_invite(
        &forum,
        &cookie,
        &[
            ("email", ""),
            ("note", "Alex from quiz night"),
            ("days", "7"),
            ("uses", "1"),
        ],
    )
    .await;
    assert!(link.starts_with("/invite/"));
    // The code is shown once, and only its hash is stored.
    let page = forum.page_as("/invites", &cookie).await;
    assert!(!page.contains(&link));
    assert!(page.contains("Waiting"));
    assert!(page.contains("Alex from quiz night"));
    let code = link.trim_start_matches("/invite/");
    let stored: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM invites WHERE code_hash = sha256(convert_to($1, 'UTF8'))",
    )
    .bind(code)
    .fetch_one(&forum.db)
    .await
    .unwrap();
    assert_eq!(stored, 1);

    // The registration page is cacheable and sets no cookie.
    let invite_page = forum.get(&link).await;
    assert_eq!(invite_page.status, StatusCode::OK);
    assert_eq!(
        invite_page.header(header::CACHE_CONTROL),
        Some(PUBLIC_CACHE_CONTROL)
    );
    assert!(invite_page.header(header::SET_COOKIE).is_none());
    assert!(invite_page.body.contains("Ruth</a> has invited you"));

    let response = join(&forum, &link, "alex", "alex@example.com").await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    assert_eq!(response.location(), "/settings?done=welcome-verify");
    let alex_cookie = cookie_set_by(&response).unwrap();
    let settings = forum.page_as(response.location(), &alex_cookie).await;
    assert!(settings.contains("Welcome!"));
    assert!(settings.contains("Not confirmed"));

    let (invited_by, invite_id, role): (Option<i64>, Option<i64>, String) = sqlx::query_as(
        "SELECT invited_by_user_id, invite_id, role FROM users WHERE username = 'alex'",
    )
    .fetch_one(&forum.db)
    .await
    .unwrap();
    assert_eq!(invited_by, Some(ruth));
    assert!(invite_id.is_some());
    assert_eq!(role, "user");

    // The link is used up, and the cached page knows it.
    assert_eq!(forum.get(&link).await.status, StatusCode::GONE);
    assert_eq!(
        join(&forum, &link, "sneaky", "sneaky@example.com")
            .await
            .status,
        StatusCode::GONE
    );
    let page = forum.page_as("/invites", &cookie).await;
    assert!(page.contains("Used"));
    assert!(page.contains("href=\"/u/alex\""));

    // They get an email to confirm their address.
    let email = forum.last_email_to("alex@example.com").await;
    let verify = link_in(&email.body, "/verify/");
    forum.submit(&verify, &[], Some(&alex_cookie)).await;
    assert!(
        forum
            .page_as("/settings", &alex_cookie)
            .await
            .contains("Confirmed")
    );
}

#[sqlx::test]
async fn an_emailed_invite_is_for_that_address(db: PgPool) {
    let Inviter { forum, cookie, .. } = setup(db, "user").await;
    let response = forum
        .submit(
            "/invites",
            &[
                ("email", "alex@example.com"),
                ("note", ""),
                ("days", "7"),
                ("uses", "1"),
            ],
            Some(&cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::OK);
    assert!(
        response
            .body
            .contains("emailing an invitation to <strong>alex@example.com")
    );
    let email = forum.last_email_to("alex@example.com").await;
    assert!(email.subject.contains("Ruth has invited you"));
    let link = link_in(&email.body, "/invite/");
    assert_eq!(link, shown_link(&response.body));

    let page = forum.page(&link).await;
    assert!(page.contains("value=\"alex@example.com\" readonly"));
    // Whatever address is typed in, the account gets the invited one, and
    // it counts as confirmed.
    let response = join(&forum, &link, "alex", "someone-else@example.com").await;
    assert_eq!(response.location(), "/settings?done=welcome");
    let (email, verified): (String, bool) = sqlx::query_as(
        "SELECT email::text, email_verified_at IS NOT NULL FROM users WHERE username = 'alex'",
    )
    .fetch_one(&forum.db)
    .await
    .unwrap();
    assert_eq!(email, "alex@example.com");
    assert!(verified);
    // No confirmation email was needed.
    assert_eq!(forum.emails().await.len(), 1);
}

#[sqlx::test]
async fn registration_checks_what_was_typed(db: PgPool) {
    let Inviter { forum, cookie, .. } = setup(db, "user").await;
    forum.member("taken", "user", None).await;
    let link = make_invite(&forum, &cookie, &[("days", "7"), ("uses", "1")]).await;
    for (username, email, password, problem) in [
        ("a", "a@example.com", "", "2 to 24 characters"),
        (
            "has space",
            "x@example.com",
            "",
            "letters, digits and underscores",
        ),
        ("admin", "x@example.com", "", "reserved"),
        ("TAKEN", "x@example.com", "", "username is taken"),
        ("newbie", "TAKEN@example.com", "", "already an account"),
        ("newbie", "not-an-address", "", "valid email"),
        ("newbie", "x@example.com", "short", "at least 10"),
    ] {
        let response = forum
            .submit(
                &link,
                &[
                    ("username", username),
                    ("display_name", "New"),
                    ("email", email),
                    ("password", password),
                ],
                None,
            )
            .await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "{username}");
        assert!(
            response.body.contains(problem),
            "{problem}: {}",
            response.body
        );
        assert!(response.header(header::SET_COOKIE).is_none());
        // What was typed is kept.
        assert!(response.body.contains(&format!("value=\"{username}\"")));
    }
    // None of that used the invite up.
    let response = join(&forum, &link, "newbie", "newbie@example.com").await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
}

#[sqlx::test]
async fn a_password_chosen_when_joining_works(db: PgPool) {
    let Inviter { forum, cookie, .. } = setup(db, "user").await;
    let link = make_invite(&forum, &cookie, &[("days", "7"), ("uses", "1")]).await;
    forum
        .submit(
            &link,
            &[
                ("username", "alex"),
                ("display_name", "Alex"),
                ("email", "alex@example.com"),
                ("password", "alex's long password"),
            ],
            None,
        )
        .await;
    let response = forum
        .submit(
            "/login",
            &[("identifier", "alex"), ("password", "alex's long password")],
            None,
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
}

#[sqlx::test]
async fn invites_can_be_revoked(db: PgPool) {
    let Inviter { forum, cookie, .. } = setup(db, "user").await;
    let link = make_invite(&forum, &cookie, &[("days", "7"), ("uses", "1")]).await;
    // Cache the registration page, then revoke.
    assert_eq!(forum.get(&link).await.status, StatusCode::OK);
    let id: i64 = sqlx::query_scalar("SELECT id FROM invites")
        .fetch_one(&forum.db)
        .await
        .unwrap();
    // Someone else can't revoke it.
    let jonah = forum.member("jonah", "user", None).await;
    let jonah_cookie = forum.session(jonah).await;
    let response = forum
        .submit(&format!("/invites/{id}/revoke"), &[], Some(&jonah_cookie))
        .await;
    assert_eq!(response.status, StatusCode::NOT_FOUND);
    assert_eq!(forum.get(&link).await.status, StatusCode::OK);

    let response = forum
        .submit(&format!("/invites/{id}/revoke"), &[], Some(&cookie))
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    let page = forum.get(&link).await;
    assert_eq!(page.status, StatusCode::GONE);
    assert!(page.body.contains("withdrawn"));
    assert!(forum.page_as("/invites", &cookie).await.contains("Revoked"));
    assert_eq!(
        join(&forum, &link, "alex", "alex@example.com").await.status,
        StatusCode::GONE
    );
}

#[sqlx::test]
async fn staff_can_revoke_anyones_invite(db: PgPool) {
    let Inviter { forum, cookie, .. } = setup(db, "user").await;
    make_invite(&forum, &cookie, &[("days", "7"), ("uses", "1")]).await;
    let id: i64 = sqlx::query_scalar("SELECT id FROM invites")
        .fetch_one(&forum.db)
        .await
        .unwrap();
    let sam = forum.member("sam", "moderator", None).await;
    let sam_cookie = forum.session(sam).await;
    let response = forum
        .submit(&format!("/invites/{id}/revoke"), &[], Some(&sam_cookie))
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
}

#[sqlx::test]
async fn expired_invites_cant_be_used(db: PgPool) {
    let Inviter { forum, cookie, .. } = setup(db, "user").await;
    let link = make_invite(&forum, &cookie, &[("days", "1"), ("uses", "1")]).await;
    forum
        .exec("UPDATE invites SET expires_at = now() - interval '1 second'")
        .await;
    let page = forum.get(&link).await;
    assert_eq!(page.status, StatusCode::GONE);
    assert!(page.body.contains("expired"));
    assert_eq!(
        join(&forum, &link, "alex", "alex@example.com").await.status,
        StatusCode::GONE
    );
    assert!(forum.page_as("/invites", &cookie).await.contains("Expired"));
}

#[sqlx::test]
async fn unknown_invite_codes_are_not_found(db: PgPool) {
    let forum = Forum::new(db);
    for path in [
        "/invite/nonsense",
        "/invite/AAAAAAAAAAAAAAAAAAAAAA",
        "/invite/..%2F",
    ] {
        assert_eq!(
            forum.get(path).await.status,
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }
    assert_eq!(
        join(
            &forum,
            "/invite/AAAAAAAAAAAAAAAAAAAAAA",
            "alex",
            "alex@example.com"
        )
        .await
        .status,
        StatusCode::NOT_FOUND
    );
}

#[sqlx::test]
async fn members_have_limits_staff_dont(db: PgPool) {
    let Inviter { forum, cookie, .. } = setup(db, "user").await;
    for (fields, problem) in [
        (
            vec![("days", "7"), ("uses", "5")],
            "Each invite can be used once",
        ),
        (vec![("days", "90"), ("uses", "1")], "how long"),
        (
            vec![("days", "7"), ("uses", "1"), ("group", "3")],
            "Only staff",
        ),
        (
            vec![("days", "7"), ("uses", "1"), ("email", "bad")],
            "valid email",
        ),
        (
            vec![("days", "7"), ("uses", "1"), ("email", "ruth@example.com")],
            "already a member",
        ),
    ] {
        let response = forum.submit("/invites", &fields, Some(&cookie)).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "{fields:?}");
        assert!(
            response.body.contains(problem),
            "{problem}: {}",
            response.body
        );
    }
    for _ in 0..10 {
        make_invite(&forum, &cookie, &[("days", "7"), ("uses", "1")]).await;
    }
    let response = forum
        .submit("/invites", &[("days", "7"), ("uses", "1")], Some(&cookie))
        .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(response.body.contains("10 invites waiting"));
}

#[sqlx::test]
async fn staff_links_can_be_used_several_times_and_record_groups(db: PgPool) {
    let Inviter {
        forum,
        id: sam,
        cookie,
    } = setup(db, "admin").await;
    let page = forum.page_as("/invites", &cookie).await;
    assert!(page.contains("Number of uses"));
    let link = make_invite(
        &forum,
        &cookie,
        &[
            ("days", "90"),
            ("uses", "3"),
            ("group", "7"),
            ("group", "9"),
        ],
    )
    .await;
    let groups: Vec<i64> = sqlx::query_scalar("SELECT unnest(grant_group_ids) FROM invites")
        .fetch_all(&forum.db)
        .await
        .unwrap();
    assert_eq!(groups, vec![7, 9]);
    for name in ["alex", "bea", "cal"] {
        let response = join(&forum, &link, name, &format!("{name}@example.com")).await;
        assert_eq!(response.status, StatusCode::SEE_OTHER, "{name}");
    }
    assert_eq!(
        join(&forum, &link, "dee", "dee@example.com").await.status,
        StatusCode::GONE
    );
    let inviters: Vec<Option<i64>> = sqlx::query_scalar(
        "SELECT invited_by_user_id FROM users WHERE username <> 'ruth' ORDER BY id",
    )
    .fetch_all(&forum.db)
    .await
    .unwrap();
    assert_eq!(inviters, vec![Some(sam); 3]);
    assert!(
        forum
            .page_as("/invites", &cookie)
            .await
            .contains("used 3 of 3")
    );
    // An emailed invite is always single-use.
    let response = forum
        .submit(
            "/invites",
            &[("days", "7"), ("uses", "2"), ("email", "x@example.com")],
            Some(&cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
}

#[sqlx::test]
async fn who_can_invite_is_a_site_setting(db: PgPool) {
    let Inviter { forum, cookie, .. } = setup(db, "user").await;
    forum
        .exec(r#"INSERT INTO settings (key, value) VALUES ('invite_policy', '{"who": "staff"}')"#)
        .await;
    let page = forum.page_as("/invites", &cookie).await;
    assert!(page.contains("only moderators and admins can invite"));
    assert!(!page.contains("Create invite"));
    let response = forum
        .submit("/invites", &[("days", "7"), ("uses", "1")], Some(&cookie))
        .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);

    forum
        .exec(r#"UPDATE settings SET value = '{"who": "members_older_than", "days": 30}' WHERE key = 'invite_policy'"#)
        .await;
    assert!(forum.page_as("/invites", &cookie).await.contains("30 days"));
    forum
        .exec("UPDATE users SET created_at = now() - interval '31 days'")
        .await;
    // The viewer's account age is read with the session.
    make_invite(&forum, &cookie, &[("days", "7"), ("uses", "1")]).await;
}

#[sqlx::test]
async fn unconfirmed_members_cant_invite(db: PgPool) {
    let Inviter { forum, cookie, .. } = setup(db, "user").await;
    forum
        .exec("UPDATE users SET email_verified_at = NULL")
        .await;
    let page = forum.page_as("/invites", &cookie).await;
    assert!(page.contains("Confirm your email address"));
}

#[sqlx::test]
async fn closed_registration_stops_invites_being_used(db: PgPool) {
    let Inviter { forum, cookie, .. } = setup(db, "user").await;
    let link = make_invite(&forum, &cookie, &[("days", "7"), ("uses", "1")]).await;
    forum.page(&link).await;
    forum
        .exec(r#"INSERT INTO settings (key, value) VALUES ('registration_mode', '"closed"')"#)
        .await;
    forum.changed(parlour::changes::Change::SiteChanged).await;
    let page = forum.get(&link).await;
    assert_eq!(page.status, StatusCode::FORBIDDEN);
    assert!(page.body.contains("Registration is closed"));
    assert_eq!(
        join(&forum, &link, "alex", "alex@example.com").await.status,
        StatusCode::FORBIDDEN
    );
    assert!(
        forum
            .page_as("/invites", &cookie)
            .await
            .contains("Registration is closed")
    );
}

#[sqlx::test]
async fn logged_in_members_cant_register_again(db: PgPool) {
    let Inviter { forum, cookie, .. } = setup(db, "user").await;
    let link = make_invite(&forum, &cookie, &[("days", "7"), ("uses", "1")]).await;
    let page = forum.page_as(&link, &cookie).await;
    assert!(page.contains("already a member"));
}

#[sqlx::test]
async fn redemption_is_rate_limited(db: PgPool) {
    let forum = Forum::new(db);
    let mut statuses = Vec::new();
    for _ in 0..11 {
        statuses.push(
            join(
                &forum,
                "/invite/AAAAAAAAAAAAAAAAAAAAAA",
                "alex",
                "a@example.com",
            )
            .await
            .status,
        );
    }
    assert_eq!(statuses[9], StatusCode::NOT_FOUND);
    assert_eq!(statuses[10], StatusCode::TOO_MANY_REQUESTS);
}

#[sqlx::test]
async fn who_invited_whom_cant_be_changed(db: PgPool) {
    let Inviter { forum, cookie, .. } = setup(db, "user").await;
    let link = make_invite(&forum, &cookie, &[("days", "7"), ("uses", "1")]).await;
    join(&forum, &link, "alex", "alex@example.com").await;
    let jonah = forum.user("jonah").await;
    for sql in [
        format!("UPDATE users SET invited_by_user_id = {jonah} WHERE username = 'alex'"),
        "UPDATE users SET invited_by_user_id = NULL WHERE username = 'alex'".to_owned(),
        "UPDATE users SET invite_id = NULL WHERE username = 'alex'".to_owned(),
        format!("UPDATE users SET invited_by_user_id = {jonah} WHERE username = 'ruth'"),
    ] {
        let result = sqlx::raw_sql(sqlx::AssertSqlSafe(sql.clone()))
            .execute(&forum.db)
            .await;
        assert!(result.is_err(), "{sql}");
    }
    // Other changes are fine.
    forum
        .exec("UPDATE users SET display_name = 'Alex B' WHERE username = 'alex'")
        .await;
}

// ---------------------------------------------------------------------------
// The tree and profiles

#[sqlx::test]
async fn the_invite_tree_shows_who_invited_whom(db: PgPool) {
    let Inviter { forum, cookie, .. } = setup(db, "admin").await;
    // Cache the tree first: joining must clear it.
    let before = forum.page("/users/tree").await;
    assert!(before.contains("1 member."));
    let link = make_invite(&forum, &cookie, &[("days", "7"), ("uses", "2")]).await;
    join(&forum, &link, "alex", "alex@example.com").await;
    let alex = cookie_set_by(&join(&forum, &link, "bea", "bea@example.com").await).unwrap();
    let response = forum.get("/users/tree").await;
    assert_eq!(
        response.header(header::CACHE_CONTROL),
        Some(PUBLIC_CACHE_CONTROL)
    );
    let tree = response.body;
    assert!(tree.contains("3 members."));
    assert!(tree.contains("2 people below"));
    let ruth = tree.find("href=\"/u/ruth\"").unwrap();
    let alex_pos = tree.find("href=\"/u/alex\"").unwrap();
    let bea_pos = tree.find("href=\"/u/bea\"").unwrap();
    assert!(ruth < alex_pos && alex_pos < bea_pos);
    // Nested inside ruth's entry, which works without JavaScript.
    assert!(tree[ruth.saturating_sub(400)..ruth].contains("<details open>"));
    assert!(tree.contains("Admin"));
    assert!(!tree.contains("badge-you"));
    // Logged in, you are marked, and the page isn't cached.
    let mine = forum.get_as("/users/tree", Some(&alex)).await;
    assert_eq!(
        mine.header(header::CACHE_CONTROL),
        Some(PRIVATE_CACHE_CONTROL)
    );
    assert!(mine.body.contains("badge-you"));
}

#[sqlx::test]
async fn profiles_show_who_invited_them_and_whom_they_invited(db: PgPool) {
    let Inviter { forum, cookie, .. } = setup(db, "user").await;
    let ruth_profile = forum.page("/u/ruth").await;
    assert!(ruth_profile.contains("A founding member"));
    assert!(ruth_profile.contains("hasn’t invited anyone yet"));
    let link = make_invite(&forum, &cookie, &[("days", "7"), ("uses", "1")]).await;
    join(&forum, &link, "alex", "alex@example.com").await;

    let ruth_profile = forum.page("/u/ruth").await;
    assert!(ruth_profile.contains("Invited 1 person"));
    assert!(ruth_profile.contains("href=\"/u/alex\""));
    let alex_profile = forum.page("/u/alex").await;
    assert!(alex_profile.contains("Invited by"));
    assert!(alex_profile.contains("href=\"/u/ruth\">Ruth</a>"));
}

#[sqlx::test]
async fn profile_urls_are_canonical(db: PgPool) {
    let forum = Forum::new(db);
    forum.member("ruth", "user", None).await;
    let response = forum.get("/u/RUTH").await;
    assert_eq!(response.status, StatusCode::PERMANENT_REDIRECT);
    assert_eq!(response.location(), "/u/ruth");
    assert_eq!(forum.get("/u/ruth?x=1").await.location(), "/u/ruth");
    for path in ["/u/nobody", "/u/r", "/u/no%20body", "/u/%E2%98%83"] {
        assert_eq!(
            forum.get(path).await.status,
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }
}

#[sqlx::test]
async fn profile_post_counts_depend_on_who_is_looking(db: PgPool) {
    let forum = Forum::new(db);
    let ruth = forum.member("ruth", "user", None).await;
    let public = forum.category("lobby", "public", None, 0).await;
    let members = forum.category("back-room", "members", None, 1).await;
    forum.thread(public, ruth, "Hello", "Public post").await;
    forum.thread(members, ruth, "Psst", "Members post").await;
    let cookie = forum.session(ruth).await;
    let anonymous = forum.page("/u/ruth").await;
    assert!(anonymous.contains("<dt>Posts</dt><dd>1</dd>"));
    let member = forum.page_as("/u/ruth", &cookie).await;
    assert!(member.contains("<dt>Posts</dt><dd>2</dd>"));
}

// ---------------------------------------------------------------------------
// A model of invites, checked against random histories

mod model {
    use super::common::Forum;
    use hegel::TestCase;
    use hegel::generators as gs;
    use parlour::invites::{self, InviteStatus, NewInvite, RedeemError, Registration};
    use parlour::viewer::UserCtx;
    use sqlx::PgPool;
    use tokio::runtime::Handle;

    #[derive(Clone, Debug)]
    struct Invite {
        id: i64,
        code: String,
        creator: usize,
        email: Option<String>,
        max_uses: i32,
        uses: i32,
        revoked: bool,
        expired: bool,
    }

    impl Invite {
        fn status(&self) -> InviteStatus {
            if self.revoked {
                InviteStatus::Revoked
            } else if self.uses >= self.max_uses {
                InviteStatus::Used
            } else if self.expired {
                InviteStatus::Expired
            } else {
                InviteStatus::Valid
            }
        }
    }

    pub struct World {
        rt: Handle,
        forum: Forum,
        /// An admin and two ordinary members, who make the invites.
        creators: Vec<UserCtx>,
        invites: Vec<Invite>,
        /// (username, email) of everyone, lowercased.
        taken: Vec<(String, String)>,
        /// Who joined, with the invite they used.
        joined: Vec<(i64, usize)>,
        closed: bool,
    }

    const USERNAMES: &[&str] = &["alex", "Alex", "bea", "cal", "dee", "ruth"];
    const EMAILS: &[&str] = &[
        "a@x.example",
        "A@X.example",
        "b@x.example",
        "c@x.example",
        "ruth@example.com",
    ];

    impl World {
        pub fn new(rt: Handle, db: PgPool) -> World {
            let forum = Forum::new(db);
            let creators = rt.block_on(async {
                sqlx::raw_sql("TRUNCATE settings, users RESTART IDENTITY CASCADE")
                    .execute(&forum.db)
                    .await
                    .unwrap();
                let mut creators = Vec::new();
                for (name, role) in [("sam", "admin"), ("ruth", "user"), ("jonah", "user")] {
                    let id = forum.member(name, role, None).await;
                    let cookie = forum.session(id).await;
                    let token = cookie.trim_start_matches("parlour_session=");
                    creators.push(
                        parlour::sessions::lookup(&forum.db, token)
                            .await
                            .unwrap()
                            .unwrap(),
                    );
                }
                creators
            });
            let taken = creators
                .iter()
                .map(|c| (c.username.to_lowercase(), c.email.to_lowercase()))
                .collect();
            World {
                rt,
                forum,
                creators,
                invites: Vec::new(),
                taken,
                joined: Vec::new(),
                closed: false,
            }
        }

        fn pending(&self, creator: usize) -> usize {
            self.invites
                .iter()
                .filter(|i| i.creator == creator && i.status() == InviteStatus::Valid)
                .count()
        }

        fn email_taken(&self, email: &str) -> bool {
            self.taken.iter().any(|(_, e)| e == &email.to_lowercase())
        }
    }

    #[hegel::state_machine]
    impl World {
        #[rule]
        fn create_invite(&mut self, tc: TestCase) {
            let creator = tc.draw(gs::integers::<usize>().max_value(2));
            let staff = creator == 0;
            let email = if tc.draw(gs::booleans()) {
                Some(tc.draw(gs::sampled_from(EMAILS.to_vec())).to_owned())
            } else {
                None
            };
            let max_uses = if staff && email.is_none() {
                tc.draw(gs::integers::<i32>().min_value(1).max_value(3))
            } else {
                1
            };
            let new = NewInvite {
                email: email.clone(),
                note: String::new(),
                expires_in_days: 7,
                max_uses,
                grant_group_ids: Vec::new(),
            };
            let result = self
                .rt
                .block_on(invites::create(
                    &self.forum.state,
                    &self.creators[creator],
                    &new,
                ))
                .unwrap();
            let expected_ok = (staff || !self.closed)
                && !email.as_deref().is_some_and(|e| self.email_taken(e))
                && (staff || self.pending(creator) < invites::MAX_PENDING as usize);
            assert_eq!(result.is_ok(), expected_ok, "{result:?}");
            if let Ok(created) = result {
                self.invites.push(Invite {
                    id: created.id,
                    code: created.code,
                    creator,
                    email,
                    max_uses,
                    uses: 0,
                    revoked: false,
                    expired: false,
                });
            }
        }

        #[rule]
        fn redeem(&mut self, tc: TestCase) {
            tc.assume(!self.invites.is_empty());
            let index = tc.draw(gs::integers::<usize>().max_value(self.invites.len() - 1));
            let username = tc.draw(gs::sampled_from(USERNAMES.to_vec())).to_owned();
            let typed_email = tc.draw(gs::sampled_from(EMAILS.to_vec())).to_owned();
            let invite = self.invites[index].clone();
            let registration = Registration {
                username: username.clone(),
                display_name: "Newcomer".into(),
                email: typed_email.clone(),
                password: None,
            };
            let result = self
                .rt
                .block_on(invites::redeem(
                    &self.forum.state,
                    &invite.code,
                    &registration,
                ))
                .unwrap();
            let email = invite.email.clone().unwrap_or(typed_email);
            let username_taken = self
                .taken
                .iter()
                .any(|(u, _)| u == &username.to_lowercase());
            match result {
                Err(RedeemError::Closed) => assert!(self.closed),
                Err(RedeemError::Unusable(status)) => {
                    assert!(!self.closed);
                    assert_ne!(invite.status(), InviteStatus::Valid);
                    assert_eq!(status, invite.status());
                }
                Err(RedeemError::Invalid(problems)) => {
                    assert!(!self.closed && invite.status() == InviteStatus::Valid);
                    assert!(username_taken || self.email_taken(&email), "{problems:?}");
                }
                Err(RedeemError::NotFound) => panic!("invite {} not found", invite.id),
                Ok(joined) => {
                    assert!(!self.closed && invite.status() == InviteStatus::Valid);
                    assert!(!username_taken && !self.email_taken(&email));
                    assert_eq!(joined.invite_id, invite.id);
                    assert_eq!(joined.email_verified, invite.email.is_some());
                    self.invites[index].uses += 1;
                    self.taken
                        .push((username.to_lowercase(), email.to_lowercase()));
                    self.joined.push((joined.user_id, index));
                }
            }
        }

        #[rule]
        fn revoke(&mut self, tc: TestCase) {
            tc.assume(!self.invites.is_empty());
            let index = tc.draw(gs::integers::<usize>().max_value(self.invites.len() - 1));
            let actor = tc.draw(gs::integers::<usize>().max_value(2));
            let revoked = self
                .rt
                .block_on(invites::revoke(
                    &self.forum.db,
                    &self.creators[actor],
                    self.invites[index].id,
                ))
                .unwrap();
            let allowed = actor == 0 || actor == self.invites[index].creator;
            assert_eq!(revoked, allowed);
            if allowed {
                self.invites[index].revoked = true;
            }
        }

        #[rule]
        fn expire(&mut self, tc: TestCase) {
            tc.assume(!self.invites.is_empty());
            let index = tc.draw(gs::integers::<usize>().max_value(self.invites.len() - 1));
            self.rt.block_on(self.forum.exec(&format!(
                "UPDATE invites SET expires_at = now() - interval '1 second' WHERE id = {}",
                self.invites[index].id
            )));
            self.invites[index].expired = true;
        }

        #[rule]
        fn toggle_registration(&mut self, _: TestCase) {
            self.closed = !self.closed;
            let mode = if self.closed { "closed" } else { "invite_only" };
            self.rt.block_on(self.forum.exec(&format!(
                "INSERT INTO settings (key, value) VALUES ('registration_mode', '\"{mode}\"')
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value"
            )));
        }

        /// Uses never exceed the limit and always match who joined, and
        /// everyone who joined is recorded as invited by the invite's maker.
        #[invariant]
        fn records_agree(&self, _: TestCase) {
            let rows: Vec<(i64, i32, i32, i64)> = self.rt.block_on(
                sqlx::query_as(
                    "SELECT i.id, i.uses, i.max_uses, (SELECT count(*) FROM users u WHERE u.invite_id = i.id)
                     FROM invites i ORDER BY i.id",
                )
                .fetch_all(&self.forum.db),
            )
            .unwrap();
            assert_eq!(rows.len(), self.invites.len());
            for ((id, uses, max_uses, members), model) in rows.iter().zip(&self.invites) {
                assert_eq!(*id, model.id);
                assert_eq!(*uses, model.uses);
                assert_eq!(i64::from(*uses), *members);
                assert!(uses <= max_uses);
            }
            for (user_id, index) in &self.joined {
                let (invited_by, invite_id): (Option<i64>, Option<i64>) = self
                    .rt
                    .block_on(
                        sqlx::query_as(
                            "SELECT invited_by_user_id, invite_id FROM users WHERE id = $1",
                        )
                        .bind(user_id)
                        .fetch_one(&self.forum.db),
                    )
                    .unwrap();
                let invite = &self.invites[*index];
                assert_eq!(invited_by, Some(self.creators[invite.creator].id));
                assert_eq!(invite_id, Some(invite.id));
            }
        }
    }

    #[hegel::standalone_function(test_cases = 100)]
    pub fn run(tc: TestCase, rt: &Handle, db: &PgPool) {
        let world = World::new(rt.clone(), db.clone());
        hegel::stateful::machine(world).steps(30).run(tc);
    }
}

/// Random histories of making, revoking, expiring and using invites, with
/// registration opening and closing, agree with a simple model.
#[sqlx::test]
async fn invites_behave_like_the_model(db: PgPool) {
    let rt = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || model::run(&rt, &db))
        .await
        .unwrap();
}
