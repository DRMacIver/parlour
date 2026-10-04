//! Break-glass grants (DESIGN.md §8): the `staff_access` log and the
//! `staff_access_views` recorded under each grant. Both are append-only
//! (migration 0032): nothing here, or anywhere, changes or removes a row.
//!
//! Opening a grant is all this step of milestone 15 does; the confirmation
//! page that asks for the reason, and the notices to everyone who can read
//! what it opens, come with the pages that use it. Reads under a grant go
//! through [`crate::content`], which records each page shown.

use chrono::{DateTime, Utc};

use crate::db::{Handle, KEY};

/// How long a grant lasts (DESIGN.md §8).
pub const HOURS: i64 = 72;

/// What a grant opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// One thread, or one conversation.
    Thread(i64),
    /// A whole space, every thread in it included.
    Space(i64),
}

pub struct NewAccess<'a> {
    pub actor_id: i64,
    pub scope: Scope,
    /// The report that led to it, if one did.
    pub report_id: Option<i64>,
    pub reason: &'a str,
}

/// Opens a grant for [`HOURS`] hours from now, returning its id.
pub async fn open<'c>(db: impl Handle<'c>, access: &NewAccess<'_>) -> sqlx::Result<i64> {
    let mut conn = db.open(KEY).acquire().await?;
    let (scope, thread_id, space_id) = match access.scope {
        Scope::Thread(id) => ("thread", Some(id), None),
        Scope::Space(id) => ("space", None, Some(id)),
    };
    sqlx::query_scalar!(
        r#"INSERT INTO staff_access (actor_id, scope, thread_id, space_id, report_id, reason, expires_at)
           VALUES ($1, $2, $3, $4, $5, $6, now() + make_interval(hours => $7))
           RETURNING id"#,
        access.actor_id,
        scope,
        thread_id,
        space_id,
        access.report_id,
        access.reason,
        HOURS as i32,
    )
    .fetch_one(&mut *conn)
    .await
}

/// One page shown under a grant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct View {
    pub path: String,
    pub viewed_at: DateTime<Utc>,
}

/// The pages shown under a grant, first first.
pub async fn views<'c>(db: impl Handle<'c>, access_id: i64) -> sqlx::Result<Vec<View>> {
    let mut conn = db.open(KEY).acquire().await?;
    sqlx::query_as!(
        View,
        "SELECT path, viewed_at FROM staff_access_views WHERE access_id = $1 ORDER BY viewed_at",
        access_id
    )
    .fetch_all(&mut *conn)
    .await
}
