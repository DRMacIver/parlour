//! Who is looking at a page (DESIGN.md §6). Every content query takes a
//! `&Viewer` and filters with the shared SQL functions `thread_readable`
//! and `space_readable`, which take the parameters [`Viewer::filter`]
//! produces.
//!
//! The session middleware ([`crate::sessions::middleware`]) works out the
//! viewer from the session cookie and leaves it in the request's
//! extensions, where the [`Viewer`] extractor finds it.
//!
//! Logged-in members can read public and members-only spaces. A listed
//! space's read grant adds it to [`ReadFilter`] for the members it lets in
//! (by name, by a group or list of theirs, or, for a grant to staff, by
//! their role), and the member's groups and lists, with who they are,
//! decide which threads with their own restriction (§3.4) they read.
//!
//! Staff roles grant powers, not readership (DESIGN.md §1, §3.1): a
//! moderator reads what a member with the same memberships reads. Beyond
//! that they read only under a break-glass grant (§8), whose threads and
//! spaces [`ReadFilter`] carries for the content functions that serve a
//! page under one, which record every such page in `staff_access_views`.

use std::convert::Infallible;

use axum::extract::FromRequestParts;
use axum::http::HeaderMap;
use axum::http::header::COOKIE;
use axum::http::request::Parts;
use chrono::{DateTime, Utc};

use crate::accounts::Role;

/// The name of the session cookie. A request carrying it is never answered
/// from the anonymous cache.
pub const SESSION_COOKIE: &str = "parlour_session";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Viewer {
    Anonymous,
    User(UserCtx),
}

/// A logged-in member.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserCtx {
    pub id: i64,
    pub username: String,
    pub display_name: String,
    pub email: String,
    pub email_verified: bool,
    pub role: Role,
    pub created_at: DateTime<Utc>,
    /// The groups this member belongs to.
    pub group_ids: Vec<i64>,
    /// The spaces those groups may read. They matter only while a
    /// space's read level is `groups`.
    pub restricted_space_ids: Vec<i64>,
    /// Until when a moderator has silenced this member: they can read but
    /// not write (DESIGN.md §8).
    pub silenced_until: Option<DateTime<Utc>>,
    /// Until when they are suspended: they read only what visitors read,
    /// and can do nothing but contest it.
    pub suspended_until: Option<DateTime<Utc>>,
    /// The members whose posts they have asked not to see: those they
    /// blocked, unless they are staff now (staff can't be blocked).
    pub blocked_user_ids: Vec<i64>,
    /// What reading under a break-glass grant needs (boxed, as most
    /// viewers never use it).
    pub glass: Box<Glass>,
}

/// The break-glass grants (DESIGN.md §8) a member holds now, while they
/// are staff, and the page they are being shown.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Glass {
    pub access: Vec<StaffAccess>,
    /// The page being rendered, path and query, against which a read
    /// under one of those grants is recorded. The session middleware sets
    /// it; outside a request it is `None`, and grants then open nothing,
    /// so that every read under a grant has a page to record.
    pub page: Option<String>,
}

/// One break-glass grant: a thread (or conversation) or a whole space a
/// staff member may read until it runs out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StaffAccess {
    pub id: i64,
    pub thread_id: Option<i64>,
    pub space_id: Option<i64>,
}

impl UserCtx {
    /// Whether the member is suspended now.
    pub fn is_suspended(&self) -> bool {
        self.suspended_until.is_some_and(|until| until > Utc::now())
    }

    /// Whether the member is silenced now, or suspended, which includes it.
    pub fn is_silenced(&self) -> bool {
        self.is_suspended() || self.silenced_until.is_some_and(|until| until > Utc::now())
    }

    /// Whether the member may do anything beyond reading and contesting
    /// moderators' actions: not while suspended.
    pub fn may_act(&self) -> bool {
        !self.is_suspended()
    }

    /// Whether the member may write things others read: posts, edits,
    /// messages, reactions and invites. Not while silenced or suspended.
    pub fn may_write(&self) -> bool {
        !self.is_silenced()
    }

    /// Whether the member is a moderator or admin, and not suspended.
    pub fn is_staff(&self) -> bool {
        self.role.is_staff() && !self.is_suspended()
    }
}

