//! Logged-in sessions (DESIGN.md §7). A session is a random token in a
//! `HttpOnly; SameSite=Lax` cookie (and `Secure` unless turned off for
//! local http development). The database keeps only the token's SHA-256.
//!
//! A session lasts until it has gone unused for [`IDLE_DAYS`] days, or until
//! its owner logs out or resets their password.

use crate::AppState;
use crate::accounts::Role;
use crate::db::{Db, Handle, KEY};
use crate::tokens;
use crate::viewer::{SESSION_COOKIE, UserCtx, Viewer, session_cookie};
use axum::extract::{Request, State};
use axum::http::header::{SET_COOKIE, USER_AGENT};
use axum::http::{HeaderMap, HeaderValue};
use axum::middleware::Next;
use axum::response::Response;

/// Sessions unused for this long are no longer valid.
pub const IDLE_DAYS: i32 = 30;
/// How long the browser keeps the cookie: its upper limit, 400 days. The
/// server decides when the session really ends.
const COOKIE_MAX_AGE: u32 = 400 * 24 * 3600;

/// The `Set-Cookie` value that stores a session.
pub fn cookie(token: &str, secure: bool) -> HeaderValue {
    let secure = if secure { "; Secure" } else { "" };
    HeaderValue::from_str(&format!(
        "{SESSION_COOKIE}={token}; Path=/; Max-Age={COOKIE_MAX_AGE}; HttpOnly; SameSite=Lax{secure}"
    ))
    .expect("tokens are valid header text")
}

/// The `Set-Cookie` value that removes the session cookie.
pub fn clear_cookie(secure: bool) -> HeaderValue {
    let secure = if secure { "; Secure" } else { "" };
    HeaderValue::from_str(&format!(
        "{SESSION_COOKIE}=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax{secure}"
    ))
    .expect("valid header text")
}

