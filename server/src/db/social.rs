//! How members relate to each other and to what they read: blocks, who
//! may message them, what they watch, and which emails they want
//! (DESIGN.md §3.7, §9).

use serde::{Deserialize, Serialize};

use crate::accounts::Role;
use crate::content::UserRef;
use crate::db::{Db, Handle, KEY};

// ---------------------------------------------------------------------------
// Members by name

/// A member, as someone to send a message to.
#[derive(Clone, Debug)]
pub struct Recipient {
    pub user: UserRef,
    pub role: Role,
    pub dm_policy: DmPolicy,
}

/// The members with these usernames (in any case), skipping names that
/// don't belong to anyone. Only exact names are looked up: there is no
/// searching for members.
pub async fn members_named(db: &Db, usernames: &[String]) -> sqlx::Result<Vec<Recipient>> {
    let lower: Vec<String> = usernames.iter().map(|n| n.to_lowercase()).collect();
    let rows = sqlx::query!(
        r#"SELECT id, username, display_name, role, dm_policy FROM users
           WHERE lower(username::text) = ANY($1) AND deleted_at IS NULL
           ORDER BY id"#,
        &lower
    )
    .fetch_all(db.pool())
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| Recipient {
            user: UserRef {
                id: row.id,
                username: row.username.to_string(),
                display_name: row.display_name,
            },
            role: Role::parse(&row.role).unwrap_or(Role::User),
            dm_policy: DmPolicy::parse(&row.dm_policy).unwrap_or_default(),
        })
        .collect())
}

/// How long a member has been here and how much they have written in
/// categories, for the new-member limits (DESIGN.md §7).
pub struct Standing {
    pub days: i64,
    pub posts: i64,
}

pub async fn standing(db: &Db, user_id: i64) -> sqlx::Result<Standing> {
    let row = sqlx::query!(
        r#"SELECT extract(epoch FROM now() - u.created_at)::bigint / 86400 AS "days!",
                  (SELECT count(*) FROM posts p JOIN threads t ON t.id = p.thread_id
                   WHERE p.author_id = u.id AND t.category_id IS NOT NULL
                     AND p.deleted_at IS NULL AND p.hidden_at IS NULL) AS "posts!"
           FROM users u WHERE u.id = $1"#,
        user_id
    )
    .fetch_one(db.pool())
    .await?;
    Ok(Standing {
        days: row.days,
        posts: row.posts,
    })
}

// ---------------------------------------------------------------------------
// Who may message a member

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DmPolicy {
    #[default]
    Everyone,
    /// Only people the member has interacted with: see [`interacted`].
    Interacted,
    Nobody,
}

impl DmPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            DmPolicy::Everyone => "everyone",
            DmPolicy::Interacted => "interacted",
            DmPolicy::Nobody => "nobody",
        }
    }

    pub fn parse(s: &str) -> Option<DmPolicy> {
        match s {
            "everyone" => Some(DmPolicy::Everyone),
            "interacted" => Some(DmPolicy::Interacted),
            "nobody" => Some(DmPolicy::Nobody),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            DmPolicy::Everyone => "everyone",
            DmPolicy::Interacted => "people you’ve interacted with",
            DmPolicy::Nobody => "nobody but staff",
        }
    }
}

pub async fn dm_policy(db: &Db, user_id: i64) -> sqlx::Result<DmPolicy> {
    let policy = sqlx::query_scalar!("SELECT dm_policy FROM users WHERE id = $1", user_id)
        .fetch_one(db.pool())
        .await?;
    Ok(DmPolicy::parse(&policy).unwrap_or_default())
}

pub async fn set_dm_policy(db: &Db, user_id: i64, policy: DmPolicy) -> sqlx::Result<()> {
    sqlx::query!(
        "UPDATE users SET dm_policy = $2 WHERE id = $1",
        user_id,
        policy.as_str()
    )
    .execute(db.pool())
    .await?;
    Ok(())
}