/// The bind parameters for the visibility condition in SQL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadFilter {
    /// Visibility levels readable everywhere (`$vis` in `thread_readable`).
    pub visibilities: Vec<String>,
    /// Group-restricted spaces this viewer may read.
    pub restricted_space_ids: Vec<i64>,
    /// Who the viewer is, for `dm_readable` and for the people a thread's
    /// own restriction names in `thread_readable`: `None` for visitors and
    /// for suspended members, who read no conversations and are named by
    /// no restriction.
    pub dm_viewer: Option<i64>,
    /// The groups and lists that let the viewer in where they are named,
    /// for a thread's own restriction: empty for visitors and suspended
    /// members.
    pub group_ids: Vec<i64>,
    /// The threads and spaces open to a staff member under break-glass
    /// grants, for `thread_granted`: empty for everyone else, for staff
    /// who are suspended, and outside a request. Only the content
    /// functions that serve a page under a grant use them, and they
    /// record the view (DESIGN.md §6, §8).
    pub granted_thread_ids: Vec<i64>,
    pub granted_space_ids: Vec<i64>,
}

impl Viewer {
    pub fn filter(&self) -> ReadFilter {
        // A suspended member reads only what visitors read (DESIGN.md
        // §3.1: members-only means members who are not suspended).
        let user = self.user().filter(|user| !user.is_suspended());
        let levels: &[&str] = match user {
            None => &["public"],
            Some(_) => &["public", "members"],
        };
        // Grants open something only to staff, and only on a page.
        let access: &[StaffAccess] = match user {
            Some(user) if user.is_staff() && user.glass.page.is_some() => &user.glass.access,
            _ => &[],
        };
        ReadFilter {
            visibilities: levels.iter().map(|level| (*level).to_owned()).collect(),
            restricted_space_ids: user
                .map(|user| user.restricted_space_ids.clone())
                .unwrap_or_default(),
            dm_viewer: user.map(|user| user.id),
            group_ids: user.map(|user| user.group_ids.clone()).unwrap_or_default(),
            granted_thread_ids: access.iter().filter_map(|a| a.thread_id).collect(),
            granted_space_ids: access.iter().filter_map(|a| a.space_id).collect(),
        }
    }

    /// The page being rendered, if the viewer reads under a grant there:
    /// what a read under one is recorded against.
    pub fn page(&self) -> Option<&str> {
        self.user()?.glass.page.as_deref()
    }

    pub fn user(&self) -> Option<&UserCtx> {
        match self {
            Viewer::Anonymous => None,
            Viewer::User(user) => Some(user),
        }
    }

    pub fn user_id(&self) -> Option<i64> {
        self.user().map(|user| user.id)
    }

    /// Whether the viewer is a moderator or admin who isn't suspended.
    pub fn is_staff(&self) -> bool {
        self.user().is_some_and(UserCtx::is_staff)
    }

