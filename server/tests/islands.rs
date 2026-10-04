//! The JavaScript islands (DESIGN.md §5) and the JSON endpoints they use
//! (DESIGN.md §11). Islands load only for logged-in members; each wraps a
//! plain form that works without them; the endpoints answer through the
//! same access layer as the pages.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{Forum, ORIGIN, Response, post_html};
use parlour::islands::IslandAssets;
use parlour::store::{self, NewConversation};
use serde_json::{Value, json};
use sqlx::PgPool;

fn islands() -> IslandAssets {
    IslandAssets {
        entry: Some("/static/islands/assets/main-test.js".into()),
        preload: vec!["/static/islands/assets/shared-test.js".into()],
        css: vec![],
    }
}

struct Setup {
    f: Forum,
    public: i64,
    members: i64,
    ruth: i64,
    ruth_cookie: String,
    jonah: i64,
    jonah_cookie: String,
}

async fn setup(pool: PgPool) -> Setup {
    let mut f = Forum::new(pool).without_rate_limits();
    f.state = f.state.clone().with_islands(islands());
    f.no_new_members().await;
    let public = f.space("games", "public", None, 0).await;
    let members = f.space("lounge", "members", None, 1).await;
    let ruth = f.member("ruth", "user", None).await;
    let jonah = f.member("jonah", "user", None).await;
    Setup {
        ruth_cookie: f.session(ruth).await,
        jonah_cookie: f.session(jonah).await,
        f,
        public,
        members,
        ruth,
        jonah,
    }
}

async fn api(
    f: &Forum,
    method: &str,
    path: &str,
    body: Option<Value>,
    cookie: Option<&str>,
) -> Response {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::ORIGIN, ORIGIN)
        .header(header::ACCEPT, "application/json");
    if let Some(cookie) = cookie {
        request = request.header(header::COOKIE, cookie);
    }
    let body = match body {
        Some(json) => {
            request = request.header(header::CONTENT_TYPE, "application/json");
            Body::from(json.to_string())
        }
        None => Body::empty(),
    };
    f.request(request.body(body).unwrap()).await
}

fn json_of(response: &Response) -> Value {
    serde_json::from_str(&response.body)
        .unwrap_or_else(|_| panic!("not JSON ({}): {}", response.status, response.body))
}

/// The props of the first island of this kind on a page.
fn props(page: &str, island: &str) -> Value {
    let start = page
        .find(&format!("data-island=\"{island}\""))
        .unwrap_or_else(|| panic!("no {island} island"));
    let marker = "<script type=\"application/json\" data-island-props>";
    let at = start + page[start..].find(marker).unwrap() + marker.len();
    let end = at + page[at..].find("</script>").unwrap();
    serde_json::from_str(&page[at..end]).unwrap()
}

#[sqlx::test]
async fn islands_load_only_for_members(pool: PgPool) {
    let s = setup(pool).await;
    let thread =
        s.f.thread(s.public, s.ruth, "Heat", "We played Heat.")
            .await;
    let href = format!("/t/{}/heat", thread.thread_id);
    // Anonymous pages have no scripts at all, and so neither does the
    // anonymous cache.
    for path in [
        "/",
        "/latest",
        "/c/games",
        &href,
        "/search?q=heat",
        "/u/ruth",
        "/login",
    ] {
        let page = s.f.page(path).await;
        assert!(!page.contains("<script"), "{path}");
        assert!(!page.contains("data-island"), "{path}");
    }
    let page = s.f.page_as(&href, &s.jonah_cookie).await;
    assert!(
        page.contains(
            "<script type=\"module\" src=\"/static/islands/assets/main-test.js\"></script>"
        )
    );
    assert!(
        page.contains(
            "<link rel=\"modulepreload\" href=\"/static/islands/assets/shared-test.js\">"
        )
    );
    for island in [
        "composer",
        "reactions",
        "notifications",
        "new-posts",
        "fork-dialog",
    ] {
        assert!(
            page.contains(&format!("data-island=\"{island}\"")),
            "{island}"
        );
    }
    // Without a build there are no islands, and the pages are the same
    // plain pages.
    let mut plain = Forum::new(s.f.db.clone());
    plain.state = plain.state.clone().with_islands(IslandAssets::default());
    let page = plain.page_as(&href, &s.jonah_cookie).await;
    assert!(!page.contains("<script type=\"module\""));
}

