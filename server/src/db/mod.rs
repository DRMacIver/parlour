//! Everything that touches the database (DESIGN.md §6).
//!
//! Only the modules in here can run SQL. The rest of the app, and request
//! handlers in particular, get a [`Db`] or a [`Tx`]: opaque handles they
//! can pass to the functions in these modules but can't run queries on.
//! Every read of forum content goes through [`content`], whose functions
//! take the [`crate::viewer::Viewer`] and filter with the one shared SQL
//! visibility condition; writes go through [`store`] and [`groups`]; and
//! accounts, sessions, invites, settings and jobs have their own modules.
//!
//! [`Handle`] is what these functions take when they can work inside a
//! transaction or outside one. It is implemented for the opaque handles and
//! for sqlx's own pool, connection and transaction types, so that tests and
//! the command-line tools, which own a pool, can call the same functions.
//! Its one method needs a key that only this module can make, so holding a
//! handle never gives access to the connection inside it.

pub mod changes;
pub mod content;
pub mod groups;
pub mod invites;
pub mod jobs;
pub mod login_tokens;
pub mod members;
pub mod moderation;
pub mod notifications;
pub mod seed;
pub mod sessions;
pub mod settings;
pub mod social;
pub mod store;
pub mod users;

use std::ops::{Deref, DerefMut};

use sqlx::pool::PoolConnection;
use sqlx::{Acquire, PgConnection, PgPool, Postgres, Transaction};

/// The database, as the rest of the app sees it.
#[derive(Clone, Debug)]
pub struct Db {
    pool: PgPool,
}

impl Db {
    pub fn new(pool: PgPool) -> Db {
        Db { pool }
    }

    pub(in crate::db) fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Starts a transaction, for several writes that must happen together.
    pub async fn begin(&self) -> sqlx::Result<Tx> {
        Ok(Tx {
            inner: self.pool.begin().await?,
        })
    }

    /// Checks that the database answers.
    pub async fn ping(&self) -> sqlx::Result<()> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }
}

/// An open transaction. Dropping it without [`Tx::commit`] rolls it back.
#[derive(Debug)]
pub struct Tx {
    inner: Transaction<'static, Postgres>,
}

impl Tx {
    pub async fn commit(self) -> sqlx::Result<()> {
        self.inner.commit().await
    }

    pub(in crate::db) fn conn(&mut self) -> &mut PgConnection {
        &mut self.inner
    }
}

mod private {
    /// Proof that the caller is inside [`crate::db`].
    pub struct Key(pub(in crate::db) ());
}

pub(in crate::db) const KEY: private::Key = private::Key(());

/// Somewhere to run queries: a [`Db`] or [`Tx`], or (for tests and tools)
/// sqlx's pool, connection or transaction.
pub trait Handle<'c>: Send {
    /// What to run queries on. Only code in [`crate::db`] can call this.
    fn open(self, key: private::Key) -> Source<'c>;
}

/// What a [`Handle`] opens to.
pub struct Source<'c>(SourceInner<'c>);

enum SourceInner<'c> {
    Pool(&'c PgPool),
    Conn(&'c mut PgConnection),
}

impl<'c> Source<'c> {
    /// A connection to run queries on.
    pub(in crate::db) async fn acquire(self) -> sqlx::Result<Conn<'c>> {
        Ok(match self.0 {
            SourceInner::Pool(pool) => Conn::Pooled(pool.acquire().await?),
            SourceInner::Conn(conn) => Conn::Borrowed(conn),
        })
    }

    /// A transaction, or a savepoint inside the open one.
    pub(in crate::db) async fn begin(self) -> sqlx::Result<Transaction<'c, Postgres>> {
        match self.0 {
            SourceInner::Pool(pool) => pool.begin().await,
            SourceInner::Conn(conn) => conn.begin().await,
        }
    }
}

/// A connection from a [`Source`].
pub(in crate::db) enum Conn<'c> {
    Pooled(PoolConnection<Postgres>),
    Borrowed(&'c mut PgConnection),
}

impl Deref for Conn<'_> {
    type Target = PgConnection;
    fn deref(&self) -> &PgConnection {
        match self {
            Conn::Pooled(conn) => conn,
            Conn::Borrowed(conn) => conn,
        }
    }
}

impl DerefMut for Conn<'_> {
    fn deref_mut(&mut self) -> &mut PgConnection {
        match self {
            Conn::Pooled(conn) => conn,
            Conn::Borrowed(conn) => conn,
        }
    }
}

impl<'c> Handle<'c> for &'c Db {
    fn open(self, _: private::Key) -> Source<'c> {
        Source(SourceInner::Pool(&self.pool))
    }
}

impl<'c> Handle<'c> for &'c mut Tx {
    fn open(self, _: private::Key) -> Source<'c> {
        Source(SourceInner::Conn(&mut self.inner))
    }
}

impl<'c> Handle<'c> for &'c PgPool {
    fn open(self, _: private::Key) -> Source<'c> {
        Source(SourceInner::Pool(self))
    }
}

impl<'c> Handle<'c> for &'c mut PgConnection {
    fn open(self, _: private::Key) -> Source<'c> {
        Source(SourceInner::Conn(self))
    }
}

impl<'c, 't> Handle<'c> for &'c mut Transaction<'t, Postgres> {
    fn open(self, _: private::Key) -> Source<'c> {
        Source(SourceInner::Conn(self))
    }
}
