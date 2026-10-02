//! Who may write where, and the "at most as visible" rule for quote-forks
//! (DESIGN.md §3.1, §3.2).
//!
//! Reading is checked in SQL by the access layer ([`crate::content`]);
//! these are the decisions made in Rust before a write. They work on
//! [`Audience`]s, which name the group sets of group-restricted categories
//! in full. Groups arrive in M5: until then a group-restricted category has
//! no groups, so only staff can read or write in it, and the rule below is
//! already complete for when they do.

use std::collections::BTreeSet;

use crate::viewer::{UserCtx, Viewer};

/// Who can read something.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Audience {
    /// Anyone, including anonymous visitors.
    Public,
    /// Any logged-in member.
    Members,
    /// Members of any of these groups (and staff).
    Groups(BTreeSet<i64>),
    /// The participants of a private thread (a DM).
    Private,
}

impl Audience {
    /// The audience of a category with this read level and these read
    /// groups. An unknown level is treated as the most private there is.
    pub fn from_level(level: &str, groups: impl IntoIterator<Item = i64>) -> Audience {
        match level {
            "public" => Audience::Public,
            "members" => Audience::Members,
            "groups" => Audience::Groups(groups.into_iter().collect()),
            _ => Audience::Private,
        }
    }
}

/// Whether everyone who can see `dest` can also see `source`, computed
/// conservatively (the table in DESIGN.md §3.1). Content in DMs can never
/// be quoted out, and DMs never contain quote-forks.
pub fn at_most_as_visible(source: &Audience, dest: &Audience) -> bool {
    use Audience::*;
    match (source, dest) {
        (Private, _) | (_, Private) => false,
        (Public, _) => true,
        (Members, Members | Groups(_)) => true,
        (Members, Public) => false,
        (Groups(gs), Groups(gd)) => gd.is_subset(gs),
        (Groups(_), Public | Members) => false,
    }
}

/// Who may start threads and reply in a category.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WriteLevel {
    /// Every member who can read the category.
    Members,
    /// Members of any of these groups (and staff).
    Groups(BTreeSet<i64>),
    /// Moderators and admins.
    Staff,
}

impl WriteLevel {
    /// An unknown level is treated as staff-only.
    pub fn from_level(level: &str, groups: impl IntoIterator<Item = i64>) -> WriteLevel {
        match level {
            "members" => WriteLevel::Members,
            "groups" => WriteLevel::Groups(groups.into_iter().collect()),
            _ => WriteLevel::Staff,
        }
    }
}

/// Whether a member can read something with this audience. The access
/// layer decides this in SQL for reads; this is the same rule, for checking
/// the destination of a write.
pub fn user_can_read(user: &UserCtx, audience: &Audience) -> bool {
    match audience {
        Audience::Public | Audience::Members => true,
        Audience::Groups(groups) => {
            user.role.is_staff() || user.group_ids.iter().any(|g| groups.contains(g))
        }
        // DMs (M6) check their participant list instead.
        Audience::Private => false,
    }
}

/// Whether the viewer may start threads and reply in a category that they
/// can read.
pub fn may_post(viewer: &Viewer, read: &Audience, write: &WriteLevel) -> bool {
    let Some(user) = viewer.user() else {
        return false;
    };
    user_can_read(user, read)
        && match write {
            WriteLevel::Members => true,
            WriteLevel::Groups(groups) => {
                user.role.is_staff() || user.group_ids.iter().any(|g| groups.contains(g))
            }
            WriteLevel::Staff => user.role.is_staff(),
        }
}

