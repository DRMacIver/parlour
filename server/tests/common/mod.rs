//! Shared helpers for integration tests: an app over a test database, and
//! shorthand for creating forum content.

#![allow(dead_code)]

pub mod smtp;

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use http_body_util::BodyExt;
use parlour::cache::PageCache;
use parlour::changes::Change;
use parlour::mail::{Email, Mailer};
use parlour::store::{self, CreatedThread, NewCategory, NewPost, NewThread, NewUser};
use parlour::{AppState, app};
use sqlx::PgPool;
use tower::ServiceExt;

#[derive(Debug)]
pub struct Response {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: String,
}

impl Response {
    pub fn header(&self, name: header::HeaderName) -> Option<&str> {
        self.headers.get(name).map(|v| v.to_str().unwrap())
    }

    pub fn location(&self) -> &str {
        self.header(header::LOCATION).expect("no Location header")
    }
}

pub struct Forum {
    pub db: PgPool,
    pub state: AppState,
}

impl Forum {
    /// A forum with development settings whose email is kept in memory.
    pub fn new(db: PgPool) -> Forum {
        Forum {
            state: AppState::new(db.clone()).with_mailer(Mailer::capture()),
            db,
        }
    }

    /// A forum whose pages are never cached, for comparison.
    pub fn uncached(db: PgPool) -> Forum {
        Forum {
            state: AppState::with_cache(db.clone(), PageCache::disabled())
                .with_mailer(Mailer::capture()),
            db,
        }
    }

    pub async fn request(&self, request: Request<Body>) -> Response {
        let response = app(self.state.clone()).oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        Response {
            status,
            headers,
            body: String::from_utf8(bytes.to_vec()).unwrap(),
        }
    }

    pub async fn get(&self, path: &str) -> Response {
        self.request(Request::get(path).body(Body::empty()).unwrap())
            .await
    }

