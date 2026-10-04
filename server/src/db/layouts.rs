//! Layouts (DESIGN.md §3.5): how a front page is arranged. A layout is an
//! ordered list of named sections, each an ordered list of spaces. The
//! `layouts` row with no user is the admin-set **default**; every member
//! has a row of their own, copied from the default when their account is
//! made and theirs to edit from then on. "Following" a space is putting
//! it in one of your sections, and nothing more.
//!
//! A member who hasn't edited their copy (`edited_at` is null) keeps
//! following the default: every change to the default is copied into
//! their layout again, by `layout_copy_default` (migration 0024, first
//! written in 0023), which keeps their own "Feeds" section and what is in
//! it, and leaves out the spaces they have muted. Any change a member
//! makes to their sections marks the layout edited; muting doesn't, and
//! nor does a step that leaves the layout as it was (a [`Step`] that is
//! `Unchanged`), which writes nothing.
//! `layout_propagate_default` checks each follower again once it has
//! their row lock, so a first edit that lands while a change to the
//! default is being copied out isn't copied over.
//!
//! Sections keep the positions they are given and sort by `(position,
//! id)`, so a layout editor may number them as it likes, or hand
//! [`reorder_sections`] the order it wants. Within a section a space is
//! placed at an index, and the section's spaces are renumbered from 0. A
//! space is in at most one section of a layout (a trigger makes sure):
//! following it again moves it. Putting a space in a member's sections,
//! by following or moving it, unmutes it for them.
//!
//! Also here: muting a space (it leaves the member's sections and front
//! page, and is left out of the All tab, the default view of the space
//! index and search results), and marking a space essential (shown at the
//! top of every front page, and never muted), which are admin and member
//! writes rather than content, so they live beside the layout writes.
//!
//! What a viewer is shown of a layout is read through
//! [`crate::content::front_page`], which applies their permissions.

use chrono::{DateTime, Utc};
use sqlx::PgConnection;

use crate::db::{Handle, KEY};

/// The section every member's layout ends with, where their own feed goes
/// (DESIGN.md §3.5).
pub const FEEDS_SECTION: &str = "Feeds";

/// The section made for a member who follows a space from its page while
/// their layout has no sections at all.
pub const FOLLOWING_SECTION: &str = "Following";

pub const MAX_SECTION_NAME_CHARS: usize = 80;

/// Tidies a section name, or says what is wrong with it.
pub fn clean_section_name(name: &str) -> Result<String, String> {
    let name = name
        .replace(|c: char| c.is_control(), " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if name.is_empty() || name.chars().count() > MAX_SECTION_NAME_CHARS {
        Err(format!(
            "A section name should be between 1 and {MAX_SECTION_NAME_CHARS} characters."
        ))
    } else {
        Ok(name)
    }
}

/// A layout as stored, with everything in it whether or not any viewer
/// may see it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    pub id: i64,
    /// `None` for the default.
    pub user_id: Option<i64>,
    /// When the member last changed it; `None` while it follows the
    /// default.
    pub edited_at: Option<DateTime<Utc>>,
    pub sections: Vec<Section>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Section {
    pub id: i64,
    pub name: String,
    pub position: i32,
    pub spaces: Vec<SectionSpace>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SectionSpace {
    pub id: i64,
    pub slug: String,
    pub name: String,
    pub essential: bool,
    pub archived: bool,
}

// ---------------------------------------------------------------------------
// Finding layouts

/// The default layout's id, making the row if there isn't one yet (a
/// fresh database has one from the migration, but tests empty tables).
pub async fn default_id<'c>(db: impl Handle<'c>) -> sqlx::Result<i64> {
    let mut conn = db.open(KEY).acquire().await?;
    ensure_default(&mut conn).await
}

async fn ensure_default(conn: &mut PgConnection) -> sqlx::Result<i64> {
    sqlx::query!(
        "INSERT INTO layouts (user_id) VALUES (NULL)
         ON CONFLICT ((user_id IS NULL)) WHERE user_id IS NULL DO NOTHING"
    )
    .execute(&mut *conn)
    .await?;
    sqlx::query_scalar!("SELECT id FROM layouts WHERE user_id IS NULL")
        .fetch_one(conn)
        .await
}

