//! The forum's id, which the startup guard compares with the copy kept in
//! the uploads directory (see [`crate::startup`]). These run before the
//! migrations, on a database that may be empty, so they take the pool.

use sqlx::PgPool;

const SETTING: &str = "forum_id";

/// What the database says about which forum it holds.
pub enum Found {
    /// No tables at all: a new database.
    Empty,
    /// Tables but no id: a forum from before ids, or a damaged database.
    NoId,
    Id(String),
}

/// What the database says about itself. Safe to call before migrating.
pub async fn found(db: &PgPool) -> anyhow::Result<Found> {
    let tables: i64 = sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!" FROM pg_tables WHERE schemaname = 'public'"#
    )
    .fetch_one(db)
    .await?;
    if tables == 0 {
        return Ok(Found::Empty);
    }
    Ok(database_id(db).await?.map_or(Found::NoId, Found::Id))
}

/// The id the database holds, if it has one.
async fn database_id(db: &PgPool) -> anyhow::Result<Option<String>> {
    let has_settings: bool =
        sqlx::query_scalar!(r#"SELECT to_regclass('public.settings') IS NOT NULL AS "exists!""#)
            .fetch_one(db)
            .await?;
    if !has_settings {
        return Ok(None);
    }
    let value: Option<serde_json::Value> =
        sqlx::query_scalar!("SELECT value FROM settings WHERE key = $1", SETTING)
            .fetch_optional(db)
            .await?;
    Ok(value.and_then(|v| v.as_str().map(str::to_owned)))
}

/// The database's id, first giving it `new_id` if it has none.
pub async fn ensure(db: &PgPool, new_id: &str) -> anyhow::Result<String> {
    let value = serde_json::Value::String(new_id.to_owned());
    sqlx::query!(
        "INSERT INTO settings (key, value) VALUES ($1, $2) ON CONFLICT (key) DO NOTHING",
        SETTING,
        value
    )
    .execute(db)
    .await?;
    database_id(db)
        .await?
        .ok_or_else(|| anyhow::anyhow!("the database has no forum id after recording one"))
}
