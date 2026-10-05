//! Break-glass grants (DESIGN.md §8): the `staff_access` log and the
//! `staff_access_views` recorded under each grant. Both are append-only
//! (migration 0032): nothing here, or anywhere, changes or removes a row.
//!
//! A grant is opened from the confirmation page (or a change that would
//! let its maker read what they can't), by [`crate::glass`], which also
//! tells everyone who can read what it opens. Reads under a grant go
//! through [`crate::content`], which records each page shown; what the
//! log shows its readers is in [`crate::content::access`].

use chrono::{DateTime, Utc};

use crate::db::{Db, Handle, KEY, Tx};

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
    /// Whether a change of who may read what it opens is what opens it, so
    /// that its maker reads it as one of its readers for as long as the
    /// change stands, rather than page by page (migration 0036).
    pub by_change: bool,
}

/// Opens a grant for [`HOURS`] hours from now, returning its id.
pub async fn open<'c>(db: impl Handle<'c>, access: &NewAccess<'_>) -> sqlx::Result<i64> {
    let mut conn = db.open(KEY).acquire().await?;
    let (scope, thread_id, space_id) = match access.scope {
        Scope::Thread(id) => ("thread", Some(id), None),
        Scope::Space(id) => ("space", None, Some(id)),
    };
    sqlx::query_scalar!(
        r#"INSERT INTO staff_access (actor_id, scope, thread_id, space_id, report_id, reason, expires_at,
                                     by_change)
           VALUES ($1, $2, $3, $4, $5, $6, now() + make_interval(hours => $7), $8)
           RETURNING id"#,
        access.actor_id,
        scope,
        thread_id,
        space_id,
        access.report_id,
        access.reason,
        HOURS as i32,
        access.by_change,
    )
    .fetch_one(&mut *conn)
    .await
}

/// Whether a grant `actor_id` holds now already covers `scope`: one on
/// it, or, for a thread, on its space. Asked in the transaction that would
/// open another, after waiting for any other such transaction of the same
/// member of staff to finish, so that a confirmation sent twice at once
/// opens one grant.
pub async fn covered(tx: &mut Tx, actor_id: i64, scope: Scope) -> sqlx::Result<bool> {
    let conn = tx.conn();
    sqlx::query!(
        "SELECT pg_advisory_xact_lock(hashtextextended('parlour break glass ' || $1::bigint, 0))",
        actor_id
    )
    .execute(&mut *conn)
    .await?;
    let (thread_id, space_id) = match scope {
        Scope::Thread(id) => (Some(id), None),
        Scope::Space(id) => (None, Some(id)),
    };
    sqlx::query_scalar!(
        r#"SELECT EXISTS (
               SELECT 1 FROM staff_access a
               WHERE a.actor_id = $1 AND a.expires_at > now()
                 AND (a.thread_id = $2 OR a.space_id = $3
                      OR a.space_id = (SELECT t.space_id FROM threads t WHERE t.id = $2))
           ) AS "covered!""#,
        actor_id,
        thread_id,
        space_id,
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

/// The members who can read what a grant on `scope` would open, now, as
/// the access layer decides for each of them (`Viewer::filter`: members
/// read public and members-only spaces, and what a grant or restriction
/// names them in), apart from `except`, the member of staff opening it.
/// They are the people told about it (DESIGN.md §8). A suspended member,
/// who reads only what visitors read while the suspension lasts, is
/// counted by the memberships they will read with again: it is their
/// conversation or thread that was read, and they are often the one a
/// report was about (`docs/v02-decisions.md` #147).
pub async fn readers<'c>(db: impl Handle<'c>, scope: Scope, except: i64) -> sqlx::Result<Vec<i64>> {
    let mut conn = db.open(KEY).acquire().await?;
    let (thread_id, space_id) = match scope {
        Scope::Thread(id) => (Some(id), None),
        Scope::Space(id) => (None, Some(id)),
    };
    sqlx::query_scalar!(
        r#"WITH viewers AS (
               SELECT m.id, ARRAY['public', 'members'] AS vis, m.restricted_space_ids AS restricted,
                      m.id AS reader, m.group_ids AS groups
               FROM member_contexts m
               WHERE m.id <> $3
           )
           SELECT v.id AS "id!" FROM viewers v
           LEFT JOIN threads t ON t.id = $1
           LEFT JOIN spaces c ON c.id = $2
           WHERE (t.id IS NOT NULL AND t.deleted_at IS NULL
                  AND (thread_readable(t, v.vis, v.restricted, v.reader, v.groups)
                       OR dm_readable(t.id, t.visibility, v.reader)))
              OR (c.id IS NOT NULL AND space_readable(c.visibility, c.id, v.vis, v.restricted))
           ORDER BY v.id"#,
        thread_id,
        space_id,
        except,
    )
    .fetch_all(&mut *conn)
    .await
}

/// How many break-glass grants were opened in each month, newest first,
/// over the `months` months up to and including this one, months with
/// none included, as `/moderation` shows them (DESIGN.md §8).
pub async fn counts_by_month(db: &Db, months: i32) -> sqlx::Result<Vec<(DateTime<Utc>, i64)>> {
    let rows = sqlx::query!(
        r#"SELECT m.month AS "month!", count(a.id) AS "n!"
           FROM generate_series(date_trunc('month', now() AT TIME ZONE 'UTC')
                                    - make_interval(months => $1 - 1),
                                date_trunc('month', now() AT TIME ZONE 'UTC'),
                                interval '1 month') AS m(month)
           LEFT JOIN staff_access a
             ON date_trunc('month', a.created_at AT TIME ZONE 'UTC') = m.month
           GROUP BY m.month
           ORDER BY m.month DESC"#,
        months,
    )
    .fetch_all(db.pool())
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.month.and_utc(), row.n))
        .collect())
}

/// How many break-glass grants there have been in all.
pub async fn total(db: &Db) -> sqlx::Result<i64> {
    sqlx::query_scalar!(r#"SELECT count(*) AS "n!" FROM staff_access"#)
        .fetch_one(db.pool())
        .await
}