/// A member's own layout, if they have one.
pub async fn member_id<'c>(db: impl Handle<'c>, user_id: i64) -> sqlx::Result<Option<i64>> {
    let mut conn = db.open(KEY).acquire().await?;
    sqlx::query_scalar!("SELECT id FROM layouts WHERE user_id = $1", user_id)
        .fetch_optional(&mut *conn)
        .await
}

/// The layout a viewer's front page comes from: their own, or the default
/// for visitors and for a member who somehow has none. For reading only:
/// a member's writes go to [`own_id`], never to the default, which is
/// everyone's who follows it.
pub async fn id_for<'c>(db: impl Handle<'c>, user_id: Option<i64>) -> sqlx::Result<i64> {
    let mut conn = db.open(KEY).acquire().await?;
    if let Some(user_id) = user_id
        && let Some(id) = sqlx::query_scalar!("SELECT id FROM layouts WHERE user_id = $1", user_id)
            .fetch_optional(&mut *conn)
            .await?
    {
        return Ok(id);
    }
    ensure_default(&mut conn).await
}

/// The layout a member's own changes go to: theirs, made as a copy of the
/// default first if they somehow have none (every account is given one,
/// so this is belt and braces). Never the default.
pub async fn own_id<'c>(db: impl Handle<'c>, user_id: i64) -> sqlx::Result<i64> {
    let mut conn = db.open(KEY).acquire().await?;
    match sqlx::query_scalar!("SELECT id FROM layouts WHERE user_id = $1", user_id)
        .fetch_optional(&mut *conn)
        .await?
    {
        Some(id) => Ok(id),
        None => create_member_copy(&mut *conn, user_id).await,
    }
}

/// A layout with its sections and their spaces, in order.
pub async fn read<'c>(db: impl Handle<'c>, layout_id: i64) -> sqlx::Result<Option<Layout>> {
    let mut conn = db.open(KEY).acquire().await?;
    let Some(layout) = sqlx::query!(
        "SELECT id, user_id, edited_at FROM layouts WHERE id = $1",
        layout_id
    )
    .fetch_optional(&mut *conn)
    .await?
    else {
        return Ok(None);
    };
    let rows = sqlx::query!(
        r#"
        SELECT s.id AS section_id, s.name AS section_name, s.position AS section_position,
               c.id AS "space_id?", c.slug AS "slug?", c.name AS "space_name?",
               c.essential AS "essential?", c.archived_at IS NOT NULL AS "archived?"
        FROM layout_sections s
        LEFT JOIN layout_section_spaces x ON x.section_id = s.id
        LEFT JOIN spaces c ON c.id = x.space_id
        WHERE s.layout_id = $1
        ORDER BY s.position, s.id, x.position, c.id
        "#,
        layout_id
    )
    .fetch_all(&mut *conn)
    .await?;
    let mut sections: Vec<Section> = Vec::new();
    for row in rows {
        if sections.last().is_none_or(|s| s.id != row.section_id) {
            sections.push(Section {
                id: row.section_id,
                name: row.section_name,
                position: row.section_position,
                spaces: Vec::new(),
            });
        }
        if let (Some(id), Some(slug), Some(name)) = (row.space_id, row.slug, row.space_name) {
            sections
                .last_mut()
                .expect("a section was just pushed")
                .spaces
                .push(SectionSpace {
                    id,
                    slug,
                    name,
                    essential: row.essential.unwrap_or(false),
                    archived: row.archived.unwrap_or(false),
                });
        }
    }
    Ok(Some(Layout {
        id: layout.id,
        user_id: layout.user_id,
        edited_at: layout.edited_at,
        sections,
    }))
}

/// Where a space is in a layout: the section's id and name, if it is in
/// one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placement {
    pub section_id: i64,
    pub section_name: String,
}

pub async fn placement<'c>(
    db: impl Handle<'c>,
    layout_id: i64,
    space_id: i64,
) -> sqlx::Result<Option<Placement>> {
    let mut conn = db.open(KEY).acquire().await?;
    let row = sqlx::query!(
        "SELECT s.id, s.name FROM layout_section_spaces x JOIN layout_sections s ON s.id = x.section_id
         WHERE s.layout_id = $1 AND x.space_id = $2",
        layout_id,
        space_id
    )
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.map(|row| Placement {
        section_id: row.id,
        section_name: row.name,
    }))
}

