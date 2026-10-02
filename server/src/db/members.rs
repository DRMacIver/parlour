//! Member profiles and the public invite tree (DESIGN.md §3.6).
//!
//! Profile details and who invited whom are public. The one thing on a
//! profile that depends on the viewer, the post count, comes from
//! [`crate::content`].

use std::collections::HashMap;

use chrono::{DateTime, Utc};

use crate::accounts::Role;
use crate::cache::Tag;
use crate::content::{self, Deps, UserRef};
use crate::db::Db;
use crate::viewer::Viewer;

pub struct Profile {
    pub user: UserRef,
    pub role: Role,
    pub title: Option<String>,
    pub pronouns: Option<String>,
    pub status_line: Option<String>,
    pub location: Option<String>,
    pub joined_at: DateTime<Utc>,
    /// Posts the viewer can see.
    pub post_count: i64,
    pub invited_by: Option<UserRef>,
    /// Members who joined with this member's invites, oldest first.
    pub invitees: Vec<Member>,
}

#[derive(Clone, Debug)]
pub struct Member {
    pub user: UserRef,
    pub joined_at: DateTime<Utc>,
}

/// A member's profile, by username (any capitalisation).
pub async fn profile(
    db: &Db,
    viewer: &Viewer,
    username: &str,
    deps: &mut Deps,
) -> sqlx::Result<Option<Profile>> {
    let Some(row) = sqlx::query!(
        r#"SELECT u.id AS "id!", u.username AS "username!", u.display_name AS "display_name!", u.role AS "role!",
                  u.title, u.pronouns, u.status_line, u.location,
                  u.created_at AS "created_at!", i.id AS "inviter_id?", i.username AS "inviter_username?",
                  i.display_name AS "inviter_display_name?"
           FROM users u LEFT JOIN users i ON i.id = u.invited_by_user_id
           WHERE u.username = $1::citext AND u.deleted_at IS NULL"#,
        username as _
    )
    .fetch_optional(db.pool())
    .await?
    else {
        return Ok(None);
    };
    deps.add(Tag::User(row.id));
    let invitees = sqlx::query!(
        "SELECT id, username, display_name, created_at FROM users
         WHERE invited_by_user_id = $1 AND deleted_at IS NULL
         ORDER BY created_at, id",
        row.id
    )
    .fetch_all(db.pool())
    .await?
    .into_iter()
    .map(|r| Member {
        user: UserRef {
            id: r.id,
            username: r.username.to_string(),
            display_name: r.display_name,
        },
        joined_at: r.created_at,
    })
    .collect();
    // New invitees clear `members`; renames clear `site`.
    deps.add(Tag::Members);
    let post_count = content::visible_post_count(db, viewer, row.id).await?;
    Ok(Some(Profile {
        user: UserRef {
            id: row.id,
            username: row.username.to_string(),
            display_name: row.display_name,
        },
        role: Role::parse(&row.role).unwrap_or(Role::User),
        title: row.title,
        pronouns: row.pronouns,
        status_line: row.status_line,
        location: row.location,
        joined_at: row.created_at,
        post_count,
        invited_by: match (
            row.inviter_id,
            row.inviter_username,
            row.inviter_display_name,
        ) {
            (Some(id), Some(username), Some(display_name)) => Some(UserRef {
                id,
                username: username.to_string(),
                display_name,
            }),
            _ => None,
        },
        invitees,
    }))
}

/// One member in the tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeMember {
    pub id: i64,
    pub username: String,
    pub display_name: String,
    pub role: Role,
    pub joined_at: DateTime<Utc>,
    /// How many members joined below this one, at any depth.
    pub descendants: usize,
    /// How far down the tree this member is: 0 for a member nobody here
    /// invited, 1 for someone they invited, and so on.
    pub depth: usize,
}

/// Below this depth, the tree stops indenting on narrow screens, and
/// marks each member's level instead, so that long invite chains fit.
pub const NARROW_INDENT_DEPTH: usize = 6;
/// The same for wide screens.
pub const WIDE_INDENT_DEPTH: usize = 16;

impl TreeMember {
    /// The level shown next to members below where the tree stops
    /// indenting, counting from 1.
    pub fn level(&self) -> usize {
        self.depth + 1
    }

    /// Whether this member is deeper than the narrow screen indents.
    pub fn past_narrow_indent(&self) -> bool {
        self.depth > NARROW_INDENT_DEPTH
    }

    /// Whether this member is deeper than wide screens indent.
    pub fn past_wide_indent(&self) -> bool {
        self.depth > WIDE_INDENT_DEPTH
    }

