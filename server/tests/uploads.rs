//! Image uploads (DESIGN.md §2, §9): images in posts and avatars,
//! re-encoded to drop their metadata, limited in size, stored by content
//! hash, and served only to those who may read something they are in.

mod common;

use std::io::Cursor;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use common::{Forum, ORIGIN, Response};
use image::{ImageFormat, RgbImage, RgbaImage};
use parlour::store::{self, NewConversation};
use sqlx::PgPool;

const BOUNDARY: &str = "----parlourtestboundary";
const SECRET: &str = "SECRET-GPS-51.5N";

fn multipart(file_field: &str, filename: &str, content_type: &str, data: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(
        format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{file_field}\"; filename=\"{filename}\"\r\nContent-Type: {content_type}\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(data);
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    body
}

async fn upload(f: &Forum, path: &str, data: &[u8], cookie: Option<&str>) -> Response {
    let mut request = Request::post(path)
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .header(header::ORIGIN, ORIGIN);
    if let Some(cookie) = cookie {
        request = request.header(header::COOKIE, cookie);
    }
    f.request(
        request
            .body(Body::from(multipart(
                "file",
                "photo.jpg",
                "image/jpeg",
                data,
            )))
            .unwrap(),
    )
    .await
}

/// Raw bytes of a response, for images.
async fn get_bytes(
    f: &Forum,
    path: &str,
    cookie: Option<&str>,
) -> (StatusCode, header::HeaderMap, Vec<u8>) {
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let mut request = Request::get(path);
    if let Some(cookie) = cookie {
        request = request.header(header::COOKIE, cookie);
    }
    let response = parlour::app(f.state.clone())
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, headers, bytes.to_vec())
}

fn photo(width: u32, height: u32) -> RgbImage {
    RgbImage::from_fn(width, height, |x, y| {
        image::Rgb([(x * 7 % 256) as u8, (y * 5 % 256) as u8, 120])
    })
}

