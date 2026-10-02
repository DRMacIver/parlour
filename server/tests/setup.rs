//! First-run setup (DESIGN.md §10) and the admin settings page.

mod common;

use axum::http::{StatusCode, header};
use common::smtp::FakeSmtp;
use common::{Forum, cookie_set_by, hidden_field};
use parlour::setup;
use sqlx::PgPool;

fn token_of(link: &str) -> String {
    link.split("token=").nth(1).unwrap().to_owned()
}

const ADMIN: &[(&str, &str)] = &[
    ("site_name", "The Tuesday Club"),
    ("username", "sam"),
    ("display_name", "Sam"),
    ("email", "sam@example.com"),
    ("password", "a strong admin password"),
    ("confirm", "a strong admin password"),
];

fn with_token<'a>(token: &'a str, fields: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
    let mut all = vec![("token", token)];
    all.extend_from_slice(fields);
    all
}

#[sqlx::test]
async fn setup_creates_the_admin_with_the_printed_token(db: PgPool) {
    let forum = Forum::new(db);
    let link = setup::prepare(&forum.state)
        .await
        .unwrap()
        .expect("setup is needed");
    assert!(link.starts_with("http://localhost:8080/setup?token="));
    let token = token_of(&link);

    // The wrong token gets nowhere.
    for path in [
        "/setup",
        "/setup?token=wrong",
        &format!("/setup?token={token}x"),
    ] {
        let response = forum.get(path).await;
        assert_eq!(response.status, StatusCode::FORBIDDEN, "{path}");
    }
    let wrong = forum
        .submit("/setup", &with_token("wrong", ADMIN), None)
        .await;
    assert_eq!(wrong.status, StatusCode::FORBIDDEN);

    let page = forum.get(&format!("/setup?token={token}")).await;
    assert_eq!(page.status, StatusCode::OK);
    assert_eq!(page.header(header::REFERRER_POLICY), Some("same-origin"));
    assert!(page.header(header::SET_COOKIE).is_none());
    assert_eq!(hidden_field(&page.body, "token"), token);

    assert!(forum.page("/").await.contains("<title>Parlour</title>"));
    let response = forum
        .submit("/setup", &with_token(&token, ADMIN), None)
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    assert_eq!(response.location(), "/setup/email");
    let cookie = cookie_set_by(&response).unwrap();
    let (role, verified): (String, bool) = sqlx::query_as(
        "SELECT role, email_verified_at IS NOT NULL FROM users WHERE username = 'sam'",
    )
    .fetch_one(&forum.db)
    .await
    .unwrap();
    assert_eq!(role, "admin");
    assert!(verified);
    // The site has its name, everywhere (the index was cached before).
    assert!(forum.page("/").await.contains("The Tuesday Club"));
    assert!(
        forum
            .page_as("/setup/email", &cookie)
            .await
            .contains("Set up email")
    );

    // Setup is over: the token is spent and the pages say so.
    assert_eq!(
        forum.get(&format!("/setup?token={token}")).await.status,
        StatusCode::NOT_FOUND
    );
    let again = forum
        .submit("/setup", &with_token(&token, ADMIN), None)
        .await;
    assert_eq!(again.status, StatusCode::NOT_FOUND);
    assert!(setup::prepare(&forum.state).await.unwrap().is_none());
}

#[sqlx::test]
async fn setup_checks_the_form(db: PgPool) {
    let forum = Forum::new(db);
    let token = token_of(&setup::prepare(&forum.state).await.unwrap().unwrap());
    let mut fields = with_token(&token, ADMIN);
    fields.retain(|(k, _)| *k != "confirm");
    fields.push(("confirm", "something else entirely"));
    let response = forum.submit("/setup", &fields, None).await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(response.body.contains("don’t match"));
    assert!(response.body.contains("value=\"sam@example.com\""));
    // The token still works after a mistake.
    let response = forum
        .submit("/setup", &with_token(&token, ADMIN), None)
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
}

#[sqlx::test]
async fn a_new_token_replaces_the_old_one(db: PgPool) {
    let forum = Forum::new(db);
    let first = token_of(&setup::prepare(&forum.state).await.unwrap().unwrap());
    let second = token_of(&setup::prepare(&forum.state).await.unwrap().unwrap());
    assert_ne!(first, second);
    assert_eq!(
        forum.get(&format!("/setup?token={first}")).await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        forum.get(&format!("/setup?token={second}")).await.status,
        StatusCode::OK
    );
}

