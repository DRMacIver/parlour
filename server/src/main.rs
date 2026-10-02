use std::net::SocketAddr;

use anyhow::Context;
use parlour::config::Config;
use parlour::{AppState, MIGRATOR, app, jobs, seed, setup};
use sqlx::postgres::PgPoolOptions;
use tracing_subscriber::EnvFilter;

const USAGE: &str = "usage: parlour [serve]          run the web server
       parlour seed [--reset]   fill an empty database with sample content
                                (--reset first deletes all users and content,
                                but only if `parlour seed` created them and
                                nothing has been added since)

Environment:
  DATABASE_URL           required
  PARLOUR_BIND           address to listen on (default 0.0.0.0:8080)
  PARLOUR_BASE_URL       the site's address, e.g. https://forum.example.com
                         (or PARLOUR_DOMAIN=forum.example.com; default
                         http://localhost:8080)
  PARLOUR_COOKIE_SECURE  mark the session cookie Secure (default: true
                         when the base URL is https)
  PARLOUR_TRUST_PROXY    take client addresses from X-Forwarded-For
  PARLOUR_SMTP_HOST, PARLOUR_SMTP_PORT, PARLOUR_SMTP_SECURITY
  (none|starttls|tls), PARLOUR_SMTP_USERNAME, PARLOUR_SMTP_PASSWORD,
  PARLOUR_MAIL_FROM      the SMTP server for email, unless one is saved
                         in the admin settings";

enum Command {
    Serve,
    Seed { reset: bool },
}

fn parse_args(args: &[String]) -> Option<Command> {
    match args {
        [] => Some(Command::Serve),
        [cmd] if cmd == "serve" => Some(Command::Serve),
        [cmd] if cmd == "seed" => Some(Command::Seed { reset: false }),
        [cmd, flag] if cmd == "seed" && flag == "--reset" => Some(Command::Seed { reset: true }),
        _ => None,
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(command) = parse_args(&args) else {
        eprintln!("{USAGE}");
        std::process::exit(2);
    };

    let database_url = std::env::var("DATABASE_URL").context("DATABASE_URL must be set")?;
    let db = PgPoolOptions::new()
        .max_connections(10)
        .connect(&database_url)
        .await
        .context("could not connect to the database")?;
    MIGRATOR
        .run(&db)
        .await
        .context("database migration failed")?;

    match command {
        Command::Serve => serve(db, Config::from_env()?).await,
        Command::Seed { reset } => {
            if reset {
                seed::reset(&db).await?;
            }
            let summary = seed::seed(&db).await?;
            println!(
                "Seeded {} users, {} categories, {} threads, {} posts and {} conversations. \
                 Every member's password is {:?} (try sam, the admin, or priya). \
                 Restart any running server so it doesn't serve cached pages from before.",
                summary.users,
                summary.categories,
                summary.threads,
                summary.posts,
                summary.conversations,
                seed::DEV_PASSWORD
            );
            Ok(())
        }
    }
}

async fn serve(db: sqlx::PgPool, config: Config) -> anyhow::Result<()> {
    let bind: SocketAddr = std::env::var("PARLOUR_BIND")
        .unwrap_or_else(|_| "0.0.0.0:8080".to_owned())
        .parse()
        .context("PARLOUR_BIND must be an address like 0.0.0.0:8080")?;
    let listener = tokio::net::TcpListener::bind(bind).await?;
    let state = AppState::new(db).with_config(config);
    if let Some(link) = setup::prepare(&state).await? {
        tracing::warn!("This forum has no admin account yet. To set it up, open {link}");
    }
    jobs::spawn_workers(&state, 2);
    tracing::info!(%bind, base_url = state.config().base_url, "parlour listening");
    axum::serve(
        listener,
        app(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.ok();
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            signal.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Option<Command> {
        parse_args(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn parses_commands() {
        assert!(matches!(parse(&[]), Some(Command::Serve)));
        assert!(matches!(parse(&["serve"]), Some(Command::Serve)));
        assert!(matches!(
            parse(&["seed"]),
            Some(Command::Seed { reset: false })
        ));
        assert!(matches!(
            parse(&["seed", "--reset"]),
            Some(Command::Seed { reset: true })
        ));
        assert!(parse(&["seed", "--force"]).is_none());
        assert!(parse(&["serve", "extra"]).is_none());
        assert!(parse(&["nonsense"]).is_none());
    }
}