/// A JPEG with an EXIF segment (saying the picture must be turned a
/// quarter clockwise, and holding a "location") and a comment.
fn jpeg_with_metadata(width: u32, height: u32) -> Vec<u8> {
    let mut plain = Vec::new();
    photo(width, height)
        .write_to(&mut Cursor::new(&mut plain), ImageFormat::Jpeg)
        .unwrap();
    // A big-endian TIFF block with two entries: Orientation = 6, and an
    // ImageDescription holding the secret.
    let mut tiff = vec![0x4D, 0x4D, 0x00, 0x2A, 0, 0, 0, 8, 0, 2];
    tiff.extend_from_slice(&[0x01, 0x12, 0, 3, 0, 0, 0, 1, 0, 6, 0, 0]);
    let description_offset = 8 + 2 + 2 * 12 + 4;
    tiff.extend_from_slice(&[0x01, 0x0E, 0, 2]);
    tiff.extend_from_slice(&((SECRET.len() + 1) as u32).to_be_bytes());
    tiff.extend_from_slice(&(description_offset as u32).to_be_bytes());
    tiff.extend_from_slice(&[0, 0, 0, 0]);
    tiff.extend_from_slice(SECRET.as_bytes());
    tiff.push(0);
    let mut app1 = b"Exif\0\0".to_vec();
    app1.extend_from_slice(&tiff);
    let mut out = vec![0xFF, 0xD8, 0xFF, 0xE1];
    out.extend_from_slice(&((app1.len() + 2) as u16).to_be_bytes());
    out.extend_from_slice(&app1);
    let comment = format!("comment {SECRET}");
    out.extend_from_slice(&[0xFF, 0xFE]);
    out.extend_from_slice(&((comment.len() + 2) as u16).to_be_bytes());
    out.extend_from_slice(comment.as_bytes());
    out.extend_from_slice(&plain[2..]);
    out
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// A PNG with a text chunk holding the secret.
fn png_with_text(width: u32, height: u32) -> Vec<u8> {
    let mut plain = Vec::new();
    RgbaImage::from_fn(width, height, |x, _| {
        image::Rgba([10, 20, 30, (x % 256) as u8])
    })
    .write_to(&mut Cursor::new(&mut plain), ImageFormat::Png)
    .unwrap();
    let iend = plain.len() - 12;
    let mut chunk = b"tEXt".to_vec();
    chunk.extend_from_slice(format!("Comment\0{SECRET}").as_bytes());
    let mut out = plain[..iend].to_vec();
    out.extend_from_slice(&((chunk.len() - 4) as u32).to_be_bytes());
    out.extend_from_slice(&chunk);
    out.extend_from_slice(&crc32(&chunk).to_be_bytes());
    out.extend_from_slice(&plain[iend..]);
    out
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// The `/uploads/…` address in an upload page or JSON answer.
fn upload_url(text: &str) -> String {
    let start = text.find("/uploads/").expect("no upload address");
    let rest = &text[start..];
    let end = rest
        .find(|c: char| c == '"' || c == ')' || c == '<' || c.is_whitespace())
        .unwrap();
    rest[..end].to_owned()
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
    let f = Forum::new(pool).without_rate_limits();
    f.no_new_members().await;
    let public = f.category("games", "public", None, 0).await;
    let members = f.category("lounge", "members", None, 1).await;
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

#[sqlx::test]
async fn uploads_are_re_encoded_without_their_metadata(pool: PgPool) {
    let s = setup(pool).await;
    let original = jpeg_with_metadata(40, 20);
    assert!(contains(&original, SECRET.as_bytes()));
    let page = s.f.page_as("/uploads/new", &s.ruth_cookie).await;
    assert!(page.contains("enctype=\"multipart/form-data\""));
    let response = upload(&s.f, "/uploads/new", &original, Some(&s.ruth_cookie)).await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    let url = upload_url(&response.body);
    assert!(url.ends_with(".jpg"), "{url}");
    // The page gives the Markdown to put in a post.
    assert!(response.body.contains(&format!("![]({url})")));

    let (status, headers, bytes) = get_bytes(&s.f, &url, Some(&s.ruth_cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "image/jpeg");
    assert_eq!(headers["x-content-type-options"], "nosniff");
    assert!(!contains(&bytes, SECRET.as_bytes()));
    assert!(!contains(&bytes, b"Exif"));
    // The orientation was applied before it was dropped: 40×20 turned a
    // quarter is 20×40.
    let decoded = image::load_from_memory(&bytes).unwrap();
    assert_eq!((decoded.width(), decoded.height()), (20, 40));

    // PNG text chunks go too.
    let response = upload(
        &s.f,
        "/uploads/new",
        &png_with_text(30, 30),
        Some(&s.ruth_cookie),
    )
    .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    let url = upload_url(&response.body);
    assert!(url.ends_with(".png"));
    let (_, headers, bytes) = get_bytes(&s.f, &url, Some(&s.ruth_cookie)).await;
    assert_eq!(headers[header::CONTENT_TYPE], "image/png");
    assert!(!contains(&bytes, SECRET.as_bytes()));
    let decoded = image::load_from_memory(&bytes).unwrap();
    assert_eq!(decoded.width(), 30);
}

#[sqlx::test]
async fn uploads_are_stored_once_by_content_hash(pool: PgPool) {
    let s = setup(pool).await;
    let data = png_with_text(16, 16);
    let first = upload_url(
        &upload(&s.f, "/uploads/new", &data, Some(&s.ruth_cookie))
            .await
            .body,
    );
    let again = upload_url(
        &upload(&s.f, "/uploads/new", &data, Some(&s.ruth_cookie))
            .await
            .body,
    );
    let jonahs = upload_url(
        &upload(&s.f, "/uploads/new", &data, Some(&s.jonah_cookie))
            .await
            .body,
    );
    assert_eq!(first, again);
    assert_eq!(first, jonahs);
    let name = first.trim_start_matches("/uploads/");
    let hash = &name[..64];
    assert!(hash.bytes().all(|b| b.is_ascii_hexdigit()));
    let path = s.f.state.config().uploads_dir.join(&hash[..2]).join(name);
    let stored = std::fs::read(&path).unwrap();
    use sha2::Digest;
    assert_eq!(hex(&sha2::Sha256::digest(&stored)), hash);
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[sqlx::test]
async fn images_are_served_to_those_who_can_read_a_post_they_are_in(pool: PgPool) {
    let s = setup(pool).await;
    let url = upload_url(
        &upload(
            &s.f,
            "/uploads/new",
            &png_with_text(8, 8),
            Some(&s.ruth_cookie),
        )
        .await
        .body,
    );
    let kit = s.f.member("kit", "user", None).await;
    let kit_cookie = s.f.session(kit).await;
    // Before it is in any post, only its uploader can fetch it.
    assert_eq!(
        get_bytes(&s.f, &url, Some(&s.ruth_cookie)).await.0,
        StatusCode::OK
    );
    assert_eq!(
        get_bytes(&s.f, &url, Some(&s.jonah_cookie)).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(get_bytes(&s.f, &url, None).await.0, StatusCode::NOT_FOUND);

    // In a members-only thread: members may, visitors may not.
    let lounge =
        s.f.thread(s.members, s.ruth, "Photos", &format!("Look: ![]({url})"))
            .await;
    let (status, headers, _) = get_bytes(&s.f, &url, Some(&s.jonah_cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers[header::CACHE_CONTROL],
        "private, max-age=31536000, immutable"
    );
    let (status, headers, _) = get_bytes(&s.f, &url, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(headers.get(header::SET_COOKIE).is_none());

    // In a public thread too: everyone may, and caches may keep it.
    let public =
        s.f.thread(s.public, s.ruth, "Public photos", &format!("![]({url})"))
            .await;
    let (status, headers, _) = get_bytes(&s.f, &url, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers[header::CACHE_CONTROL],
        "public, max-age=31536000, immutable"
    );
    assert!(headers.get(header::SET_COOKIE).is_none());
    let etag = headers[header::ETAG].to_str().unwrap().to_owned();
    let response =
        s.f.request(
            Request::get(&url)
                .header(header::IF_NONE_MATCH, &etag)
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(response.status, StatusCode::NOT_MODIFIED);

    // Deleting the public post takes it back to members only.
    s.f.exec(&format!(
        "UPDATE posts SET deleted_at = now() WHERE id = {}",
        public.first_post_id
    ))
    .await;
    assert_eq!(get_bytes(&s.f, &url, None).await.0, StatusCode::NOT_FOUND);
    // And editing it out of the members-only post leaves only its uploader.
    store::edit_post(&s.f.db, lounge.first_post_id, s.ruth, "No photo after all.")
        .await
        .unwrap();
    assert_eq!(
        get_bytes(&s.f, &url, Some(&kit_cookie)).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get_bytes(&s.f, &url, Some(&s.ruth_cookie)).await.0,
        StatusCode::OK
    );
    // Names that aren't uploads are simply not found.
    for path in [
        "/uploads/nothing.png",
        &format!("/uploads/{}.png", "0".repeat(64)),
        &url.replace(".png", ".jpg"),
        &url.to_uppercase(),
    ] {
        assert_eq!(
            get_bytes(&s.f, path, Some(&s.ruth_cookie)).await.0,
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }
}

#[sqlx::test]
async fn images_in_conversations_are_for_the_people_in_them(pool: PgPool) {
    let s = setup(pool).await;
    let kit = s.f.member("kit", "user", None).await;
    let kit_cookie = s.f.session(kit).await;
    let url = upload_url(
        &upload(
            &s.f,
            "/uploads/new",
            &png_with_text(9, 9),
            Some(&s.ruth_cookie),
        )
        .await
        .body,
    );
    store::create_conversation(
        &s.f.db,
        &NewConversation {
            author_id: s.ruth,
            title: String::new(),
            recipient_ids: vec![s.jonah],
            body_md: format!("![]({url})"),
            created_at: None,
        },
    )
    .await
    .unwrap();
    let (status, headers, _) = get_bytes(&s.f, &url, Some(&s.jonah_cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers[header::CACHE_CONTROL],
        "private, max-age=31536000, immutable"
    );
    assert_eq!(
        get_bytes(&s.f, &url, Some(&kit_cookie)).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(get_bytes(&s.f, &url, None).await.0, StatusCode::NOT_FOUND);
}

#[sqlx::test]
async fn uploads_are_limited(pool: PgPool) {
    let s = setup(pool).await;
    // Not an image.
    let response = upload(
        &s.f,
        "/uploads/new",
        b"#!/bin/sh\necho hi\n",
        Some(&s.ruth_cookie),
    )
    .await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(response.body.contains("PNG, JPEG, GIF or WebP"));
    // Too wide.
    let mut wide = Vec::new();
    RgbImage::new(8001, 1)
        .write_to(&mut Cursor::new(&mut wide), ImageFormat::Png)
        .unwrap();
    let response = upload(&s.f, "/uploads/new", &wide, Some(&s.ruth_cookie)).await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    assert!(response.body.contains("8,000"), "{}", response.body);
    // Too big a file: the body is cut off before it is all read.
    let big = vec![0u8; parlour::uploads::MAX_UPLOAD_BYTES + 1];
    let response = upload(&s.f, "/uploads/new", &big, Some(&s.ruth_cookie)).await;
    assert_eq!(response.status, StatusCode::PAYLOAD_TOO_LARGE);
    // Truncated image data.
    let mut cut = png_with_text(50, 50);
    cut.truncate(cut.len() / 2);
    let response = upload(&s.f, "/uploads/new", &cut, Some(&s.ruth_cookie)).await;
    assert_eq!(response.status, StatusCode::BAD_REQUEST);
    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM uploads")
        .fetch_one(&s.f.db)
        .await
        .unwrap();
    assert_eq!(stored, 0);
}

#[sqlx::test]
async fn visitors_and_silenced_members_cannot_upload(pool: PgPool) {
    let s = setup(pool).await;
    let data = png_with_text(4, 4);
    let response = upload(&s.f, "/uploads/new", &data, None).await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert!(response.location().starts_with("/login"));
    s.f.exec(&format!(
        "UPDATE users SET silenced_until = now() + interval '1 day' WHERE id = {}",
        s.ruth
    ))
    .await;
    let response = upload(&s.f, "/uploads/new", &data, Some(&s.ruth_cookie)).await;
    assert_eq!(response.status, StatusCode::FORBIDDEN);
    // Uploads from other sites are refused like any other form.
    let response =
        s.f.request(
            Request::post("/uploads/new")
                .header(
                    header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={BOUNDARY}"),
                )
                .header(header::ORIGIN, "https://evil.example")
                .header(header::COOKIE, &s.jonah_cookie)
                .body(Body::from(multipart("file", "a.png", "image/png", &data)))
                .unwrap(),
        )
        .await;
    assert_eq!(response.status, StatusCode::FORBIDDEN);
}

#[sqlx::test]
async fn members_choose_an_avatar(pool: PgPool) {
    let s = setup(pool).await;
    let thread = s.f.thread(s.public, s.ruth, "Hello", "Hi all.").await;
    let href = format!("/t/{}/hello", thread.thread_id);
    // Visitors have the page cached before the change.
    assert!(!s.f.page(&href).await.contains("<img class=\"avatar"));
    let settings = s.f.page_as("/settings", &s.ruth_cookie).await;
    assert!(settings.contains("action=\"/settings/avatar\""));
    let response = upload(
        &s.f,
        "/settings/avatar",
        &jpeg_with_metadata(600, 300),
        Some(&s.ruth_cookie),
    )
    .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER, "{}", response.body);
    assert_eq!(response.location(), "/settings?done=avatar");

    let page = s.f.page(&href).await;
    let post = common::post_html(&page, thread.first_post_id);
    let url = upload_url(post);
    assert!(post.contains("<img class=\"avatar"), "{post}");
    // Avatars are square and small, and anyone may fetch them.
    let (status, headers, bytes) = get_bytes(&s.f, &url, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers[header::CACHE_CONTROL],
        "public, max-age=31536000, immutable"
    );
    let decoded = image::load_from_memory(&bytes).unwrap();
    assert_eq!((decoded.width(), decoded.height()), (256, 256));
    assert!(!contains(&bytes, SECRET.as_bytes()));
    assert!(s.f.page("/u/ruth").await.contains(&url));

    // Taking it away.
    let response =
        s.f.submit("/settings/avatar/remove", &[], Some(&s.ruth_cookie))
            .await;
    assert_eq!(response.status, StatusCode::SEE_OTHER);
    assert!(!s.f.page(&href).await.contains(&url));
    assert_eq!(get_bytes(&s.f, &url, None).await.0, StatusCode::NOT_FOUND);
    // The uploader can still fetch it.
    assert_eq!(
        get_bytes(&s.f, &url, Some(&s.ruth_cookie)).await.0,
        StatusCode::OK
    );
}

/// Moderators reviewing a post held for approval see its images, which
/// nobody else does until it is approved.
#[sqlx::test]
async fn staff_see_the_images_in_held_posts(pool: PgPool) {
    let s = setup(pool).await;
    let sam = s.f.member("sam", "moderator", None).await;
    let sam_cookie = s.f.session(sam).await;
    let url = upload_url(
        &upload(
            &s.f,
            "/uploads/new",
            &png_with_text(7, 7),
            Some(&s.jonah_cookie),
        )
        .await
        .body,
    );
    let thread = s.f.thread(s.public, s.ruth, "Photos", "Show us.").await;
    let held = parlour::mod_records::hold(
        &s.f.db,
        s.jonah,
        &parlour::mod_records::HeldPlace::Reply {
            thread_id: thread.thread_id,
            reply_to_post_id: None,
        },
        &format!("Here: ![]({url})"),
    )
    .await
    .unwrap();
    // It is on the page the moderator reviews it on.
    let page = s.f.page_as(&format!("/held/{held}"), &sam_cookie).await;
    assert!(page.contains(&url), "{page}");
    let (status, headers, _) = get_bytes(&s.f, &url, Some(&sam_cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        headers[header::CACHE_CONTROL]
            .to_str()
            .unwrap()
            .starts_with("private")
    );
    // Nobody else gets it from the held post.
    assert_eq!(
        get_bytes(&s.f, &url, Some(&s.ruth_cookie)).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(get_bytes(&s.f, &url, None).await.0, StatusCode::NOT_FOUND);
}

/// An image only in an earlier version of a post may be fetched by whoever
/// may read that version on the post's edit history, until a moderator
/// redacts the version.
#[sqlx::test]
async fn images_in_earlier_versions_follow_the_history(pool: PgPool) {
    let s = setup(pool).await;
    let sam = s.f.member("sam", "moderator", None).await;
    let url = upload_url(
        &upload(
            &s.f,
            "/uploads/new",
            &png_with_text(6, 6),
            Some(&s.ruth_cookie),
        )
        .await
        .body,
    );
    let thread =
        s.f.thread(s.public, s.ruth, "Photos", &format!("Look: ![]({url})"))
            .await;
    // Past the grace period, an edit keeps the old version.
    s.f.exec(&format!(
        "UPDATE posts SET created_at = now() - interval '1 hour' WHERE id = {}",
        thread.first_post_id
    ))
    .await;
    store::edit_post(&s.f.db, thread.first_post_id, s.ruth, "No photo after all.")
        .await
        .unwrap();
    let history =
        s.f.page_as(
            &format!("/p/{}/revisions?v=1", thread.first_post_id),
            &s.jonah_cookie,
        )
        .await;
    assert!(history.contains(&url), "{history}");
    assert_eq!(
        get_bytes(&s.f, &url, Some(&s.jonah_cookie)).await.0,
        StatusCode::OK
    );
    assert_eq!(get_bytes(&s.f, &url, None).await.0, StatusCode::OK);
    parlour::mod_records::redact_revision(&s.f.db, thread.first_post_id, 1, sam)
        .await
        .unwrap();
    assert_eq!(
        get_bytes(&s.f, &url, Some(&s.jonah_cookie)).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(get_bytes(&s.f, &url, None).await.0, StatusCode::NOT_FOUND);
}