#[sqlx::test]
async fn islands_wrap_the_plain_forms(pool: PgPool) {
    let s = setup(pool).await;
    let thread =
        s.f.thread(s.public, s.ruth, "Heat", "We played Heat.")
            .await;
    s.f.exec(&format!(
        "INSERT INTO reactions (post_id, user_id, emoji) VALUES ({}, {}, '👍')",
        thread.first_post_id, s.ruth
    ))
    .await;
    let page =
        s.f.page_as(&format!("/t/{}/heat", thread.thread_id), &s.jonah_cookie)
            .await;
    // The reactions island holds the plain forms, and has the counts.
    let post = post_html(&page, thread.first_post_id);
    assert!(post.contains("data-island=\"reactions\""));
    assert!(post.contains("action=\"/p/"));
    let reactions = props(post, "reactions");
    assert_eq!(reactions["post"], thread.first_post_id);
    assert_eq!(reactions["reactions"][0]["emoji"], "👍");
    assert_eq!(reactions["reactions"][0]["count"], 1);
    assert_eq!(reactions["reactions"][0]["mine"], false);
    assert!(reactions["types"].as_array().unwrap().len() >= 6);
    // The quick reply's composer.
    let composer = props(&page, "composer");
    assert_eq!(composer["id"], "reply-body");
    assert_eq!(composer["quotes"], true);
    // The live bar follows this thread from its last post.
    let bar = props(&page, "new-posts");
    assert_eq!(bar["events"], format!("/events/t/{}", thread.thread_id));
    assert_eq!(bar["lastSeq"], 1);
    // Quote links say which post they quote, for the composer.
    assert!(page.contains(&format!("data-quote=\"{}\"", thread.first_post_id)));
    assert!(page.contains(&format!("data-fork=\"{}\"", thread.first_post_id)));
    // The full composer page.
    let page = s.f.page_as("/c/games/new", &s.jonah_cookie).await;
    let composer = props(&page, "composer");
    assert_eq!(composer["id"], "body");
    assert_eq!(composer["quotes"], false);
}

#[sqlx::test]
async fn island_props_cannot_break_out_of_their_script(pool: PgPool) {
    let s = setup(pool).await;
    let thread = s.f.thread(s.public, s.ruth, "Heat", "Text.").await;
    // Reaction names come from members' display names.
    s.f.exec(&format!(
        "UPDATE users SET display_name = '</script><script>alert(1)</script>' WHERE id = {}",
        s.ruth
    ))
    .await;
    s.f.exec(&format!(
        "INSERT INTO reactions (post_id, user_id, emoji) VALUES ({}, {}, '👍')",
        thread.first_post_id, s.ruth
    ))
    .await;
    let page =
        s.f.page_as(&format!("/t/{}/heat", thread.thread_id), &s.jonah_cookie)
            .await;
    assert!(!page.contains("<script>alert"));
    assert_eq!(
        props(&page, "reactions")["reactions"][0]["names"][0],
        "</script><script>alert(1)</script>"
    );
}

