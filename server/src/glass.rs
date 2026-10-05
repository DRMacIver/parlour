//! Breaking glass (DESIGN.md §8): how a member of staff comes to read
//! something their own memberships don't let them.
//!
//! Three things lead here, and each shows one confirmation page
//! ([`crate::glass_pages`]) that names what would be opened (this
//! conversation, this thread, or this whole space), asks why in free text
//! and says who will be told: opening reported content they can't read
//! (from the moderation queue), opening a thread or space they can't read
//! from a staff page (a thread's moderation page, a space's settings), and
//! changing who may read something, or a group's members, so that it would
//! let them in. No second approver is needed, and no fresh login: the
//! session is enough.
//!
//! Confirming opens a `staff_access` grant on each thing for
//! [`staff_access::HOURS`] hours, through [`notify::open_staff_access`],
//! which tells everyone who could read it at that moment, at once. A
//! change of who may read something tells those who could read it before
//! the change: theirs is the privacy it touched, and they are told even if
//! the change shuts them out. The change, its grants and their notices are
//! written in one transaction ([`record`]).

use chrono::{DateTime, Duration, Utc};

use crate::AppState;
use crate::content::{self, access::Closed};
use crate::db::Tx;
use crate::error::AppError;
use crate::moderation::{MAX_REASON_CHARS, clean_line, staff};
use crate::notify;
use crate::staff_access::{self, NewAccess, Scope};
use crate::viewer::{UserCtx, Viewer};
use crate::writing::{Outcome, Refused};

/// What a member of staff asks to read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ask {
    /// The thread or conversation a reported post is in.
    Report(i64),
    /// A thread or conversation, by id.
    Thread(i64),
    /// A whole space, by its address.
    Space(String),
}

/// What breaking glass would open, as the confirmation page describes it.
#[derive(Clone, Debug)]
pub struct Proposal {
    pub opens: Vec<Closed>,
    /// How many people would be told: everyone who can read any of it,
    /// but the one breaking glass.
    pub readers: usize,
    /// The report that led here, if one did.
    pub report_id: Option<i64>,
    /// When a grant opened now would run out.
    pub expires_at: DateTime<Utc>,
}

/// Where an [`Ask`] leads.
#[derive(Clone, Debug)]
pub enum Prospect {
    /// Somewhere the viewer reads already, by their memberships or under a
    /// grant still running: no glass to break.
    Open(String),
    /// The confirmation page.
    Confirm(Proposal),
}

/// The problem with a reason for breaking glass, if there is one.
pub fn reason_problem(reason: &str) -> Option<String> {
    let reason = clean_line(reason);
    if reason.is_empty() {
        Some("Say why you need to read it. It goes in the access log.".to_owned())
    } else if reason.chars().count() > MAX_REASON_CHARS {
        Some(format!("Keep the reason to {MAX_REASON_CHARS} characters."))
    } else {
        None
    }
}

/// Whether a grant the member holds now already covers this.
fn covered(user: &UserCtx, closed: &Closed) -> bool {
    user.glass.access.iter().any(|access| match closed.scope {
        Scope::Thread(id) => {
            access.thread_id == Some(id)
                || (access.space_id.is_some() && access.space_id == closed.space_id)
        }
        Scope::Space(id) => access.space_id == Some(id),
    })
}

/// The scope an ask names, and the report behind it, or `None` if there
/// is no such thing.
async fn scope_of(
    state: &AppState,
    viewer: &Viewer,
    ask: &Ask,
) -> Result<Option<(Scope, Option<i64>)>, AppError> {
    let db = state.db();
    Ok(match ask {
        Ask::Report(report_id) => content::access::reported(db, viewer, *report_id)
            .await?
            .map(|(thread_id, _)| (Scope::Thread(thread_id), Some(*report_id))),
        Ask::Thread(id) => Some((Scope::Thread(*id), None)),
        Ask::Space(slug) => content::administered_space(db, viewer, slug)
            .await?
            .map(|space| (Scope::Space(space.space.id), None)),
    })
}

/// What asking to read something leads to: straight there, if the viewer
/// may read it already, or the confirmation page.
pub async fn prospect(state: &AppState, viewer: &Viewer, ask: &Ask) -> Outcome<Prospect> {
    let user = match staff(viewer) {
        Ok(user) => user,
        Err(refusal) => return Ok(Err(refusal)),
    };
    let db = state.db();
    let Some((scope, report_id)) = scope_of(state, viewer, ask).await? else {
        return Ok(Err(Refused::NotFound));
    };
    if let Some(href) = content::access::open_href(db, viewer, scope).await? {
        return Ok(Ok(Prospect::Open(href)));
    }
    let Some(closed) = content::access::closed(db, viewer, scope).await? else {
        return Ok(Err(Refused::NotFound));
    };
    if covered(user, &closed) {
        return Ok(Ok(Prospect::Open(closed.href)));
    }
    Ok(Ok(Prospect::Confirm(
        proposal(state, user, vec![closed], report_id).await?,
    )))
}

