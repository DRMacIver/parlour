//! Site settings, stored as JSON in the `settings` table and edited in
//! `/setup` and `/admin/settings`.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sqlx::{PgConnection, PgPool};

pub const DEFAULT_SITE_NAME: &str = "Parlour";

pub async fn get<T: DeserializeOwned>(db: &PgPool, key: &str) -> sqlx::Result<Option<T>> {
    let value: Option<serde_json::Value> =
        sqlx::query_scalar!("SELECT value FROM settings WHERE key = $1", key)
            .fetch_optional(db)
            .await?;
    Ok(value.and_then(|v| serde_json::from_value(v).ok()))
}

pub async fn set<T: Serialize>(db: &mut PgConnection, key: &str, value: &T) -> sqlx::Result<()> {
    let value = serde_json::to_value(value).expect("settings serialise");
    sqlx::query!(
        "INSERT INTO settings (key, value) VALUES ($1, $2)
         ON CONFLICT (key) DO UPDATE SET value = excluded.value, updated_at = now()",
        key,
        value
    )
    .execute(db)
    .await?;
    Ok(())
}

pub async fn site_name(db: &PgPool) -> sqlx::Result<String> {
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

pub async fn registration_mode(db: &PgPool) -> sqlx::Result<RegistrationMode> {
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

pub async fn invite_policy(db: &PgPool) -> sqlx::Result<InvitePolicy> {
    Ok(get(db, "invite_policy").await?.unwrap_or_default())
}