#[sqlx::test]
async fn previews_render_as_posts_do(pool: PgPool) {
    let s = setup(pool).await;
    let response = api(
        &s.f,
        "POST",
        "/api/preview",
        Some(json!({"body": "Hello **@jonah**"})),
        Some(&s.ruth_cookie),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    let html = json_of(&response)["html"].as_str().unwrap().to_owned();
    assert!(html.contains("<strong>"), "{html}");
    assert!(html.contains("href=\"/u/jonah\""), "{html}");
    assert!(
        response
            .header(header::CACHE_CONTROL)
            .unwrap()
            .contains("no-store")
    );
    let response = api(
        &s.f,
        "POST",
        "/api/preview",
        Some(json!({"body": "   "})),
        Some(&s.ruth_cookie),
    )
    .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(
        json_of(&response)["error"]
            .as_str()
            .unwrap()
            .contains("Write something")
    );
    let response = api(
        &s.f,
        "POST",
        "/api/preview",
        Some(json!({"body": "x"})),
        None,
    )
    .await;
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
    // Other sites can't use it as the member.
    let response =
        s.f.request(
            Request::post("/api/preview")
                .header(header::ORIGIN, "https://evil.example")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::COOKIE, &s.ruth_cookie)
                .body(Body::from(r#"{"body":"x"}"#))
                .unwrap(),
        )
        .await;
    assert_eq!(response.status, StatusCode::FORBIDDEN);
}

#[sqlx::test]
async fn post_sources_are_for_those_who_can_read_them(pool: PgPool) {
    let s = setup(pool).await;
    let public =
        s.f.thread(s.public, s.ruth, "Open", "Line one\n\nLine two")
            .await;
    let private =
        s.f.thread(s.members, s.ruth, "Closed", "Members' text")
            .await;
    let response = api(
        &s.f,
        "GET",
        &format!("/api/posts/{}/source", public.first_post_id),
        None,
        Some(&s.jonah_cookie),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    let source = json_of(&response);
    assert_eq!(
        source["quote"],
        "> @ruth wrote:\n> Line one\n>\n> Line two\n\n"
    );
    assert_eq!(source["seq"], 1);
    assert_eq!(source["thread"], public.thread_id);
    // Visitors get nothing, and nobody gets what they can't read.
    let response = api(
        &s.f,
        "GET",
        &format!("/api/posts/{}/source", private.first_post_id),
        None,
        None,
    )
    .await;
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
    assert!(!response.body.contains("Members' text"));
    let kit = s.f.member("kit", "user", None).await;
    let conversation = store::create_conversation(
        &s.f.db,
        &NewConversation {
            author_id: s.ruth,
            title: String::new(),
            recipient_ids: vec![s.jonah],
            body_md: "Just between us.".into(),
            created_at: None,
        },
    )
    .await
    .unwrap();
    let kit_cookie = s.f.session(kit).await;
    for path in [
        format!("/api/posts/{}/source", conversation.first_post_id),
        format!("/api/posts/{}/fork", conversation.first_post_id),
    ] {
        let response = api(&s.f, "GET", &path, None, Some(&kit_cookie)).await;
        assert_eq!(response.status, StatusCode::NOT_FOUND, "{path}");
        assert!(!response.body.contains("Just between us"));
    }
    s.f.exec(&format!(
        "UPDATE posts SET hidden_at = now() WHERE id = {}",
        public.first_post_id
    ))
    .await;
    let response = api(
        &s.f,
        "GET",
        &format!("/api/posts/{}/source", public.first_post_id),
        None,
        Some(&s.jonah_cookie),
    )
    .await;
    assert_eq!(response.status, StatusCode::NOT_FOUND);
    assert!(!response.body.contains("Line one"));
}

#[sqlx::test]
async fn reactions_change_without_a_page(pool: PgPool) {
    let s = setup(pool).await;
    let thread = s.f.thread(s.public, s.ruth, "Heat", "Text.").await;
    let path = format!("/api/posts/{}/reactions", thread.first_post_id);
    // Visitors have the page cached before the change.
    let href = format!("/t/{}/heat", thread.thread_id);
    assert!(!s.f.page(&href).await.contains("reaction-count"));
    let response = api(
        &s.f,
        "POST",
        &path,
        Some(json!({"emoji": "👍", "action": "add"})),
        Some(&s.jonah_cookie),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    let reactions = json_of(&response)["reactions"].clone();
    assert_eq!(reactions[0]["emoji"], "👍");
    assert_eq!(reactions[0]["count"], 1);
    assert_eq!(reactions[0]["mine"], true);
    assert_eq!(reactions[0]["names"][0], "Jonah");
    assert!(s.f.page(&href).await.contains("reaction-count"));
    let response = api(
        &s.f,
        "POST",
        &path,
        Some(json!({"emoji": "👍", "action": "remove"})),
        Some(&s.jonah_cookie),
    )
    .await;
    assert_eq!(json_of(&response)["reactions"], json!([]));
    // Refusals.
    let response = api(
        &s.f,
        "POST",
        &path,
        Some(json!({"emoji": "🦀", "action": "add"})),
        Some(&s.jonah_cookie),
    )
    .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    let response = api(
        &s.f,
        "POST",
        &path,
        Some(json!({"emoji": "👍", "action": "add"})),
        None,
    )
    .await;
    assert_eq!(response.status, StatusCode::UNAUTHORIZED);
    let private = s.f.thread(s.members, s.ruth, "Closed", "Text.").await;
    let response = api(
        &s.f,
        "POST",
        &format!("/api/posts/{}/reactions", private.first_post_id),
        Some(json!({"emoji": "👍", "action": "add"})),
        Some(&s.jonah_cookie),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    s.f.exec(&format!(
        "UPDATE users SET silenced_until = now() + interval '1 day' WHERE id = {}",
        s.jonah
    ))
    .await;
    let response = api(
        &s.f,
        "POST",
        &path,
        Some(json!({"emoji": "👍", "action": "add"})),
        Some(&s.jonah_cookie),
    )
    .await;
    assert_eq!(response.status, StatusCode::FORBIDDEN);
}

#[sqlx::test]
async fn the_fork_dialog_offers_only_allowed_spaces(pool: PgPool) {
    let s = setup(pool).await;
    let public = s.f.thread(s.public, s.ruth, "Open", "Quote *me*.").await;
    let private =
        s.f.thread(s.members, s.ruth, "Closed", "Members' text")
            .await;
    let response = api(
        &s.f,
        "GET",
        &format!("/api/posts/{}/fork", public.first_post_id),
        None,
        Some(&s.jonah_cookie),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    let fork = json_of(&response);
    assert_eq!(fork["action"], format!("/p/{}/fork", public.first_post_id));
    assert!(
        fork["source"]["html"]
            .as_str()
            .unwrap()
            .contains("<em>me</em>")
    );
    let slugs: Vec<&str> = fork["spaces"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["slug"].as_str().unwrap())
        .collect();
    // Jonah may start threads in his own feed too.
    assert_eq!(slugs, vec!["games", "jonah", "lounge"]);
    assert_eq!(fork["spaces"][0]["selected"], true);
    // A members-only post can't be quoted into a public space.
    let response = api(
        &s.f,
        "GET",
        &format!("/api/posts/{}/fork", private.first_post_id),
        None,
        Some(&s.jonah_cookie),
    )
    .await;
    let slugs: Vec<Value> = json_of(&response)["spaces"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["slug"].clone())
        .collect();
    assert_eq!(slugs, vec![json!("jonah"), json!("lounge")]);
}

#[sqlx::test]
async fn the_notifications_menu(pool: PgPool) {
    let s = setup(pool).await;
    let thread = s.f.thread(s.public, s.ruth, "Heat", "Text.").await;
    let reply =
        s.f.reply(
            thread.thread_id,
            s.jonah,
            "A reply.",
            Some(thread.first_post_id),
        )
        .await;
    parlour::notify::post_created(&s.f.state, reply)
        .await
        .unwrap();
    let response = api(
        &s.f,
        "GET",
        "/api/notifications",
        None,
        Some(&s.ruth_cookie),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    let menu = json_of(&response);
    assert_eq!(menu["unread"], 1);
    assert_eq!(menu["items"][0]["actor"], "Jonah");
    assert_eq!(menu["items"][0]["title"], "Heat");
    assert_eq!(menu["items"][0]["read"], false);
    let page = s.f.page_as("/", &s.ruth_cookie).await;
    assert_eq!(props(&page, "notifications")["notifications"], 1);
    let response = api(
        &s.f,
        "POST",
        "/api/notifications/read",
        None,
        Some(&s.ruth_cookie),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK);
    let menu = json_of(
        &api(
            &s.f,
            "GET",
            "/api/notifications",
            None,
            Some(&s.ruth_cookie),
        )
        .await,
    );
    assert_eq!(menu["unread"], 0);
    assert_eq!(menu["items"][0]["read"], true);
    assert_eq!(
        api(&s.f, "GET", "/api/notifications", None, None)
            .await
            .status,
        StatusCode::UNAUTHORIZED
    );
}

#[sqlx::test]
async fn built_island_files_are_served_for_good(pool: PgPool) {
    let f = Forum::new(pool);
    let response = f.get("/static/islands/.vite/manifest.json").await;
    assert_eq!(response.status, StatusCode::NOT_FOUND);
    let response = f.get("/static/islands/nothing-here.js").await;
    assert_eq!(response.status, StatusCode::NOT_FOUND);
}