#[sqlx::test]
async fn sites_with_an_admin_need_no_setup(db: PgPool) {
    let forum = Forum::new(db);
    forum.member("sam", "admin", None).await;
    assert!(setup::prepare(&forum.state).await.unwrap().is_none());
    assert_eq!(
        forum.get("/setup?token=anything").await.status,
        StatusCode::NOT_FOUND
    );
}

#[sqlx::test]
async fn setup_tests_and_saves_email_settings(db: PgPool) {
    let server = FakeSmtp::start().await;
    let mut forum = Forum::new(db);
    // Really send, through the fake server.
    forum.state = forum.state.clone().with_mailer(parlour::mail::Mailer::Smtp);
    let sam = forum.member("sam", "admin", None).await;
    let cookie = forum.session(sam).await;
    let port = server.port.to_string();
    let smtp = [
        ("smtp_host", "127.0.0.1"),
        ("smtp_port", port.as_str()),
        ("smtp_security", "none"),
        ("smtp_username", ""),
        ("smtp_password", ""),
        ("smtp_from", "Club <club@example.com>"),
    ];
    let mut test = smtp.to_vec();
    test.push(("action", "test"));
    let response = forum.submit("/setup/email", &test, Some(&cookie)).await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    assert!(
        response
            .body
            .contains("A test email is on its way to sam@example.com")
    );
    assert_eq!(server.messages().len(), 1);
    assert!(server.messages()[0].contains("Test email from"));
    // Testing didn't save anything.
    assert!(
        parlour::mail::saved_smtp_settings(forum.state.db())
            .await
            .unwrap()
            .is_none()
    );

    let mut save = smtp.to_vec();
    save.push(("action", "save"));
    let response = forum.submit("/setup/email", &save, Some(&cookie)).await;
    assert_eq!(response.location(), "/setup/invites");
    let saved = parlour::mail::saved_smtp_settings(forum.state.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved.port, server.port);

    // A broken server is reported, not saved.
    let mut broken = smtp.to_vec();
    broken.retain(|(k, _)| *k != "smtp_port");
    broken.push(("smtp_port", "1"));
    broken.push(("action", "test"));
    let response = forum.submit("/setup/email", &broken, Some(&cookie)).await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(response.body.contains("couldn’t be sent"));
}