    /// Whether the list of this member's invitees is indented on narrow
    /// screens.
    pub fn indents_narrow(&self) -> bool {
        self.depth < NARROW_INDENT_DEPTH
    }

    /// The same for wide screens.
    pub fn indents_wide(&self) -> bool {
        self.depth < WIDE_INDENT_DEPTH
    }
}

/// The tree, flattened for the template: a member with nobody below them
/// is a `Leaf`; one with invitees is `Open`, followed by their subtree and
/// a `Close`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TreeItem {
    Leaf(TreeMember),
    Open(TreeMember),
    Close,
}

/// A member as read from the database, before arranging into a tree.
#[derive(Clone, Debug)]
pub struct TreeRow {
    pub member: TreeMember,
    pub invited_by: Option<i64>,
}

/// The whole invite tree, and the number of members in it.
pub async fn tree(db: &Db, deps: &mut Deps) -> sqlx::Result<(Vec<TreeItem>, usize)> {
    deps.add(Tag::Members);
    let rows = sqlx::query!(
        "SELECT id, username, display_name, role, created_at, invited_by_user_id FROM users
         WHERE deleted_at IS NULL
         ORDER BY created_at, id"
    )
    .fetch_all(db.pool())
    .await?;
    let rows: Vec<TreeRow> = rows
        .into_iter()
        .map(|r| TreeRow {
            member: TreeMember {
                id: r.id,
                username: r.username.to_string(),
                display_name: r.display_name,
                role: Role::parse(&r.role).unwrap_or(Role::User),
                joined_at: r.created_at,
                descendants: 0,
                depth: 0,
            },
            invited_by: r.invited_by_user_id,
        })
        .collect();
    let count = rows.len();
    Ok((arrange(rows), count))
}

