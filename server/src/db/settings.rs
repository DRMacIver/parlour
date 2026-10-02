//! Site settings, stored as JSON in the `settings` table and edited in
//! `/setup` and `/admin/settings`.

use crate::db::{Db, Handle, KEY};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

pub const DEFAULT_SITE_NAME: &str = "Parlour";

pub async fn get<T: DeserializeOwned>(db: &Db, key: &str) -> sqlx::Result<Option<T>> {
    let value: Option<serde_json::Value> =
        sqlx::query_scalar!("SELECT value FROM settings WHERE key = $1", key)
            .fetch_optional(db.pool())
            .await?;
    Ok(value.and_then(|v| serde_json::from_value(v).ok()))
}

pub async fn set<'c, T: Serialize>(db: impl Handle<'c>, key: &str, value: &T) -> sqlx::Result<()> {
    let mut conn = db.open(KEY).acquire().await?;
    let value = serde_json::to_value(value).expect("settings serialise");
    sqlx::query!(
        "INSERT INTO settings (key, value) VALUES ($1, $2)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value, updated_at = now()",
        key,
        value
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Forgets a setting, so that its default applies again.
pub async fn unset<'c>(db: impl Handle<'c>, key: &str) -> sqlx::Result<()> {
    let mut conn = db.open(KEY).acquire().await?;
    sqlx::query!("DELETE FROM settings WHERE key = $1", key)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

pub async fn site_name(db: &Db) -> sqlx::Result<String> {
    Ok(get::<String>(db, "site_name")
        .await?
        .unwrap_or_else(|| DEFAULT_SITE_NAME.to_owned()))
}

/// Who may join (DESIGN.md §7). Open registration modes come later.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegistrationMode {
    /// Anyone with a valid invite.
    #[default]
    InviteOnly,
    /// Nobody; invites can't be used.
    Closed,
}

impl RegistrationMode {
    pub fn as_str(self) -> &'static str {
        match self {
            RegistrationMode::InviteOnly => "invite_only",
            RegistrationMode::Closed => "closed",
        }
    }

    pub fn parse(s: &str) -> Option<RegistrationMode> {
        match s {
            "invite_only" => Some(RegistrationMode::InviteOnly),
            "closed" => Some(RegistrationMode::Closed),
            _ => None,
        }
    }
}

pub async fn registration_mode(db: &Db) -> sqlx::Result<RegistrationMode> {
    Ok(get(db, "registration_mode").await?.unwrap_or_default())
}

/// Who may invite people (DESIGN.md §3.6). Staff always may.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "who", rename_all = "snake_case")]
#[derive(Default)]
pub enum InvitePolicy {
    /// Every member.
    #[default]
    Everyone,
    /// Members whose accounts are at least this many days old.
    MembersOlderThan { days: u32 },
    /// Only moderators and admins.
    Staff,
}

pub async fn invite_policy(db: &Db) -> sqlx::Result<InvitePolicy> {
    Ok(get(db, "invite_policy").await?.unwrap_or_default())
}

/// The site's rules for direct messages (DESIGN.md §3.7, §7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MessageSettings {
    /// Whether members may send direct messages at all. Staff always may.
    pub enabled: bool,
    /// The most people a member may write to in one conversation, not
    /// counting themselves.
    pub max_recipients: u32,
    /// A member is new, and can't start conversations, until their account
    /// is this many days old and they have written this many posts.
    pub new_user_days: u32,
    pub new_user_posts: u32,
}

impl Default for MessageSettings {
    fn default() -> Self {
        MessageSettings {
            enabled: true,
            max_recipients: 10,
            new_user_days: 3,
            new_user_posts: 5,
        }
    }
}

pub async fn message_settings(db: &Db) -> sqlx::Result<MessageSettings> {
    Ok(get(db, "messages").await?.unwrap_or_default())
}

/// When notifications are emailed (DESIGN.md §9).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotificationSettings {
    /// A notification is emailed only if it is still unread this many
    /// minutes after it was made, and its member hasn't been seen since.
    pub email_delay_minutes: u32,
}

impl Default for NotificationSettings {
    fn default() -> Self {
        NotificationSettings {
            email_delay_minutes: 15,
        }
    }
}

pub async fn notification_settings(db: &Db) -> sqlx::Result<NotificationSettings> {
    Ok(get(db, "notifications").await?.unwrap_or_default())
}

/// The site's secret for signing links in emails (such as one-click
/// unsubscribe links), made the first time it is needed.
pub async fn signing_secret(db: &Db) -> sqlx::Result<Vec<u8>> {
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let fresh = serde_json::Value::String(crate::tokens::random_token(32));
    sqlx::query!(
        "INSERT INTO settings (key, value) VALUES ('signing_secret', $1) ON CONFLICT (key) DO NOTHING",
        fresh
    )
    .execute(db.pool())
    .await?;
    let secret: String = get(db, "signing_secret")
        .await?
        .expect("the signing secret was just saved");
    Ok(URL_SAFE_NO_PAD
        .decode(secret.as_bytes())
        .unwrap_or_else(|_| secret.into_bytes()))
}
