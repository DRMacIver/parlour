//! Conversations (direct messages, DESIGN.md §3.7), as the people in them
//! see them. A conversation is a thread with no space whose visibility
//! is `private`; only its current participants can read it, which the
//! shared `dm_readable` SQL function checks. Staff are no exception: a
//! staff member who isn't in one reads it only under a break-glass grant
//! on it, through [`conversation_page`], which records every page of it
//! they are shown (DESIGN.md §3.9, §8).
//!
//! Lists of space threads (the index, `/latest`, space pages,
//! profiles' post counts) use `thread_readable`, which never matches a
//! private thread, so conversations can't appear in them.

use super::*;

pub const CONVERSATIONS_PER_PAGE: i64 = 30;

/// Someone in a conversation.
#[derive(Clone, Debug)]
pub struct Participant {
    pub user: UserRef,
    pub staff: bool,
}

/// The people in a conversation.
#[derive(Clone, Debug)]
pub struct ConversationTarget {
    /// The people in it now, in the order they joined.
    pub participants: Vec<Participant>,
    /// How many people have ever been in it, counting those who left.
    pub ever: i64,
    pub started_by: UserRef,
    /// Whether whoever started it is staff now. A staff message can't be
    /// blocked, and is labelled as one.
    pub staff_message: bool,
}

impl ConversationTarget {
    /// A conversation that started with more than two people. Only these
    /// can have people added: a conversation between two people stays
    /// between them.
    pub fn is_group(&self) -> bool {
        self.ever > 2
    }

    pub fn includes(&self, user_id: i64) -> bool {
        self.participants.iter().any(|p| p.user.id == user_id)
    }

    /// Everyone in it but `me`.
    pub fn others(&self, me: Option<i64>) -> Vec<&Participant> {
        self.participants
            .iter()
            .filter(|p| Some(p.user.id) != me)
            .collect()
    }

    /// Its title, or, without one, the names of the others in it.
    pub fn name(&self, title: &str, me: Option<i64>) -> String {
        if !title.is_empty() {
            return title.to_owned();
        }
        let names = self.others_names(me);
        if names.is_empty() {
            "Just you".to_owned()
        } else {
            names
        }
    }

    /// The names of the others in it, as "Ruth and Jonah": empty if there
    /// is nobody else.
    pub fn others_names(&self, me: Option<i64>) -> String {
        let names: Vec<String> = self
            .others(me)
            .iter()
            .map(|p| p.user.display_name.clone())
            .collect();
        and_list(&names)
    }

    /// Who is in it, as "Ruth, Jonah and you".
    pub fn people(&self, me: Option<i64>) -> String {
        let mut names: Vec<String> = self
            .others(me)
            .iter()
            .map(|p| {
                if p.staff {
                    format!("{} (staff)", p.user.display_name)
                } else {
                    p.user.display_name.clone()
                }
            })
            .collect();
        if me.is_some_and(|me| self.includes(me)) {
            names.push("you".to_owned());
        }
        and_list(&names)
    }
}

