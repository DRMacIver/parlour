//! Logging in and out, sessions, magic links, password resets, email
//! verification, account settings, CSRF checks and rate limits
//! (DESIGN.md §7).

mod common;

use axum::http::{StatusCode, header};
use common::{Forum, cookie_set_by, hidden_field, link_in};
use parlour::config::Config;
use parlour::http_cache::{PRIVATE_CACHE_CONTROL, PUBLIC_CACHE_CONTROL};
use sqlx::PgPool;

const PASSWORD: &str = "correct horse battery";

async fn forum_with_ruth(db: PgPool) -> (Forum, i64) {
    let forum = Forum::new(db);
    let ruth = forum.member("ruth", "user", Some(PASSWORD)).await;
    (forum, ruth)
}

async fn log_in(forum: &Forum, identifier: &str, password: &str) -> common::Response {
    forum
        .submit(
            "/login",
            &[
                ("identifier", identifier),
                ("password", password),
                ("next", ""),
            ],
            None,
        )
        .await
}

// ---------------------------------------------------------------------------
// Passwords and sessions

#[sqlx::test]
async fn logging_in_with_a_password_starts_a_session(db: PgPool) {
    let (forum, _) = forum_with_ruth(db).await;
    let response = log_in(&forum, "ruth", PASSWORD).await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert_eq!(response.location(), "/");
    let set_cookie = response.header(header::SET_COOKIE).unwrap();
    assert!(set_cookie.contains("HttpOnly"));
    assert!(set_cookie.contains("SameSite=Lax"));
    assert!(set_cookie.contains("Path=/"));
    // Development settings serve plain http, so the cookie can't be Secure.
    assert!(!set_cookie.contains("Secure"));

    let cookie = cookie_set_by(&response).unwrap();
    let index = forum.get_as("/", Some(&cookie)).await;
    assert_eq!(index.status, StatusCode::OK);
    assert!(index.body.contains("class=\"nav-user\""));
    assert!(index.body.contains(">ruth</a>"));
    assert!(index.body.contains("action=\"/logout\""));
    assert_eq!(
        index.header(header::CACHE_CONTROL),
        Some(PRIVATE_CACHE_CONTROL)
    );
}

#[sqlx::test]
async fn email_addresses_also_log_in_and_case_does_not_matter(db: PgPool) {
    let (forum, _) = forum_with_ruth(db).await;
    for identifier in ["ruth@example.com", "RUTH", "Ruth@Example.com"] {
        let response = log_in(&forum, identifier, PASSWORD).await;
        assert_eq!(response.status, StatusCode::SEE_OTHER, "{identifier}");
    }
}

#[sqlx::test]
async fn secure_cookies_when_configured(db: PgPool) {
    let mut forum = Forum::new(db);
    let config = Config {
        cookie_secure: true,
        ..Config::development()
    };
    forum.state = forum.state.clone().with_config(config);
    forum.member("ruth", "user", Some(PASSWORD)).await;
    let response = log_in(&forum, "ruth", PASSWORD).await;
    assert!(
        response
            .header(header::SET_COOKIE)
            .unwrap()
            .contains("; Secure")
    );
}

#[sqlx::test]
async fn wrong_passwords_are_refused_without_a_cookie(db: PgPool) {
    let (forum, _) = forum_with_ruth(db).await;
    for (who, password) in [
        ("ruth", "wrong password!"),
        ("nobody", PASSWORD),
        ("ruth", ""),
        ("", ""),
    ] {
        let response = log_in(&forum, who, password).await;
        assert_eq!(response.status, StatusCode::UNAUTHORIZED, "{who}");
        assert!(response.header(header::SET_COOKIE).is_none());
        assert!(response.body.contains("don’t match an account"));
    }
}

