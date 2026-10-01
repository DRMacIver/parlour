use sqlx::PgPool;

const DEFAULT_SITE_NAME: &str = "Parlour";

pub async fn site_name(db: &PgPool) -> sqlx::Result<String> {
    let name: Option<String> =
        sqlx::query_scalar("SELECT value #>> '{}' FROM settings WHERE key = 'site_name'")
            .fetch_optional(db)
            .await?;
    Ok(name.unwrap_or_else(|| DEFAULT_SITE_NAME.to_owned()))
}
