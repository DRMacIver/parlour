//! Who may do what in a space, and the "at most as visible" rule for
//! quote-forks (DESIGN.md §3.1, §3.6).
//!
//! Reading is checked in SQL by the access layer ([`crate::content`]);
//! these are the decisions made in Rust before a write. A space holds a
//! [`Grant`] for each of four verbs: reading, replying, starting threads
//! and administering. A grant is a [`Level`] and, for the `listed` level,
//! the groups and people named.

use std::collections::BTreeSet;

use crate::viewer::{UserCtx, Viewer};

/// The verbs a space grants.
pub const VERBS: [&str; 4] = ["read", "reply", "start", "administer"];

/// Who a grant lets through, before any names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// Anyone, including anonymous visitors. Only valid for reading.
    Public,
    /// Any logged-in member who isn't suspended.
    Members,
    /// Moderators and admins.
    Staff,
    /// Only the groups and people named.
    Listed,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::Public => "public",
            Level::Members => "members",
            Level::Staff => "staff",
            Level::Listed => "listed",
        }
    }

    pub fn parse(s: &str) -> Option<Level> {
        match s {
            "public" => Some(Level::Public),
            "members" => Some(Level::Members),
            "staff" => Some(Level::Staff),
            "listed" => Some(Level::Listed),
            _ => None,
        }
    }
}

/// Who may do one thing in a space.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grant {
    pub level: Level,
    /// With [`Level::Listed`], the groups whose members may.
    pub group_ids: BTreeSet<i64>,
    /// With [`Level::Listed`], the people who may.
    pub user_ids: BTreeSet<i64>,
}

impl Grant {
    fn of(level: Level) -> Grant {
        Grant {
            level,
            group_ids: BTreeSet::new(),
            user_ids: BTreeSet::new(),
        }
    }

    pub fn public() -> Grant {
        Grant::of(Level::Public)
    }

    pub fn members() -> Grant {
        Grant::of(Level::Members)
    }

    pub fn staff() -> Grant {
        Grant::of(Level::Staff)
    }

    pub fn listed(
        groups: impl IntoIterator<Item = i64>,
        users: impl IntoIterator<Item = i64>,
    ) -> Grant {
        Grant {
            level: Level::Listed,
            group_ids: groups.into_iter().collect(),
            user_ids: users.into_iter().collect(),
        }
    }

    /// A grant as the database holds it. Names are dropped for levels that
    /// don't use them, and an unknown level lets nobody but staff through.
    pub fn from_parts(level: &str, group_ids: &[i64], user_ids: &[i64]) -> Grant {
        match Level::parse(level) {
            Some(Level::Listed) => {
                Grant::listed(group_ids.iter().copied(), user_ids.iter().copied())
            }
            Some(level) => Grant::of(level),
            None => Grant::staff(),
        }
    }

    pub fn group_ids(&self) -> Vec<i64> {
        self.group_ids.iter().copied().collect()
    }

    pub fn user_ids(&self) -> Vec<i64> {
        self.user_ids.iter().copied().collect()
    }

    /// Whether the grant names this member, or a group they are in.
    pub fn names(&self, user: &UserCtx) -> bool {
        self.user_ids.contains(&user.id)
            || user.group_ids.iter().any(|g| self.group_ids.contains(g))
    }

    /// Whether this member gets through the grant. Suspended members get
    /// through nothing but `public`.
    pub fn allows(&self, user: &UserCtx) -> bool {
        match self.level {
            Level::Public => true,
            Level::Members => !user.is_suspended(),
            Level::Staff => user.is_staff(),
            Level::Listed => !user.is_suspended() && self.names(user),
        }
    }

    /// Who a read grant lets read.
    pub fn audience(&self) -> Audience {
        match self.level {
            Level::Public => Audience::Public,
            Level::Members => Audience::Members,
            Level::Staff => Audience::Staff,
            Level::Listed => Audience::Restricted {
                groups: self.group_ids.clone(),
                users: self.user_ids.clone(),
            },
        }
    }

    /// The visibility a space with this read grant has, as `spaces` and
    /// `threads` store it.
    pub fn visibility(&self) -> &'static str {
        match self.level {
            Level::Public => "public",
            Level::Members => "members",
            Level::Staff | Level::Listed => "restricted",
        }
    }
}

/// Who can read something.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Audience {
    /// Anyone, including anonymous visitors.
    Public,
    /// Any logged-in member.
    Members,
    /// Moderators and admins.
    Staff,
    /// The members of these groups and these people.
    Restricted {
        groups: BTreeSet<i64>,
        users: BTreeSet<i64>,
    },
    /// The participants of a private thread (a DM).
    Private,
}