/// The section of a layout that a space is in, if any.
pub async fn section_of<'c>(
    db: impl Handle<'c>,
    layout_id: i64,
    space_id: i64,
) -> sqlx::Result<Option<i64>> {
    let mut conn = db.open(KEY).acquire().await?;
    sqlx::query_scalar!(
        "SELECT s.id FROM layout_section_spaces x JOIN layout_sections s ON s.id = x.section_id
         WHERE s.layout_id = $1 AND x.space_id = $2",
        layout_id,
        space_id
    )
    .fetch_optional(&mut *conn)
    .await
}

// ---------------------------------------------------------------------------
// Copies of the default

/// Gives a member a layout: a copy of the default with an empty "Feeds"
/// section at the end. Called wherever an account is made. A member who
/// has one already keeps it.
pub async fn create_member_copy<'c>(db: impl Handle<'c>, user_id: i64) -> sqlx::Result<i64> {
    let mut tx = db.open(KEY).begin().await?;
    ensure_default(&mut tx).await?;
    // Copying waits for any change to the default in progress, so the new
    // copy is of the default as it is once that is done.
    sqlx::query!("SELECT id FROM layouts WHERE user_id IS NULL FOR SHARE")
        .fetch_optional(&mut *tx)
        .await?;
    let created = sqlx::query_scalar!(
        "INSERT INTO layouts (user_id) VALUES ($1) ON CONFLICT (user_id) DO NOTHING RETURNING id",
        user_id
    )
    .fetch_optional(&mut *tx)
    .await?;
    let id = match created {
        Some(id) => {
            sqlx::query_scalar!(r#"SELECT layout_copy_default($1) AS "id!""#, id)
                .fetch_one(&mut *tx)
                .await?
        }
        None => {
            sqlx::query_scalar!("SELECT id FROM layouts WHERE user_id = $1", user_id)
                .fetch_one(&mut *tx)
                .await?
        }
    };
    tx.commit().await?;
    Ok(id)
}

/// Copies the default again into every member's layout that follows it
/// (one they haven't edited). The writes below do this themselves after
/// changing the default; it is for callers that changed it some other
/// way.
pub async fn default_changed<'c>(db: impl Handle<'c>) -> sqlx::Result<()> {
    let mut tx = db.open(KEY).begin().await?;
    let default = ensure_default(&mut tx).await?;
    lock_layout(&mut tx, default).await?;
    propagate_default(&mut tx).await?;
    tx.commit().await
}

async fn propagate_default(conn: &mut PgConnection) -> sqlx::Result<()> {
    sqlx::query_scalar!(r#"SELECT layout_propagate_default() AS "n!""#)
        .fetch_one(conn)
        .await?;
    Ok(())
}

/// What every change to a layout ends with: the default's followers are
/// brought up to date, or the member's copy is marked as edited.
async fn after_write(conn: &mut PgConnection, layout_id: i64) -> sqlx::Result<()> {
    let owner = sqlx::query_scalar!("SELECT user_id FROM layouts WHERE id = $1", layout_id)
        .fetch_one(&mut *conn)
        .await?;
    match owner {
        None => propagate_default(conn).await,
        Some(_) => {
            sqlx::query!(
                "UPDATE layouts SET edited_at = now() WHERE id = $1",
                layout_id
            )
            .execute(conn)
            .await?;
            Ok(())
        }
    }
}

/// Takes the layout's row lock for the rest of the transaction, so that
/// writers to one layout (and copies of the default into it) go one at a
/// time. Writes to the default lock it first and then, while copying, each
/// follower's layout in id order; a member's write locks only their own.
async fn lock_layout(conn: &mut PgConnection, layout_id: i64) -> sqlx::Result<()> {
    sqlx::query!("SELECT id FROM layouts WHERE id = $1 FOR UPDATE", layout_id)
        .fetch_optional(conn)
        .await?;
    Ok(())
}

async fn section_in_layout(
    conn: &mut PgConnection,
    layout_id: i64,
    section_id: i64,
) -> sqlx::Result<bool> {
    sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM layout_sections WHERE id = $1 AND layout_id = $2) AS "e!""#,
        section_id,
        layout_id
    )
    .fetch_one(conn)
    .await
}