#[sqlx::test]
async fn members_without_passwords_cant_log_in_with_one(db: PgPool) {
    let forum = Forum::new(db);
    forum.member("maya", "user", None).await;
    let response = log_in(&forum, "maya", "").await;
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test]
async fn login_goes_back_to_where_it_was_asked_from(db: PgPool) {
    let (forum, _) = forum_with_ruth(db).await;
    let page = forum.page("/login?next=/invites").await;
    assert_eq!(hidden_field(&page, "next"), "/invites");
    let response = forum
        .submit(
            "/login",
            &[
                ("identifier", "ruth"),
                ("password", PASSWORD),
                ("next", "/invites"),
            ],
            None,
        )
        .await;
    assert_eq!(response.location(), "/invites");
    // Other sites are ignored.
    for next in ["//evil.com", "https://evil.com", "/\\evil.com"] {
        let response = forum
            .submit(
                "/login",
                &[
                    ("identifier", "ruth"),
                    ("password", PASSWORD),
                    ("next", next),
                ],
                None,
            )
            .await;
        assert_eq!(response.location(), "/", "{next}");
        let page = forum
            .page(&format!("/login?next={}", next.replace('\\', "%5C")))
            .await;
        assert_eq!(hidden_field(&page, "next"), "", "{next}");
    }
}

#[sqlx::test]
async fn pages_that_need_a_login_send_you_to_log_in(db: PgPool) {
    let forum = Forum::new(db);
    for (path, back) in [
        ("/settings", "/settings"),
        ("/invites", "/invites"),
        ("/admin/settings", "/admin/settings"),
    ] {
        let response = forum.get(path).await;
        assert_eq!(response.status, StatusCode::SEE_OTHER, "{path}");
        assert_eq!(response.location(), format!("/login?next={back}"));
        assert!(response.header(header::SET_COOKIE).is_none());
    }
}

#[sqlx::test]
async fn the_login_page_is_cached_for_anonymous_visitors(db: PgPool) {
    let forum = Forum::new(db);
    for path in ["/login", "/reset"] {
        let response = forum.get(path).await;
        assert_eq!(response.status, StatusCode::OK);
        assert_eq!(
            response.header(header::CACHE_CONTROL),
            Some(PUBLIC_CACHE_CONTROL)
        );
        assert!(response.header(header::SET_COOKIE).is_none());
        assert!(!response.body.contains("csrf"));
    }
}

#[sqlx::test]
async fn logging_out_ends_the_session(db: PgPool) {
    let (forum, ruth) = forum_with_ruth(db).await;
    let cookie = forum.session(ruth).await;
    let other = forum.session(ruth).await;
    let confirm = forum.get_as("/logout", Some(&cookie)).await;
    assert!(confirm.body.contains("action=\"/logout\""));
    let response = forum.submit("/logout", &[], Some(&cookie)).await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert!(
        response
            .header(header::SET_COOKIE)
            .unwrap()
            .contains("Max-Age=0")
    );
    // That session is gone; the other browser is still logged in.
    let after = forum.get_as("/settings", Some(&cookie)).await;
    assert_eq!(after.status, StatusCode::SEE_OTHER);
    assert_eq!(
        forum.get_as("/settings", Some(&other)).await.status,
        StatusCode::OK
    );
}

#[sqlx::test]
async fn unknown_session_cookies_are_cleared(db: PgPool) {
    let forum = Forum::new(db);
    for cookie in [
        "parlour_session=nonsense",
        "parlour_session=",
        "parlour_session=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
    ] {
        let response = forum.get_as("/", Some(cookie)).await;
        assert_eq!(response.status, StatusCode::OK);
        assert!(response.body.contains("href=\"/login\""));
        assert_eq!(
            response.header(header::CACHE_CONTROL),
            Some(PRIVATE_CACHE_CONTROL)
        );
        assert!(
            response
                .header(header::SET_COOKIE)
                .unwrap()
                .contains("Max-Age=0")
        );
    }
}

#[sqlx::test]
async fn idle_sessions_expire(db: PgPool) {
    let (forum, ruth) = forum_with_ruth(db).await;
    let cookie = forum.session(ruth).await;
    forum
        .exec("UPDATE sessions SET last_seen_at = now() - interval '31 days'")
        .await;
    let response = forum.get_as("/settings", Some(&cookie)).await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
}

#[sqlx::test]
async fn deleted_accounts_are_logged_out(db: PgPool) {
    let (forum, ruth) = forum_with_ruth(db).await;
    let cookie = forum.session(ruth).await;
    forum.exec("UPDATE users SET deleted_at = now()").await;
    assert_eq!(
        forum.get_as("/settings", Some(&cookie)).await.status,
        StatusCode::SEE_OTHER
    );
}

