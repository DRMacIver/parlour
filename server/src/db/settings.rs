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
    Ok(site_name_from(get::<String>(db, "site_name").await?))
}

/// The site's name, given the stored `site_name` setting if there is one.
pub fn site_name_from(stored: Option<String>) -> String {
    stored.unwrap_or_else(|| DEFAULT_SITE_NAME.to_owned())
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

/// Who may make spaces (DESIGN.md §3.1).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpaceCreation {
    #[default]
    Members,
    Staff,
}

impl SpaceCreation {
    pub fn as_str(self) -> &'static str {
        match self {
            SpaceCreation::Members => "members",
            SpaceCreation::Staff => "staff",
        }
    }

    pub fn parse(s: &str) -> Option<SpaceCreation> {
        match s {
            "members" => Some(SpaceCreation::Members),
            "staff" => Some(SpaceCreation::Staff),
            _ => None,
        }
    }
}

pub async fn space_creation(db: &Db) -> sqlx::Result<SpaceCreation> {
    Ok(get(db, "space_creation").await?.unwrap_or_default())
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

/// The moderation settings (DESIGN.md §7, §8).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModerationSettings {
    /// A new member's posts are held for a moderator to approve until this
    /// many of their posts have been published (0 turns holding off). Who
    /// is new is set by [`MessageSettings::new_user_days`] and
    /// [`MessageSettings::new_user_posts`].
    pub hold_posts: u32,
    /// How many days the addresses members use are kept.
    pub ip_retention_days: u32,
}

impl Default for ModerationSettings {
    fn default() -> Self {
        ModerationSettings {
            hold_posts: 2,
            ip_retention_days: 90,
        }
    }
}

pub async fn moderation_settings(db: &Db) -> sqlx::Result<ModerationSettings> {
    Ok(get(db, "moderation").await?.unwrap_or_default())
}

/// The site's terms and privacy pages, as the admin wrote them in Markdown
/// and as rendered when saved.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SitePages {
    pub terms_md: String,
    pub terms_html: String,
    pub privacy_md: String,
    pub privacy_html: String,
}

/// What the privacy page says until an admin writes their own, and what
/// its form starts with: what Parlour does about staff reading private
/// content, and what it can't do (DESIGN.md §8). The admin may change it.
pub const DEFAULT_PRIVACY: &str = "\
## Moderators and private content

Moderators and admins read only what their own groups and lists let them read, like everyone else. \
If one of them needs to read a private thread, space or conversation beyond that, to deal with a report \
for instance, they have to say why. Everyone who can read it is told at once, by a notification and \
an email, and its page says so from then on. The reason goes in a log that can't be changed, which \
admins can read, and you can see every time it touched something you could read under \
[Moderators who looked](/settings/access). How many times it has happened each month is shown on the \
[moderation log](/moderation).

## Whoever runs the server

None of that covers whoever runs the server this forum is on. They can read its database, private \
conversations included. Parlour keeps the moderators apart from that person, but it can't keep that \
person out.
";

impl SitePages {
    /// The pages as they are before an admin has saved any: no terms, and
    /// the default privacy page.
    pub fn starting() -> SitePages {
        SitePages {
            terms_md: String::new(),
            terms_html: String::new(),
            privacy_md: DEFAULT_PRIVACY.to_owned(),
            privacy_html: crate::markdown::render(DEFAULT_PRIVACY),
        }
    }
}

pub async fn site_pages(db: &Db) -> sqlx::Result<SitePages> {
    Ok(get(db, "pages").await?.unwrap_or_else(SitePages::starting))
}