/// Whether `member` has interacted with `other`, for the "people I've
/// interacted with" setting. Only what `member` did counts, so nobody can
/// make themselves count by replying to a member: `member` replied to or
/// reacted to one of `other`'s posts, wrote in a conversation `other` was
/// in, or one of them invited the other.
pub async fn interacted(db: &Db, member: i64, other: i64) -> sqlx::Result<bool> {
    sqlx::query_scalar!(
        r#"SELECT EXISTS (
               SELECT 1 FROM posts mine JOIN posts theirs ON theirs.id = mine.reply_to_post_id
               WHERE mine.author_id = $1 AND theirs.author_id = $2
           ) OR EXISTS (
               SELECT 1 FROM reactions r JOIN posts theirs ON theirs.id = r.post_id
               WHERE r.user_id = $1 AND theirs.author_id = $2
           ) OR EXISTS (
               SELECT 1 FROM posts mine
               JOIN private_thread_participants them ON them.thread_id = mine.thread_id AND them.user_id = $2
               WHERE mine.author_id = $1
           ) OR EXISTS (
               SELECT 1 FROM users u
               WHERE (u.id = $1 AND u.invited_by_user_id = $2) OR (u.id = $2 AND u.invited_by_user_id = $1)
           ) AS "interacted!""#,
        member,
        other
    )
    .fetch_one(db.pool())
    .await
}

// ---------------------------------------------------------------------------
// Blocks

/// Records that `user_id` blocks `blocked`. Returns whether it is new.
pub async fn block(db: &Db, user_id: i64, blocked: i64) -> sqlx::Result<bool> {
    let done = sqlx::query!(
        "INSERT INTO blocks (user_id, blocked_user_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
        user_id,
        blocked
    )
    .execute(db.pool())
    .await?;
    Ok(done.rows_affected() == 1)
}

pub async fn unblock(db: &Db, user_id: i64, blocked: i64) -> sqlx::Result<bool> {
    let done = sqlx::query!(
        "DELETE FROM blocks WHERE user_id = $1 AND blocked_user_id = $2",
        user_id,
        blocked
    )
    .execute(db.pool())
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Whether `user_id` has blocked `other`.
pub async fn has_blocked(db: &Db, user_id: i64, other: i64) -> sqlx::Result<bool> {
    sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM blocks WHERE user_id = $1 AND blocked_user_id = $2) AS "b!""#,
        user_id,
        other
    )
    .fetch_one(db.pool())
    .await
}

/// Whether `user_id` was in a conversation and left it.
pub async fn left_conversation(db: &Db, thread_id: i64, user_id: i64) -> sqlx::Result<bool> {
    sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM private_thread_participants
                          WHERE thread_id = $1 AND user_id = $2 AND left_at IS NOT NULL) AS "left!""#,
        thread_id,
        user_id
    )
    .fetch_one(db.pool())
    .await
}

/// Whether a block stands between `user_id` and anyone in a conversation
/// now, either way. Blocks on staff don't count: staff can't be blocked.
pub async fn blocked_in_conversation(db: &Db, thread_id: i64, user_id: i64) -> sqlx::Result<bool> {
    sqlx::query_scalar!(
        r#"SELECT EXISTS (
               SELECT 1 FROM private_thread_participants p
               JOIN users them ON them.id = p.user_id
               JOIN users me ON me.id = $2
               JOIN blocks b ON (b.user_id = me.id AND b.blocked_user_id = them.id AND them.role = 'user')
                             OR (b.user_id = them.id AND b.blocked_user_id = me.id AND me.role = 'user')
               WHERE p.thread_id = $1 AND p.left_at IS NULL AND p.user_id <> $2
           ) AS "blocked!""#,
        thread_id,
        user_id
    )
    .fetch_one(db.pool())
    .await
}