#[sqlx::test]
async fn sessions_record_activity_without_writing_on_every_request(db: PgPool) {
    let (forum, ruth) = forum_with_ruth(db).await;
    let cookie = forum.session(ruth).await;
    forum
        .exec("UPDATE sessions SET last_seen_at = now() - interval '1 hour'")
        .await;
    forum.page_as("/", &cookie).await;
    let recent: bool =
        sqlx::query_scalar("SELECT last_seen_at > now() - interval '1 minute' FROM sessions")
            .fetch_one(&forum.db)
            .await
            .unwrap();
    assert!(recent);
}

// ---------------------------------------------------------------------------
// Visibility for logged-in members

#[sqlx::test]
async fn members_see_members_only_categories_and_staff_see_everything(db: PgPool) {
    let forum = Forum::new(db);
    let author = forum.user("author").await;
    let member = forum.member("ruth", "user", None).await;
    let moderator = forum.member("sam", "moderator", None).await;
    let public = forum.category("lobby", "public", None, 0).await;
    let members = forum.category("back-room", "members", None, 1).await;
    let groups = forum.category("committee", "groups", None, 2).await;
    let t_public = forum
        .thread(public, author, "Public thread", "Hello everyone")
        .await;
    let t_members = forum
        .thread(members, author, "Members thread", "Members only text")
        .await;
    let t_groups = forum
        .thread(groups, author, "Committee thread", "Committee only text")
        .await;

    let anonymous = forum.page("/").await;
    assert!(!anonymous.contains("Back room"));
    let member_cookie = forum.session(member).await;
    let index = forum.page_as("/", &member_cookie).await;
    assert!(index.contains("Back room"));
    assert!(index.contains("Members only"));
    assert!(!index.contains("Committee"));
    let staff_cookie = forum.session(moderator).await;
    let index = forum.page_as("/", &staff_cookie).await;
    assert!(index.contains("Back room") && index.contains("Committee"));

    for (thread, member_sees, anon_sees) in [
        (t_public.thread_id, true, true),
        (t_members.thread_id, true, false),
        (t_groups.thread_id, false, false),
    ] {
        let path = format!("/t/{thread}");
        assert_eq!(
            forum.get(&path).await.status == StatusCode::PERMANENT_REDIRECT,
            anon_sees
        );
        assert_eq!(
            forum.get_as(&path, Some(&member_cookie)).await.status
                == StatusCode::PERMANENT_REDIRECT,
            member_sees
        );
        assert_eq!(
            forum.get_as(&path, Some(&staff_cookie)).await.status,
            StatusCode::PERMANENT_REDIRECT
        );
    }
    // A member's page never ends up in the anonymous cache.
    let latest = forum.page_as("/latest", &member_cookie).await;
    assert!(latest.contains("Members thread"));
    assert!(!forum.page("/latest").await.contains("Members thread"));
}

// ---------------------------------------------------------------------------
// CSRF

#[sqlx::test]
async fn forms_from_other_sites_are_refused(db: PgPool) {
    let (forum, ruth) = forum_with_ruth(db).await;
    let cookie = forum.session(ruth).await;
    let fields = [("identifier", "ruth"), ("password", PASSWORD)];
    for origin in [Some("https://evil.example.com"), Some("null"), None] {
        let response = forum.submit_from("/login", &fields, None, origin).await;
        assert_eq!(response.status, StatusCode::FORBIDDEN, "{origin:?}");
        assert!(response.header(header::SET_COOKIE).is_none());
        let response = forum
            .submit_from("/logout", &[], Some(&cookie), origin)
            .await;
        assert_eq!(response.status, StatusCode::FORBIDDEN, "{origin:?}");
    }
    // The session survived.
    assert_eq!(
        forum.get_as("/settings", Some(&cookie)).await.status,
        StatusCode::OK
    );
}

