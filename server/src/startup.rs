//! The guard against starting on the wrong database (DESIGN.md §10).
//!
//! The first time a forum starts, it gives itself a random id, kept in the
//! database (the `forum_id` setting) and in a file in the uploads
//! directory, [`MARKER_FILE`]. The two are stored on different volumes.
//! On every later start, before migrating, the server checks that the
//! database has the id the file names, and refuses to start if not.
//!
//! This catches a forum coming back up on an empty database, as happens
//! when a Postgres upgrade or a volume mix-up leaves the data where the
//! new server doesn't look (postgres:18 moved its data directory). Without
//! the check the server would happily print a new `/setup` link, and the
//! first person to see the log could take over the forum. It also catches
//! a server pointed at another forum's database.
//!
//! To start a new forum on purpose, remove the file.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use sqlx::PgPool;

use crate::db::forum_id::{self, Found};

/// The file in the uploads directory that names the forum's id.
pub const MARKER_FILE: &str = ".parlour-forum-id";

fn marker_path(uploads_dir: &Path) -> PathBuf {
    uploads_dir.join(MARKER_FILE)
}

/// Refuses to go on if the uploads directory names a forum whose id the
/// database doesn't have. Call before migrating, so that a refused
/// database is left as it was.
pub async fn check(db: &PgPool, uploads_dir: &Path) -> anyhow::Result<()> {
    let marker = marker_path(uploads_dir);
    let expected = match std::fs::read_to_string(&marker) {
        Ok(text) => text.trim().to_owned(),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            return Err(err).with_context(|| format!("could not read {}", marker.display()));
        }
    };
    match forum_id::found(db)
        .await
        .context("could not check the database")?
    {
        Found::Id(id) if id == expected => Ok(()),
        Found::Id(_) | Found::NoId => bail!(
            "refusing to start: this database belongs to a different forum from the one \
             whose uploads are in {dir}. Check DATABASE_URL and PARLOUR_UPLOADS_DIR. \
             If you really mean to use this database with these uploads, remove {marker} \
             and start again.",
            dir = uploads_dir.display(),
            marker = marker.display(),
        ),
        Found::Empty => bail!(
            "refusing to start: the database is empty, but {marker} \
             says this forum has run before, so its data should be there. This can happen \
             after a Postgres upgrade or when the database volume has changed: the old data \
             is probably still on disk where the new server doesn't look. Restore the data \
             or a backup into this database. If you really mean to start a new forum, \
             remove {marker} and start again.",
            marker = marker.display(),
        ),
    }
}

/// Gives the database an id if it has none, and writes it to the uploads
/// directory if the file isn't there. Call after migrating.
pub async fn record(db: &PgPool, uploads_dir: &Path) -> anyhow::Result<()> {
    let id = forum_id::ensure(db, &crate::tokens::random_token(16)).await?;
    let marker = marker_path(uploads_dir);
    if marker.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(uploads_dir)
        .with_context(|| format!("could not create {}", uploads_dir.display()))?;
    // Written whole and renamed into place, so a crash can't leave half an
    // id behind.
    let partial = uploads_dir.join(format!("{MARKER_FILE}.partial"));
    std::fs::write(&partial, format!("{id}\n"))
        .with_context(|| format!("could not write {}", partial.display()))?;
    std::fs::rename(&partial, &marker)
        .with_context(|| format!("could not write {}", marker.display()))?;
    Ok(())
}