    pub fn is_admin(&self) -> bool {
        self.user()
            .is_some_and(|user| user.role == Role::Admin && !user.is_suspended())
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Viewer {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Viewer, Infallible> {
        Ok(parts
            .extensions
            .get::<Viewer>()
            .cloned()
            .unwrap_or(Viewer::Anonymous))
    }
}

/// Whether the request carries a session cookie, valid or not.
pub fn has_session_cookie(headers: &HeaderMap) -> bool {
    session_cookie(headers).is_some()
}

/// The value of the session cookie, if the request has one.
pub fn session_cookie(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .find_map(|pair| {
            let (name, value) = pair.trim().split_once('=')?;
            (name.trim() == SESSION_COOKIE).then(|| value.trim())
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(cookies: &[&str]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for cookie in cookies {
            map.append(COOKIE, HeaderValue::from_str(cookie).unwrap());
        }
        map
    }

    fn user(role: Role) -> Viewer {
        Viewer::User(UserCtx {
            id: 1,
            username: "ruth".into(),
            display_name: "Ruth".into(),
            email: "ruth@example.com".into(),
            email_verified: true,
            role,
            created_at: Utc::now(),
            group_ids: Vec::new(),
            restricted_space_ids: Vec::new(),
            silenced_until: None,
            suspended_until: None,
            blocked_user_ids: Vec::new(),
            glass: Box::default(),
        })
    }

    #[test]
    fn detects_session_cookie() {
        assert!(!has_session_cookie(&headers(&[])));
        assert!(has_session_cookie(&headers(&["parlour_session=abc"])));
        assert!(has_session_cookie(&headers(&[
            "theme=dark; parlour_session=abc"
        ])));
        assert!(has_session_cookie(&headers(&[
            "theme=dark",
            "parlour_session="
        ])));
        assert!(!has_session_cookie(&headers(&["parlour_session_old=abc"])));
        assert!(!has_session_cookie(&headers(&["theme=parlour_session"])));
        assert_eq!(
            session_cookie(&headers(&["a=b; parlour_session=tok ; c=d"])),
            Some("tok")
        );
    }

    #[test]
    fn anonymous_reads_only_public_content() {
        assert_eq!(
            Viewer::Anonymous.filter(),
            ReadFilter {
                visibilities: vec!["public".into()],
                restricted_space_ids: vec![],
                dm_viewer: None,
                group_ids: vec![],
                granted_thread_ids: vec![],
                granted_space_ids: vec![],
            }
        );
    }

    #[test]
    fn members_read_members_only_content_and_staff_read_no_more() {
        for role in [Role::User, Role::Moderator, Role::Admin] {
            assert_eq!(user(role).filter().visibilities, vec!["public", "members"]);
        }
        assert!(!user(Role::User).is_staff());
        let Viewer::User(mut ctx) = user(Role::User) else {
            unreachable!()
        };
        ctx.group_ids = vec![4];
        ctx.restricted_space_ids = vec![7, 9];
        assert_eq!(
            Viewer::User(ctx).filter(),
            ReadFilter {
                visibilities: vec!["public".into(), "members".into()],
                restricted_space_ids: vec![7, 9],
                dm_viewer: Some(1),
                group_ids: vec![4],
                granted_thread_ids: vec![],
                granted_space_ids: vec![],
            }
        );
        assert!(user(Role::Moderator).is_staff());
        assert!(!user(Role::Moderator).is_admin());
        assert!(user(Role::Admin).is_admin());
    }

    fn with_access(role: Role, page: Option<&str>) -> UserCtx {
        let Viewer::User(mut ctx) = user(role) else {
            unreachable!()
        };
        ctx.glass.access = vec![
            StaffAccess {
                id: 10,
                thread_id: Some(5),
                space_id: None,
            },
            StaffAccess {
                id: 11,
                thread_id: None,
                space_id: Some(8),
            },
        ];
        ctx.glass.page = page.map(str::to_owned);
        ctx
    }

    #[test]
    fn grants_open_threads_and_spaces_to_staff_on_a_page() {
        for role in [Role::Moderator, Role::Admin] {
            let viewer = Viewer::User(with_access(role, Some("/t/5/x")));
            let filter = viewer.filter();
            assert_eq!(filter.granted_thread_ids, vec![5]);
            assert_eq!(filter.granted_space_ids, vec![8]);
            assert_eq!(filter.visibilities, vec!["public", "members"]);
            assert_eq!(viewer.page(), Some("/t/5/x"));
            // Outside a request there is no page to record a view against,
            // so the grants open nothing.
            let filter = Viewer::User(with_access(role, None)).filter();
            assert!(filter.granted_thread_ids.is_empty() && filter.granted_space_ids.is_empty());
            // Nor while suspended.
            let mut ctx = with_access(role, Some("/t/5/x"));
            ctx.suspended_until = Some(Utc::now() + chrono::Duration::days(1));
            assert_eq!(Viewer::User(ctx).filter(), Viewer::Anonymous.filter());
        }
        // A member who isn't staff opens nothing with them.
        let filter = Viewer::User(with_access(Role::User, Some("/t/5/x"))).filter();
        assert!(filter.granted_thread_ids.is_empty() && filter.granted_space_ids.is_empty());
    }

    #[test]
    fn suspended_members_read_what_visitors_read() {
        for role in [Role::User, Role::Moderator, Role::Admin] {
            let Viewer::User(mut ctx) = user(role) else {
                unreachable!()
            };
            ctx.restricted_space_ids = vec![7];
            ctx.group_ids = vec![3];
            ctx.suspended_until = Some(Utc::now() + chrono::Duration::days(1));
            let viewer = Viewer::User(ctx.clone());
            assert_eq!(viewer.filter(), Viewer::Anonymous.filter());
            assert!(!viewer.is_staff() && !viewer.is_admin());
            assert!(ctx.is_silenced() && !ctx.may_write() && !ctx.may_act());
            // Once it has run out, it no longer applies.
            ctx.suspended_until = Some(Utc::now() - chrono::Duration::seconds(1));
            assert!(!ctx.is_suspended() && ctx.may_act() && ctx.may_write());
            assert_eq!(Viewer::User(ctx).filter().dm_viewer, Some(1));
        }
    }

    #[test]
    fn silenced_members_read_but_dont_write() {
        let Viewer::User(mut ctx) = user(Role::User) else {
            unreachable!()
        };
        let before = Viewer::User(ctx.clone()).filter();
        ctx.silenced_until = Some(Utc::now() + chrono::Duration::hours(1));
        assert_eq!(Viewer::User(ctx.clone()).filter(), before);
        assert!(ctx.is_silenced() && !ctx.may_write() && ctx.may_act());
    }
}