/// What a step in arranging a layout did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Changed,
    /// The layout already was as asked, so nothing was written: a
    /// member's layout isn't marked as edited, and the default isn't
    /// copied out again (docs/v02-decisions.md, 30).
    Unchanged,
    /// The layout has no such section, or there is no such space.
    NotFound,
}

// ---------------------------------------------------------------------------
// Sections

/// Adds a section at the end of a layout, returning its id. The name has
/// been through [`clean_section_name`].
pub async fn add_section<'c>(db: impl Handle<'c>, layout_id: i64, name: &str) -> sqlx::Result<i64> {
    let mut tx = db.open(KEY).begin().await?;
    lock_layout(&mut tx, layout_id).await?;
    let id = sqlx::query_scalar!(
        "INSERT INTO layout_sections (layout_id, name, position)
         SELECT $1, $2, coalesce(max(position), -1) + 1 FROM layout_sections WHERE layout_id = $1
         RETURNING id",
        layout_id,
        name
    )
    .fetch_one(&mut *tx)
    .await?;
    after_write(&mut tx, layout_id).await?;
    tx.commit().await?;
    Ok(id)
}

/// Renames a section of the layout; giving it the name it has changes
/// nothing.
pub async fn rename_section<'c>(
    db: impl Handle<'c>,
    layout_id: i64,
    section_id: i64,
    name: &str,
) -> sqlx::Result<Step> {
    let mut tx = db.open(KEY).begin().await?;
    lock_layout(&mut tx, layout_id).await?;
    let done = sqlx::query!(
        "UPDATE layout_sections SET name = $3 WHERE id = $1 AND layout_id = $2 AND name <> $3",
        section_id,
        layout_id,
        name
    )
    .execute(&mut *tx)
    .await?;
    if done.rows_affected() == 0 {
        return unchanged_or_missing(&mut tx, layout_id, section_id).await;
    }
    after_write(&mut tx, layout_id).await?;
    tx.commit().await?;
    Ok(Step::Changed)
}

/// What a write to a section that changed no row did: nothing, if the
/// layout has the section.
async fn unchanged_or_missing(
    conn: &mut PgConnection,
    layout_id: i64,
    section_id: i64,
) -> sqlx::Result<Step> {
    Ok(if section_in_layout(conn, layout_id, section_id).await? {
        Step::Unchanged
    } else {
        Step::NotFound
    })
}

/// Gives a section of the layout a position among its sections (lower
/// first; equal positions keep their order of creation). Giving it the
/// position it has changes nothing.
pub async fn move_section<'c>(
    db: impl Handle<'c>,
    layout_id: i64,
    section_id: i64,
    position: i32,
) -> sqlx::Result<Step> {
    let mut tx = db.open(KEY).begin().await?;
    lock_layout(&mut tx, layout_id).await?;
    let done = sqlx::query!(
        "UPDATE layout_sections SET position = $3
         WHERE id = $1 AND layout_id = $2 AND position <> $3",
        section_id,
        layout_id,
        position
    )
    .execute(&mut *tx)
    .await?;
    if done.rows_affected() == 0 {
        return unchanged_or_missing(&mut tx, layout_id, section_id).await;
    }
    after_write(&mut tx, layout_id).await?;
    tx.commit().await?;
    Ok(Step::Changed)
}

/// Deletes a section of the layout, and so unfollows the spaces in it.
/// False if the layout has no such section.
pub async fn delete_section<'c>(
    db: impl Handle<'c>,
    layout_id: i64,
    section_id: i64,
) -> sqlx::Result<bool> {
    let mut tx = db.open(KEY).begin().await?;
    lock_layout(&mut tx, layout_id).await?;
    let done = sqlx::query!(
        "DELETE FROM layout_sections WHERE id = $1 AND layout_id = $2",
        section_id,
        layout_id
    )
    .execute(&mut *tx)
    .await?;
    if done.rows_affected() == 0 {
        return Ok(false);
    }
    after_write(&mut tx, layout_id).await?;
    tx.commit().await?;
    Ok(true)
}