/// Arranges members (in the order given, which siblings keep) into the
/// flattened tree. Members whose inviter isn't in the list are roots.
pub fn arrange(rows: Vec<TreeRow>) -> Vec<TreeItem> {
    let n = rows.len();
    let index: HashMap<i64, usize> = rows
        .iter()
        .enumerate()
        .map(|(i, row)| (row.member.id, i))
        .collect();
    let mut children = vec![Vec::new(); n];
    let mut has_parent = vec![false; n];
    for (i, row) in rows.iter().enumerate() {
        if let Some(&p) = row.invited_by.and_then(|id| index.get(&id))
            && p != i
        {
            children[p].push(i);
            has_parent[i] = true;
        }
    }

    // Walk down from the roots. The database can't hold a cycle, but if
    // there were one its members would be unreachable, so a second pass
    // starts from the oldest member not yet reached.
    let mut seen = vec![false; n];
    let mut tree_children: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut roots = Vec::new();
    let mut preorder = Vec::with_capacity(n);
    let starts = (0..n).filter(|&i| !has_parent[i]).chain(0..n);
    for start in starts {
        if seen[start] {
            continue;
        }
        seen[start] = true;
        roots.push(start);
        let mut stack = vec![start];
        while let Some(i) = stack.pop() {
            preorder.push(i);
            let kids: Vec<usize> = children[i].iter().copied().filter(|&k| !seen[k]).collect();
            for &k in &kids {
                seen[k] = true;
            }
            stack.extend(kids.iter().rev());
            tree_children[i] = kids;
        }
    }
    let mut descendants = vec![0usize; n];
    for &i in preorder.iter().rev() {
        descendants[i] = tree_children[i].iter().map(|&k| descendants[k] + 1).sum();
    }

    let mut items = Vec::with_capacity(n * 2);
    // (member, whether its subtree has been opened)
    let mut stack: Vec<(usize, bool)> = roots.iter().rev().map(|&r| (r, false)).collect();
    let mut depth = 0;
    while let Some((i, opened)) = stack.pop() {
        if opened {
            items.push(TreeItem::Close);
            depth -= 1;
            continue;
        }
        let member = TreeMember {
            descendants: descendants[i],
            depth,
            ..rows[i].member.clone()
        };
        if tree_children[i].is_empty() {
            items.push(TreeItem::Leaf(member));
        } else {
            items.push(TreeItem::Open(member));
            depth += 1;
            stack.push((i, true));
            stack.extend(tree_children[i].iter().rev().map(|&k| (k, false)));
        }
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use hegel::TestCase;
    use hegel::generators as gs;

    fn member(id: i64) -> TreeMember {
        TreeMember {
            id,
            username: format!("u{id}"),
            display_name: format!("U{id}"),
            role: Role::User,
            joined_at: DateTime::UNIX_EPOCH,
            descendants: 0,
            depth: 0,
        }
    }

    /// Members invited by earlier members, as the database guarantees.
    fn forest(tc: &TestCase) -> Vec<TreeRow> {
        let n = tc.draw(gs::integers::<usize>().max_value(60));
        (0..n)
            .map(|i| TreeRow {
                member: member(i as i64 + 1),
                invited_by: if i == 0 || tc.draw(gs::integers::<u8>().max_value(9)) == 0 {
                    None
                } else {
                    Some(tc.draw(gs::integers::<i64>().min_value(1).max_value(i as i64)))
                },
            })
            .collect()
    }

    /// Reads the flattened tree back into (member, parent, descendants).
    fn read(items: &[TreeItem]) -> Vec<(i64, Option<i64>, usize)> {
        let mut open: Vec<i64> = Vec::new();
        let mut out = Vec::new();
        for item in items {
            match item {
                TreeItem::Leaf(m) => out.push((m.id, open.last().copied(), m.descendants)),
                TreeItem::Open(m) => {
                    out.push((m.id, open.last().copied(), m.descendants));
                    open.push(m.id);
                }
                TreeItem::Close => {
                    open.pop().expect("Close matches an Open");
                }
            }
        }
        assert!(open.is_empty(), "every Open is closed");
        out
    }

    #[hegel::test]
    fn every_member_appears_once_under_their_inviter(tc: TestCase) {
        let rows = forest(&tc);
        let read = read(&arrange(rows.clone()));
        assert_eq!(read.len(), rows.len());
        let parents: HashMap<i64, Option<i64>> =
            read.iter().map(|(id, parent, _)| (*id, *parent)).collect();
        assert_eq!(parents.len(), rows.len());
        for row in &rows {
            assert_eq!(parents[&row.member.id], row.invited_by);
        }
    }

    #[hegel::test]
    fn descendants_are_counted(tc: TestCase) {
        let rows = forest(&tc);
        let read = read(&arrange(rows.clone()));
        for (id, _, descendants) in &read {
            // Count by walking up from every member.
            let mut expected = 0;
            for row in &rows {
                let mut cur = row.invited_by;
                while let Some(p) = cur {
                    if p == *id {
                        expected += 1;
                        break;
                    }
                    cur = rows.iter().find(|r| r.member.id == p).unwrap().invited_by;
                }
            }
            assert_eq!(*descendants, expected, "member {id}");
        }
    }

    #[hegel::test]
    fn depth_counts_inviters_up_to_the_root(tc: TestCase) {
        let rows = forest(&tc);
        let mut open = 0;
        for item in arrange(rows.clone()) {
            match item {
                TreeItem::Leaf(m) => assert_eq!(m.depth, open, "{}", m.id),
                TreeItem::Open(m) => {
                    assert_eq!(m.depth, open, "{}", m.id);
                    // Walk up from the member to a root.
                    let mut ancestors = 0;
                    let mut cur = rows
                        .iter()
                        .find(|r| r.member.id == m.id)
                        .unwrap()
                        .invited_by;
                    while let Some(p) = cur {
                        ancestors += 1;
                        cur = rows.iter().find(|r| r.member.id == p).unwrap().invited_by;
                    }
                    assert_eq!(m.depth, ancestors, "{}", m.id);
                    open += 1;
                }
                TreeItem::Close => open -= 1,
            }
        }
    }

    #[hegel::test]
    fn siblings_keep_their_order(tc: TestCase) {
        let rows = forest(&tc);
        let read = read(&arrange(rows.clone()));
        let position: HashMap<i64, usize> = read
            .iter()
            .enumerate()
            .map(|(i, (id, _, _))| (*id, i))
            .collect();
        for a in &rows {
            for b in &rows {
                if a.member.id < b.member.id && a.invited_by == b.invited_by {
                    assert!(position[&a.member.id] < position[&b.member.id]);
                }
            }
        }
    }

    #[test]
    fn cycles_and_missing_inviters_still_show_everyone() {
        let rows = vec![
            TreeRow {
                member: member(1),
                invited_by: Some(2),
            },
            TreeRow {
                member: member(2),
                invited_by: Some(1),
            },
            TreeRow {
                member: member(3),
                invited_by: Some(99),
            },
            TreeRow {
                member: member(4),
                invited_by: Some(4),
            },
        ];
        let read = read(&arrange(rows));
        let mut ids: Vec<i64> = read.iter().map(|(id, _, _)| *id).collect();
        ids.sort();
        assert_eq!(ids, vec![1, 2, 3, 4]);
    }
}