/// Whether the viewer may add posts to a thread they can read: they may
/// post in its category, and it isn't locked (staff can still post in
/// locked threads).
pub fn may_reply(viewer: &Viewer, read: &Audience, write: &WriteLevel, locked: bool) -> bool {
    may_post(viewer, read, write) && (!locked || viewer.is_staff())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::Role;
    use chrono::Utc;
    use hegel::TestCase;
    use hegel::generators::{self as gs, PrintableGenerator};

    fn groups(ids: &[i64]) -> Audience {
        Audience::Groups(ids.iter().copied().collect())
    }

    #[test]
    fn the_table_in_the_design() {
        use Audience::*;
        let g12 = groups(&[1, 2]);
        let g1 = groups(&[1]);
        let g3 = groups(&[3]);
        let none = groups(&[]);
        let yes = [
            (Public, Public),
            (Public, Members),
            (Public, g12.clone()),
            (Members, Members),
            (Members, g1.clone()),
            (Members, none.clone()),
            (g12.clone(), g1.clone()),
            (g12.clone(), g12.clone()),
            (g12.clone(), none.clone()),
            (none.clone(), none.clone()),
        ];
        for (s, d) in yes {
            assert!(at_most_as_visible(&s, &d), "{s:?} -> {d:?}");
        }
        let no = [
            (Public, Private),
            (Members, Public),
            (Members, Private),
            (g1.clone(), Public),
            (g1.clone(), Members),
            (g1.clone(), g12.clone()),
            (g1.clone(), g3.clone()),
            (none.clone(), g1.clone()),
            (Private, Public),
            (Private, Members),
            (Private, g1.clone()),
            (Private, Private),
        ];
        for (s, d) in no {
            assert!(!at_most_as_visible(&s, &d), "{s:?} -> {d:?}");
        }
    }

    // -- A model of who can see what, written independently of the rule.

    /// Somebody looking at the forum.
    #[derive(Clone, Debug)]
    enum Person {
        Anonymous,
        Member {
            id: i64,
            staff: bool,
            groups: BTreeSet<i64>,
        },
    }

    /// An audience together with, for a DM, who takes part in it.
    #[derive(Clone, Debug)]
    struct Place {
        audience: Audience,
        participants: BTreeSet<i64>,
    }

    fn sees(person: &Person, place: &Place) -> bool {
        match (person, &place.audience) {
            (_, Audience::Public) => true,
            (Person::Anonymous, _) => false,
            (Person::Member { .. }, Audience::Members) => true,
            (Person::Member { staff, groups, .. }, Audience::Groups(allowed)) => {
                *staff || !groups.is_disjoint(allowed)
            }
            // Staff don't read DMs (only reported posts in them, in M7).
            (Person::Member { id, .. }, Audience::Private) => place.participants.contains(id),
        }
    }

    fn group_ids() -> impl PrintableGenerator<BTreeSet<i64>> {
        gs::btree_sets(gs::integers::<i64>().min_value(0).max_value(4)).max_size(4)
    }

    fn place(tc: &TestCase) -> Place {
        let audience = match tc.draw(gs::integers::<u8>().max_value(3)) {
            0 => Audience::Public,
            1 => Audience::Members,
            2 => Audience::Groups(tc.draw(group_ids())),
            _ => Audience::Private,
        };
        Place {
            audience,
            participants: tc.draw(group_ids()),
        }
    }

    fn person(tc: &TestCase) -> Person {
        if tc.draw(gs::booleans()) {
            Person::Anonymous
        } else {
            Person::Member {
                id: tc.draw(gs::integers::<i64>().min_value(0).max_value(4)),
                staff: tc.draw(gs::booleans()),
                groups: tc.draw(group_ids()),
            }
        }
    }

    /// Nobody who can see the destination is ever shown a post they
    /// couldn't see where it was.
    #[hegel::test(test_cases = 1000)]
    fn forks_never_widen_the_audience(tc: TestCase) {
        let source = place(&tc);
        let dest = place(&tc);
        let viewer = person(&tc);
        if at_most_as_visible(&source.audience, &dest.audience) && sees(&viewer, &dest) {
            assert!(sees(&viewer, &source), "{viewer:?} {source:?} {dest:?}");
        }
    }

    /// The rule refuses only what it must: whenever it says no to a
    /// destination outside DMs, there is somebody who could see the
    /// destination but not the source.
    #[hegel::test(test_cases = 1000)]
    fn forks_are_refused_only_when_someone_would_see_more(tc: TestCase) {
        let source = place(&tc);
        let dest = place(&tc);
        tc.assume(dest.audience != Audience::Private);
        if at_most_as_visible(&source.audience, &dest.audience) {
            return;
        }
        let outsider = -1;
        let mut candidates = vec![
            Person::Anonymous,
            Person::Member {
                id: outsider,
                staff: false,
                groups: BTreeSet::new(),
            },
            Person::Member {
                id: outsider,
                staff: true,
                groups: BTreeSet::new(),
            },
        ];
        if let Audience::Groups(allowed) = &dest.audience {
            for g in allowed {
                candidates.push(Person::Member {
                    id: outsider,
                    staff: false,
                    groups: [*g].into(),
                });
            }
        }
        assert!(
            candidates
                .iter()
                .any(|p| sees(p, &dest) && !sees(p, &source)),
            "{source:?} -> {dest:?} refused, but nobody would see more"
        );
    }

    fn user(role: Role, groups: BTreeSet<i64>) -> UserCtx {
        UserCtx {
            id: 1,
            username: "ruth".into(),
            display_name: "Ruth".into(),
            email: "ruth@example.com".into(),
            email_verified: true,
            role,
            created_at: Utc::now(),
            group_ids: groups.into_iter().collect(),
        }
    }

    fn write_level(tc: &TestCase) -> WriteLevel {
        match tc.draw(gs::integers::<u8>().max_value(2)) {
            0 => WriteLevel::Members,
            1 => WriteLevel::Groups(tc.draw(group_ids())),
            _ => WriteLevel::Staff,
        }
    }

    /// Nobody can post where they can't read, anonymous visitors can't
    /// post at all, and posting follows the write level.
    #[hegel::test(test_cases = 1000)]
    fn posting_follows_the_levels(tc: TestCase) {
        let read = place(&tc).audience;
        let write = write_level(&tc);
        let role = [Role::User, Role::Moderator, Role::Admin]
            [tc.draw(gs::integers::<usize>().max_value(2))];
        let member_groups = tc.draw(group_ids());
        let viewer = Viewer::User(user(role, member_groups.clone()));
        let person = Person::Member {
            id: 1,
            staff: role.is_staff(),
            groups: member_groups.clone(),
        };
        let place = Place {
            audience: read.clone(),
            participants: BTreeSet::new(),
        };
        let allowed = may_post(&viewer, &read, &write);
        let expected = sees(&person, &place)
            && match &write {
                WriteLevel::Members => true,
                WriteLevel::Groups(g) => role.is_staff() || !g.is_disjoint(&member_groups),
                WriteLevel::Staff => role.is_staff(),
            };
        assert_eq!(
            allowed, expected,
            "{role:?} {member_groups:?} {read:?} {write:?}"
        );
        assert!(!may_post(&Viewer::Anonymous, &read, &write));
        let locked = tc.draw(gs::booleans());
        assert_eq!(
            may_reply(&viewer, &read, &write, locked),
            allowed && (!locked || role.is_staff())
        );
    }

    #[test]
    fn levels_parse() {
        assert_eq!(Audience::from_level("public", []), Audience::Public);
        assert_eq!(Audience::from_level("members", [3]), Audience::Members);
        assert_eq!(Audience::from_level("groups", [3]), groups(&[3]));
        assert_eq!(Audience::from_level("bogus", []), Audience::Private);
        assert_eq!(WriteLevel::from_level("members", []), WriteLevel::Members);
        assert_eq!(WriteLevel::from_level("staff", []), WriteLevel::Staff);
        assert_eq!(WriteLevel::from_level("bogus", []), WriteLevel::Staff);
    }
}
