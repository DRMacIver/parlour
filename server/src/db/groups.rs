//! Groups of members (DESIGN.md §3.1, §3.3). Admins make them and add
//! members to them, and invites can add the people who use them. A space's
//! grants name the groups that may read, post in or administer it (in
//! `space_grants`, set by [`crate::store::update_space`]).
//!
//! Group names show on the index next to restricted spaces, but only to
//! viewers who can read those spaces, and they are never in the anonymous
//! cache, so changing groups clears no cached pages.

use chrono::{DateTime, Utc};

use crate::content::UserRef;
use crate::db::{Db, Handle, KEY};

/// What the admin types about a group.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GroupFields {
    pub slug: String,
    pub name: String,
    pub description: String,
}

pub const MAX_SLUG_CHARS: usize = 40;
pub const MAX_NAME_CHARS: usize = 60;
pub const MAX_DESCRIPTION_CHARS: usize = 300;

impl GroupFields {
    /// Tidies the fields, returning the problems with them if any.
    pub fn clean(&self) -> Result<GroupFields, Vec<String>> {
        let fields = GroupFields {
            slug: self.slug.trim().to_ascii_lowercase(),
            name: self
                .name
                .replace(|c: char| c.is_control(), " ")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" "),
            description: self.description.replace('\0', "").trim().to_owned(),
        };
        let mut problems = Vec::new();
        if !valid_slug(&fields.slug) || fields.slug.len() > MAX_SLUG_CHARS {
            problems.push(format!(
                "The short name should be up to {MAX_SLUG_CHARS} lower-case letters and digits, with single hyphens between words."
            ));
        }
        if fields.name.is_empty() || fields.name.chars().count() > MAX_NAME_CHARS {
            problems.push(format!(
                "The name should be between 1 and {MAX_NAME_CHARS} characters."
            ));
        }
        if fields.description.chars().count() > MAX_DESCRIPTION_CHARS {
            problems.push(format!(
                "Keep the description to {MAX_DESCRIPTION_CHARS} characters."
            ));
        }
        if problems.is_empty() {
            Ok(fields)
        } else {
            Err(problems)
        }
    }
}

/// Whether text has the form the database requires of space and group
/// slugs: lower-case words of letters and digits joined by single hyphens.
pub fn valid_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug.split('-').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
}

const SLUG_TAKEN: &str = "Another group has that short name.";
const NAME_TAKEN: &str = "Another group has that name.";

/// What a clash with another group means for the form.
fn clash(err: &sqlx::Error) -> Option<String> {
    let sqlx::Error::Database(err) = err else {
        return None;
    };
    if !err.is_unique_violation() {
        return None;
    }
    Some(match err.constraint() {
        Some("groups_name") => NAME_TAKEN.to_owned(),
        _ => SLUG_TAKEN.to_owned(),
    })
}

/// Makes a group from fields that [`GroupFields::clean`] accepted. Returns
/// the problem if another group has the same name or short name.
pub async fn create(db: &Db, fields: &GroupFields) -> sqlx::Result<Result<i64, String>> {
    let inserted = sqlx::query_scalar!(
        "INSERT INTO groups (slug, name, description) VALUES ($1, $2, $3) RETURNING id",
        fields.slug,
        fields.name,
        fields.description,
    )
    .fetch_one(db.pool())
    .await;
    match inserted {
        Ok(id) => Ok(Ok(id)),
        Err(err) => match clash(&err) {
            Some(why) => Ok(Err(why)),
            None => Err(err),
        },
    }
}

/// Changes a group's fields. `Ok(Ok(false))` if there is no such group.
pub async fn update(db: &Db, id: i64, fields: &GroupFields) -> sqlx::Result<Result<bool, String>> {
    let updated = sqlx::query!(
        "UPDATE groups SET slug = $2, name = $3, description = $4 WHERE id = $1",
        id,
        fields.slug,
        fields.name,
        fields.description,
    )
    .execute(db.pool())
    .await;
    match updated {
        Ok(done) => Ok(Ok(done.rows_affected() == 1)),
        Err(err) => match clash(&err) {
            Some(why) => Ok(Err(why)),
            None => Err(err),
        },
    }
}

