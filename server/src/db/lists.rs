//! Lists (DESIGN.md §3.3): groups that a member owns. They live in the
//! `groups` table with `owner_user_id` set (site groups, in
//! [`crate::groups`], have none), and grants name them like any group.
//!
//! Only the owner adds and removes the people on a list, renames it and
//! deletes it. A list is hidden: only its owner and the people on it know
//! it exists, so everything here is asked for by someone, and answers only
//! about the lists that person owns or is on. Wherever a grant names a
//! list, its owner gets through too (`member_group_ids`, migration 0026),
//! so the owner is never one of its members.
//!
//! Every member has a list called "Friends" from the moment their account
//! exists (a trigger makes it). When the owner's account is deleted the
//! list is frozen: nobody can change who is on it, and grants naming it
//! keep working. A list a grant names can't be deleted.
//!
//! Who is on a list decides what they can read, but only on pages for
//! logged-in members, which are never cached, so changing a list clears
//! no cached pages.

use chrono::{DateTime, Utc};

use crate::content::UserRef;
use crate::db::Db;

/// The list every member starts with.
pub const FRIENDS: &str = "Friends";

pub const MAX_NAME_CHARS: usize = 60;

/// A list's name as typed, tidied, or why it won't do.
pub fn clean_name(name: &str) -> Result<String, String> {
    let name = name
        .replace(|c: char| c.is_control(), " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if name.is_empty() || name.chars().count() > MAX_NAME_CHARS {
        return Err(format!(
            "A list’s name should be between 1 and {MAX_NAME_CHARS} characters."
        ));
    }
    Ok(name)
}

fn name_taken(name: &str) -> String {
    format!("You already have a list called “{name}”.")
}

fn is_name_clash(err: &sqlx::Error) -> bool {
    matches!(err, sqlx::Error::Database(err)
        if err.is_unique_violation() && err.constraint() == Some("groups_list_name"))
}

fn is_frozen(err: &sqlx::Error) -> bool {
    matches!(err, sqlx::Error::Database(err) if err.is_check_violation())
}

/// Makes a list for `owner`, with nobody on it. Returns the problem if the
/// name won't do or they have a list called that already.
pub async fn create(db: &Db, owner: i64, name: &str) -> sqlx::Result<Result<i64, String>> {
    let name = match clean_name(name) {
        Ok(name) => name,
        Err(why) => return Ok(Err(why)),
    };
    let inserted = sqlx::query_scalar!(
        "INSERT INTO groups (name, owner_user_id) VALUES ($1, $2) RETURNING id",
        name,
        owner,
    )
    .fetch_one(db.pool())
    .await;
    match inserted {
        Ok(id) => Ok(Ok(id)),
        Err(err) if is_name_clash(&err) => Ok(Err(name_taken(&name))),
        Err(err) => Err(err),
    }
}

/// What became of changing a list.
#[derive(Debug, PartialEq, Eq)]
pub enum Change {
    Done,
    /// It isn't a list of theirs (any more).
    NotFound,
    /// Why it can't be done.
    Refused(String),
}

/// Renames one of `owner`'s lists.
pub async fn rename(db: &Db, owner: i64, id: i64, name: &str) -> sqlx::Result<Change> {
    let name = match clean_name(name) {
        Ok(name) => name,
        Err(why) => return Ok(Change::Refused(why)),
    };
    let updated = sqlx::query!(
        "UPDATE groups SET name = $3 WHERE id = $1 AND owner_user_id = $2",
        id,
        owner,
        name,
    )
    .execute(db.pool())
    .await;
    match updated {
        Ok(done) if done.rows_affected() == 1 => Ok(Change::Done),
        Ok(_) => Ok(Change::NotFound),
        Err(err) if is_name_clash(&err) => Ok(Change::Refused(name_taken(&name))),
        Err(err) => Err(err),
    }
}

/// How many spaces' grants name a group or list.
pub async fn spaces_naming(db: &Db, id: i64) -> sqlx::Result<i64> {
    sqlx::query_scalar!(
        r#"SELECT count(DISTINCT space_id) AS "n!" FROM space_grants WHERE group_ids @> ARRAY[$1]::bigint[]"#,
        id
    )
    .fetch_one(db.pool())
    .await
}

/// How many live threads' own restrictions (DESIGN.md §3.4) name a group
/// or list. A deleted thread's restriction can't be changed, so it doesn't
/// hold on to what it names: deleting the group takes it out of the thread
/// (migration 0030).
pub async fn threads_naming(db: &Db, id: i64) -> sqlx::Result<i64> {
    sqlx::query_scalar!(
        r#"SELECT count(*) AS "n!" FROM threads
           WHERE deleted_at IS NULL
             AND (read_group_ids @> ARRAY[$1]::bigint[] OR reply_group_ids @> ARRAY[$1]::bigint[])"#,
        id
    )
    .fetch_one(db.pool())
    .await
}