/// Puts the sections named first, in this order, before any others of
/// the layout (which keep their order after them), renumbering them all
/// from 0. Not found if any of them isn't a section of the layout; the
/// order they already have changes nothing.
pub async fn reorder_sections<'c>(
    db: impl Handle<'c>,
    layout_id: i64,
    first: &[i64],
) -> sqlx::Result<Step> {
    let mut tx = db.open(KEY).begin().await?;
    lock_layout(&mut tx, layout_id).await?;
    let current = sqlx::query_scalar!(
        "SELECT id FROM layout_sections WHERE layout_id = $1 ORDER BY position, id",
        layout_id
    )
    .fetch_all(&mut *tx)
    .await?;
    if first.iter().any(|id| !current.contains(id)) {
        return Ok(Step::NotFound);
    }
    let mut order: Vec<i64> = Vec::with_capacity(current.len());
    for id in first.iter().chain(current.iter()) {
        if !order.contains(id) {
            order.push(*id);
        }
    }
    if order == current {
        return Ok(Step::Unchanged);
    }
    sqlx::query!(
        "UPDATE layout_sections s SET position = o.ord - 1
         FROM unnest($2::bigint[]) WITH ORDINALITY AS o (id, ord)
         WHERE s.layout_id = $1 AND s.id = o.id",
        layout_id,
        &order
    )
    .execute(&mut *tx)
    .await?;
    after_write(&mut tx, layout_id).await?;
    tx.commit().await?;
    Ok(Step::Changed)
}

/// How many members follow the default: have a layout they haven't
/// edited.
pub async fn follower_count<'c>(db: impl Handle<'c>) -> sqlx::Result<i64> {
    let mut conn = db.open(KEY).acquire().await?;
    sqlx::query_scalar!(
        r#"SELECT count(*) AS "n!" FROM layouts WHERE user_id IS NOT NULL AND edited_at IS NULL"#
    )
    .fetch_one(&mut *conn)
    .await
}

// ---------------------------------------------------------------------------
// Following

/// Takes a space out of whichever section of the layout it is in.
async fn remove_placement(
    conn: &mut PgConnection,
    layout_id: i64,
    space_id: i64,
) -> sqlx::Result<bool> {
    let done = sqlx::query!(
        "DELETE FROM layout_section_spaces x USING layout_sections s
         WHERE s.id = x.section_id AND s.layout_id = $1 AND x.space_id = $2",
        layout_id,
        space_id
    )
    .execute(conn)
    .await?;
    Ok(done.rows_affected() == 1)
}

fn no_such_space(err: &sqlx::Error) -> bool {
    matches!(err, sqlx::Error::Database(err) if err.is_foreign_key_violation())
}

/// Puts a space at the end of a section of the layout, taking it out of
/// any other section of that layout; for a member's layout, this also
/// unmutes it. False if the layout has no such section, or there is no
/// such space.
pub async fn follow<'c>(
    db: impl Handle<'c>,
    layout_id: i64,
    section_id: i64,
    space_id: i64,
) -> sqlx::Result<bool> {
    let mut tx = db.open(KEY).begin().await?;
    lock_layout(&mut tx, layout_id).await?;
    if !section_in_layout(&mut tx, layout_id, section_id).await? {
        return Ok(false);
    }
    remove_placement(&mut tx, layout_id, space_id).await?;
    let inserted = sqlx::query!(
        "INSERT INTO layout_section_spaces (section_id, space_id, position)
         SELECT $1, $2, coalesce(max(position), -1) + 1 FROM layout_section_spaces WHERE section_id = $1",
        section_id,
        space_id
    )
    .execute(&mut *tx)
    .await;
    match inserted {
        Ok(_) => {}
        Err(err) if no_such_space(&err) => return Ok(false),
        Err(err) => return Err(err),
    }
    unmute_for_owner(&mut tx, layout_id, space_id).await?;
    after_write(&mut tx, layout_id).await?;
    tx.commit().await?;
    Ok(true)
}

/// Putting a space in a member's sections unmutes it for them (a default
/// layout has no owner, so this does nothing to it).
async fn unmute_for_owner(
    conn: &mut PgConnection,
    layout_id: i64,
    space_id: i64,
) -> sqlx::Result<()> {
    sqlx::query!(
        "DELETE FROM space_mutes m USING layouts l
         WHERE l.id = $1 AND m.user_id = l.user_id AND m.space_id = $2",
        layout_id,
        space_id
    )
    .execute(conn)
    .await?;
    Ok(())
}

