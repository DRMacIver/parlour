//! Single-use tokens sent by email (DESIGN.md §4 `login_tokens`): magic
//! links, email verification and password resets.
//!
//! A magic link can also be used by typing a six-digit code into the page
//! where it was asked for, for when the email is read on another device.
//! That page holds a random request id; the code is only accepted together
//! with it, at most [`MAX_CODE_ATTEMPTS`] times. Anyone can ask for links,
//! so the caller also limits guesses per account across all its requests
//! (see [`try_code`]).

use std::time::Duration;

use sqlx::{Acquire, PgConnection, PgPool};

use crate::tokens;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Purpose {
    MagicLink,
    VerifyEmail,
    Reset,
}

impl Purpose {
    pub fn as_str(self) -> &'static str {
        match self {
            Purpose::MagicLink => "magic_link",
            Purpose::VerifyEmail => "verify_email",
            Purpose::Reset => "reset",
        }
    }

    /// How long a token stays valid.
    pub fn lifetime(self) -> Duration {
        match self {
            Purpose::MagicLink => Duration::from_secs(15 * 60),
            Purpose::VerifyEmail => Duration::from_secs(3 * 24 * 3600),
            Purpose::Reset => Duration::from_secs(3600),
        }
    }
}

pub const MAX_CODE_ATTEMPTS: i32 = 5;
pub const CODE_DIGITS: u32 = 6;

/// A token that has just been made, with the secrets to send.
#[derive(Debug)]
pub struct Issued {
    pub token: String,
    /// For a magic link: the id of the page where the code can be typed.
    pub request: Option<String>,
    /// For a magic link: the code.
    pub code: Option<String>,
}

fn code_hash(request: &str, code: &str) -> Vec<u8> {
    tokens::hash(&format!("{request}:{code}"))
}

/// Makes a token. A new password-reset or verification token replaces any
/// earlier unused one of the same kind, so only the latest email works.
/// `email` is the address being verified, for `VerifyEmail`.
pub async fn issue(
    db: &mut PgConnection,
    user_id: i64,
    purpose: Purpose,
    email: Option<&str>,
) -> sqlx::Result<Issued> {
    let mut tx = db.begin().await?;
    let token = tokens::new_token();
    let (request, code) = match purpose {
        Purpose::MagicLink => (
            Some(tokens::new_token()),
            Some(tokens::random_code(CODE_DIGITS)),
        ),
        _ => (None, None),
    };
    sqlx::query!(
        "DELETE FROM login_tokens
         WHERE user_id = $1 AND (expires_at < now() OR used_at IS NOT NULL OR ($2 <> 'magic_link' AND purpose = $2))",
        user_id,
        purpose.as_str(),
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "INSERT INTO login_tokens (token_hash, user_id, purpose, email, request_hash, code_hash, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6, now() + make_interval(secs => $7))",
        tokens::hash(&token),
        user_id,
        purpose.as_str(),
        email.filter(|_| purpose == Purpose::VerifyEmail) as _,
        request.as_deref().map(tokens::hash),
        request.as_deref().zip(code.as_deref()).map(|(r, c)| code_hash(r, c)),
        purpose.lifetime().as_secs_f64(),
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Issued {
        token,
        request,
        code,
    })
}

/// A token that has been used.
#[derive(Debug, PartialEq, Eq)]
pub struct Consumed {
    pub user_id: i64,
    /// For `VerifyEmail`: the address verified.
    pub email: Option<String>,
}

/// Uses up a token, if it is current and for this purpose.
pub async fn consume(
    db: &mut PgConnection,
    token: &str,
    purpose: Purpose,
) -> sqlx::Result<Option<Consumed>> {
    if !tokens::plausible(token) {
        return Ok(None);
    }
    let row = sqlx::query!(
        r#"UPDATE login_tokens SET used_at = now()
           WHERE token_hash = $1 AND purpose = $2 AND used_at IS NULL AND expires_at > now()
           RETURNING user_id, email AS "email: String""#,
        tokens::hash(token),
        purpose.as_str(),
    )
    .fetch_optional(db)
    .await?;
    Ok(row.map(|row| Consumed {
        user_id: row.user_id,
        email: row.email,
    }))
}

/// Whether a token is current, without using it up.
pub async fn is_current(db: &PgPool, token: &str, purpose: Purpose) -> sqlx::Result<bool> {
    if !tokens::plausible(token) {
        return Ok(false);
    }
    sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM login_tokens
                          WHERE token_hash = $1 AND purpose = $2 AND used_at IS NULL AND expires_at > now()) AS "e!""#,
        tokens::hash(token),
        purpose.as_str(),
    )
    .fetch_one(db)
    .await
}

#[derive(Debug, PartialEq, Eq)]
pub enum CodeResult {
    /// The code was right; here is whose it was.
    LoggedIn { user_id: i64 },
    /// Wrong code; this many tries are left.
    Wrong { attempts_left: i32 },
    /// No such request, or it has expired, been used, or had too many
    /// wrong codes.
    Expired,
    /// The account has had too many guesses lately, across all its
    /// requests; the code wasn't checked.
    TooManyGuesses,
}

/// Tries the code from a magic-link email against the page's request id.
/// `allow` is asked, with the account's id, whether that account may have
/// another guess; it is only asked about requests that are current.
pub async fn try_code(
    db: &PgPool,
    request: &str,
    code: &str,
    allow: impl FnOnce(i64) -> bool,
) -> sqlx::Result<CodeResult> {
    if !tokens::plausible(request) {
        return Ok(CodeResult::Expired);
    }
    let code: String = code.chars().filter(|c| !c.is_whitespace()).collect();
    let mut tx = db.begin().await?;
    let Some(row) = sqlx::query!(
        "SELECT token_hash, user_id, code_hash, attempts FROM login_tokens
         WHERE request_hash = $1 AND purpose = 'magic_link' AND used_at IS NULL AND expires_at > now()
           AND attempts < $2
         FOR UPDATE",
        tokens::hash(request),
        MAX_CODE_ATTEMPTS,
    )
    .fetch_optional(&mut *tx)
    .await?
    else {
        return Ok(CodeResult::Expired);
    };
    if !allow(row.user_id) {
        return Ok(CodeResult::TooManyGuesses);
    }
    let right = row
        .code_hash
        .as_deref()
        .is_some_and(|expected| tokens::same(expected, &code_hash(request, &code)));
    let result = if right {
        sqlx::query!(
            "UPDATE login_tokens SET used_at = now() WHERE token_hash = $1",
            row.token_hash
        )
        .execute(&mut *tx)
        .await?;
        CodeResult::LoggedIn {
            user_id: row.user_id,
        }
    } else {
        sqlx::query!(
            "UPDATE login_tokens SET attempts = attempts + 1 WHERE token_hash = $1",
            row.token_hash
        )
        .execute(&mut *tx)
        .await?;
        let left = MAX_CODE_ATTEMPTS - row.attempts - 1;
        if left > 0 {
            CodeResult::Wrong {
                attempts_left: left,
            }
        } else {
            CodeResult::Expired
        }
    };
    tx.commit().await?;
    Ok(result)
}