/// Starts a session for a user, returning the token for the cookie.
pub async fn create<'c>(
    db: impl Handle<'c>,
    user_id: i64,
    headers: &HeaderMap,
    ip: &str,
) -> sqlx::Result<String> {
    let token = tokens::new_token();
    let user_agent = headers
        .get(USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(|ua| ua.chars().take(300).collect::<String>());
    let mut tx = db.open(KEY).begin().await?;
    sqlx::query!(
        "DELETE FROM sessions WHERE user_id = $1 AND last_seen_at < now() - make_interval(days => $2)",
        user_id,
        IDLE_DAYS
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "INSERT INTO sessions (token_hash, user_id, ip, user_agent) VALUES ($1, $2, $3, $4)",
        tokens::hash(&token),
        user_id,
        ip,
        user_agent,
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "UPDATE users SET last_seen_at = now() WHERE id = $1",
        user_id
    )
    .execute(&mut *tx)
    .await?;
    crate::db::moderation::record_ip(&mut *tx, user_id, ip).await?;
    tx.commit().await?;
    Ok(token)
}

/// The user a session token belongs to, if it is a current session of an
/// account that hasn't been deleted. Every few minutes this counts as a
/// visit from `ip`, which is recorded for moderators (DESIGN.md §8).
pub async fn lookup(db: &Db, token: &str, ip: &str) -> sqlx::Result<Option<UserCtx>> {
    if !tokens::plausible(token) {
        return Ok(None);
    }
    let hash = tokens::hash(token);
    let Some(row) = sqlx::query!(
        r#"SELECT s.user_id, s.last_seen_at < now() - interval '5 minutes' AS "stale!"
           FROM sessions s
           WHERE s.token_hash = $1 AND s.last_seen_at > now() - make_interval(days => $2)"#,
        hash,
        IDLE_DAYS,
    )
    .fetch_optional(db.pool())
    .await?
    else {
        return Ok(None);
    };
    let Some(user) = user_ctx(db, row.user_id).await? else {
        return Ok(None);
    };
    // Recording every request would mean a write per page view; once every
    // few minutes is enough to keep the session alive.
    if row.stale {
        sqlx::query!(
            "WITH s AS (UPDATE sessions SET last_seen_at = now() WHERE token_hash = $1 RETURNING user_id)
             UPDATE users SET last_seen_at = now() WHERE id IN (SELECT user_id FROM s)",
            hash
        )
        .execute(db.pool())
        .await?;
        crate::db::moderation::record_ip(db, user.id, ip).await?;
    }
    Ok(Some(user))
}

/// The user a session token belongs to, as [`lookup`] finds them, but
/// without counting as a use of the session: for checking again, during a
/// long-lived response such as a live stream, that the session hasn't
/// ended and what its owner may see now.
pub async fn still_current(db: &Db, token: &str) -> sqlx::Result<Option<UserCtx>> {
    if !tokens::plausible(token) {
        return Ok(None);
    }
    let Some(user_id) = sqlx::query_scalar!(
        "SELECT user_id FROM sessions
         WHERE token_hash = $1 AND last_seen_at > now() - make_interval(days => $2)",
        tokens::hash(token),
        IDLE_DAYS,
    )
    .fetch_optional(db.pool())
    .await?
    else {
        return Ok(None);
    };
    user_ctx(db, user_id).await
}

/// Everything about a member that decides what they may see and do, as
/// it is now: their role, and their groups and the categories those groups
/// may read. `None` if the account doesn't exist or was deleted.
pub async fn user_ctx(db: &Db, user_id: i64) -> sqlx::Result<Option<UserCtx>> {
    let row = sqlx::query!(
        r#"SELECT u.id, u.username, u.display_name, u.email, u.email_verified_at IS NOT NULL AS "verified!",
                  u.role, u.created_at, u.silenced_until, u.suspended_until,
                  array(SELECT m.group_id FROM group_members m WHERE m.user_id = u.id ORDER BY 1)
                      AS "group_ids!",
                  array(SELECT DISTINCT cg.category_id FROM category_groups cg
                        JOIN group_members m ON m.group_id = cg.group_id
                        WHERE m.user_id = u.id AND cg.access = 'read' ORDER BY 1) AS "group_category_ids!"
           FROM users u
           WHERE u.id = $1 AND u.deleted_at IS NULL"#,
        user_id,
    )
    .fetch_optional(db.pool())
    .await?;
    Ok(row.map(|row| UserCtx {
        id: row.id,
        username: row.username.to_string(),
        display_name: row.display_name,
        email: row.email.to_string(),
        email_verified: row.verified,
        role: Role::parse(&row.role).unwrap_or(Role::User),
        created_at: row.created_at,
        group_ids: row.group_ids,
        group_category_ids: row.group_category_ids,
        silenced_until: row.silenced_until,
        suspended_until: row.suspended_until,
    }))
}

/// Ends one session.
pub async fn delete(db: &Db, token: &str) -> sqlx::Result<()> {
    sqlx::query!(
        "DELETE FROM sessions WHERE token_hash = $1",
        tokens::hash(token)
    )
    .execute(db.pool())
    .await?;
    Ok(())
}

/// Ends every session of a user except (optionally) one.
pub async fn delete_all<'c>(
    db: impl Handle<'c>,
    user_id: i64,
    except_token: Option<&str>,
) -> sqlx::Result<()> {
    let mut conn = db.open(KEY).acquire().await?;
    let keep = except_token.map(tokens::hash);
    sqlx::query!(
        "DELETE FROM sessions WHERE user_id = $1 AND token_hash IS DISTINCT FROM $2",
        user_id,
        keep
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Works out the [`Viewer`] from the session cookie. A cookie that isn't a
/// current session is removed, so that the browser's later requests can be
/// served from the anonymous cache again.
pub async fn middleware(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let mut clear = false;
    if let Some(token) = session_cookie(request.headers()).map(str::to_owned) {
        let peer = request
            .extensions()
            .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
            .map(|axum::extract::ConnectInfo(addr)| *addr);
        let ip = crate::client::client_ip(state.config().trust_proxy, request.headers(), peer);
        match lookup(state.db(), &token, &ip).await {
            Ok(Some(user)) => {
                request.extensions_mut().insert(Viewer::User(user));
            }
            Ok(None) => clear = true,
            Err(err) => tracing::error!(%err, "could not look up a session"),
        }
    }
    let mut response = next.run(request).await;
    if clear && !response.headers().contains_key(SET_COOKIE) {
        response
            .headers_mut()
            .append(SET_COOKIE, clear_cookie(state.config().cookie_secure));
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookies_have_the_right_attributes() {
        let secure = cookie("abc", true);
        assert_eq!(
            secure,
            "parlour_session=abc; Path=/; Max-Age=34560000; HttpOnly; SameSite=Lax; Secure"
        );
        let insecure = cookie("abc", false);
        assert!(!insecure.to_str().unwrap().contains("Secure"));
        assert!(clear_cookie(true).to_str().unwrap().contains("Max-Age=0"));
    }
}
