//! A job queue in Postgres, run inside the app process (DESIGN.md §9).
//!
//! A job is a row in `jobs` with a `kind` and a JSON `payload`. Code that
//! wants something done later inserts one with [`enqueue`], usually in the
//! same transaction as the change that needs it, so the job exists exactly
//! when the change does; after committing it calls [`Jobs::wake`].
//!
//! Workers claim one due job at a time with `FOR UPDATE SKIP LOCKED`, so
//! several workers never take the same job, and hold it with a lease
//! (`locked_until`): if a worker dies mid-job, the job becomes due again
//! when the lease runs out. A job that succeeds is deleted. One that fails
//! is retried with exponential backoff until it has had `max_attempts`
//! tries, and then kept with `failed_at` set for an admin to look at, with
//! anything secret removed from its payload ([`Jobs::scrub_on_failure`]).
//!
//! Handlers are registered by kind with [`Jobs::register`]. They must
//! tolerate running more than once, since a lease can expire while a slow
//! handler is still working.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use crate::db::{Handle, KEY};
use chrono::{DateTime, Utc};
use serde_json::Value;
use tokio::sync::Notify;

use crate::AppState;

/// How long a worker may hold a job before others may take it.
const LEASE: Duration = Duration::from_secs(300);
/// How long a handler may run.
const TIMEOUT: Duration = Duration::from_secs(120);
/// How often idle workers look for due jobs, in case a wake-up was missed
/// or a retry has come due.
const POLL: Duration = Duration::from_secs(5);

pub type JobFuture = Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>>;
type Handler = Arc<dyn Fn(AppState, Value) -> JobFuture + Send + Sync>;

/// Rewrites the payload of a job that has failed for the last time.
pub type Scrubber = fn(Value) -> Value;

/// The registered job kinds and the signal that wakes workers.
#[derive(Default)]
pub struct Jobs {
    handlers: RwLock<HashMap<String, Handler>>,
    scrubbers: RwLock<HashMap<String, Scrubber>>,
    notify: Notify,
}

impl Jobs {
    /// Registers the handler for one kind of job.
    pub fn register<F, Fut>(&self, kind: &str, handler: F)
    where
        F: Fn(AppState, Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        let handler: Handler = Arc::new(move |state, payload| Box::pin(handler(state, payload)));
        self.handlers
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(kind.to_owned(), handler);
    }

    /// Has jobs of `kind` that fail for the last time keep only what
    /// `scrub` leaves of their payload, for payloads holding secrets that
    /// shouldn't outlive the job's purpose.
    pub fn scrub_on_failure(&self, kind: &str, scrub: Scrubber) {
        self.scrubbers
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(kind.to_owned(), scrub);
    }

    fn scrubber(&self, kind: &str) -> Option<Scrubber> {
        self.scrubbers
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(kind)
            .copied()
    }

    fn handler(&self, kind: &str) -> Option<Handler> {
        self.handlers
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(kind)
            .cloned()
    }

    /// Tells idle workers that there may be new jobs.
    pub fn wake(&self) {
        self.notify.notify_waiters();
    }
}

/// Adds a job that is due now.
pub async fn enqueue<'c>(db: impl Handle<'c>, kind: &str, payload: &Value) -> sqlx::Result<i64> {
    enqueue_at(db, kind, payload, None).await
}

/// Adds a job that is due at `run_at` (or now).
pub async fn enqueue_at<'c>(
    db: impl Handle<'c>,
    kind: &str,
    payload: &Value,
    run_at: Option<DateTime<Utc>>,
) -> sqlx::Result<i64> {
    let mut conn = db.open(KEY).acquire().await?;
    sqlx::query_scalar!(
        "INSERT INTO jobs (kind, payload, run_at) VALUES ($1, $2, coalesce($3, now())) RETURNING id",
        kind,
        payload,
        run_at,
    )
    .fetch_one(&mut *conn)
    .await
}

/// What happened to a job a worker ran.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Done {
        id: i64,
    },
    /// Failed; will be tried again later.
    Retrying {
        id: i64,
        error: String,
    },
    /// Failed for the last time.
    Failed {
        id: i64,
        error: String,
    },
}

/// The wait before retrying a job that has failed `attempts` times.
pub fn backoff(attempts: i32) -> Duration {
    let exponent = attempts.saturating_sub(1).clamp(0, 20) as u32;
    Duration::from_secs(30)
        .saturating_mul(2u32.saturating_pow(exponent))
        .min(Duration::from_secs(6 * 3600))
}

