//! Who may do what in a space or a thread, and the "at most as visible"
//! rule for quote-forks (DESIGN.md §3.1, §3.4, §3.6).
//!
//! Reading is checked in SQL by the access layer ([`crate::content`]);
//! these are the decisions made in Rust before a write. A space holds a
//! [`Grant`] for each of four verbs: reading, replying, starting threads
//! and administering. A grant is a [`Level`] and, for the `listed` level,
//! the groups and people named. A thread may narrow reading and replying
//! further with its own [`ThreadRestriction`].

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
    /// A thread's own read restriction within its space's audience
    /// (DESIGN.md §3.4): those who can read the space and whom the
    /// restriction names. Made by [`Audience::narrowed`], whose `space` is
    /// never public, members or private.
    Within {
        space: Box<Audience>,
        groups: BTreeSet<i64>,
        users: BTreeSet<i64>,
    },
}

impl Audience {
    /// The audience of a thread in a space with this audience, given its
    /// own read restriction if it has one: the intersection of the two.
    /// Restricting a thread in a public or members-only space leaves just
    /// the people named (visitors and suspended members are never named),
    /// and a conversation has no restriction.
    pub fn narrowed(self, restriction: Option<&Grant>) -> Audience {
        let Some(restriction) = restriction else {
            return self;
        };
        let (groups, users) = (restriction.group_ids.clone(), restriction.user_ids.clone());
        match self {
            Audience::Public | Audience::Members => Audience::Restricted { groups, users },
            Audience::Private => Audience::Private,
            space @ (Audience::Staff | Audience::Restricted { .. } | Audience::Within { .. }) => {
                Audience::Within {
                    space: Box::new(space),
                    groups,
                    users,
                }
            }
        }
    }

    /// The audiences whose intersection this is.
    fn terms(&self) -> Vec<&Audience> {
        match self {
            Audience::Within { space, .. } => {
                let mut terms = space.terms();
                terms.push(self);
                terms
            }
            other => vec![other],
        }
    }

    /// The names a restriction adds, as an audience of its own.
    fn own_names(&self) -> Audience {
        match self {
            Audience::Within { groups, users, .. } => Audience::Restricted {
                groups: groups.clone(),
                users: users.clone(),
            },
            other => other.clone(),
        }
    }
}

/// Whether everyone who can see `dest` can also see `source`, computed
/// conservatively (the table in DESIGN.md §3.1): a restricted destination
/// must name a subset of what the source names, and names are compared,
/// not the people behind them, so the answer doesn't change as groups
/// change. Content in DMs can never be quoted out, and DMs never contain
/// quote-forks.
///
/// A thread narrowed by its own restriction ([`Audience::Within`]) is the
/// intersection of its space's audience and the names it gives, so the
/// table is applied to each part: every part of the source must hold
/// everyone in some part of the destination.
pub fn at_most_as_visible(source: &Audience, dest: &Audience) -> bool {
    let (sources, dests) = (source.terms(), dest.terms());
    sources.iter().all(|s| {
        dests
            .iter()
            .any(|d| basic_at_most_as_visible(&s.own_names(), &d.own_names()))
    })
}