/// The members `user_id` has blocked, by name.
pub async fn blocked_members(db: &Db, user_id: i64) -> sqlx::Result<Vec<UserRef>> {
    let rows = sqlx::query!(
        r#"SELECT u.id, u.username, u.display_name FROM blocks b JOIN users u ON u.id = b.blocked_user_id
           WHERE b.user_id = $1 ORDER BY lower(u.username::text)"#,
        user_id
    )
    .fetch_all(db.pool())
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| UserRef {
            id: row.id,
            username: row.username.to_string(),
            display_name: row.display_name,
        })
        .collect())
}

// ---------------------------------------------------------------------------
// Watching

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WatchLevel {
    /// Notify about every new post (in a thread) or thread (in a category).
    Watching,
    /// Notify about replies and mentions only.
    #[default]
    Normal,
    /// Never notify.
    Muted,
}

impl WatchLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            WatchLevel::Watching => "watching",
            WatchLevel::Normal => "normal",
            WatchLevel::Muted => "muted",
        }
    }

    /// The level a stored choice means: the default if there is none.
    pub fn from_stored(level: Option<&str>) -> WatchLevel {
        level.and_then(WatchLevel::parse).unwrap_or_default()
    }

    pub fn parse(s: &str) -> Option<WatchLevel> {
        match s {
            "watching" => Some(WatchLevel::Watching),
            "normal" => Some(WatchLevel::Normal),
            "muted" => Some(WatchLevel::Muted),
            _ => None,
        }
    }
}

/// What a watch is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Watched {
    Thread(i64),
    Category(i64),
}

pub async fn set_watch<'c>(
    db: impl Handle<'c>,
    user_id: i64,
    on: Watched,
    level: WatchLevel,
) -> sqlx::Result<()> {
    let mut tx = db.open(KEY).begin().await?;
    let (thread_id, category_id) = match on {
        Watched::Thread(id) => (Some(id), None),
        Watched::Category(id) => (None, Some(id)),
    };
    sqlx::query!(
        "DELETE FROM watches WHERE user_id = $1 AND thread_id IS NOT DISTINCT FROM $2
         AND category_id IS NOT DISTINCT FROM $3",
        user_id,
        thread_id,
        category_id
    )
    .execute(&mut *tx)
    .await?;
    if level != WatchLevel::Normal {
        sqlx::query!(
            "INSERT INTO watches (user_id, thread_id, category_id, level) VALUES ($1, $2, $3, $4)",
            user_id,
            thread_id,
            category_id,
            level.as_str()
        )
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await
}

pub async fn watch_level(db: &Db, user_id: i64, on: Watched) -> sqlx::Result<WatchLevel> {
    let (thread_id, category_id) = match on {
        Watched::Thread(id) => (Some(id), None),
        Watched::Category(id) => (None, Some(id)),
    };
    let level = sqlx::query_scalar!(
        "SELECT level FROM watches WHERE user_id = $1 AND thread_id IS NOT DISTINCT FROM $2
         AND category_id IS NOT DISTINCT FROM $3",
        user_id,
        thread_id,
        category_id
    )
    .fetch_optional(db.pool())
    .await?;
    Ok(WatchLevel::from_stored(level.as_deref()))
}

// ---------------------------------------------------------------------------
// Email preferences

/// Which notifications a member wants emailed when they are away. Stored
/// in `users.email_prefs`; anything missing is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EmailPrefs {
    pub replies: bool,
    pub mentions: bool,
    pub messages: bool,
    /// New posts in watched threads, and new threads in watched categories.
    pub watching: bool,
    pub moderation: bool,
}

impl Default for EmailPrefs {
    fn default() -> Self {
        EmailPrefs {
            replies: true,
            mentions: true,
            messages: true,
            watching: true,
            moderation: true,
        }
    }
}

/// A kind of email a member can turn off, as named in unsubscribe links.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmailTopic {
    Replies,
    Mentions,
    Messages,
    Watching,
    Moderation,
    /// Every notification email.
    All,
}