/// Why a list that spaces' grants or threads' restrictions name can't be
/// deleted.
pub fn named_in(spaces: i64, threads: i64) -> String {
    let mut by = Vec::new();
    if spaces > 0 {
        by.push(crate::format::plural(spaces, "space", "spaces"));
    }
    if threads > 0 {
        by.push(crate::format::plural(threads, "thread", "threads"));
    }
    format!(
        "The list can’t be deleted while {} {} it. You can rename it, or take everyone off it, instead.",
        by.join(" and "),
        if spaces + threads == 1 {
            "names"
        } else {
            "name"
        }
    )
}

/// Deletes one of `owner`'s lists, unless a space's grants or a live
/// thread's restriction name it (DESIGN.md §3.3, §3.4).
pub async fn delete(db: &Db, owner: i64, id: i64) -> sqlx::Result<Change> {
    let mut tx = db.pool().begin().await?;
    let Some(_) = sqlx::query_scalar!(
        "SELECT id FROM groups WHERE id = $1 AND owner_user_id = $2 FOR UPDATE",
        id,
        owner
    )
    .fetch_optional(&mut *tx)
    .await?
    else {
        return Ok(Change::NotFound);
    };
    let named = sqlx::query!(
        r#"SELECT (SELECT count(DISTINCT space_id) FROM space_grants
                   WHERE group_ids @> ARRAY[$1]::bigint[]) AS "spaces!",
                  (SELECT count(*) FROM threads
                   WHERE deleted_at IS NULL
                     AND (read_group_ids @> ARRAY[$1]::bigint[]
                          OR reply_group_ids @> ARRAY[$1]::bigint[])) AS "threads!""#,
        id
    )
    .fetch_one(&mut *tx)
    .await?;
    if named.spaces > 0 || named.threads > 0 {
        return Ok(Change::Refused(named_in(named.spaces, named.threads)));
    }
    let deleted = sqlx::query!("DELETE FROM groups WHERE id = $1", id)
        .execute(&mut *tx)
        .await;
    match deleted {
        Ok(_) => {
            tx.commit().await?;
            Ok(Change::Done)
        }
        // A grant or a restriction naming it arrived in the meantime.
        Err(sqlx::Error::Database(err)) if err.is_foreign_key_violation() => {
            Ok(Change::Refused(
                "The list can’t be deleted now that something names it. You can rename it, or take everyone off it, instead."
                    .to_owned(),
            ))
        }
        Err(err) => Err(err),
    }
}

/// Puts the member called `username` on one of `owner`'s lists. Being on
/// it already is fine.
pub async fn add_member(db: &Db, owner: i64, id: i64, username: &str) -> sqlx::Result<Change> {
    let username = username.trim().trim_start_matches('@');
    let Some(user_id) = crate::users::id_by_username(db, username).await? else {
        return Ok(Change::Refused(format!(
            "There is no member called “{username}”."
        )));
    };
    if user_id == owner {
        return Ok(Change::Refused(
            "You don’t need to be on your own list: wherever it lets people in, it lets you in too."
                .to_owned(),
        ));
    }
    let added = sqlx::query!(
        "INSERT INTO group_members (group_id, user_id, added_by)
         SELECT g.id, $3, $2 FROM groups g
         WHERE g.id = $1 AND g.owner_user_id = $2 AND g.frozen_at IS NULL
         ON CONFLICT DO NOTHING",
        id,
        owner,
        user_id,
    )
    .execute(db.pool())
    .await;
    match added {
        Ok(_) => Ok(Change::Done),
        Err(err) if is_frozen(&err) => Ok(Change::NotFound),
        Err(err) => Err(err),
    }
}

/// Takes someone off one of `owner`'s lists. `NotFound` if they weren't on
/// it, or it isn't the owner's.
pub async fn remove_member(db: &Db, owner: i64, id: i64, user_id: i64) -> sqlx::Result<Change> {
    let removed = sqlx::query!(
        "DELETE FROM group_members m USING groups g
         WHERE m.group_id = $1 AND m.user_id = $3
           AND g.id = m.group_id AND g.owner_user_id = $2 AND g.frozen_at IS NULL",
        id,
        owner,
        user_id,
    )
    .execute(db.pool())
    .await;
    match removed {
        Ok(done) if done.rows_affected() == 1 => Ok(Change::Done),
        Ok(_) => Ok(Change::NotFound),
        Err(err) if is_frozen(&err) => Ok(Change::NotFound),
        Err(err) => Err(err),
    }
}

/// The id of a member's Friends list, if they still have one by that name.
pub async fn friends_id(db: &Db, owner: i64) -> sqlx::Result<Option<i64>> {
    sqlx::query_scalar!(
        "SELECT id FROM groups WHERE owner_user_id = $1 AND lower(name) = lower($2)",
        owner,
        FRIENDS,
    )
    .fetch_optional(db.pool())
    .await
}