/// Claims and runs one due job, if there is one.
pub async fn run_next(state: &AppState) -> sqlx::Result<Option<Outcome>> {
    let db = state.db().pool();
    let mut tx = db.begin().await?;
    let Some(job) = sqlx::query!(
        r#"SELECT id, kind, payload, attempts, max_attempts FROM jobs
           WHERE run_at <= now() AND failed_at IS NULL
             AND (locked_until IS NULL OR locked_until < now())
           ORDER BY run_at, id
           LIMIT 1
           FOR UPDATE SKIP LOCKED"#
    )
    .fetch_optional(&mut *tx)
    .await?
    else {
        return Ok(None);
    };
    let attempts = job.attempts + 1;
    sqlx::query!(
        "UPDATE jobs SET attempts = $2, locked_until = now() + make_interval(secs => $3) WHERE id = $1",
        job.id,
        attempts,
        LEASE.as_secs_f64(),
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    let result = match state.jobs().handler(&job.kind) {
        None => Err(anyhow::anyhow!(
            "no handler for jobs of kind {:?}",
            job.kind
        )),
        Some(handler) => {
            match tokio::time::timeout(TIMEOUT, handler(state.clone(), job.payload.clone())).await {
                Ok(result) => result,
                Err(_) => Err(anyhow::anyhow!("timed out after {TIMEOUT:?}")),
            }
        }
    };

    match result {
        Ok(()) => {
            sqlx::query!("DELETE FROM jobs WHERE id = $1", job.id)
                .execute(db)
                .await?;
            Ok(Some(Outcome::Done { id: job.id }))
        }
        Err(err) => {
            let error = format!("{err:#}");
            if attempts >= job.max_attempts {
                tracing::error!(job = job.id, kind = job.kind, %error, "job failed for the last time");
                let payload = match state.jobs().scrubber(&job.kind) {
                    Some(scrub) => scrub(job.payload),
                    None => job.payload,
                };
                sqlx::query!(
                    "UPDATE jobs SET failed_at = now(), locked_until = NULL, last_error = $2, payload = $3
                     WHERE id = $1",
                    job.id,
                    error,
                    payload,
                )
                .execute(db)
                .await?;
                Ok(Some(Outcome::Failed { id: job.id, error }))
            } else {
                tracing::warn!(job = job.id, kind = job.kind, %error, attempts, "job failed; will retry");
                sqlx::query!(
                    "UPDATE jobs SET run_at = now() + make_interval(secs => $3), locked_until = NULL,
                                     last_error = $2
                     WHERE id = $1",
                    job.id,
                    error,
                    backoff(attempts).as_secs_f64(),
                )
                .execute(db)
                .await?;
                Ok(Some(Outcome::Retrying { id: job.id, error }))
            }
        }
    }
}

/// Runs jobs until none are due, returning what happened to each.
pub async fn run_due(state: &AppState) -> sqlx::Result<Vec<Outcome>> {
    let mut outcomes = Vec::new();
    while let Some(outcome) = run_next(state).await? {
        outcomes.push(outcome);
    }
    Ok(outcomes)
}

/// Starts `count` workers that run jobs until the process exits.
pub fn spawn_workers(state: &AppState, count: usize) {
    for _ in 0..count {
        let state = state.clone();
        tokio::spawn(async move {
            loop {
                // Register interest before looking, so a wake-up that
                // arrives while this worker is busy isn't lost.
                let notified = state.jobs().notify.notified();
                if let Err(err) = run_due(&state).await {
                    tracing::error!(%err, "job worker could not reach the database");
                }
                tokio::select! {
                    _ = notified => {}
                    _ = tokio::time::sleep(POLL) => {}
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_is_capped() {
        assert_eq!(backoff(1), Duration::from_secs(30));
        assert_eq!(backoff(2), Duration::from_secs(60));
        assert_eq!(backoff(3), Duration::from_secs(120));
        assert_eq!(backoff(100), Duration::from_secs(6 * 3600));
        assert_eq!(backoff(i32::MAX), Duration::from_secs(6 * 3600));
        assert_eq!(backoff(0), Duration::from_secs(30));
        assert_eq!(backoff(i32::MIN), Duration::from_secs(30));
    }
}