impl EmailTopic {
    pub const ALL: [EmailTopic; 6] = [
        EmailTopic::Replies,
        EmailTopic::Mentions,
        EmailTopic::Messages,
        EmailTopic::Watching,
        EmailTopic::Moderation,
        EmailTopic::All,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            EmailTopic::Replies => "replies",
            EmailTopic::Mentions => "mentions",
            EmailTopic::Messages => "messages",
            EmailTopic::Watching => "watching",
            EmailTopic::Moderation => "moderation",
            EmailTopic::All => "all",
        }
    }

    pub fn parse(s: &str) -> Option<EmailTopic> {
        EmailTopic::ALL.into_iter().find(|t| t.as_str() == s)
    }

    /// What it covers, as in "emails about …".
    pub fn describe(self) -> &'static str {
        match self {
            EmailTopic::Replies => "replies to your posts",
            EmailTopic::Mentions => "mentions of you",
            EmailTopic::Messages => "new messages",
            EmailTopic::Watching => "threads and categories you watch",
            EmailTopic::Moderation => "notes from the moderators",
            EmailTopic::All => "anything",
        }
    }
}

impl EmailPrefs {
    pub fn wants(&self, topic: EmailTopic) -> bool {
        match topic {
            EmailTopic::Replies => self.replies,
            EmailTopic::Mentions => self.mentions,
            EmailTopic::Messages => self.messages,
            EmailTopic::Watching => self.watching,
            EmailTopic::Moderation => self.moderation,
            EmailTopic::All => self.replies || self.mentions || self.messages || self.watching,
        }
    }

    /// These preferences with `topic` turned off.
    pub fn without(mut self, topic: EmailTopic) -> EmailPrefs {
        match topic {
            EmailTopic::Replies => self.replies = false,
            EmailTopic::Mentions => self.mentions = false,
            EmailTopic::Messages => self.messages = false,
            EmailTopic::Watching => self.watching = false,
            EmailTopic::Moderation => self.moderation = false,
            EmailTopic::All => {
                self = EmailPrefs {
                    replies: false,
                    mentions: false,
                    messages: false,
                    watching: false,
                    moderation: false,
                }
            }
        }
        self
    }
}

pub async fn email_prefs(db: &Db, user_id: i64) -> sqlx::Result<EmailPrefs> {
    let value = sqlx::query_scalar!("SELECT email_prefs FROM users WHERE id = $1", user_id)
        .fetch_one(db.pool())
        .await?;
    Ok(serde_json::from_value(value).unwrap_or_default())
}

pub async fn set_email_prefs(db: &Db, user_id: i64, prefs: &EmailPrefs) -> sqlx::Result<()> {
    let value = serde_json::to_value(prefs).expect("preferences serialise");
    sqlx::query!(
        "UPDATE users SET email_prefs = $2 WHERE id = $1",
        user_id,
        value
    )
    .execute(db.pool())
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for policy in [DmPolicy::Everyone, DmPolicy::Interacted, DmPolicy::Nobody] {
            assert_eq!(DmPolicy::parse(policy.as_str()), Some(policy));
        }
        for level in [WatchLevel::Watching, WatchLevel::Normal, WatchLevel::Muted] {
            assert_eq!(WatchLevel::parse(level.as_str()), Some(level));
        }
        for topic in EmailTopic::ALL {
            assert_eq!(EmailTopic::parse(topic.as_str()), Some(topic));
        }
        assert_eq!(DmPolicy::parse("anyone"), None);
    }

    #[test]
    fn missing_preferences_are_on() {
        let prefs: EmailPrefs = serde_json::from_str(r#"{"replies": false}"#).unwrap();
        assert!(!prefs.replies && prefs.mentions && prefs.messages && prefs.watching);
        for topic in EmailTopic::ALL {
            let off = EmailPrefs::default().without(topic);
            assert!(!off.wants(topic), "{topic:?}");
            if topic != EmailTopic::All {
                assert!(off.wants(EmailTopic::All) || topic == EmailTopic::Moderation);
            }
        }
    }
}
