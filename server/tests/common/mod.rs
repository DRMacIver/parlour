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

    /// Trusts `X-Forwarded-For`, so that [`Forum::submit_as_ip`] can say
    /// which address a request comes from.
    pub fn trusting_proxy(mut self) -> Forum {
        let config = parlour::config::Config {
            trust_proxy: true,
            ..self.state.config().clone()
        };
        self.state = self.state.clone().with_config(config);
        self
    }

    /// Submits a form from the client address `ip` (with
    /// [`Forum::trusting_proxy`]).
    pub async fn submit_as_ip(&self, path: &str, fields: &[(&str, &str)], ip: &str) -> Response {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(fields)
            .finish();
        let request = Request::post(path)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::ORIGIN, ORIGIN)
            .header("x-forwarded-for", ip);
        self.request(request.body(Body::from(body)).unwrap()).await
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

    /// Lifts every rate limit, for tests about something else that write
    /// a lot.
    pub fn without_rate_limits(mut self) -> Forum {
        fn generous(
            _: parlour::ratelimit::Action,
            _: parlour::ratelimit::Scope,
        ) -> parlour::ratelimit::Rule {
            parlour::ratelimit::Rule {
                burst: 1_000_000,
                interval: std::time::Duration::from_millis(1),
            }
        }
        self.state = self
            .state
            .clone()
            .with_limiter(parlour::ratelimit::RateLimiter::new(generous));
        self
    }

    /// The id of the member with this username.
    pub async fn user_id(&self, username: &str) -> i64 {
        sqlx::query_scalar("SELECT id FROM users WHERE username = $1")
            .bind(username)
            .fetch_one(&self.db)
            .await
            .unwrap()
    }

    /// A member invited by `inviter`.
    pub async fn invitee(&self, username: &str, inviter: i64) -> i64 {
        store::create_user(
            &self.db,
            &NewUser {
                username: username.into(),
                display_name: capitalise(username),
                email: format!("{username}@example.com"),
                invited_by_user_id: Some(inviter),
                ..NewUser::default()
            },
        )
        .await
        .unwrap()
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

    /// A group, with no members.
    pub async fn group(&self, slug: &str, name: &str) -> i64 {
        parlour::groups::create(
            self.state.db(),
            &parlour::groups::GroupFields {
                slug: slug.into(),
                name: name.into(),
                description: String::new(),
            },
        )
        .await
        .unwrap()
        .unwrap()
    }

    pub async fn join_group(&self, group_id: i64, user_id: i64) {
        assert!(
            parlour::groups::add_member(self.state.db(), group_id, user_id, None)
                .await
                .unwrap()
        );
    }

    /// Lets these groups read (and, with `write`, post in) a category,
    /// which becomes group-restricted.
    pub async fn restrict(&self, category_id: i64, groups: &[i64], write: Option<&[i64]>) {
        let db = self.state.db();
        let viewer = parlour::viewer::Viewer::User(
            parlour::sessions::user_ctx(db, self.staff().await)
                .await
                .unwrap()
                .unwrap(),
        );
        let mut settings = parlour::content::category_by_id(db, &viewer, category_id)
            .await
            .unwrap()
            .unwrap()
            .settings();
        settings.read_level = "groups".into();
        settings.read_groups = groups.to_vec();
        if let Some(write) = write {
            settings.write_level = "groups".into();
            settings.write_groups = write.to_vec();
        }
        let settings = settings.clean().unwrap();
        parlour::store::update_category(db, category_id, &settings)
            .await
            .unwrap()
            .unwrap();
        self.changed(Change::CategoryChanged { category_id }).await;
    }

    /// Some admin's id, making one if there is none.
    async fn staff(&self) -> i64 {
        let existing: Option<i64> =
            sqlx::query_scalar("SELECT id FROM users WHERE role = 'admin' ORDER BY id LIMIT 1")
                .fetch_optional(&self.db)
                .await
                .unwrap();
        match existing {
            Some(id) => id,
            None => self.member("root", "admin", None).await,
        }
    }

    /// Counts nobody as a new member (DESIGN.md §7), so that nobody's posts
    /// are held for approval or limited to new members' rates. Tests of
    /// those limits leave them on.
    pub async fn no_new_members(&self) {
        parlour::settings::set(
            &self.db,
            "messages",
            &parlour::settings::MessageSettings {
                new_user_days: 0,
                new_user_posts: 0,
                ..parlour::settings::MessageSettings::default()
            },
        )
        .await
        .unwrap();
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