/// What "Follow" on a space's page does (DESIGN.md §3.5): puts the space
/// in the first section of the layout, or, if the layout has no sections,
/// in a new one called "Following". False if there is no such space.
pub async fn follow_in_first_section<'c>(
    db: impl Handle<'c>,
    layout_id: i64,
    space_id: i64,
) -> sqlx::Result<bool> {
    let mut tx = db.open(KEY).begin().await?;
    lock_layout(&mut tx, layout_id).await?;
    let first = sqlx::query_scalar!(
        "SELECT id FROM layout_sections WHERE layout_id = $1 ORDER BY position, id LIMIT 1",
        layout_id
    )
    .fetch_optional(&mut *tx)
    .await?;
    let section = match first {
        Some(id) => id,
        None => add_section(&mut tx, layout_id, FOLLOWING_SECTION).await?,
    };
    let done = follow(&mut tx, layout_id, section, space_id).await?;
    tx.commit().await?;
    Ok(done)
}

/// Takes a space out of the layout. False if it wasn't in it.
pub async fn unfollow<'c>(
    db: impl Handle<'c>,
    layout_id: i64,
    space_id: i64,
) -> sqlx::Result<bool> {
    let mut tx = db.open(KEY).begin().await?;
    lock_layout(&mut tx, layout_id).await?;
    if !remove_placement(&mut tx, layout_id, space_id).await? {
        return Ok(false);
    }
    after_write(&mut tx, layout_id).await?;
    tx.commit().await?;
    Ok(true)
}

/// Puts a space at `index` (from 0; past the end means last) in a section
/// of the layout, moving it from wherever in the layout it was, and
/// renumbers that section's spaces; for a member's layout, this also
/// unmutes it, as following does. Putting it where it already is
/// changes nothing.
pub async fn move_space<'c>(
    db: impl Handle<'c>,
    layout_id: i64,
    space_id: i64,
    section_id: i64,
    index: i32,
) -> sqlx::Result<Step> {
    let mut tx = db.open(KEY).begin().await?;
    lock_layout(&mut tx, layout_id).await?;
    if !section_in_layout(&mut tx, layout_id, section_id).await? {
        return Ok(Step::NotFound);
    }
    let before = sqlx::query_scalar!(
        "SELECT space_id FROM layout_section_spaces WHERE section_id = $1 ORDER BY position, space_id",
        section_id
    )
    .fetch_all(&mut *tx)
    .await?;
    let mut order: Vec<i64> = before
        .iter()
        .copied()
        .filter(|&id| id != space_id)
        .collect();
    let index = usize::try_from(index).unwrap_or(0).min(order.len());
    order.insert(index, space_id);
    if order == before {
        return Ok(Step::Unchanged);
    }
    remove_placement(&mut tx, layout_id, space_id).await?;
    let inserted = sqlx::query!(
        "INSERT INTO layout_section_spaces (section_id, space_id, position) VALUES ($1, $2, $3)",
        section_id,
        space_id,
        index as i32
    )
    .execute(&mut *tx)
    .await;
    match inserted {
        Ok(_) => {}
        Err(err) if no_such_space(&err) => return Ok(Step::NotFound),
        Err(err) => return Err(err),
    }
    sqlx::query!(
        "UPDATE layout_section_spaces x SET position = o.ord - 1
         FROM unnest($2::bigint[]) WITH ORDINALITY AS o (space_id, ord)
         WHERE x.section_id = $1 AND x.space_id = o.space_id",
        section_id,
        &order
    )
    .execute(&mut *tx)
    .await?;
    unmute_for_owner(&mut tx, layout_id, space_id).await?;
    after_write(&mut tx, layout_id).await?;
    tx.commit().await?;
    Ok(Step::Changed)
}

// ---------------------------------------------------------------------------
// Mutes

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MuteOutcome {
    Muted,
    AlreadyMuted,
    /// Essential spaces can't be muted (DESIGN.md §3.5).
    Essential,
    NoSuchSpace,
}