/// "a", "a and b", "a, b and c".
fn and_list(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// The people in each of these conversations. The caller has checked that
/// the viewer may read them.
async fn participants_of(
    db: &Db,
    thread_ids: &[i64],
) -> sqlx::Result<HashMap<i64, ConversationTarget>> {
    let rows = sqlx::query!(
        r#"
        SELECT p.thread_id, p.left_at IS NULL AS "current!", u.id, u.username, u.display_name, u.role,
               t.author_id, a.username AS author_username, a.display_name AS author_display_name,
               a.role AS author_role
        FROM private_thread_participants p
        JOIN users u ON u.id = p.user_id
        JOIN threads t ON t.id = p.thread_id
        JOIN users a ON a.id = t.author_id
        WHERE p.thread_id = ANY($1)
        ORDER BY p.thread_id, p.joined_at, u.id
        "#,
        thread_ids
    )
    .fetch_all(db.pool())
    .await?;
    let mut out: HashMap<i64, ConversationTarget> = HashMap::new();
    for row in rows {
        let entry = out
            .entry(row.thread_id)
            .or_insert_with(|| ConversationTarget {
                participants: Vec::new(),
                ever: 0,
                started_by: UserRef {
                    id: row.author_id,
                    username: row.author_username.to_string(),
                    display_name: row.author_display_name.clone(),
                },
                staff_message: row.author_role != "user",
            });
        entry.ever += 1;
        if row.current {
            entry.participants.push(Participant {
                user: UserRef {
                    id: row.id,
                    username: row.username.to_string(),
                    display_name: row.display_name,
                },
                staff: row.role != "user",
            });
        }
    }
    Ok(out)
}

/// The people in a conversation the caller has checked the viewer can read.
pub(super) async fn conversation_target(
    db: &Db,
    thread_id: i64,
) -> sqlx::Result<ConversationTarget> {
    Ok(participants_of(db, &[thread_id])
        .await?
        .remove(&thread_id)
        .expect("a readable conversation has participants"))
}

/// One conversation in the viewer's list.
pub struct ConversationRow {
    pub id: i64,
    pub title: String,
    pub conversation: ConversationTarget,
    pub last_post_at: DateTime<Utc>,
    pub post_count: i64,
    /// Messages the viewer hasn't read.
    pub unread: i64,
    /// The newest message, as "who: what", shortened.
    pub snippet: Option<String>,
}

pub struct ConversationList {
    pub total: i64,
    pub rows: Vec<ConversationRow>,
}

/// How long a snippet of the newest message may be.
const SNIPPET_CHARS: usize = 120;

fn snippet(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= SNIPPET_CHARS {
        flat
    } else {
        let cut: String = flat.chars().take(SNIPPET_CHARS - 1).collect();
        format!("{}…", cut.trim_end())
    }
}

/// The viewer's conversations, most recently active first. Visitors have
/// none.
pub async fn conversations(db: &Db, viewer: &Viewer, page: i64) -> sqlx::Result<ConversationList> {
    let Some(user_id) = viewer.filter().dm_viewer else {
        return Ok(ConversationList {
            total: 0,
            rows: Vec::new(),
        });
    };
    let total = sqlx::query_scalar!(
        r#"SELECT count(*) AS "n!" FROM private_thread_participants me JOIN threads t ON t.id = me.thread_id
           WHERE me.user_id = $1 AND me.left_at IS NULL AND t.deleted_at IS NULL
             AND dm_readable(t.id, t.visibility, $1)"#,
        user_id
    )
    .fetch_one(db.pool())
    .await?;
    let Some(offset) = page_offset(page, CONVERSATIONS_PER_PAGE) else {
        return Ok(ConversationList {
            total,
            rows: Vec::new(),
        });
    };
    let blocked = blocked_authors(viewer);
    let rows = sqlx::query!(
        r#"
        SELECT t.id, t.title, t.last_post_at, t.last_seq,
               t.last_seq - coalesce(tr.last_read_seq, 0) AS "unread!",
               lp.body_md AS "last_body?", lu.id AS "last_user_id?", lu.display_name AS "last_name?"
        FROM private_thread_participants me
        JOIN threads t ON t.id = me.thread_id
        LEFT JOIN thread_reads tr ON tr.thread_id = t.id AND tr.user_id = $1
        LEFT JOIN LATERAL (
            SELECT p.body_md, p.author_id FROM posts p
            WHERE p.thread_id = t.id AND p.deleted_at IS NULL AND p.hidden_at IS NULL
            ORDER BY p.seq DESC LIMIT 1
        ) lp ON true
        LEFT JOIN users lu ON lu.id = lp.author_id
        WHERE me.user_id = $1 AND me.left_at IS NULL AND t.deleted_at IS NULL
          AND dm_readable(t.id, t.visibility, $1)
        ORDER BY t.last_post_at DESC, t.id DESC
        LIMIT $2 OFFSET $3
        "#,
        user_id,
        CONVERSATIONS_PER_PAGE,
        offset,
    )
    .fetch_all(db.pool())
    .await?;
    let ids: Vec<i64> = rows.iter().map(|row| row.id).collect();
    let mut people = participants_of(db, &ids).await?;
    Ok(ConversationList {
        total,
        rows: rows
            .into_iter()
            .filter_map(|row| {
                let conversation = people.remove(&row.id)?;
                let snippet = match (row.last_body, row.last_user_id, row.last_name) {
                    (Some(_), Some(author), Some(_)) if blocked.contains(&author) => {
                        Some("A message from someone you’ve blocked".to_owned())
                    }
                    (Some(body), Some(author), Some(name)) => Some(format!(
                        "{}: {}",
                        if Some(author) == viewer.user_id() {
                            "You"
                        } else {
                            &name
                        },
                        snippet(&body)
                    )),
                    _ => None,
                };
                Some(ConversationRow {
                    id: row.id,
                    title: row.title,
                    conversation,
                    last_post_at: row.last_post_at,
                    post_count: i64::from(row.last_seq),
                    unread: i64::from(row.unread).max(0),
                    snippet,
                })
            })
            .collect(),
    })
}

/// How many of the viewer's conversations have messages they haven't read.
pub async fn unread_conversations(db: &Db, viewer: &Viewer) -> sqlx::Result<i64> {
    let Some(user_id) = viewer.filter().dm_viewer else {
        return Ok(0);
    };
    sqlx::query_scalar!(r#"SELECT unread_conversation_count($1) AS "n!""#, user_id)
        .fetch_one(db.pool())
        .await
}

pub struct ConversationPage {
    pub thread: ThreadRef,
    pub conversation: ConversationTarget,
    pub created_at: DateTime<Utc>,
    pub last_seq: i64,
    pub posts: Vec<PostView>,
    /// How far the viewer had read before this page.
    pub read_marker: Option<i64>,
    /// The break-glass grants on this conversation, which its page names to
    /// everyone in it (DESIGN.md §3.9, §8).
    pub notices: Vec<super::access::PageNotice>,
}

/// One page of a conversation the viewer is in, or `None`. A staff
/// member who isn't in it may read it under a break-glass grant, which
/// records the view (DESIGN.md §3.9, §8).
pub async fn conversation_page(
    db: &Db,
    viewer: &Viewer,
    thread_id: i64,
    page: i64,
    deps: &mut Deps,
) -> sqlx::Result<Option<ConversationPage>> {
    let filter = viewer.filter();
    let Some(user_id) = filter.dm_viewer else {
        return Ok(None);
    };
    let Some(row) = sqlx::query!(
        r#"
        SELECT t.id, t.title, t.slug, t.created_at, t.last_seq,
               (SELECT tr.last_read_seq FROM thread_reads tr
                WHERE tr.thread_id = t.id AND tr.user_id = $2) AS read_marker,
               dm_readable(t.id, t.visibility, $2) AS "readable!",
               EXISTS (SELECT 1 FROM staff_access sa WHERE sa.thread_id = t.id) AS "noticed!"
        FROM threads t
        WHERE t.id = $1 AND t.deleted_at IS NULL AND t.space_id IS NULL
          AND (dm_readable(t.id, t.visibility, $2)
               OR (t.visibility = 'private' AND thread_granted(t, $3, '{}')))
        "#,
        thread_id,
        user_id,
        &filter.granted_thread_ids,
    )
    .fetch_optional(db.pool())
    .await?
    else {
        return Ok(None);
    };
    if !row.readable {
        super::record_granted_view(
            db,
            viewer,
            super::Granted::Thread {
                thread_id: row.id,
                space_id: None,
            },
        )
        .await?;
    }
    deps.add(Tag::Thread(row.id));
    let conversation = conversation_target(db, row.id).await?;
    let posts = posts_on_page(db, viewer, row.id, page, viewer.is_staff(), deps).await?;
    Ok(Some(ConversationPage {
        thread: ThreadRef {
            id: row.id,
            title: row.title,
            slug: row.slug,
        },
        conversation,
        created_at: row.created_at,
        last_seq: i64::from(row.last_seq),
        posts,
        read_marker: row.read_marker.map(i64::from),
        notices: if row.noticed {
            super::access::thread_notices(db, row.id, super::access::What::Conversation).await?
        } else {
            Vec::new()
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_of_names() {
        let names = |n: &[&str]| n.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(and_list(&names(&[])), "");
        assert_eq!(and_list(&names(&["Ruth"])), "Ruth");
        assert_eq!(and_list(&names(&["Ruth", "you"])), "Ruth and you");
        assert_eq!(and_list(&names(&["Dev", "Lin", "you"])), "Dev, Lin and you");
    }

    #[test]
    fn snippets_are_short_and_flat() {
        assert_eq!(snippet("Hello\n\n  there"), "Hello there");
        let long = "word ".repeat(100);
        let cut = snippet(&long);
        assert!(cut.chars().count() <= SNIPPET_CHARS, "{cut}");
        assert!(cut.ends_with('…'));
    }
}