/// The table in DESIGN.md §3.1, for audiences that aren't intersections.
fn basic_at_most_as_visible(source: &Audience, dest: &Audience) -> bool {
    use Audience::*;
    match (source, dest) {
        (Private, _) | (_, Private) => false,
        (Within { .. }, _) | (_, Within { .. }) => {
            unreachable!("intersections are taken apart first")
        }
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

/// Whether a thread's restriction of one verb leaves out someone its
/// space's grant of that verb lets in, comparing names: unless everyone the
/// grant lets in is let in by the grant narrowed by the restriction.
pub fn narrows(space: &Grant, restriction: Option<&Grant>) -> bool {
    restriction.is_some_and(|restriction| {
        let whole = space.audience();
        !at_most_as_visible(&whole.clone().narrowed(Some(restriction)), &whole)
    })
}

/// Whether a member can read something with this audience. The access
/// layer decides this in SQL for reads; this is the same rule, for checking
/// the destination of a write. Staff read only what their memberships let
/// them, like anyone else (DESIGN.md §3.1): a grant to staff lets them in,
/// their role alone doesn't. A break-glass grant (§8) opens pages to read,
/// never somewhere to write, so it plays no part here.
pub fn user_can_read(user: &UserCtx, audience: &Audience) -> bool {
    match audience {
        Audience::Public => true,
        // A suspended member reads only what visitors read.
        Audience::Members => !user.is_suspended(),
        Audience::Staff => user.is_staff(),
        Audience::Restricted { groups, users } => {
            !user.is_suspended()
                && (users.contains(&user.id) || user.group_ids.iter().any(|g| groups.contains(g)))
        }
        // DMs check their participant list instead.
        Audience::Private => false,
        Audience::Within { space, .. } => {
            user_can_read(user, space) && user_can_read(user, &audience.own_names())
        }
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
    /// lets them, and it isn't locked (those who moderate here, staff and
    /// the space's administrators, can still post in locked threads).
    pub fn may_reply(&self, viewer: &Viewer, locked: bool) -> bool {
        self.writer(viewer)
            .is_some_and(|user| self.reply.allows(user) && (!locked || self.may_administer(viewer)))
    }

    /// Whether the viewer may change the space's settings and moderate
    /// within it: pin, lock, rename and delete its threads, hide and delete
    /// posts in it, and move threads out of it (DESIGN.md §3.1, §3.2).
    /// Staff administer every space, whether or not they can read it
    /// (DESIGN.md §3.1: they act on what they can't read without being
    /// shown it); the administer grant names the rest, who must be able to
    /// read it. Archiving doesn't close this.
    pub fn may_administer(&self, viewer: &Viewer) -> bool {
        viewer.user().is_some_and(|user| {
            user.may_act()
                && (user.is_staff()
                    || (user_can_read(user, &self.read.audience()) && self.administer.allows(user)))
        })
    }

    /// Whether the viewer runs the space as anyone else the administer
    /// grant names would: it names them (by more than their role, since a
    /// grant to "staff" is the role again), and they can read the space.
    /// Staff who don't still administer it, and every such action of
    /// theirs is recorded in the moderation log (DESIGN.md §3.1, §8).
    pub fn runs_in_own_right(&self, viewer: &Viewer) -> bool {
        viewer.user().is_some_and(|user| {
            user.may_act()
                && self.administer.level != Level::Staff
                && self.administer.allows(user)
                && user_can_read(user, &self.read.audience())
        })
    }
}

/// A thread's own restriction (DESIGN.md §3.4): of those its space lets
/// read and reply, which may read it and which may reply in it. Each part
/// is a `listed` grant naming groups, lists and people, or `None` when the
/// thread inherits its space's grant. Starting threads and administering
/// stay with the space, and "nobody may reply" is the lock.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ThreadRestriction {
    pub read: Option<Grant>,
    pub reply: Option<Grant>,
}

impl ThreadRestriction {
    /// A restriction as the database holds it: each part's group and user
    /// ids, or `None` for a part the thread inherits.
    pub fn from_parts(
        read_groups: Option<&[i64]>,
        read_users: Option<&[i64]>,
        reply_groups: Option<&[i64]>,
        reply_users: Option<&[i64]>,
    ) -> ThreadRestriction {
        let part = |groups: Option<&[i64]>, users: Option<&[i64]>| match (groups, users) {
            (None, None) => None,
            (groups, users) => Some(Grant::listed(
                groups.unwrap_or_default().iter().copied(),
                users.unwrap_or_default().iter().copied(),
            )),
        };
        ThreadRestriction {
            read: part(read_groups, read_users),
            reply: part(reply_groups, reply_users),
        }
    }

    /// Whether the thread simply inherits its space's audience.
    pub fn is_inherited(&self) -> bool {
        self.read.is_none() && self.reply.is_none()
    }

    /// Whether each part names people and groups, as a restriction must:
    /// it can only narrow, so "every member" or "staff" mean nothing here.
    pub fn is_valid(&self) -> bool {
        [&self.read, &self.reply]
            .into_iter()
            .flatten()
            .all(|grant| grant.level == Level::Listed)
    }

    /// The same restriction with this member named in each part, so that
    /// it can never leave them out.
    pub fn naming(mut self, user_id: i64) -> ThreadRestriction {
        for grant in [&mut self.read, &mut self.reply].into_iter().flatten() {
            grant.user_ids.insert(user_id);
        }
        self
    }

    /// Whether everyone the reply restriction names is named for reading
    /// too, by the same names, when both parts restrict: repliers must be
    /// readers (DESIGN.md §3.4), and names are compared rather than the
    /// people behind them, as §3.1 compares audiences, so that the answer
    /// doesn't change as memberships do or tell anyone who is on a list.
    pub fn reply_within_read(&self) -> bool {
        match (&self.read, &self.reply) {
            (Some(read), Some(reply)) => {
                reply.group_ids.is_subset(&read.group_ids)
                    && reply.user_ids.is_subset(&read.user_ids)
            }
            _ => true,
        }
    }

    /// Whether its read restriction leaves out someone its space lets
    /// read, so that the thread is narrower than its space and shows its
    /// lock (DESIGN.md §3.4). Names are compared, as [`at_most_as_visible`]
    /// compares them: a restriction naming every group and person the
    /// space's grant names (and perhaps more) narrows nothing.
    pub fn narrows_reading(&self, space: &SpaceRules) -> bool {
        narrows(&space.read, self.read.as_ref())
    }

    /// The same for its reply restriction and the space's reply grant.
    pub fn narrows_replying(&self, space: &SpaceRules) -> bool {
        narrows(&space.reply, self.reply.as_ref())
    }

    /// Who can read the thread: its space's readers, narrowed by its own
    /// read restriction.
    pub fn audience(&self, space: &SpaceRules) -> Audience {
        space.read.audience().narrowed(self.read.as_ref())
    }

    /// Whether the viewer may add posts to the thread: the space lets them
    /// reply (and it isn't locked, or they are staff), they can read the
    /// thread, since repliers must be readers, and its own reply
    /// restriction, if any, names them.
    pub fn may_reply(&self, space: &SpaceRules, viewer: &Viewer, locked: bool) -> bool {
        space.may_reply(viewer, locked)
            && viewer.user().is_some_and(|user| {
                user_can_read(user, &self.audience(space))
                    && self.reply.as_ref().is_none_or(|grant| grant.allows(user))
            })
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

    #[test]
    fn a_restriction_narrows_unless_it_names_all_the_space_names() {
        let g1 = || Grant::listed([1], []);
        let cases = [
            (Grant::public(), None, false),
            (Grant::public(), Some(Grant::listed([1, 2], [7])), true),
            (Grant::members(), Some(Grant::listed([1], [])), true),
            (Grant::staff(), Some(Grant::listed([1], [7])), true),
            (g1(), None, false),
            (g1(), Some(g1()), false),
            (g1(), Some(Grant::listed([1, 2], [7])), false),
            (g1(), Some(Grant::listed([2], [7])), true),
            (g1(), Some(Grant::listed([], [7])), true),
            (Grant::listed([1], [7]), Some(g1()), true),
            (
                Grant::listed([1], [7]),
                Some(Grant::listed([1], [7])),
                false,
            ),
        ];
        for (space, restriction, narrower) in cases {
            assert_eq!(
                narrows(&space, restriction.as_ref()),
                narrower,
                "{space:?} narrowed by {restriction:?}"
            );
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
        sees_audience(person, &place.audience, &place.participants)
    }

    fn sees_audience(person: &Person, audience: &Audience, participants: &BTreeSet<i64>) -> bool {
        match (person, audience) {
            (_, Audience::Public) => true,
            (Person::Anonymous, _) => false,
            (Person::Member { .. }, Audience::Members) => true,
            (Person::Member { staff, .. }, Audience::Staff) => *staff,
            (
                Person::Member { id, groups, .. },
                Audience::Restricted {
                    groups: allowed,
                    users,
                },
            ) => users.contains(id) || !groups.is_disjoint(allowed),
            // Staff don't read DMs.
            (Person::Member { id, .. }, Audience::Private) => participants.contains(id),
            // A thread's restriction: they read its space, and it names
            // them. Staff are no exception (DESIGN.md §3.1).
            (
                Person::Member { id, groups, .. },
                Audience::Within {
                    space,
                    groups: allowed,
                    users,
                },
            ) => {
                sees_audience(person, space, participants)
                    && (users.contains(id) || !groups.is_disjoint(allowed))
            }
        }
    }

    fn ids() -> impl PrintableGenerator<BTreeSet<i64>> {
        gs::btree_sets(gs::integers::<i64>().min_value(0).max_value(4)).max_size(4)
    }

    fn audience(tc: &TestCase) -> Audience {
        let space = match tc.draw(gs::integers::<u8>().max_value(4)) {
            0 => Audience::Public,
            1 => Audience::Members,
            2 => Audience::Staff,
            3 => Audience::Restricted {
                groups: tc.draw(ids()),
                users: tc.draw(ids()),
            },
            _ => Audience::Private,
        };
        // A thread in the space may have its own read restriction.
        if tc.draw(gs::booleans()) {
            space.narrowed(Some(&Grant::listed(tc.draw(ids()), tc.draw(ids()))))
        } else {
            space
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
    /// couldn't see where it was, staff included: they read only what
    /// their memberships let them (DESIGN.md §3.1).
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
    /// destination outside DMs, there is somebody (not staff) who could see
    /// the destination but not the source. This holds of spaces and of
    /// threads that inherit their space's audience; for a thread narrowed
    /// by its own restriction the rule is applied to each part of the
    /// intersection, which may refuse more than it must (see
    /// `narrowing_only_ever_takes_destinations_away`).
    #[hegel::test(test_cases = 1000)]
    fn forks_are_refused_only_when_someone_would_see_more(tc: TestCase) {
        let source = place(&tc);
        let dest = place(&tc);
        // DMs are never quoted out of or into, whoever is in them.
        tc.assume(source.audience != Audience::Private);
        tc.assume(dest.audience != Audience::Private);
        tc.assume(!matches!(source.audience, Audience::Within { .. }));
        tc.assume(!matches!(dest.audience, Audience::Within { .. }));
        if at_most_as_visible(&source.audience, &dest.audience) {
            return;
        }
        // A staff-only destination is readable only by staff, who read
        // public and members-only sources but no restricted one unless
        // they are in it, which the rule, comparing names, can't know.
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
        // A listed destination naming nobody is readable by nobody, so
        // refusing a quote into it costs nothing.
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

    /// Narrowing a thread with its own restriction (DESIGN.md §3.4) can
    /// only take away places its posts may be quoted, and narrowing a
    /// destination can only add them: a narrower source is seen by fewer,
    /// a narrower destination by fewer still.
    #[hegel::test(test_cases = 1000)]
    fn narrowing_only_ever_takes_destinations_away(tc: TestCase) {
        let source = audience(&tc);
        let dest = audience(&tc);
        let restriction = Grant::listed(tc.draw(ids()), tc.draw(ids()));
        let narrowed_source = source.clone().narrowed(Some(&restriction));
        if at_most_as_visible(&narrowed_source, &dest) {
            assert!(
                at_most_as_visible(&source, &dest),
                "{source:?} -> {dest:?} refused, {narrowed_source:?} allowed"
            );
        }
        let narrowed_dest = dest.clone().narrowed(Some(&restriction));
        if at_most_as_visible(&source, &dest) {
            assert!(
                at_most_as_visible(&source, &narrowed_dest),
                "{source:?} -> {dest:?} allowed, -> {narrowed_dest:?} refused"
            );
        }
        // And a thread's posts may always go somewhere no wider than the
        // thread itself, its own space with the same restriction.
        if source != Audience::Private {
            assert!(at_most_as_visible(&narrowed_source, &narrowed_source));
        }
    }

    #[test]
    fn a_restriction_narrows_its_space() {
        let names = Grant::listed([1], [7]);
        let r17 = restricted(&[1], &[7]);
        assert_eq!(Audience::Public.narrowed(Some(&names)), r17);
        assert_eq!(Audience::Members.narrowed(Some(&names)), r17);
        assert_eq!(Audience::Private.narrowed(Some(&names)), Audience::Private);
        assert_eq!(Audience::Members.narrowed(None), Audience::Members);
        let within = restricted(&[1, 2], &[]).narrowed(Some(&names));
        // Quoting a post from a thread restricted to group 1 and person 7
        // in a space for groups 1 and 2: into a space for group 1, but not
        // one for group 2 alone or for all members.
        assert!(at_most_as_visible(&within, &restricted(&[1], &[])));
        assert!(!at_most_as_visible(&within, &restricted(&[2], &[])));
        assert!(!at_most_as_visible(&within, &Audience::Members));
        assert!(at_most_as_visible(&restricted(&[1, 2], &[]), &within));
        assert!(!at_most_as_visible(&restricted(&[2], &[]), &within));
        let mut ruth = user(Role::User, [2].into());
        ruth.id = 7;
        assert!(user_can_read(&ruth, &within));
        ruth.group_ids = vec![3];
        assert!(!user_can_read(&ruth, &within));
        // Staff read by their memberships too, not by their role.
        let mut moderator = user(Role::Moderator, BTreeSet::new());
        assert!(!user_can_read(&moderator, &within));
        assert!(!user_can_read(&moderator, &restricted(&[1, 2], &[])));
        assert!(user_can_read(&moderator, &Audience::Staff));
        moderator.group_ids = vec![2];
        moderator.id = 7;
        assert!(user_can_read(&moderator, &within));
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
            glass: Box::default(),
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
                && (!locked
                    || role.is_staff()
                    || (reads && model_allows(&rules.administer, &person))),
            "reply: {role:?} {member_groups:?} {restriction} {locked} {rules:?}"
        );
        assert_eq!(
            rules.may_administer(&viewer),
            restriction != 2
                && (role.is_staff() || (reads && model_allows(&rules.administer, &person))),
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

    #[test]
    fn repliers_are_named_among_readers() {
        let r = |read: Option<Grant>, reply: Option<Grant>| ThreadRestriction { read, reply };
        // Either part may inherit.
        assert!(r(None, None).reply_within_read());
        assert!(r(Some(Grant::listed([1], [7])), None).reply_within_read());
        assert!(r(None, Some(Grant::listed([1], [7]))).reply_within_read());
        // Both restricting: replying names no more than reading does.
        assert!(
            r(
                Some(Grant::listed([1, 2], [7, 8])),
                Some(Grant::listed([2], [8]))
            )
            .reply_within_read()
        );
        assert!(r(Some(Grant::listed([1], [7])), Some(Grant::listed([], []))).reply_within_read());
        assert!(
            !r(Some(Grant::listed([1], [7])), Some(Grant::listed([2], []))).reply_within_read()
        );
        // Even someone who is in a group reading names: names are compared.
        assert!(!r(Some(Grant::listed([1], [])), Some(Grant::listed([], [7]))).reply_within_read());
        // Naming someone in every part keeps it so.
        let named = r(Some(Grant::listed([1], [])), Some(Grant::listed([1], []))).naming(9);
        assert!(named.reply_within_read());
    }
}