/// Someone on a list.
#[derive(Clone, Debug)]
pub struct ListMember {
    pub user: UserRef,
    pub added_at: DateTime<Utc>,
}

/// A list, as its owner or someone on it sees it.
#[derive(Clone, Debug)]
pub struct List {
    pub id: i64,
    pub name: String,
    pub owner: UserRef,
    /// Whether the viewer owns it.
    pub yours: bool,
    /// When it was frozen, because its owner's account was deleted.
    pub frozen_at: Option<DateTime<Utc>>,
    /// Everyone on it whose account still exists, by username.
    pub members: Vec<ListMember>,
}

impl List {
    /// The people on it besides `viewer`.
    pub fn others(&self, viewer: &i64) -> impl Iterator<Item = &ListMember> {
        let viewer = *viewer;
        self.members.iter().filter(move |m| m.user.id != viewer)
    }

    pub fn frozen(&self) -> bool {
        self.frozen_at.is_some()
    }
}

/// The lists `viewer` owns or is on, or (with `id`) just that one: their
/// own first, by name, then the others by owner and name.
async fn lists(db: &Db, viewer: i64, id: Option<i64>) -> sqlx::Result<Vec<List>> {
    let rows = sqlx::query!(
        r#"SELECT g.id, g.name, g.frozen_at, g.owner_user_id = $1 AS "yours!",
                  o.id AS owner_id, o.username AS "owner_username: String",
                  o.display_name AS owner_display_name
           FROM groups g JOIN users o ON o.id = g.owner_user_id
           WHERE (g.owner_user_id = $1
                  OR EXISTS (SELECT 1 FROM group_members m WHERE m.group_id = g.id AND m.user_id = $1))
             AND ($2::bigint IS NULL OR g.id = $2)
           ORDER BY g.owner_user_id = $1 DESC, lower(o.display_name), o.id, lower(g.name), g.id"#,
        viewer,
        id,
    )
    .fetch_all(db.pool())
    .await?;
    let ids: Vec<i64> = rows.iter().map(|row| row.id).collect();
    let members = sqlx::query!(
        r#"SELECT m.group_id, m.added_at, u.id, u.username AS "username: String", u.display_name
           FROM group_members m JOIN users u ON u.id = m.user_id
           WHERE m.group_id = ANY($1) AND u.deleted_at IS NULL
           ORDER BY lower(u.username::text)"#,
        &ids,
    )
    .fetch_all(db.pool())
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| List {
            id: row.id,
            name: row.name,
            owner: UserRef {
                id: row.owner_id,
                username: row.owner_username,
                display_name: row.owner_display_name,
            },
            yours: row.yours,
            frozen_at: row.frozen_at,
            members: members
                .iter()
                .filter(|m| m.group_id == row.id)
                .map(|m| ListMember {
                    user: UserRef {
                        id: m.id,
                        username: m.username.clone(),
                        display_name: m.display_name.clone(),
                    },
                    added_at: m.added_at,
                })
                .collect(),
        })
        .collect())
}

/// Every list `viewer` owns or is on.
pub async fn known_to(db: &Db, viewer: i64) -> sqlx::Result<Vec<List>> {
    lists(db, viewer, None).await
}

/// One list, if `viewer` owns it or is on it; otherwise, as far as they
/// are concerned, there is no such list.
pub async fn known(db: &Db, viewer: i64, id: i64) -> sqlx::Result<Option<List>> {
    Ok(lists(db, viewer, Some(id)).await?.pop())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hegel::TestCase;
    use hegel::generators as gs;

    #[test]
    fn names_are_tidied_and_checked() {
        assert_eq!(clean_name("  Book \t club "), Ok("Book club".to_owned()));
        assert!(clean_name(" ").is_err());
        assert!(clean_name(&"x".repeat(61)).is_err());
        assert_eq!(clean_name(&"é".repeat(60)), Ok("é".repeat(60)));
    }

    /// Whatever is typed, an accepted name is what the database accepts,
    /// and tidying it again changes nothing.
    #[hegel::test(test_cases = 500)]
    fn accepted_names_fit_the_schema(tc: TestCase) {
        let typed: String = tc.draw(gs::text());
        if let Ok(name) = clean_name(&typed) {
            assert!((1..=MAX_NAME_CHARS).contains(&name.chars().count()));
            assert!(!name.contains(char::is_control));
            assert_eq!(clean_name(&name), Ok(name.clone()));
        }
    }

    #[test]
    fn says_how_many_spaces_name_a_list() {
        assert!(named_in(1, 0).contains("while 1 space names it"));
        assert!(named_in(3, 0).contains("while 3 spaces name it"));
        assert!(named_in(0, 1).contains("while 1 thread names it"));
        assert!(named_in(2, 1).contains("while 2 spaces and 1 thread name it"));
    }
}
