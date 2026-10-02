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