/// Deletes a group that no space's grants name (DESIGN.md §3.3), taking
/// it off every invite. Returns the problem if a space names it, and
/// `Ok(Ok(false))` if there is no such group.
pub async fn delete(db: &Db, id: i64) -> sqlx::Result<Result<bool, String>> {
    let named = sqlx::query_scalar!(
        r#"SELECT count(DISTINCT space_id) AS "n!" FROM space_grants WHERE group_ids @> ARRAY[$1]::bigint[]"#,
        id
    )
    .fetch_one(db.pool())
    .await?;
    if named > 0 {
        return Ok(Err(format!(
            "The group can’t be deleted while {} name it. Take it out of their settings first.",
            crate::format::plural(named, "space", "spaces")
        )));
    }
    let done = sqlx::query!("DELETE FROM groups WHERE id = $1", id)
        .execute(db.pool())
        .await?;
    Ok(Ok(done.rows_affected() == 1))
}

/// Adds a member to a group, returning whether they weren't in it already.
pub async fn add_member(
    db: &Db,
    group_id: i64,
    user_id: i64,
    added_by: Option<i64>,
) -> sqlx::Result<bool> {
    let done = sqlx::query!(
        "INSERT INTO group_members (group_id, user_id, added_by) VALUES ($1, $2, $3)
         ON CONFLICT DO NOTHING",
        group_id,
        user_id,
        added_by,
    )
    .execute(db.pool())
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Takes a member out of a group, returning whether they were in it.
pub async fn remove_member(db: &Db, group_id: i64, user_id: i64) -> sqlx::Result<bool> {
    let done = sqlx::query!(
        "DELETE FROM group_members WHERE group_id = $1 AND user_id = $2",
        group_id,
        user_id,
    )
    .execute(db.pool())
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Adds a member to those of these groups that still exist, as an invite
/// does when it is used.
pub async fn grant<'c>(
    db: impl Handle<'c>,
    user_id: i64,
    group_ids: &[i64],
    added_by: Option<i64>,
) -> sqlx::Result<()> {
    let mut conn = db.open(KEY).acquire().await?;
    sqlx::query!(
        "INSERT INTO group_members (group_id, user_id, added_by)
         SELECT g.id, $1, $3 FROM groups g WHERE g.id = ANY($2)
         ON CONFLICT DO NOTHING",
        user_id,
        group_ids,
        added_by,
    )
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// A group, as listed for admins and in pickers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupSummary {
    pub id: i64,
    pub slug: String,
    pub name: String,
    pub description: String,
    pub member_count: i64,
}

/// Every group, by name.
pub async fn list(db: &Db) -> sqlx::Result<Vec<GroupSummary>> {
    sqlx::query_as!(
        GroupSummary,
        r#"SELECT g.id, g.slug, g.name, g.description,
                  (SELECT count(*) FROM group_members m JOIN users u ON u.id = m.user_id
                   WHERE m.group_id = g.id AND u.deleted_at IS NULL) AS "member_count!"
           FROM groups g ORDER BY lower(g.name), g.id"#
    )
    .fetch_all(db.pool())
    .await
}

/// Those of these ids that are groups.
pub async fn existing(db: &Db, ids: &[i64]) -> sqlx::Result<Vec<i64>> {
    sqlx::query_scalar!("SELECT id FROM groups WHERE id = ANY($1) ORDER BY id", ids)
        .fetch_all(db.pool())
        .await
}

/// A member of a group.
pub struct GroupMember {
    pub user: UserRef,
    pub added_at: DateTime<Utc>,
    pub added_by: Option<UserRef>,
}

/// A space whose grants name a group.
pub struct GroupSpace {
    pub slug: String,
    pub name: String,
    pub read: bool,
    /// Named for replying or starting threads.
    pub write: bool,
    pub administer: bool,
}

/// A group in full, for its admin page.
pub struct GroupDetail {
    pub group: GroupSummary,
    pub members: Vec<GroupMember>,
    pub spaces: Vec<GroupSpace>,
    /// Invites that can still be used and add people to the group.
    pub usable_invites: i64,
}

pub async fn detail(db: &Db, id: i64) -> sqlx::Result<Option<GroupDetail>> {
    let Some(group) = list(db).await?.into_iter().find(|g| g.id == id) else {
        return Ok(None);
    };
    let members = sqlx::query!(
        r#"SELECT u.id, u.username AS "username: String", u.display_name, m.added_at,
                  a.id AS "added_by_id?", a.username AS "added_by_username?: String",
                  a.display_name AS "added_by_name?"
           FROM group_members m
           JOIN users u ON u.id = m.user_id
           LEFT JOIN users a ON a.id = m.added_by
           WHERE m.group_id = $1 AND u.deleted_at IS NULL
           ORDER BY lower(u.username::text)"#,
        id
    )
    .fetch_all(db.pool())
    .await?
    .into_iter()
    .map(|row| GroupMember {
        user: UserRef {
            id: row.id,
            username: row.username,
            display_name: row.display_name,
        },
        added_at: row.added_at,
        added_by: match (row.added_by_id, row.added_by_username, row.added_by_name) {
            (Some(id), Some(username), Some(display_name)) => Some(UserRef {
                id,
                username,
                display_name,
            }),
            _ => None,
        },
    })
    .collect();
    let spaces = sqlx::query!(
        r#"SELECT c.slug, c.name,
                  bool_or(g.verb = 'read') AS "read!",
                  bool_or(g.verb IN ('reply', 'start')) AS "write!",
                  bool_or(g.verb = 'administer') AS "administer!"
           FROM space_grants g JOIN spaces c ON c.id = g.space_id
           WHERE g.group_ids @> ARRAY[$1]::bigint[]
           GROUP BY c.id
           ORDER BY c.position, c.id"#,
        id
    )
    .fetch_all(db.pool())
    .await?
    .into_iter()
    .map(|row| GroupSpace {
        slug: row.slug,
        name: row.name,
        read: row.read,
        write: row.write,
        administer: row.administer,
    })
    .collect();
    let usable_invites = sqlx::query_scalar!(
        r#"SELECT count(*) AS "n!" FROM invites
           WHERE $1 = ANY(grant_group_ids) AND revoked_at IS NULL
             AND expires_at > now() AND uses < max_uses"#,
        id
    )
    .fetch_one(db.pool())
    .await?;
    Ok(Some(GroupDetail {
        group,
        members,
        spaces,
        usable_invites,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hegel::TestCase;
    use hegel::generators as gs;

    #[test]
    fn fields_are_tidied_and_checked() {
        let fields = GroupFields {
            slug: " Organisers ".into(),
            name: "  The   organisers ".into(),
            description: " Plan things. ".into(),
        }
        .clean()
        .unwrap();
        assert_eq!(
            fields,
            GroupFields {
                slug: "organisers".into(),
                name: "The organisers".into(),
                description: "Plan things.".into(),
            }
        );
        for slug in ["", "a b", "a--b", "-a", "a_b", &"a".repeat(41)] {
            let problems = GroupFields {
                slug: slug.into(),
                name: "x".into(),
                description: String::new(),
            }
            .clean()
            .unwrap_err();
            assert_eq!(problems.len(), 1, "{slug:?}");
        }
        assert!(
            GroupFields {
                slug: "x".into(),
                name: " ".into(),
                description: String::new(),
            }
            .clean()
            .is_err()
        );
    }

    /// Whatever is typed, accepted fields are what the database accepts.
    #[hegel::test(test_cases = 500)]
    fn accepted_fields_fit_the_schema(tc: TestCase) {
        let fields = GroupFields {
            slug: tc.draw(gs::text()),
            name: tc.draw(gs::text()),
            description: tc.draw(gs::text()),
        };
        if let Ok(clean) = fields.clean() {
            assert!(valid_slug(&clean.slug) && clean.slug.len() <= MAX_SLUG_CHARS);
            let name = clean.name.chars().count();
            assert!((1..=MAX_NAME_CHARS).contains(&name));
            assert!(clean.description.chars().count() <= MAX_DESCRIPTION_CHARS);
            assert!(!clean.name.contains(char::is_control) && !clean.description.contains('\0'));
            assert_eq!(clean.clean(), Ok(clean.clone()));
        }
    }
}
