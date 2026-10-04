//! Full-text search (DESIGN.md §9).
//!
//! Posts are matched on their text and on their thread's title (which
//! finds the thread's first post), with `websearch_to_tsquery`, so quoted
//! phrases, `or` and `-word` work as on web search engines. What may be
//! found is decided in the same query, by the shared visibility condition:
//! `thread_readable` for space threads and, for conversations,
//! `dm_searchable`, which only their current participants pass. Deleted
//! posts and threads are never found, and hidden posts only by staff, as
//! with reading them where they are. Spaces the viewer has muted are left
//! out (DESIGN.md §3.5), unless the search is within one space.

use chrono::{DateTime, Utc};

use crate::content::{UserRef, messages};
use crate::db::Db;
use crate::markdown;
use crate::viewer::Viewer;

/// The longest query searched for, in characters.
pub const MAX_QUERY_CHARS: usize = 200;

/// What to search for.
#[derive(Clone, Debug, Default)]
pub struct SearchQuery {
    pub text: String,
    /// Only in this space (by slug).
    pub space: Option<String>,
    /// Only posts by this member (by username).
    pub author: Option<String>,
    /// Newest first, rather than best match first.
    pub newest_first: bool,
}

/// Where a post that was found is.
#[derive(Clone, Debug)]
pub enum HitPlace {
    Space {
        slug: String,
        name: String,
    },
    /// A conversation, named for this viewer.
    Conversation {
        /// Its title, or without one, who it is with.
        name: String,
        /// The others in it, as "Ruth and Jonah" (empty if nobody else).
        with: String,
    },
}

#[derive(Clone, Debug)]
pub struct SearchHit {
    pub post_id: i64,
    pub seq: i64,
    pub thread_id: i64,
    pub thread_title: String,
    pub place: HitPlace,
    pub author: UserRef,
    pub created_at: DateTime<Utc>,
    /// Whether a moderator hid it (only staff find hidden posts).
    pub hidden: bool,
    /// Part of the post's text with the words found marked: HTML, escaped
    /// here, with `<mark>` around each match.
    pub snippet_html: String,
}

pub enum SearchOutcome {
    /// The query has no words that can be searched for: it is empty, or
    /// every word is too common to index ("the", "and").
    NoTerms,
    Hits {
        hits: Vec<SearchHit>,
        /// Whether there are more after these.
        more: bool,
    },
}

// Marks put around matches by `ts_headline`. They are removed from the
// text first, so that a post can't contain them.
const START: char = '\u{27E6}';
const STOP: char = '\u{27E7}';