    /// GET and expect a 200.
    pub async fn page(&self, path: &str) -> String {
        let response = self.get(path).await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "GET {path}: {}",
            response.body
        );
        response.body
    }

    /// GET, as the browser holding `cookie` (a `name=value` pair) if any.
    pub async fn get_as(&self, path: &str, cookie: Option<&str>) -> Response {
        let mut request = Request::get(path);
        if let Some(cookie) = cookie {
            request = request.header(header::COOKIE, cookie);
        }
        self.request(request.body(Body::empty()).unwrap()).await
    }

    /// GET as `cookie`, expecting a 200.
    pub async fn page_as(&self, path: &str, cookie: &str) -> String {
        let response = self.get_as(path, Some(cookie)).await;
        assert_eq!(
            response.status,
            StatusCode::OK,
            "GET {path}: {}",
            response.body
        );
        response.body
    }

    /// Submits a form from one of the site's own pages.
    pub async fn submit(
        &self,
        path: &str,
        fields: &[(&str, &str)],
        cookie: Option<&str>,
    ) -> Response {
        self.submit_from(path, fields, cookie, Some(ORIGIN)).await
    }

    /// Submits a form with the given `Origin` header (or none).
    pub async fn submit_from(
        &self,
        path: &str,
        fields: &[(&str, &str)],
        cookie: Option<&str>,
        origin: Option<&str>,
    ) -> Response {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(fields)
            .finish();
        let mut request =
            Request::post(path).header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if let Some(origin) = origin {
            request = request.header(header::ORIGIN, origin);
        }
        if let Some(cookie) = cookie {
            request = request.header(header::COOKIE, cookie);
        }
        self.request(request.body(Body::from(body)).unwrap()).await
    }

    /// A member with a confirmed email address and, optionally, a password.
    pub async fn member(&self, username: &str, role: &str, password: Option<&str>) -> i64 {
        let id = store::create_user(
            &self.db,
            &NewUser {
                username: username.into(),
                display_name: capitalise(username),
                email: format!("{username}@example.com"),
                role: Some(role.into()),
                ..NewUser::default()
            },
        )
        .await
        .unwrap();
        let hash = match password {
            Some(password) => Some(parlour::accounts::hash_password(password).await),
            None => None,
        };
        sqlx::query("UPDATE users SET email_verified_at = now(), password_hash = $2 WHERE id = $1")
            .bind(id)
            .bind(hash)
            .execute(&self.db)
            .await
            .unwrap();
        id
    }

    /// Logs a user in directly, returning the cookie to send.
    pub async fn session(&self, user_id: i64) -> String {
        let token = parlour::sessions::create(&self.db, user_id, &HeaderMap::new(), "test")
            .await
            .unwrap();
        format!("parlour_session={token}")
    }

    /// Runs every queued job, as the workers would.
    pub async fn run_jobs(&self) {
        parlour::jobs::run_due(&self.state).await.unwrap();
    }

    /// Every email sent so far, after running queued jobs.
    pub async fn emails(&self) -> Vec<Email> {
        self.run_jobs().await;
        self.state.mailer().sent()
    }

    /// The last email sent to `to`.
    pub async fn last_email_to(&self, to: &str) -> Email {
        self.emails()
            .await
            .into_iter()
            .rev()
            .find(|email| email.to == to)
            .unwrap_or_else(|| panic!("no email to {to}"))
    }

    pub async fn changed(&self, change: Change) {
        self.state.changed(change).await.unwrap();
    }

    pub async fn user(&self, username: &str) -> i64 {
        store::create_user(
            &self.db,
            &NewUser {
                username: username.into(),
                display_name: capitalise(username),
                email: format!("{username}@example.com"),
                ..NewUser::default()
            },
        )
        .await
        .unwrap()
    }

    pub async fn section(&self, name: &str, position: i32) -> i64 {
        store::create_section(&self.db, name, position)
            .await
            .unwrap()
    }

    pub async fn category(
        &self,
        slug: &str,
        read_level: &str,
        section_id: Option<i64>,
        position: i32,
    ) -> i64 {
        store::create_category(
            &self.db,
            &NewCategory {
                section_id,
                slug: slug.into(),
                name: capitalise(&slug.replace('-', " ")),
                description: format!("All about {slug}."),
                position,
                read_level: read_level.into(),
                write_level: "members".into(),
            },
        )
        .await
        .unwrap()
    }

    pub async fn thread(
        &self,
        category_id: i64,
        author_id: i64,
        title: &str,
        body: &str,
    ) -> CreatedThread {
        self.new_thread(NewThread {
            category_id,
            title: title.into(),
            author_id,
            body_md: body.into(),
            forked_from_post_id: None,
            pinned: false,
            locked: false,
            created_at: None,
        })
        .await
    }

    pub async fn new_thread(&self, thread: NewThread) -> CreatedThread {
        store::create_thread(&self.db, &thread).await.unwrap()
    }

    pub async fn post(&self, thread_id: i64, author_id: i64, body: &str) -> i64 {
        self.reply(thread_id, author_id, body, None).await
    }

    pub async fn reply(
        &self,
        thread_id: i64,
        author_id: i64,
        body: &str,
        reply_to: Option<i64>,
    ) -> i64 {
        store::create_post(
            &self.db,
            &NewPost {
                thread_id,
                author_id,
                body_md: body.into(),
                reply_to_post_id: reply_to,
                created_at: None,
            },
        )
        .await
        .unwrap()
    }

    pub async fn exec(&self, sql: &str) {
        sqlx::raw_sql(sqlx::AssertSqlSafe(sql.to_owned()))
            .execute(&self.db)
            .await
            .unwrap();
    }
}

pub fn capitalise(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// The text of every element with `id="{id}"`… up to the next post, for
/// checking what one post on a thread page says.
pub fn post_html(page: &str, post_id: i64) -> &str {
    let start = page
        .find(&format!("id=\"p{post_id}\""))
        .unwrap_or_else(|| panic!("post {post_id} not on page"));
    let rest = &page[start..];
    let end = rest.find("</article>").unwrap_or(rest.len());
    &rest[..end]
}

/// The origin of the development configuration that tests use.
pub const ORIGIN: &str = "http://localhost:8080";

/// The session cookie a response sets, as a `name=value` pair to send back.
pub fn cookie_set_by(response: &Response) -> Option<String> {
    response
        .headers
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| v.starts_with("parlour_session=") && !v.contains("Max-Age=0"))
        .map(|v| v.split(';').next().unwrap().to_owned())
}

/// The path of the first link in `text` to this site that starts with
/// `prefix` (such as `/login/`).
pub fn link_in(text: &str, prefix: &str) -> String {
    let start = text
        .find(&format!("{ORIGIN}{prefix}"))
        .unwrap_or_else(|| panic!("no {prefix} link in {text}"));
    let rest = &text[start + ORIGIN.len()..];
    let end = rest
        .find(|c: char| c.is_whitespace() || c == '"' || c == '<')
        .unwrap_or(rest.len());
    rest[..end].to_owned()
}

/// The value of a hidden form field.
pub fn hidden_field(page: &str, name: &str) -> String {
    let marker = format!("name=\"{name}\" value=\"");
    let start = page
        .find(&marker)
        .unwrap_or_else(|| panic!("no field {name} in {page}"))
        + marker.len();
    let end = page[start..].find('"').unwrap();
    page[start..start + end].to_owned()
}
