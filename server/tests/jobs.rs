//! The Postgres job queue and email sending (DESIGN.md §9).

mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use common::smtp::FakeSmtp;
use hegel::TestCase;
use hegel::generators as gs;
use parlour::AppState;
use parlour::jobs::{self, Outcome};
use parlour::mail::{self, Email, Mailer, SmtpSecurity, SmtpSettings};
use serde_json::json;
use sqlx::PgPool;
use tokio::runtime::Handle;

async fn enqueue(state: &AppState, kind: &str, payload: serde_json::Value) -> i64 {
    jobs::enqueue(state.db(), kind, &payload).await.unwrap()
}

/// Makes every waiting job due now, as if time had passed.
async fn fast_forward(db: &PgPool) {
    sqlx::query("UPDATE jobs SET run_at = now() WHERE failed_at IS NULL")
        .execute(db)
        .await
        .unwrap();
}

async fn job_count(db: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM jobs")
        .fetch_one(db)
        .await
        .unwrap()
}

#[sqlx::test]
async fn jobs_run_once_and_are_deleted(db: PgPool) {
    let state = AppState::new(db.clone());
    let seen: Arc<Mutex<Vec<serde_json::Value>>> = Arc::default();
    let record = seen.clone();
    state.jobs().register("note", move |_, payload| {
        let record = record.clone();
        async move {
            record.lock().unwrap().push(payload);
            Ok(())
        }
    });
    let a = enqueue(&state, "note", json!({"n": 1})).await;
    let b = enqueue(&state, "note", json!({"n": 2})).await;
    let outcomes = jobs::run_due(&state).await.unwrap();
    assert_eq!(
        outcomes,
        vec![Outcome::Done { id: a }, Outcome::Done { id: b }]
    );
    assert_eq!(
        *seen.lock().unwrap(),
        vec![json!({"n": 1}), json!({"n": 2})]
    );
    assert_eq!(job_count(&db).await, 0);
    assert!(jobs::run_due(&state).await.unwrap().is_empty());
}

#[sqlx::test]
async fn jobs_wait_until_they_are_due(db: PgPool) {
    let state = AppState::new(db.clone());
    state.jobs().register("note", |_, _| async { Ok(()) });
    let mut conn = db.acquire().await.unwrap();
    let later = chrono::Utc::now() + chrono::Duration::hours(1);
    jobs::enqueue_at(&mut *conn, "note", &json!({}), Some(later))
        .await
        .unwrap();
    assert!(jobs::run_due(&state).await.unwrap().is_empty());
    fast_forward(&db).await;
    assert_eq!(jobs::run_due(&state).await.unwrap().len(), 1);
}