/// Mutes a space for a member: it leaves their sections and their front
/// page. Their layout isn't marked as edited by this, so one that follows
/// the default keeps doing so (and later copies of the default leave the
/// muted space out).
pub async fn mute<'c>(
    db: impl Handle<'c>,
    user_id: i64,
    space_id: i64,
) -> sqlx::Result<MuteOutcome> {
    let mut tx = db.open(KEY).begin().await?;
    let essential = sqlx::query_scalar!("SELECT essential FROM spaces WHERE id = $1", space_id)
        .fetch_optional(&mut *tx)
        .await?;
    match essential {
        None => return Ok(MuteOutcome::NoSuchSpace),
        Some(true) => return Ok(MuteOutcome::Essential),
        Some(false) => {}
    }
    let done = sqlx::query!(
        "INSERT INTO space_mutes (user_id, space_id) VALUES ($1, $2) ON CONFLICT DO NOTHING",
        user_id,
        space_id
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "DELETE FROM layout_section_spaces x USING layout_sections s, layouts l
         WHERE s.id = x.section_id AND l.id = s.layout_id AND l.user_id = $1 AND x.space_id = $2",
        user_id,
        space_id
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(if done.rows_affected() == 1 {
        MuteOutcome::Muted
    } else {
        MuteOutcome::AlreadyMuted
    })
}

/// Unmutes a space. It doesn't go back into the member's sections:
/// following it does that. False if it wasn't muted.
pub async fn unmute<'c>(db: impl Handle<'c>, user_id: i64, space_id: i64) -> sqlx::Result<bool> {
    let mut conn = db.open(KEY).acquire().await?;
    let done = sqlx::query!(
        "DELETE FROM space_mutes WHERE user_id = $1 AND space_id = $2",
        user_id,
        space_id
    )
    .execute(&mut *conn)
    .await?;
    Ok(done.rows_affected() == 1)
}

/// Whether a member has muted a space.
pub async fn is_muted<'c>(db: impl Handle<'c>, user_id: i64, space_id: i64) -> sqlx::Result<bool> {
    let mut conn = db.open(KEY).acquire().await?;
    sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM space_mutes WHERE user_id = $1 AND space_id = $2) AS "e!""#,
        user_id,
        space_id
    )
    .fetch_one(&mut *conn)
    .await
}

/// The spaces a member has muted, by id.
pub async fn muted_space_ids<'c>(db: impl Handle<'c>, user_id: i64) -> sqlx::Result<Vec<i64>> {
    let mut conn = db.open(KEY).acquire().await?;
    sqlx::query_scalar!(
        "SELECT space_id FROM space_mutes WHERE user_id = $1 ORDER BY space_id",
        user_id
    )
    .fetch_all(&mut *conn)
    .await
}

/// Whether a member has muted any space at all, which pages that leave
/// muted spaces out say.
pub async fn has_mutes<'c>(db: impl Handle<'c>, user_id: i64) -> sqlx::Result<bool> {
    let mut conn = db.open(KEY).acquire().await?;
    sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM space_mutes WHERE user_id = $1) AS "e!""#,
        user_id
    )
    .fetch_one(&mut *conn)
    .await
}

// ---------------------------------------------------------------------------
// Essential spaces

/// Marks a space essential, or not. Becoming essential unmutes it for
/// everyone. Returns whether anything changed.
pub async fn set_essential<'c>(
    db: impl Handle<'c>,
    space_id: i64,
    essential: bool,
) -> sqlx::Result<bool> {
    let mut tx = db.open(KEY).begin().await?;
    let done = sqlx::query!(
        "UPDATE spaces SET essential = $2, version = version + 1 WHERE id = $1 AND essential <> $2",
        space_id,
        essential
    )
    .execute(&mut *tx)
    .await?;
    if done.rows_affected() == 0 {
        return Ok(false);
    }
    if essential {
        sqlx::query!("DELETE FROM space_mutes WHERE space_id = $1", space_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::clean_section_name;

    #[test]
    fn section_names_are_one_tidy_line() {
        assert_eq!(
            clean_section_name("  Board \n games ").unwrap(),
            "Board games"
        );
        assert!(clean_section_name("  ").is_err());
        assert!(clean_section_name(&"x".repeat(81)).is_err());
    }
}