/// Whether everyone who can see `dest` can also see `source`, computed
/// conservatively (the table in DESIGN.md §3.1): a restricted destination
/// must name a subset of what the source names, and names are compared,
/// not the people behind them, so the answer doesn't change as groups
/// change. Content in DMs can never be quoted out, and DMs never contain
/// quote-forks.
pub fn at_most_as_visible(source: &Audience, dest: &Audience) -> bool {
    use Audience::*;
    match (source, dest) {
        (Private, _) | (_, Private) => false,
        (Public, _) => true,
        (Members, Members | Staff | Restricted { .. }) => true,
        (Members, Public) => false,
        (Staff, Staff) => true,
        (Staff, Public | Members | Restricted { .. }) => false,
        (
            Restricted { groups, users },
            Restricted {
                groups: dest_groups,
                users: dest_users,
            },
        ) => dest_groups.is_subset(groups) && dest_users.is_subset(users),
        (Restricted { .. }, Public | Members | Staff) => false,
    }
}

/// Whether a member can read something with this audience. The access
/// layer decides this in SQL for reads; this is the same rule, for checking
/// the destination of a write. Staff read every space for now (DESIGN.md
/// §13, milestone 15 takes that away), as the SQL side does too.
pub fn user_can_read(user: &UserCtx, audience: &Audience) -> bool {
    match audience {
        Audience::Public => true,
        // A suspended member reads only what visitors read.
        Audience::Members => !user.is_suspended(),
        Audience::Staff => user.is_staff(),
        Audience::Restricted { groups, users } => {
            user.is_staff()
                || (!user.is_suspended()
                    && (users.contains(&user.id)
                        || user.group_ids.iter().any(|g| groups.contains(g))))
        }
        // DMs check their participant list instead.
        Audience::Private => false,
    }
}

/// What a write needs to know about a space.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpaceRules {
    pub read: Grant,
    pub reply: Grant,
    pub start: Grant,
    pub administer: Grant,
    /// An archived space is read-only.
    pub archived: bool,
}