#[sqlx::test]
async fn failing_jobs_back_off_and_eventually_give_up(db: PgPool) {
    let state = AppState::new(db.clone());
    state.jobs().register("flaky", |_, _| async {
        Err(anyhow::anyhow!("server on fire"))
    });
    let id = enqueue(&state, "flaky", json!({})).await;
    sqlx::query("UPDATE jobs SET max_attempts = 3")
        .execute(&db)
        .await
        .unwrap();

    let outcomes = jobs::run_due(&state).await.unwrap();
    assert_eq!(
        outcomes,
        vec![Outcome::Retrying {
            id,
            error: "server on fire".into()
        }]
    );
    let (attempts, wait, error): (i32, f64, Option<String>) = sqlx::query_as(
        "SELECT attempts, extract(epoch FROM run_at - now())::float8, last_error FROM jobs",
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(attempts, 1);
    assert!((25.0..=30.0).contains(&wait), "{wait}");
    assert_eq!(error.as_deref(), Some("server on fire"));
    // Not due again yet.
    assert!(jobs::run_due(&state).await.unwrap().is_empty());

    fast_forward(&db).await;
    assert!(matches!(
        jobs::run_due(&state).await.unwrap()[..],
        [Outcome::Retrying { .. }]
    ));
    fast_forward(&db).await;
    assert_eq!(
        jobs::run_due(&state).await.unwrap(),
        vec![Outcome::Failed {
            id,
            error: "server on fire".into()
        }]
    );
    // Kept for an admin to look at, but never run again.
    fast_forward(&db).await;
    assert!(jobs::run_due(&state).await.unwrap().is_empty());
    let failed: bool = sqlx::query_scalar("SELECT failed_at IS NOT NULL FROM jobs")
        .fetch_one(&db)
        .await
        .unwrap();
    assert!(failed);
}

#[sqlx::test]
async fn jobs_of_unknown_kinds_fail(db: PgPool) {
    let state = AppState::new(db.clone());
    let id = enqueue(&state, "mystery", json!({})).await;
    let outcomes = jobs::run_due(&state).await.unwrap();
    assert!(
        matches!(&outcomes[..], [Outcome::Retrying { id: i, error }] if *i == id && error.contains("mystery"))
    );
}

#[sqlx::test]
async fn a_job_held_by_a_worker_is_left_alone_until_its_lease_runs_out(db: PgPool) {
    let state = AppState::new(db.clone());
    state.jobs().register("note", |_, _| async { Ok(()) });
    enqueue(&state, "note", json!({})).await;
    sqlx::query("UPDATE jobs SET locked_until = now() + interval '1 minute', attempts = 1")
        .execute(&db)
        .await
        .unwrap();
    assert!(jobs::run_due(&state).await.unwrap().is_empty());
    // The worker died: the lease runs out and someone else takes over.
    sqlx::query("UPDATE jobs SET locked_until = now() - interval '1 second'")
        .execute(&db)
        .await
        .unwrap();
    assert_eq!(jobs::run_due(&state).await.unwrap().len(), 1);
    assert_eq!(job_count(&db).await, 0);
}

/// Several workers at once, with jobs that fail some number of times
/// before succeeding: every job runs exactly as often as it needs to, no
/// two workers ever run the same job at the same time, and jobs that
/// fail too often are given up on.
#[hegel::standalone_function(test_cases = 40)]
fn concurrent_workers(tc: TestCase, rt: &Handle, db: &PgPool) {
    let failures: Vec<u8> = tc.draw(gs::vecs(gs::integers::<u8>().max_value(4)).max_size(20));
    let workers = tc.draw(gs::integers::<usize>().min_value(1).max_value(5));
    rt.block_on(async {
        sqlx::query("TRUNCATE jobs").execute(db).await.unwrap();
        let state = AppState::new(db.clone());
        let runs: Arc<Mutex<HashMap<u64, u8>>> = Arc::default();
        let running: Arc<Mutex<Vec<u64>>> = Arc::default();
        let (r, n) = (runs.clone(), running.clone());
        state.jobs().register("work", move |_, payload| {
            let (runs, running) = (r.clone(), n.clone());
            async move {
                let job = payload["job"].as_u64().unwrap();
                let fail_times = payload["fail"].as_u64().unwrap() as u8;
                {
                    let mut running = running.lock().unwrap();
                    assert!(!running.contains(&job), "job {job} ran twice at once");
                    running.push(job);
                }
                tokio::task::yield_now().await;
                running.lock().unwrap().retain(|j| *j != job);
                let mut runs = runs.lock().unwrap();
                let count = runs.entry(job).or_default();
                *count += 1;
                if *count <= fail_times {
                    Err(anyhow::anyhow!("failure {count}"))
                } else {
                    Ok(())
                }
            }
        });
        for (job, fail) in failures.iter().enumerate() {
            enqueue(&state, "work", json!({"job": job, "fail": fail})).await;
        }
        sqlx::query("UPDATE jobs SET max_attempts = 4")
            .execute(db)
            .await
            .unwrap();
        loop {
            let handles: Vec<_> = (0..workers)
                .map(|_| {
                    let state = state.clone();
                    tokio::spawn(async move { jobs::run_due(&state).await.unwrap() })
                })
                .collect();
            let mut ran = 0;
            for handle in handles {
                ran += handle.await.unwrap().len();
            }
            if ran == 0 {
                break;
            }
            fast_forward(db).await;
        }
        let runs = runs.lock().unwrap().clone();
        for (job, &fail) in failures.iter().enumerate() {
            let expected = (fail + 1).min(4);
            assert_eq!(
                runs.get(&(job as u64)).copied(),
                Some(expected),
                "job {job}"
            );
        }
        let failed: i64 =
            sqlx::query_scalar("SELECT count(*) FROM jobs WHERE failed_at IS NOT NULL")
                .fetch_one(db)
                .await
                .unwrap();
        assert_eq!(failed, failures.iter().filter(|&&f| f >= 4).count() as i64);
        assert_eq!(job_count(db).await, failed);
    });
}

#[sqlx::test]
async fn concurrent_workers_run_each_job_as_needed(db: PgPool) {
    let rt = Handle::current();
    tokio::task::spawn_blocking(move || concurrent_workers(&rt, &db))
        .await
        .unwrap();
}

// ---------------------------------------------------------------------------
// Email

fn smtp_settings(port: u16) -> SmtpSettings {
    SmtpSettings {
        host: "127.0.0.1".into(),
        port,
        security: SmtpSecurity::None,
        username: String::new(),
        password: String::new(),
        from: "The Club <forum@example.com>".into(),
    }
}

#[sqlx::test]
async fn queued_email_is_sent_over_smtp(db: PgPool) {
    let server = FakeSmtp::start().await;
    let config = parlour::config::Config {
        smtp: Some(smtp_settings(server.port)),
        ..parlour::config::Config::development()
    };
    let state = AppState::new(db.clone()).with_config(config);
    let email = Email {
        to: "ruth@example.com".into(),
        subject: "Hello there".into(),
        body: "A message for Ruth.".into(),
        secret: false,
    };
    let mut conn = db.acquire().await.unwrap();
    mail::queue(&mut *conn, &email).await.unwrap();
    let outcomes = jobs::run_due(&state).await.unwrap();
    assert!(
        matches!(outcomes[..], [Outcome::Done { .. }]),
        "{outcomes:?}"
    );
    let messages = server.messages();
    assert_eq!(messages.len(), 1);
    assert!(messages[0].contains("Subject: Hello there"));
    assert!(messages[0].contains("To: ruth@example.com"));
    let from = messages[0]
        .lines()
        .find(|l| l.starts_with("From:"))
        .unwrap();
    assert!(
        from.contains("The Club") && from.contains("<forum@example.com>"),
        "{from}"
    );
    assert!(messages[0].contains("A message for Ruth."));
}

#[sqlx::test]
async fn saved_smtp_settings_take_precedence(db: PgPool) {
    let env_server = FakeSmtp::start().await;
    let saved_server = FakeSmtp::start().await;
    let config = parlour::config::Config {
        smtp: Some(smtp_settings(env_server.port)),
        ..parlour::config::Config::development()
    };
    let state = AppState::new(db.clone()).with_config(config);
    let mut conn = db.acquire().await.unwrap();
    parlour::settings::set(&mut *conn, "smtp", &smtp_settings(saved_server.port))
        .await
        .unwrap();
    mail::send_now(
        &state,
        &Email {
            to: "ruth@example.com".into(),
            subject: "Hi".into(),
            body: "Hi".into(),
            secret: false,
        },
    )
    .await
    .unwrap();
    assert_eq!(saved_server.messages().len(), 1);
    assert!(env_server.messages().is_empty());
}

#[sqlx::test]
async fn email_is_retried_when_the_server_is_unreachable(db: PgPool) {
    // Nothing listens on this port once the listener is dropped.
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let config = parlour::config::Config {
        smtp: Some(smtp_settings(port)),
        ..parlour::config::Config::development()
    };
    let state = AppState::new(db.clone()).with_config(config);
    let mut conn = db.acquire().await.unwrap();
    mail::queue(
        &mut *conn,
        &Email {
            to: "ruth@example.com".into(),
            subject: "Hi".into(),
            body: "Hi".into(),
            secret: false,
        },
    )
    .await
    .unwrap();
    let outcomes = jobs::run_due(&state).await.unwrap();
    assert!(
        matches!(&outcomes[..], [Outcome::Retrying { error, .. }] if error.contains("refused")),
        "{outcomes:?}"
    );
}

#[sqlx::test]
async fn without_an_smtp_server_email_waits(db: PgPool) {
    let state = AppState::new(db.clone());
    let mut conn = db.acquire().await.unwrap();
    mail::queue(
        &mut *conn,
        &Email {
            to: "ruth@example.com".into(),
            subject: "Hi".into(),
            body: "Hi".into(),
            secret: false,
        },
    )
    .await
    .unwrap();
    let outcomes = jobs::run_due(&state).await.unwrap();
    assert!(
        matches!(&outcomes[..], [Outcome::Retrying { error, .. }] if error.contains("no SMTP server")),
        "{outcomes:?}"
    );
    assert!(matches!(state.mailer(), Mailer::Smtp));
}

/// Session tokens, login tokens and invite codes are stored only as
/// hashes, so a copy of the database can't be used to log in or join. An
/// email holding such a link is kept in the queue only while it might
/// still be sent: once it has failed for the last time, the link is
/// removed. Other failed emails are kept whole for an admin to look at.
#[sqlx::test]
async fn failed_emails_dont_keep_their_secrets(db: PgPool) {
    let state = AppState::new(db.clone());
    let link = "http://localhost:8080/reset/SECRETSECRETSECRETSECRETSECRETSECRETSECRET1";
    let mut conn = db.acquire().await.unwrap();
    mail::queue(
        &mut *conn,
        &parlour::emails::reset("ruth@example.com", "Club", "Ruth", link),
    )
    .await
    .unwrap();
    mail::queue(
        &mut *conn,
        &parlour::emails::test("ruth@example.com", "Club"),
    )
    .await
    .unwrap();
    let mut failed = 0;
    for _ in 0..20 {
        fast_forward(&db).await;
        for outcome in jobs::run_due(&state).await.unwrap() {
            if matches!(outcome, Outcome::Failed { .. }) {
                failed += 1;
            }
        }
    }
    assert_eq!(failed, 2);
    let payloads: Vec<serde_json::Value> =
        sqlx::query_scalar("SELECT payload FROM jobs WHERE failed_at IS NOT NULL ORDER BY id")
            .fetch_all(&db)
            .await
            .unwrap();
    assert_eq!(payloads.len(), 2);
    let reset = payloads[0].to_string();
    assert!(!reset.contains("SECRET"), "{reset}");
    assert!(reset.contains("ruth@example.com"));
    let test = payloads[1].to_string();
    assert!(test.contains("Test email"), "{test}");
}