/// The confirmation page's account of opening these.
pub async fn proposal(
    state: &AppState,
    user: &UserCtx,
    opens: Vec<Closed>,
    report_id: Option<i64>,
) -> Result<Proposal, AppError> {
    let mut readers = std::collections::BTreeSet::new();
    for closed in &opens {
        readers.extend(staff_access::readers(state.db(), closed.scope, user.id).await?);
    }
    Ok(Proposal {
        opens,
        readers: readers.len(),
        report_id,
        expires_at: Utc::now() + Duration::hours(staff_access::HOURS),
    })
}

/// Breaks glass on what `ask` names, for `reason`: opens a grant and tells
/// everyone who can read it. Returns where reading it starts. Asking
/// again while a grant already covers it (the form sent twice, or again
/// from the browser's history) opens nothing more and tells nobody again.
pub async fn break_glass(
    state: &AppState,
    viewer: &Viewer,
    ask: &Ask,
    reason: &str,
) -> Outcome<String> {
    let user = match staff(viewer) {
        Ok(user) => user,
        Err(refusal) => return Ok(Err(refusal)),
    };
    let db = state.db();
    let Some((scope, report_id)) = scope_of(state, viewer, ask).await? else {
        return Ok(Err(Refused::NotFound));
    };
    if let Some(href) = content::access::open_href(db, viewer, scope).await? {
        return Ok(Ok(href));
    }
    let Some(closed) = content::access::closed(db, viewer, scope).await? else {
        return Ok(Err(Refused::NotFound));
    };
    if covered(user, &closed) {
        return Ok(Ok(closed.href));
    }
    if let Some(problem) = reason_problem(reason) {
        return Ok(Err(Refused::Invalid(vec![problem])));
    }
    let readers = readers(state, user, std::slice::from_ref(&closed)).await?;
    let mut tx = db.begin().await?;
    // One sent at the same moment may have opened it since.
    if staff_access::covered(&mut tx, user.id, closed.scope).await? {
        return Ok(Ok(closed.href));
    }
    let recorded = record(&mut tx, user, &readers, reason, report_id, false).await?;
    tx.commit().await?;
    recorded.announce(state).await?;
    Ok(Ok(closed.href))
}

/// Who can read each of these now, but the member breaking glass: those
/// to tell. A change of who may read something asks this before making
/// the change.
pub async fn readers(
    state: &AppState,
    user: &UserCtx,
    opens: &[Closed],
) -> Result<Vec<(Scope, Vec<i64>)>, AppError> {
    let mut readers = Vec::new();
    for closed in opens {
        readers.push((
            closed.scope,
            staff_access::readers(state.db(), closed.scope, user.id).await?,
        ));
    }
    Ok(readers)
}

/// Grants opened in a transaction not yet committed, and who they told.
#[must_use = "announce the grants once the transaction is committed"]
pub struct Recorded {
    ids: Vec<i64>,
    told: Vec<i64>,
}

impl Recorded {
    /// Once the transaction that opened them is committed: the pages of
    /// what they opened are cleared from the cache, and those told see it
    /// on their badge and are emailed. Returns the grants' ids.
    pub async fn announce(self, state: &AppState) -> Result<Vec<i64>, AppError> {
        for &id in &self.ids {
            state
                .changed(crate::changes::Change::StaffAccessOpened { access_id: id })
                .await?;
        }
        notify::staff_access_told(state, &self.told);
        Ok(self.ids)
    }
}

/// Opens a grant on each scope, for `reason`, telling its readers, in
/// `tx`: the transaction that also makes the change opening them, if a
/// change does, so that the change is never made without its grants and
/// their notices (DESIGN.md §8). `by_change` says that the change itself
/// makes the member of staff a reader, so that the logs say they read it
/// for as long as the change stands. [`Recorded::announce`] them once it
/// is committed.
pub async fn record(
    tx: &mut Tx,
    user: &UserCtx,
    opens: &[(Scope, Vec<i64>)],
    reason: &str,
    report_id: Option<i64>,
    by_change: bool,
) -> Result<Recorded, AppError> {
    let reason = clean_line(reason);
    let mut recorded = Recorded {
        ids: Vec::new(),
        told: Vec::new(),
    };
    for (scope, readers) in opens {
        let (id, told) = notify::open_staff_access(
            tx,
            &NewAccess {
                actor_id: user.id,
                scope: *scope,
                report_id,
                reason: &reason,
                by_change,
            },
            readers,
        )
        .await?;
        recorded.ids.push(id);
        recorded.told.extend(told);
    }
    recorded.told.sort();
    recorded.told.dedup();
    Ok(recorded)
}
