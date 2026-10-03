use std::net::SocketAddr;

use anyhow::Context;
use parlour::config::Config;
use parlour::{AppState, MIGRATOR, jobs, seed, setup, startup};
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
  PARLOUR_UPLOADS_DIR    where uploaded images are kept (default ./uploads),
                         with the forum id the server checks the database
                         against at startup
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

/// Whether log lines get colour codes: only when they go to a terminal,
/// not to `docker logs` or journald, and not if `NO_COLOR` is set.
fn log_colours(stdout_is_terminal: bool, no_color: Option<&str>) -> bool {
    stdout_is_terminal && no_color.is_none_or(str::is_empty)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    use std::io::IsTerminal;
    let no_color = std::env::var("NO_COLOR").ok();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_ansi(log_colours(
            std::io::stdout().is_terminal(),
            no_color.as_deref(),
        ))
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
    let config = Config::from_env()?;
    startup::check(&db, &config.uploads_dir).await?;
    MIGRATOR
        .run(&db)
        .await
        .context("database migration failed")?;
    startup::record(&db, &config.uploads_dir).await?;

    match command {
        Command::Serve => serve(db, config).await,
        Command::Seed { reset } => {
            if reset {
                seed::reset(&db).await?;
            }
            let summary = seed::seed(&db).await?;
            println!(
                "Seeded {} users, {} spaces, {} threads, {} posts and {} conversations. \
                 Every member's password is {:?} (try sam, the admin, or priya). \
                 Restart any running server so it doesn't serve cached pages from before.",
                summary.users,
                summary.spaces,
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
    parlour::moderation::schedule_forgetting(&state).await?;
    jobs::spawn_workers(&state, 2);
    tracing::info!(%bind, base_url = state.config().base_url, "parlour listening");
    parlour::serve(listener, state, shutdown_signal()).await?;
    tracing::info!("parlour stopped");
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

    #[test]
    fn logs_are_coloured_only_on_a_terminal() {
        assert!(log_colours(true, None));
        assert!(!log_colours(false, None));
        // https://no-color.org: any non-empty value turns colour off.
        assert!(!log_colours(true, Some("1")));
        assert!(log_colours(true, Some("")));
    }
}