/// One page of results: `per_page` hits after skipping `offset`.
pub async fn search(
    db: &Db,
    viewer: &Viewer,
    query: &SearchQuery,
    offset: i64,
    per_page: i64,
) -> sqlx::Result<SearchOutcome> {
    let has_terms = sqlx::query_scalar!(
        r#"SELECT numnode(websearch_to_tsquery('english', $1)) > 0 AS "terms!""#,
        query.text
    )
    .fetch_one(db.pool())
    .await?;
    if !has_terms {
        return Ok(SearchOutcome::NoTerms);
    }
    let filter = viewer.filter();
    let staff = viewer.is_staff();
    let rows = sqlx::query!(
        r#"
        WITH q AS (SELECT websearch_to_tsquery('english', $1) AS q),
        found AS (
            SELECT p.id FROM posts p, q WHERE p.search @@ q.q
            UNION
            SELECT p.id FROM threads t JOIN posts p ON p.thread_id = t.id AND p.seq = 1, q
            WHERE t.search @@ q.q
        ),
        page AS (
            SELECT p.id, p.seq, p.created_at, p.body_md, p.hidden_at IS NOT NULL AS hidden,
                   t.id AS thread_id, t.title, c.slug AS space_slug, c.name AS space_name,
                   a.id AS author_id, a.username AS author_username, a.display_name AS author_display_name,
                   ts_rank_cd(p.search, q.q) + CASE WHEN p.seq = 1 AND t.search @@ q.q THEN 1 ELSE 0 END AS rank
            FROM found
            JOIN posts p ON p.id = found.id
            JOIN threads t ON t.id = p.thread_id
            LEFT JOIN spaces c ON c.id = t.space_id
            JOIN users a ON a.id = p.author_id
            CROSS JOIN q
            WHERE p.deleted_at IS NULL AND (p.hidden_at IS NULL OR $5)
              AND t.deleted_at IS NULL
              AND (thread_readable(t, $2, $3, $4, $12)
                   OR dm_searchable(t.id, t.visibility, $4))
              AND ($6::text IS NULL OR c.slug = $6)
              AND ($7::text IS NULL OR a.username = $7::citext)
              AND ($6::text IS NOT NULL OR $11::bigint IS NULL
                   OR NOT EXISTS (SELECT 1 FROM space_mutes m WHERE m.user_id = $11 AND m.space_id = t.space_id))
            ORDER BY CASE WHEN $8 THEN p.created_at END DESC,
                     rank DESC, p.created_at DESC, p.id DESC
            LIMIT $9 OFFSET $10
        )
        SELECT page.id AS "id!", page.seq AS "seq!", page.created_at AS "created_at!",
               page.hidden AS "hidden!", page.thread_id AS "thread_id!", page.title AS "title!",
               page.space_slug AS "space_slug?", page.space_name AS "space_name?",
               page.author_id AS "author_id!", page.author_username AS "author_username!",
               page.author_display_name AS "author_display_name!",
               page.body_md AS "body_md!"
        FROM page
        ORDER BY CASE WHEN $8 THEN page.created_at END DESC,
                 page.rank DESC, page.created_at DESC, page.id DESC
        "#,
        query.text,
        &filter.visibilities,
        &filter.restricted_space_ids,
        filter.dm_viewer,
        staff,
        query.space,
        query.author as _,
        query.newest_first,
        per_page + 1,
        offset,
        viewer.user_id(),
        &filter.group_ids,
    )
    .fetch_all(db.pool())
    .await?;
    let more = rows.len() as i64 > per_page;
    let rows: Vec<_> = rows.into_iter().take(per_page as usize).collect();
    // Snippets are of what a reader sees, not of the Markdown.
    let texts: Vec<String> = rows
        .iter()
        .map(|row| markdown::plain_text(&row.body_md).replace([START, STOP], ""))
        .collect();
    let snippets = sqlx::query_scalar!(
        r#"SELECT ts_headline('english', u.text, websearch_to_tsquery('english', $1),
                              'StartSel=' || $3 || ', StopSel=' || $4
                              || ', MaxWords=40, MinWords=20, MaxFragments=2, FragmentDelimiter=" … "')
                  AS "snippet!"
           FROM unnest($2::text[]) WITH ORDINALITY AS u(text, n)
           ORDER BY u.n"#,
        query.text,
        &texts,
        START.to_string(),
        STOP.to_string(),
    )
    .fetch_all(db.pool())
    .await?;
    let mut hits = Vec::with_capacity(rows.len());
    for (row, snippet) in rows.into_iter().zip(snippets) {
        let place = match (row.space_slug, row.space_name) {
            (Some(slug), Some(name)) => HitPlace::Space { slug, name },
            _ => {
                let conversation = messages::conversation_target(db, row.thread_id).await?;
                HitPlace::Conversation {
                    name: conversation.name(&row.title, viewer.user_id()),
                    with: conversation.others_names(viewer.user_id()),
                }
            }
        };
        hits.push(SearchHit {
            post_id: row.id,
            seq: i64::from(row.seq),
            thread_id: row.thread_id,
            thread_title: row.title,
            place,
            author: UserRef {
                id: row.author_id,
                username: row.author_username.to_string(),
                display_name: row.author_display_name,
            },
            created_at: row.created_at,
            hidden: row.hidden,
            snippet_html: snippet_html(&snippet),
        });
    }
    Ok(SearchOutcome::Hits { hits, more })
}

/// Escapes a headline and turns its marks into `<mark>` elements, keeping
/// them balanced.
fn snippet_html(headline: &str) -> String {
    let mut out = String::with_capacity(headline.len() + 32);
    let mut open = false;
    for c in headline.chars() {
        match c {
            START if !open => {
                out.push_str("<mark>");
                open = true;
            }
            STOP if open => {
                out.push_str("</mark>");
                open = false;
            }
            START | STOP => {}
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    if open {
        out.push_str("</mark>");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use hegel::TestCase;
    use hegel::generators as gs;

    #[test]
    fn snippets_mark_matches() {
        assert_eq!(
            snippet_html("a \u{27E6}<b>\u{27E7} & c"),
            "a <mark>&lt;b&gt;</mark> &amp; c"
        );
    }

    /// Whatever a headline holds, the snippet has no markup but balanced
    /// `<mark>` elements.
    #[hegel::test]
    fn snippets_are_escaped_and_balanced(tc: TestCase) {
        let text = tc.draw(gs::text().max_size(60));
        let html = snippet_html(&text);
        let bare = html.replace("<mark>", "").replace("</mark>", "");
        assert!(!bare.contains(['<', '>', '"', '\'']));
        let mut depth = 0i32;
        for (i, _) in html.match_indices('<') {
            if html[i..].starts_with("<mark>") {
                depth += 1;
            } else {
                assert!(html[i..].starts_with("</mark>"));
                depth -= 1;
            }
            assert!((0..=1).contains(&depth));
        }
        assert_eq!(depth, 0);
    }
}