impl SpaceRules {
    fn writer<'v>(&self, viewer: &'v Viewer) -> Option<&'v UserCtx> {
        let user = viewer.user()?;
        // Silenced and suspended members may not post anywhere; nobody
        // can post where they can't read, or in an archived space.
        (user.may_write() && !self.archived && user_can_read(user, &self.read.audience()))
            .then_some(user)
    }

    /// Whether the viewer may start threads here.
    pub fn may_start(&self, viewer: &Viewer) -> bool {
        self.writer(viewer)
            .is_some_and(|user| self.start.allows(user))
    }

    /// Whether the viewer may add posts to a thread here: the reply grant
    /// lets them, and it isn't locked (staff can still post in locked
    /// threads).
    pub fn may_reply(&self, viewer: &Viewer, locked: bool) -> bool {
        self.writer(viewer)
            .is_some_and(|user| self.reply.allows(user) && (!locked || user.is_staff()))
    }

    /// Whether the viewer may change the space's settings and moderate
    /// within it. Staff administer every space (DESIGN.md §3.1), the
    /// administer grant names the rest. Archiving doesn't close this.
    pub fn may_administer(&self, viewer: &Viewer) -> bool {
        viewer.user().is_some_and(|user| {
            user.may_act()
                && user_can_read(user, &self.read.audience())
                && (user.is_staff() || self.administer.allows(user))
        })
    }

    /// Whether the viewer administers the space in their own right, and
    /// not only as staff. Staff acting on other people's spaces is logged.
    pub fn administers_by_grant(&self, viewer: &Viewer) -> bool {
        viewer
            .user()
            .is_some_and(|user| user.may_act() && self.administer.allows(user))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::Role;
    use chrono::Utc;
    use hegel::TestCase;
    use hegel::generators::{self as gs, PrintableGenerator};

    fn restricted(groups: &[i64], users: &[i64]) -> Audience {
        Audience::Restricted {
            groups: groups.iter().copied().collect(),
            users: users.iter().copied().collect(),
        }
    }

    #[test]
    fn the_table_in_the_design() {
        use Audience::*;
        let g12 = restricted(&[1, 2], &[]);
        let g1 = restricted(&[1], &[]);
        let g3 = restricted(&[3], &[]);
        let u7 = restricted(&[], &[7]);
        let g1u7 = restricted(&[1], &[7]);
        let none = restricted(&[], &[]);
        let yes = [
            (Public, Public),
            (Public, Members),
            (Public, Staff),
            (Public, g12.clone()),
            (Members, Members),
            (Members, Staff),
            (Members, g1.clone()),
            (Members, none.clone()),
            (Staff, Staff),
            (g12.clone(), g1.clone()),
            (g12.clone(), g12.clone()),
            (g12.clone(), none.clone()),
            (g1u7.clone(), g1.clone()),
            (g1u7.clone(), u7.clone()),
            (none.clone(), none.clone()),
        ];
        for (s, d) in yes {
            assert!(at_most_as_visible(&s, &d), "{s:?} -> {d:?}");
        }
        let no = [
            (Public, Private),
            (Members, Public),
            (Members, Private),
            (Staff, Public),
            (Staff, Members),
            (Staff, g1.clone()),
            (Staff, none.clone()),
            (g1.clone(), Public),
            (g1.clone(), Members),
            (g1.clone(), Staff),
            (g1.clone(), g12.clone()),
            (g1.clone(), g3.clone()),
            (g1.clone(), u7.clone()),
            (u7.clone(), g1u7.clone()),
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
            (Person::Member { staff, .. }, Audience::Staff) => *staff,
            (
                Person::Member { id, staff, groups },
                Audience::Restricted {
                    groups: allowed,
                    users,
                },
            ) => *staff || users.contains(id) || !groups.is_disjoint(allowed),
            // Staff don't read DMs.
            (Person::Member { id, .. }, Audience::Private) => place.participants.contains(id),
        }
    }

    fn ids() -> impl PrintableGenerator<BTreeSet<i64>> {
        gs::btree_sets(gs::integers::<i64>().min_value(0).max_value(4)).max_size(4)
    }

    fn audience(tc: &TestCase) -> Audience {
        match tc.draw(gs::integers::<u8>().max_value(4)) {
            0 => Audience::Public,
            1 => Audience::Members,
            2 => Audience::Staff,
            3 => Audience::Restricted {
                groups: tc.draw(ids()),
                users: tc.draw(ids()),
            },
            _ => Audience::Private,
        }
    }

    fn place(tc: &TestCase) -> Place {
        Place {
            audience: audience(tc),
            participants: tc.draw(ids()),
        }
    }

    fn person(tc: &TestCase) -> Person {
        if tc.draw(gs::booleans()) {
            Person::Anonymous
        } else {
            Person::Member {
                id: tc.draw(gs::integers::<i64>().min_value(0).max_value(4)),
                staff: tc.draw(gs::booleans()),
                groups: tc.draw(ids()),
            }
        }
    }

    /// Nobody who can see the destination is ever shown a post they
    /// couldn't see where it was. (Staff see every space for now, so the
    /// check is for everyone else; the rule itself ignores that bypass.)
    #[hegel::test(test_cases = 1000)]
    fn forks_never_widen_the_audience(tc: TestCase) {
        let source = place(&tc);
        let dest = place(&tc);
        let viewer = person(&tc);
        tc.assume(!matches!(viewer, Person::Member { staff: true, .. }));
        if at_most_as_visible(&source.audience, &dest.audience) && sees(&viewer, &dest) {
            assert!(sees(&viewer, &source), "{viewer:?} {source:?} {dest:?}");
        }
    }

    /// The rule refuses only what it must: whenever it says no to a
    /// destination outside DMs, there is somebody (not staff) who could see
    /// the destination but not the source.
    #[hegel::test(test_cases = 1000)]
    fn forks_are_refused_only_when_someone_would_see_more(tc: TestCase) {
        let source = place(&tc);
        let dest = place(&tc);
        // DMs are never quoted out of or into, whoever is in them.
        tc.assume(source.audience != Audience::Private);
        tc.assume(dest.audience != Audience::Private);
        if at_most_as_visible(&source.audience, &dest.audience) {
            return;
        }
        // A staff-only destination is readable only by staff, who read every
        // source but a DM for now. What the rule refuses there is on
        // purpose: once staff stop reading everything, a staff space mustn't
        // hold a quote of a restricted one.
        if dest.audience == Audience::Staff {
            assert!(
                !matches!(
                    source.audience,
                    Audience::Public | Audience::Members | Audience::Staff
                ),
                "{source:?} -> staff refused"
            );
            return;
        }
        // A listed destination naming nobody is readable by nobody (but
        // staff, for now), so refusing a quote into it costs nothing.
        if matches!(&dest.audience, Audience::Restricted { groups, users }
            if groups.is_empty() && users.is_empty())
        {
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
        ];
        if let Audience::Restricted { groups, users } = &dest.audience {
            for g in groups {
                candidates.push(Person::Member {
                    id: outsider,
                    staff: false,
                    groups: [*g].into(),
                });
            }
            for u in users {
                candidates.push(Person::Member {
                    id: *u,
                    staff: false,
                    groups: BTreeSet::new(),
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
            restricted_space_ids: Vec::new(),
            silenced_until: None,
            suspended_until: None,
            blocked_user_ids: Vec::new(),
        }
    }

    fn grant(tc: &TestCase, read: bool) -> Grant {
        match tc.draw(gs::integers::<u8>().max_value(if read { 3 } else { 2 })) {
            0 => Grant::members(),
            1 => Grant::staff(),
            2 => Grant::listed(tc.draw(ids()), tc.draw(ids())),
            _ => Grant::public(),
        }
    }

    /// What a grant should let a member do, by the model.
    fn model_allows(grant: &Grant, person: &Person) -> bool {
        match (person, grant.level) {
            (_, Level::Public) => true,
            (Person::Anonymous, _) => false,
            (Person::Member { .. }, Level::Members) => true,
            (Person::Member { staff, .. }, Level::Staff) => *staff,
            (Person::Member { id, groups, .. }, Level::Listed) => {
                grant.user_ids.contains(id) || !groups.is_disjoint(&grant.group_ids)
            }
        }
    }

    /// Nobody can post where they can't read, anonymous visitors can't
    /// post at all, archived spaces are closed, and starting and replying
    /// follow their own grants.
    #[hegel::test(test_cases = 1000)]
    fn writing_follows_the_grants(tc: TestCase) {
        let rules = SpaceRules {
            read: grant(&tc, true),
            reply: grant(&tc, false),
            start: grant(&tc, false),
            administer: grant(&tc, false),
            archived: tc.draw(gs::booleans()),
        };
        let role = [Role::User, Role::Moderator, Role::Admin]
            [tc.draw(gs::integers::<usize>().max_value(2))];
        let member_groups = tc.draw(ids());
        // 0: in good standing; 1: silenced; 2: suspended; 3: either, but
        // it has run out.
        let restriction = tc.draw(gs::integers::<u8>().max_value(3));
        let mut ctx = user(role, member_groups.clone());
        let later = Some(Utc::now() + chrono::Duration::hours(1));
        let earlier = Some(Utc::now() - chrono::Duration::hours(1));
        match restriction {
            1 => ctx.silenced_until = later,
            2 => ctx.suspended_until = later,
            3 => {
                ctx.silenced_until = earlier;
                ctx.suspended_until = earlier;
            }
            _ => {}
        }
        let viewer = Viewer::User(ctx);
        let restricted = matches!(restriction, 1 | 2);
        let person = if restriction == 2 {
            // Suspended members read what visitors read.
            Person::Anonymous
        } else {
            Person::Member {
                id: 1,
                staff: role.is_staff(),
                groups: member_groups.clone(),
            }
        };
        let place = Place {
            audience: rules.read.audience(),
            participants: BTreeSet::new(),
        };
        let reads = sees(&person, &place);
        assert_eq!(
            user_can_read(viewer.user().unwrap(), &rules.read.audience()),
            reads,
            "{role:?} {member_groups:?} {restriction} {:?}",
            rules.read
        );
        let may_write = !restricted && !rules.archived && reads;
        assert_eq!(
            rules.may_start(&viewer),
            may_write && model_allows(&rules.start, &person),
            "start: {role:?} {member_groups:?} {restriction} {rules:?}"
        );
        let locked = tc.draw(gs::booleans());
        assert_eq!(
            rules.may_reply(&viewer, locked),
            may_write
                && model_allows(&rules.reply, &person)
                && (!locked || (role.is_staff() && restriction != 2)),
            "reply: {role:?} {member_groups:?} {restriction} {locked} {rules:?}"
        );
        assert_eq!(
            rules.may_administer(&viewer),
            restriction != 2
                && reads
                && (role.is_staff() || model_allows(&rules.administer, &person)),
            "administer: {role:?} {member_groups:?} {restriction} {rules:?}"
        );
        assert!(!rules.may_start(&Viewer::Anonymous));
        assert!(!rules.may_reply(&Viewer::Anonymous, false));
        assert!(!rules.may_administer(&Viewer::Anonymous));
    }

    #[test]
    fn grants_parse() {
        assert_eq!(Grant::from_parts("public", &[], &[]), Grant::public());
        assert_eq!(Grant::from_parts("members", &[3], &[]), Grant::members());
        assert_eq!(Grant::from_parts("staff", &[], &[]), Grant::staff());
        assert_eq!(
            Grant::from_parts("listed", &[3, 3, 1], &[7]),
            Grant::listed([1, 3], [7])
        );
        assert_eq!(Grant::from_parts("bogus", &[1], &[]), Grant::staff());
        assert_eq!(Grant::public().visibility(), "public");
        assert_eq!(Grant::members().visibility(), "members");
        assert_eq!(Grant::staff().visibility(), "restricted");
        assert_eq!(Grant::listed([1], []).visibility(), "restricted");
        for level in [Level::Public, Level::Members, Level::Staff, Level::Listed] {
            assert_eq!(Level::parse(level.as_str()), Some(level));
        }
    }
}
