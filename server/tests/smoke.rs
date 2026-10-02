use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use parlour::{AppState, app};
use sqlx::PgPool;
use tower::ServiceExt;

struct Response {
    status: StatusCode,
    headers: axum::http::HeaderMap,
    body: String,
}

async fn get(pool: PgPool, path: &str) -> Response {
    let response = app(AppState::new(pool))
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    Response {
        status,
        headers,
        body: String::from_utf8(bytes.to_vec()).unwrap(),
    }
}

#[sqlx::test]
async fn healthz_reports_ok_when_database_is_reachable(pool: PgPool) {
    let response = get(pool, "/healthz").await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.body, "ok");
}

/// Every response says it may not be framed by other sites (so a form on
/// a page here can't be clicked through an invisible frame) and that its
/// content type is to be believed.
#[sqlx::test]
async fn responses_carry_security_headers(pool: PgPool) {
    for path in ["/", "/no-such-page", "/healthz", "/static/parlour.css"] {
        let response = get(pool.clone(), path).await;
        assert_eq!(
            response
                .headers
                .get("x-frame-options")
                .map(|v| v.to_str().unwrap()),
            Some("DENY"),
            "{path}"
        );
        assert_eq!(
            response
                .headers
                .get("content-security-policy")
                .map(|v| v.to_str().unwrap()),
            Some("frame-ancestors 'none'"),
            "{path}"
        );
        assert_eq!(
            response
                .headers
                .get("x-content-type-options")
                .map(|v| v.to_str().unwrap()),
            Some("nosniff"),
            "{path}"
        );
    }
}

#[sqlx::test]
async fn index_uses_default_site_name_on_fresh_install(pool: PgPool) {
    let response = get(pool, "/").await;
    assert_eq!(response.status, StatusCode::OK);
    assert!(
        response.headers[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/html")
    );
    assert!(response.body.contains("<title>Parlour</title>"));
}

#[sqlx::test]
async fn index_uses_configured_site_name(pool: PgPool) {
    sqlx::query("INSERT INTO settings (key, value) VALUES ('site_name', to_jsonb($1::text))")
        .bind("The Tuesday Club")
        .execute(&pool)
        .await
        .unwrap();
    let response = get(pool, "/").await;
    assert!(response.body.contains("The Tuesday Club"));
}

#[sqlx::test]
async fn site_name_is_html_escaped(pool: PgPool) {
    sqlx::query("INSERT INTO settings (key, value) VALUES ('site_name', to_jsonb($1::text))")
        .bind("<script>alert(1)</script>")
        .execute(&pool)
        .await
        .unwrap();
    let response = get(pool, "/").await;
    assert!(!response.body.contains("<script>alert(1)</script>"));
    assert!(response.body.contains("&#60;script&#62;alert(1)"));
}

#[sqlx::test]
async fn index_links_the_stylesheet_and_stylesheet_is_served(pool: PgPool) {
    let index = get(pool.clone(), "/").await;
    assert!(index.body.contains("/static/parlour.css"));

    let css = get(pool, "/static/parlour.css").await;
    assert_eq!(css.status, StatusCode::OK);
    assert_eq!(css.headers[header::CONTENT_TYPE], "text/css");
    assert!(css.body.contains("--ink"));
}

#[sqlx::test]
async fn missing_static_file_is_404(pool: PgPool) {
    let response = get(pool, "/static/does-not-exist.css").await;
    assert_eq!(response.status, StatusCode::NOT_FOUND);
}

#[sqlx::test]
async fn unknown_page_is_404(pool: PgPool) {
    let response = get(pool, "/no-such-page").await;
    assert_eq!(response.status, StatusCode::NOT_FOUND);
}

#[sqlx::test]
async fn anonymous_responses_never_set_cookies(pool: PgPool) {
    for path in ["/", "/healthz", "/static/parlour.css", "/no-such-page"] {
        let response = get(pool.clone(), path).await;
        assert!(
            !response.headers.contains_key(header::SET_COOKIE),
            "{path} set a cookie"
        );
    }
}

#[sqlx::test]
async fn self_hosted_fonts_are_served(pool: PgPool) {
    let css = get(pool.clone(), "/static/parlour.css").await;
    for font in [
        "newsreader-latin-opsz-normal.woff2",
        "newsreader-latin-opsz-italic.woff2",
        "newsreader-latin-ext-opsz-normal.woff2",
        "newsreader-latin-ext-opsz-italic.woff2",
    ] {
        assert!(css.body.contains(&format!("url(fonts/{font})")), "{font}");
        let response = app(AppState::new(pool.clone()))
            .oneshot(
                Request::get(format!("/static/fonts/{font}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{font}");
        assert_eq!(response.headers()[header::CONTENT_TYPE], "font/woff2");
    }
    assert!(!css.body.contains("fonts.googleapis"));
}