#[sqlx::test]
async fn setup_makes_the_first_invites(db: PgPool) {
    let forum = Forum::new(db);
    let sam = forum.member("sam", "admin", None).await;
    let cookie = forum.session(sam).await;
    let response = forum
        .submit(
            "/setup/invites",
            &[
                (
                    "emails",
                    "ruth@example.com\njonah@example.com, maya@example.com",
                ),
                ("uses", "25"),
            ],
            Some(&cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    // Only the shared link is shown: emailed invitations go only to their
    // addresses.
    assert_eq!(response.body.matches("class=\"link-box\"").count(), 1);
    assert_eq!(response.body.matches("/invite/").count(), 1);
    assert!(
        response
            .body
            .contains("Emailing an invitation to maya@example.com")
    );
    let emails = forum.emails().await;
    for email in &emails {
        let link = common::link_in(&email.body, "/invite/");
        assert!(!response.body.contains(&link));
    }
    let mut to: Vec<&str> = emails.iter().map(|e| e.to.as_str()).collect();
    to.sort();
    assert_eq!(
        to,
        ["jonah@example.com", "maya@example.com", "ruth@example.com"]
    );
    let link_uses: i32 = sqlx::query_scalar("SELECT max_uses FROM invites WHERE email IS NULL")
        .fetch_one(&forum.db)
        .await
        .unwrap();
    assert_eq!(link_uses, 25);
}

#[sqlx::test]
async fn setup_steps_are_for_admins(db: PgPool) {
    let forum = Forum::new(db);
    let ruth = forum.member("ruth", "moderator", None).await;
    let cookie = forum.session(ruth).await;
    for path in ["/setup/email", "/setup/invites"] {
        // Logging in might help a visitor who isn't logged in…
        let response = forum.get(path).await;
        assert_eq!(response.status, StatusCode::SEE_OTHER);
        assert!(response.location().starts_with("/login?next="));
        // …but not someone who is logged in and isn't an admin, who is
        // told so rather than sent to a login page that can't help.
        let response = forum.get_as(path, Some(&cookie)).await;
        assert_eq!(response.status, StatusCode::FORBIDDEN, "{path}");
        assert!(response.body.contains("permission"));
        assert_eq!(
            forum.submit(path, &[], Some(&cookie)).await.status,
            StatusCode::FORBIDDEN,
            "{path}"
        );
    }
}

// ---------------------------------------------------------------------------
// Admin settings

#[sqlx::test]
async fn only_admins_see_site_settings(db: PgPool) {
    let forum = Forum::new(db);
    let ruth = forum.member("ruth", "moderator", None).await;
    let cookie = forum.session(ruth).await;
    assert_eq!(
        forum.get_as("/admin/settings", Some(&cookie)).await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        forum
            .submit(
                "/admin/settings",
                &[("site_name", "Mine now")],
                Some(&cookie)
            )
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert!(
        !forum
            .page_as("/", &cookie)
            .await
            .contains("href=\"/admin/settings\"")
    );
}

#[sqlx::test]
async fn admins_change_site_settings(db: PgPool) {
    let forum = Forum::new(db);
    let sam = forum.member("sam", "admin", None).await;
    let cookie = forum.session(sam).await;
    assert!(
        forum
            .page_as("/", &cookie)
            .await
            .contains("href=\"/admin/settings\"")
    );
    assert!(forum.page("/").await.contains("<title>Parlour</title>"));
    let page = forum.page_as("/admin/settings", &cookie).await;
    assert!(page.contains("value=\"invite_only\" checked"));
    assert!(page.contains("value=\"everyone\" checked"));

    let response = forum
        .submit(
            "/admin/settings",
            &[
                ("site_name", "  The   Tuesday Club "),
                ("registration_mode", "closed"),
                ("invite_who", "members_older_than"),
                ("invite_days", "14"),
                ("smtp_host", ""),
            ],
            Some(&cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    assert!(
        forum
            .page("/")
            .await
            .contains("<title>The Tuesday Club</title>")
    );
    let page = forum.page_as("/admin/settings?done=saved", &cookie).await;
    assert!(page.contains("Settings saved."));
    assert!(page.contains("value=\"closed\" checked"));
    assert!(page.contains("value=\"members_older_than\" checked"));
    assert!(page.contains("value=\"14\""));

    let bad = forum
        .submit(
            "/admin/settings",
            &[
                ("site_name", ""),
                ("registration_mode", "open"),
                ("invite_who", "everyone"),
            ],
            Some(&cookie),
        )
        .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    assert!(bad.body.contains("between 1 and 80"));
    assert!(bad.body.contains("Choose who can join"));
}

#[sqlx::test]
async fn admins_send_a_test_email(db: PgPool) {
    let forum = Forum::new(db);
    let sam = forum.member("sam", "admin", None).await;
    let cookie = forum.session(sam).await;
    let response = forum
        .submit("/admin/settings/test-email", &[], Some(&cookie))
        .await;
    assert_eq!(response.location(), "/admin/settings?done=test");
    let sent = forum.state.mailer().sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].to, "sam@example.com");
}

#[sqlx::test]
async fn the_smtp_password_is_never_shown(db: PgPool) {
    let forum = Forum::new(db);
    let sam = forum.member("sam", "admin", None).await;
    let cookie = forum.session(sam).await;
    let fields = [
        ("site_name", "Club"),
        ("registration_mode", "invite_only"),
        ("invite_who", "everyone"),
        ("smtp_host", "smtp.example.com"),
        ("smtp_port", "587"),
        ("smtp_security", "starttls"),
        ("smtp_username", "club"),
        ("smtp_password", "hunter2-secret"),
        ("smtp_from", "club@example.com"),
    ];
    forum
        .submit("/admin/settings", &fields, Some(&cookie))
        .await;
    let page = forum.page_as("/admin/settings", &cookie).await;
    assert!(!page.contains("hunter2-secret"));
    assert!(page.contains("A password is saved"));
    // Saving again without typing it keeps it.
    let mut again = fields.to_vec();
    again.retain(|(k, _)| *k != "smtp_password");
    forum.submit("/admin/settings", &again, Some(&cookie)).await;
    let saved = parlour::mail::saved_smtp_settings(forum.state.db())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved.password, "hunter2-secret");
}

/// A saved SMTP password is only reused for the server and username it was
/// saved with, so changing the server without typing a password doesn't
/// send the old server's password to the new one.
#[sqlx::test]
async fn the_smtp_password_stays_with_its_server(db: PgPool) {
    let forum = Forum::new(db);
    let sam = forum.member("sam", "admin", None).await;
    let cookie = forum.session(sam).await;
    let fields = |host: &'static str, username: &'static str, password: &'static str| {
        vec![
            ("site_name", "Club"),
            ("registration_mode", "invite_only"),
            ("invite_who", "everyone"),
            ("smtp_host", host),
            ("smtp_port", "587"),
            ("smtp_security", "starttls"),
            ("smtp_username", username),
            ("smtp_password", password),
            ("smtp_from", "club@example.com"),
        ]
    };
    let saved_password = || async {
        parlour::mail::saved_smtp_settings(forum.state.db())
            .await
            .unwrap()
            .unwrap()
            .password
    };
    forum
        .submit(
            "/admin/settings",
            &fields("smtp.old.example", "club", "old-secret"),
            Some(&cookie),
        )
        .await;
    // The same server, spelt differently: the password is kept.
    forum
        .submit(
            "/admin/settings",
            &fields("SMTP.old.example", "club", ""),
            Some(&cookie),
        )
        .await;
    assert_eq!(saved_password().await, "old-secret");
    for (host, username) in [("smtp.new.example", "club"), ("smtp.old.example", "other")] {
        forum
            .submit(
                "/admin/settings",
                &fields("smtp.old.example", "club", "old-secret"),
                Some(&cookie),
            )
            .await;
        let response = forum
            .submit(
                "/admin/settings",
                &fields(host, username, ""),
                Some(&cookie),
            )
            .await;
        assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
        assert_eq!(saved_password().await, "", "{host} {username}");
    }
}

/// Settings from the environment stay there: saving them unchanged in
/// setup keeps using the environment's, and the environment's password is
/// never copied into the database.
#[sqlx::test]
async fn setup_doesnt_copy_the_environments_smtp_password(db: PgPool) {
    let mut forum = Forum::new(db);
    let env = parlour::mail::SmtpSettings {
        host: "smtp.env.example".into(),
        port: 587,
        security: parlour::mail::SmtpSecurity::StartTls,
        username: "club".into(),
        password: "env-secret".into(),
        from: "Club <club@example.com>".into(),
    };
    forum.state = forum.state.clone().with_config(parlour::config::Config {
        smtp: Some(env.clone()),
        ..parlour::config::Config::development()
    });
    let sam = forum.member("sam", "admin", None).await;
    let cookie = forum.session(sam).await;
    let page = forum.page_as("/setup/email", &cookie).await;
    assert!(page.contains("value=\"smtp.env.example\""));
    assert!(!page.contains("env-secret"));
    let fields = |from: &'static str| {
        vec![
            ("action", "save"),
            ("smtp_host", "smtp.env.example"),
            ("smtp_port", "587"),
            ("smtp_security", "starttls"),
            ("smtp_username", "club"),
            ("smtp_password", ""),
            ("smtp_from", from),
        ]
    };
    let response = forum
        .submit(
            "/setup/email",
            &fields("Club <club@example.com>"),
            Some(&cookie),
        )
        .await;
    assert_eq!(response.location(), "/setup/invites");
    assert!(
        parlour::mail::saved_smtp_settings(forum.state.db())
            .await
            .unwrap()
            .is_none()
    );
    // Changing something else needs the password typed in.
    let response = forum
        .submit(
            "/setup/email",
            &fields("Club <hello@example.com>"),
            Some(&cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(
        response.body.contains("Type the SMTP password"),
        "{}",
        response.body
    );
    let stored: i64 =
        sqlx::query_scalar("SELECT count(*) FROM settings WHERE value::text LIKE '%env-secret%'")
            .fetch_one(&forum.db)
            .await
            .unwrap();
    assert_eq!(stored, 0);
}

/// The last step points the new admin at what to do before members
/// arrive: make categories, and know that new members' first posts are
/// held for approval.
#[sqlx::test]
async fn setup_ends_by_pointing_at_categories_and_held_posts(db: PgPool) {
    let forum = Forum::new(db);
    let sam = forum.member("sam", "admin", None).await;
    let cookie = forum.session(sam).await;
    let page = forum.page_as("/setup/invites", &cookie).await;
    assert!(page.contains("href=\"/admin/categories\""), "{page}");
    assert!(page.contains("href=\"/admin/moderation\""), "{page}");
    assert!(page.contains("first 2 posts"), "{page}");
    parlour::settings::set(
        &forum.db,
        "moderation",
        &parlour::settings::ModerationSettings {
            hold_posts: 0,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let page = forum.page_as("/setup/invites", &cookie).await;
    assert!(!page.contains("first 0 posts"), "{page}");
    assert!(page.contains("aren’t held"), "{page}");
}