#[sqlx::test]
async fn a_same_site_referer_is_enough_without_an_origin(db: PgPool) {
    let (forum, _) = forum_with_ruth(db).await;
    let body = "identifier=ruth&password=correct+horse+battery";
    let request = axum::http::Request::post("/login")
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header(header::REFERER, "http://localhost:8080/login")
        .body(axum::body::Body::from(body))
        .unwrap();
    assert_eq!(forum.request(request).await.status, StatusCode::SEE_OTHER);
}

// ---------------------------------------------------------------------------
// Rate limits

#[sqlx::test]
async fn repeated_login_attempts_are_slowed_down(db: PgPool) {
    let (forum, _) = forum_with_ruth(db).await;
    let mut statuses = Vec::new();
    for _ in 0..12 {
        statuses.push(log_in(&forum, "ruth", "wrong password!").await.status);
    }
    assert!(
        statuses[..10]
            .iter()
            .all(|s| *s == StatusCode::UNAUTHORIZED)
    );
    assert_eq!(statuses[10], StatusCode::TOO_MANY_REQUESTS);
    // Even the right password is refused for now.
    assert_eq!(
        log_in(&forum, "ruth", PASSWORD).await.status,
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[sqlx::test]
async fn magic_link_requests_are_limited_per_address(db: PgPool) {
    let (forum, _) = forum_with_ruth(db).await;
    let mut ok = 0;
    for _ in 0..5 {
        let response = forum
            .submit("/login/email", &[("email", "ruth@example.com")], None)
            .await;
        if response.status == StatusCode::OK {
            ok += 1;
        } else {
            assert_eq!(response.status, StatusCode::TOO_MANY_REQUESTS);
        }
    }
    assert_eq!(ok, 3);
    assert_eq!(forum.emails().await.len(), 3);
}

// ---------------------------------------------------------------------------
// Magic links

#[sqlx::test]
async fn magic_links_log_in_once(db: PgPool) {
    let (forum, _) = forum_with_ruth(db).await;
    let response = forum
        .submit(
            "/login/email",
            &[("email", "ruth@example.com"), ("next", "/invites")],
            None,
        )
        .await;
    assert_eq!(response.status, StatusCode::OK);
    assert!(response.body.contains("Check your email"));
    let email = forum.last_email_to("ruth@example.com").await;
    assert!(email.subject.contains("login link"));
    let link = link_in(&email.body, "/login/");
    assert!(link.ends_with("?next=/invites"), "{link}");

    // Opening the link doesn't log in by itself (mail scanners open links),
    // isn't cached, and keeps the token out of Referer headers.
    let page = forum.get(&link).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.header(header::SET_COOKIE).is_none());
    assert_eq!(
        page.header(header::CACHE_CONTROL),
        Some(PRIVATE_CACHE_CONTROL)
    );
    assert_eq!(page.header(header::REFERRER_POLICY), Some("same-origin"));
    let path = link.split('?').next().unwrap();
    assert_eq!(hidden_field(&page.body, "next"), "/invites");

    let response = forum.submit(path, &[("next", "/invites")], None).await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert_eq!(response.location(), "/invites");
    let cookie = cookie_set_by(&response).unwrap();
    assert_eq!(
        forum.get_as("/invites", Some(&cookie)).await.status,
        StatusCode::OK
    );

    // Used up.
    let again = forum.submit(path, &[], None).await;
    assert_eq!(again.status, StatusCode::GONE);
    assert!(again.header(header::SET_COOKIE).is_none());
    assert_eq!(forum.get(path).await.status, StatusCode::GONE);
}

#[sqlx::test]
async fn magic_links_expire(db: PgPool) {
    let (forum, _) = forum_with_ruth(db).await;
    forum
        .submit("/login/email", &[("email", "ruth")], None)
        .await;
    let link = link_in(
        &forum.last_email_to("ruth@example.com").await.body,
        "/login/",
    );
    forum
        .exec("UPDATE login_tokens SET expires_at = now() - interval '1 second'")
        .await;
    assert_eq!(
        forum.submit(&link, &[], None).await.status,
        StatusCode::GONE
    );
}

#[sqlx::test]
async fn the_emailed_code_logs_in_on_the_page_that_asked(db: PgPool) {
    let (forum, _) = forum_with_ruth(db).await;
    let page = forum
        .submit("/login/email", &[("email", "ruth@example.com")], None)
        .await
        .body;
    let request = hidden_field(&page, "request");
    let email = forum.last_email_to("ruth@example.com").await;
    let code: String = email
        .body
        .lines()
        .map(str::trim)
        .find(|line| line.len() == 6 && line.bytes().all(|b| b.is_ascii_digit()))
        .unwrap()
        .to_owned();
    let wrong = if code == "000000" { "111111" } else { "000000" };
    let response = forum
        .submit(
            "/login/code",
            &[("request", &request), ("code", wrong)],
            None,
        )
        .await;
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
    assert!(response.body.contains("4 tries left"));
    let spaced = format!("{} {}", &code[..3], &code[3..]);
    let response = forum
        .submit(
            "/login/code",
            &[("request", &request), ("code", &spaced)],
            None,
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert!(cookie_set_by(&response).is_some());
    // The link in the same email is used up too.
    let link = link_in(&email.body, "/login/");
    assert_eq!(
        forum.submit(&link, &[], None).await.status,
        StatusCode::GONE
    );
}

#[sqlx::test]
async fn codes_stop_working_after_five_wrong_guesses(db: PgPool) {
    let (forum, _) = forum_with_ruth(db).await;
    let page = forum
        .submit("/login/email", &[("email", "ruth")], None)
        .await
        .body;
    let request = hidden_field(&page, "request");
    let email = forum.last_email_to("ruth@example.com").await;
    let code = email
        .body
        .lines()
        .map(str::trim)
        .find(|line| line.len() == 6 && line.bytes().all(|b| b.is_ascii_digit()))
        .unwrap()
        .to_owned();
    let wrong: Vec<String> = (0..6)
        .map(|i| format!("{:06}", (code.parse::<u32>().unwrap() + 1 + i) % 1_000_000))
        .collect();
    for guess in &wrong[..5] {
        let response = forum
            .submit(
                "/login/code",
                &[("request", &request), ("code", guess)],
                None,
            )
            .await;
        assert_ne!(response.status, StatusCode::SEE_OTHER);
    }
    let response = forum
        .submit(
            "/login/code",
            &[("request", &request), ("code", &code)],
            None,
        )
        .await;
    assert_eq!(response.status, StatusCode::GONE);
}

#[sqlx::test]
async fn asking_for_a_link_doesnt_reveal_who_has_an_account(db: PgPool) {
    let (forum, _) = forum_with_ruth(db).await;
    let known = forum
        .submit("/login/email", &[("email", "ruth@example.com")], None)
        .await;
    let unknown = forum
        .submit("/login/email", &[("email", "nobody@example.com")], None)
        .await;
    assert_eq!(known.status, unknown.status);
    let strip = |body: &str| body.replace(&hidden_field(body, "request"), "");
    assert_eq!(strip(&known.body), strip(&unknown.body));
    let emails = forum.emails().await;
    assert_eq!(emails.len(), 1);
    // A code for the unknown address's page goes nowhere.
    let request = hidden_field(&unknown.body, "request");
    let response = forum
        .submit(
            "/login/code",
            &[("request", &request), ("code", "123456")],
            None,
        )
        .await;
    assert_eq!(response.status, StatusCode::GONE);
}

#[sqlx::test]
async fn a_magic_link_confirms_the_email_address(db: PgPool) {
    let forum = Forum::new(db);
    let ruth = forum.member("ruth", "user", None).await;
    forum
        .exec("UPDATE users SET email_verified_at = NULL")
        .await;
    forum
        .submit("/login/email", &[("email", "ruth@example.com")], None)
        .await;
    let link = link_in(
        &forum.last_email_to("ruth@example.com").await.body,
        "/login/",
    );
    forum.submit(&link, &[], None).await;
    let verified: bool =
        sqlx::query_scalar("SELECT email_verified_at IS NOT NULL FROM users WHERE id = $1")
            .bind(ruth)
            .fetch_one(&forum.db)
            .await
            .unwrap();
    assert!(verified);
}

// ---------------------------------------------------------------------------
// Password resets

#[sqlx::test]
async fn resetting_a_password(db: PgPool) {
    let (forum, ruth) = forum_with_ruth(db).await;
    let elsewhere = forum.session(ruth).await;
    let response = forum
        .submit("/reset", &[("email", "ruth@example.com")], None)
        .await;
    assert_eq!(response.status, StatusCode::OK);
    assert!(response.body.contains("If there is an account"));
    let link = link_in(
        &forum.last_email_to("ruth@example.com").await.body,
        "/reset/",
    );

    let page = forum.get(&link).await;
    assert_eq!(page.status, StatusCode::OK);
    assert_eq!(page.header(header::REFERRER_POLICY), Some("same-origin"));
    let mismatch = forum
        .submit(
            &link,
            &[("password", "a new password"), ("confirm", "another one")],
            None,
        )
        .await;
    assert_eq!(mismatch.status, StatusCode::BAD_REQUEST);
    assert!(mismatch.body.contains("don’t match"));
    let short = forum
        .submit(&link, &[("password", "short"), ("confirm", "short")], None)
        .await;
    assert_eq!(short.status, StatusCode::BAD_REQUEST);

    let response = forum
        .submit(
            &link,
            &[
                ("password", "a new password"),
                ("confirm", "a new password"),
            ],
            None,
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert!(cookie_set_by(&response).is_some());
    assert_eq!(
        log_in(&forum, "ruth", PASSWORD).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        log_in(&forum, "ruth", "a new password").await.status,
        StatusCode::SEE_OTHER
    );
    // Other sessions were ended.
    assert_eq!(
        forum.get_as("/settings", Some(&elsewhere)).await.status,
        StatusCode::SEE_OTHER
    );
    // The link is used up.
    assert_eq!(
        forum
            .submit(
                &link,
                &[
                    ("password", "third password"),
                    ("confirm", "third password")
                ],
                None
            )
            .await
            .status,
        StatusCode::GONE
    );
}

#[sqlx::test]
async fn only_the_latest_reset_link_works(db: PgPool) {
    let (forum, _) = forum_with_ruth(db).await;
    forum.submit("/reset", &[("email", "ruth")], None).await;
    let first = link_in(
        &forum.last_email_to("ruth@example.com").await.body,
        "/reset/",
    );
    forum.submit("/reset", &[("email", "ruth")], None).await;
    let second = link_in(
        &forum.last_email_to("ruth@example.com").await.body,
        "/reset/",
    );
    assert_ne!(first, second);
    assert_eq!(forum.get(&first).await.status, StatusCode::GONE);
    assert_eq!(forum.get(&second).await.status, StatusCode::OK);
}

#[sqlx::test]
async fn reset_requests_for_unknown_addresses_send_nothing(db: PgPool) {
    let (forum, _) = forum_with_ruth(db).await;
    let response = forum
        .submit("/reset", &[("email", "nobody@example.com")], None)
        .await;
    assert_eq!(response.status, StatusCode::OK);
    assert!(response.body.contains("If there is an account"));
    assert!(forum.emails().await.is_empty());
}

// ---------------------------------------------------------------------------
// Account settings

#[sqlx::test]
async fn changing_a_password_logs_out_other_browsers(db: PgPool) {
    let (forum, ruth) = forum_with_ruth(db).await;
    let here = forum.session(ruth).await;
    let there = forum.session(ruth).await;
    let page = forum.page_as("/settings", &here).await;
    assert!(page.contains("1 other browser or device"));
    let wrong = forum
        .submit(
            "/settings/password",
            &[
                ("current", "not it at all"),
                ("password", "brand new password"),
                ("confirm", "brand new password"),
            ],
            Some(&here),
        )
        .await;
    assert_eq!(wrong.status, StatusCode::BAD_REQUEST);
    assert!(wrong.body.contains("current password isn’t right"));
    let response = forum
        .submit(
            "/settings/password",
            &[
                ("current", PASSWORD),
                ("password", "brand new password"),
                ("confirm", "brand new password"),
            ],
            Some(&here),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert_eq!(response.location(), "/settings?done=password");
    assert_eq!(
        forum.get_as("/settings", Some(&here)).await.status,
        StatusCode::OK
    );
    assert_eq!(
        forum.get_as("/settings", Some(&there)).await.status,
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        log_in(&forum, "ruth", "brand new password").await.status,
        StatusCode::SEE_OTHER
    );
}

#[sqlx::test]
async fn members_without_a_password_can_set_one(db: PgPool) {
    let forum = Forum::new(db);
    let maya = forum.member("maya", "user", None).await;
    let cookie = forum.session(maya).await;
    let page = forum.page_as("/settings", &cookie).await;
    assert!(page.contains("Set a password"));
    let response = forum
        .submit(
            "/settings/password",
            &[
                ("password", "my first password"),
                ("confirm", "my first password"),
            ],
            Some(&cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert_eq!(
        log_in(&forum, "maya", "my first password").await.status,
        StatusCode::SEE_OTHER
    );
}

#[sqlx::test]
async fn changing_email_takes_effect_when_the_new_address_is_confirmed(db: PgPool) {
    let (forum, ruth) = forum_with_ruth(db).await;
    forum.member("jonah", "user", None).await;
    let cookie = forum.session(ruth).await;
    let taken = forum
        .submit(
            "/settings/email",
            &[("email", "JONAH@example.com"), ("password", PASSWORD)],
            Some(&cookie),
        )
        .await;
    assert_eq!(taken.status, StatusCode::BAD_REQUEST);
    assert!(taken.body.contains("already in use"));
    let no_password = forum
        .submit(
            "/settings/email",
            &[("email", "ruth@new.example")],
            Some(&cookie),
        )
        .await;
    assert_eq!(no_password.status, StatusCode::BAD_REQUEST);

    let response = forum
        .submit(
            "/settings/email",
            &[("email", "ruth@new.example"), ("password", PASSWORD)],
            Some(&cookie),
        )
        .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    // Still the old address until confirmed.
    assert!(
        forum
            .page_as("/settings", &cookie)
            .await
            .contains("ruth@example.com")
    );
    let link = link_in(
        &forum.last_email_to("ruth@new.example").await.body,
        "/verify/",
    );
    let page = forum.get(&link).await;
    assert_eq!(page.status, StatusCode::OK);
    assert_eq!(page.header(header::REFERRER_POLICY), Some("same-origin"));
    let confirmed = forum.submit(&link, &[], Some(&cookie)).await;
    assert_eq!(confirmed.location(), "/settings?done=verified");
    let settings = forum.page_as("/settings", &cookie).await;
    assert!(settings.contains("ruth@new.example"));
    assert_eq!(
        log_in(&forum, "ruth@new.example", PASSWORD).await.status,
        StatusCode::SEE_OTHER
    );
}

#[sqlx::test]
async fn unconfirmed_members_can_ask_for_another_confirmation_email(db: PgPool) {
    let forum = Forum::new(db);
    let maya = forum.member("maya", "user", None).await;
    forum
        .exec("UPDATE users SET email_verified_at = NULL")
        .await;
    let cookie = forum.session(maya).await;
    assert!(
        forum
            .page_as("/settings", &cookie)
            .await
            .contains("Not confirmed")
    );
    let response = forum.submit("/settings/verify", &[], Some(&cookie)).await;
    assert_eq!(response.location(), "/settings?done=verify");
    let link = link_in(
        &forum.last_email_to("maya@example.com").await.body,
        "/verify/",
    );
    // Anyone holding the link can confirm, logged in or not.
    let response = forum.submit(&link, &[], None).await;
    assert_eq!(response.status, StatusCode::OK);
    assert!(
        forum
            .page_as("/settings", &cookie)
            .await
            .contains("Confirmed")
    );
}

#[sqlx::test]
async fn logging_out_everywhere_else(db: PgPool) {
    let (forum, ruth) = forum_with_ruth(db).await;
    let here = forum.session(ruth).await;
    let there = forum.session(ruth).await;
    let response = forum.submit("/settings/sessions", &[], Some(&here)).await;
    assert_eq!(response.location(), "/settings?done=sessions");
    assert_eq!(
        forum.get_as("/settings", Some(&here)).await.status,
        StatusCode::OK
    );
    assert_eq!(
        forum.get_as("/settings", Some(&there)).await.status,
        StatusCode::SEE_OTHER
    );
}
