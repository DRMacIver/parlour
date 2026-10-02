//! The JavaScript islands (DESIGN.md §5): small React components, built
//! from `web/` with Vite into `web/dist`, embedded in the binary and
//! served at `/static/islands/`.
//!
//! Each island mounts on an element with `data-island="name"` that holds a
//! plain HTML form (or link) that works without JavaScript, and reads its
//! props from the `<script type="application/json" data-island-props>`
//! element straight after it. Pages load the islands' script only for
//! logged-in members, so anonymous pages (and the anonymous cache) never
//! carry any JavaScript.
//!
//! Building: `npm run build` in `web/` writes `web/dist`, with a Vite
//! manifest naming the hashed files. Cargo embeds whatever is there when
//! the server is compiled (in debug builds it is read from disk instead),
//! and rebuilds when it changes. Without a build, pages simply have no
//! islands.

use std::sync::LazyLock;

use axum::extract::Path;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};

#[derive(RustEmbed)]
#[folder = "../web/dist/"]
#[allow_missing = true]
struct Built;

/// The files a page loads for its islands.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IslandAssets {
    /// The entry module, as a URL.
    pub entry: Option<String>,
    /// Modules it imports, to preload.
    pub preload: Vec<String>,
    /// Stylesheets the build produced.
    pub css: Vec<String>,
}

#[derive(Deserialize)]
struct ManifestEntry {
    file: String,
    #[serde(default)]
    imports: Vec<String>,
    #[serde(default)]
    css: Vec<String>,
    #[serde(default, rename = "isEntry")]
    is_entry: bool,
}

const BASE: &str = "/static/islands/";

impl IslandAssets {
    /// Reads a Vite manifest (`.vite/manifest.json`). Anything it can't
    /// make sense of means no islands.
    pub fn from_manifest(json: &str) -> IslandAssets {
        let Ok(manifest) =
            serde_json::from_str::<std::collections::HashMap<String, ManifestEntry>>(json)
        else {
            return IslandAssets::default();
        };
        let Some(entry) = manifest.values().find(|e| e.is_entry) else {
            return IslandAssets::default();
        };
        IslandAssets {
            entry: Some(format!("{BASE}{}", entry.file)),
            preload: entry
                .imports
                .iter()
                .filter_map(|key| manifest.get(key))
                .map(|e| format!("{BASE}{}", e.file))
                .collect(),
            css: entry.css.iter().map(|c| format!("{BASE}{c}")).collect(),
        }
    }

    /// The islands built into this binary, if any were.
    pub fn built() -> IslandAssets {
        Built::get(".vite/manifest.json")
            .and_then(|file| String::from_utf8(file.data.into_owned()).ok())
            .map(|json| IslandAssets::from_manifest(&json))
            .unwrap_or_default()
    }
}

/// The islands built into this binary, read once.
static BUILT: LazyLock<IslandAssets> = LazyLock::new(IslandAssets::built);

impl IslandAssets {
    /// The islands to load now. Release builds read the embedded manifest
    /// once; debug builds, which serve `web/dist` from disk, read it every
    /// time, so that `npm run build` takes effect without a restart.
    pub fn current() -> IslandAssets {
        if cfg!(debug_assertions) {
            IslandAssets::built()
        } else {
            BUILT.clone()
        }
    }
}

/// `GET /static/islands/{*path}`: the built files. Their names hold a
/// hash of their contents, so they can be cached for good.
pub async fn serve(Path(path): Path<String>) -> Response {
    match Built::get(&path) {
        Some(file) if !path.starts_with('.') => {
            let mime = mime_guess::from_path(&path).first_or_octet_stream();
            (
                [
                    (header::CONTENT_TYPE, mime.as_ref().to_owned()),
                    (
                        header::CACHE_CONTROL,
                        "public, max-age=31536000, immutable".to_owned(),
                    ),
                ],
                file.data,
            )
                .into_response()
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

/// An island's props as the text of its JSON script element. `<`, `>` and
/// `&` are escaped, so nothing in the props can end the script element or
/// start a comment.
pub fn props(value: &impl Serialize) -> String {
    let json = serde_json::to_string(value).expect("island props serialise");
    let mut out = String::with_capacity(json.len());
    for c in json.chars() {
        match c {
            '<' => out.push_str("\\u003c"),
            '>' => out.push_str("\\u003e"),
            '&' => out.push_str("\\u0026"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            c => out.push(c),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Each island's props

/// The composer around a post's textarea (with this id). With `quotes`,
/// the page's "reply" and "quote" links put their post into it.
pub fn composer_props(id: &str, quotes: bool) -> String {
    props(&serde_json::json!({
        "id": id,
        "quotes": quotes,
        "preview": "/api/preview",
        "upload": "/api/uploads",
    }))
}

/// The reaction buttons on one post.
pub fn reactions_props(
    post_id: &i64,
    reactions: &[crate::content::ReactionCount],
    types: &[crate::content::ReactionType],
) -> String {
    props(&serde_json::json!({
        "post": post_id,
        "reactions": reactions,
        "types": types.iter().map(|t| t.emoji.as_str()).collect::<Vec<_>>(),
        "endpoint": format!("/api/posts/{post_id}/reactions"),
        "who": format!("/p/{post_id}/reactions"),
    }))
}

/// The "N new posts" bar on a thread or conversation.
pub fn new_posts_props(thread_id: &i64, last_seq: &i64) -> String {
    props(&serde_json::json!({
        "events": format!("/events/t/{thread_id}"),
        "lastSeq": last_seq,
    }))
}

/// The notifications menu, with the counts the page was drawn with.
pub fn notifications_props(notifications: &i64, messages: &i64) -> String {
    props(&serde_json::json!({
        "notifications": notifications,
        "messages": messages,
        "events": "/events/notifications",
        "list": "/api/notifications",
        "markRead": "/api/notifications/read",
    }))
}

/// The quote-fork dialog, opened from links marked `data-fork`.
pub fn fork_dialog_props() -> String {
    props(&serde_json::json!({ "endpoint": "/api/posts/{id}/fork" }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hegel::TestCase;
    use hegel::generators as gs;

    #[test]
    fn reads_vite_manifests() {
        let manifest = r#"{
            "src/main.tsx": {"file": "assets/main-abc.js", "isEntry": true, "imports": ["_shared"], "css": ["assets/main-abc.css"]},
            "_shared": {"file": "assets/shared-def.js"},
            "src/islands/Composer.tsx": {"file": "assets/Composer-123.js", "isDynamicEntry": true}
        }"#;
        assert_eq!(
            IslandAssets::from_manifest(manifest),
            IslandAssets {
                entry: Some("/static/islands/assets/main-abc.js".into()),
                preload: vec!["/static/islands/assets/shared-def.js".into()],
                css: vec!["/static/islands/assets/main-abc.css".into()],
            }
        );
        assert_eq!(
            IslandAssets::from_manifest("nonsense"),
            IslandAssets::default()
        );
        assert_eq!(IslandAssets::from_manifest("{}"), IslandAssets::default());
    }

    /// Whatever text goes into props, the script element can't be closed
    /// early, and the JSON reads back the same.
    #[hegel::test(test_cases = 500)]
    fn props_stay_inside_their_script(tc: TestCase) {
        let text = tc.draw(gs::text().max_size(40));
        let out = props(&serde_json::json!({ "text": text }));
        assert!(!out.contains('<') && !out.contains('>') && !out.contains('&'));
        let back: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(back["text"], text);
    }
}
