//! Shared helpers for integration tests: an app over a test database, and
//! shorthand for creating forum content.

#![allow(dead_code)]

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use http_body_util::BodyExt;
use parlour::cache::PageCache;
use parlour::changes::Change;
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
    pub fn new(db: PgPool) -> Forum {
        Forum {
            state: AppState::new(db.clone()),
            db,
        }
    }

    /// A forum whose pages are never cached, for comparison.
    pub fn uncached(db: PgPool) -> Forum {
        Forum {
            state: AppState::with_cache(db.clone(), PageCache::disabled()),
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
