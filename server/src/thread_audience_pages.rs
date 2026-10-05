//! A thread's audience page, `/t/{id}/audience` (DESIGN.md §3.4, §11):
//! who may read the thread and reply in it, naming groups and lists only
//! as DESIGN.md §3.3 allows, and, for the thread's author and the people
//! who run its space, the form that sets, changes or lifts its own
//! restriction. The decisions are made in [`writing::restrict_thread`].
//!
//! The page is for members who can read the thread, and for those who run
//! its space even when its restriction leaves them out: they may widen it
//! back to the space's audience, so they are shown who it names, but not
//! its title. What it says depends on who is looking, so it is never
//! cached. Restricting a thread clears the cached pages it was on (a
//! `ThreadChanged` change, made by [`writing::restrict_thread`]).
//!
//! The same fields, for reading only, let a member restrict a thread they
//! are starting ([`ReadPart`], [`part_grant`]), in the composer.

use askama::Template;
use axum::Form;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;

use crate::AppState;
use crate::admin_pages::DoneQuery;
use crate::content::{self, NamedGroup, ThreadAudience};
use crate::error::AppError;
use crate::format;
use crate::glass_pages;
use crate::groups::{self, GroupChoice};
use crate::messages::parse_recipients;
use crate::pages::parse_id;
use crate::permissions::{Grant, ThreadRestriction};
use crate::users;
use crate::viewer::Viewer;
use crate::web::{self, Layout, Nav, render, see_other};
use crate::write_pages::not_allowed;
use crate::writing::{self, Refused};

type Fields = Vec<(String, String)>;

/// One part of a restriction as typed: whether it restricts at all, the
/// groups and lists ticked (by the values the form offered), and the
/// members named, by username.
#[derive(Clone, Debug, Default)]
pub struct PartForm {
    pub restrict: bool,
    pub groups: Vec<String>,
    pub users: String,
}

impl PartForm {
    /// The part named `name` (`read` or `reply`) of a form's fields.
    pub fn from_fields(fields: &Fields, name: &str) -> PartForm {
        let mut part = PartForm::default();
        let (group, users) = (format!("{name}_group"), format!("{name}_users"));
        for (key, value) in fields {
            if key == name {
                part.restrict = value == "only";
            } else if *key == group {
                part.groups.push(value.clone());
            } else if *key == users {
                part.users = value.clone();
            }
        }
        part
    }

    /// A saved part, with everyone it names but those who are always
    /// named (the author, and a feed's owner), who aren't shown.
    async fn from_grant(
        state: &AppState,
        grant: Option<&Grant>,
        always: &[i64],
    ) -> Result<PartForm, AppError> {
        let Some(grant) = grant else {
            return Ok(PartForm::default());
        };
        let ids: Vec<i64> = grant
            .user_ids
            .iter()
            .copied()
            .filter(|id| !always.contains(id))
            .collect();
        let mut named = users::usernames_of(state.db(), &ids).await?;
        named.sort_by_key(|user| ids.iter().position(|id| *id == user.id));
        Ok(PartForm {
            restrict: true,
            groups: grant.group_ids.iter().map(i64::to_string).collect(),
            users: named
                .into_iter()
                .map(|user| user.username)
                .collect::<Vec<_>>()
                .join(", "),
        })
    }

    /// Whether a choice is ticked: by its value, as the form sends it
    /// back, or for a saved part by the groups it stands for.
    pub fn ticked(&self, choice: &GroupChoice) -> bool {
        self.groups.iter().any(|value| {
            value.trim() == choice.value || choice.ids.iter().any(|id| *value == id.to_string())
        })
    }
}

/// The restriction one part of the form asks for: `None` to leave it to
/// the space, or the groups and lists ticked (of `choices`, what the form
/// offered) and the members named, who must have accounts here or be
/// named by it already (`now`); or what is wrong with it.
pub async fn part_grant(
    state: &AppState,
    form: &PartForm,
    choices: &[GroupChoice],
    now: Option<&Grant>,
) -> Result<Result<Option<Grant>, Vec<String>>, AppError> {
    if !form.restrict {
        return Ok(Ok(None));
    }
    let mut problems = Vec::new();
    let group_ids = groups::chosen(choices, &form.groups).unwrap_or_else(|| {
        problems.push("Choose groups and lists from those offered.".to_owned());
        Vec::new()
    });
    let names = parse_recipients(&form.users);
    let named_now = match now {
        Some(grant) => users::usernames_of(state.db(), &grant.user_ids()).await?,
        None => Vec::new(),
    };
    let found = users::by_usernames(state.db(), &names).await?;
    let mut user_ids = Vec::new();
    for name in &names {
        let same = |user: &&users::NamedUser| user.username.eq_ignore_ascii_case(name);
        match named_now
            .iter()
            .find(same)
            .or_else(|| found.iter().find(same))
        {
            Some(user) => user_ids.push(user.id),
            None => problems.push(format!("There is no member called {name}.")),
        }
    }
    Ok(if problems.is_empty() {
        Ok(Some(Grant::listed(group_ids, user_ids)))
    } else {
        Err(problems)
    })
}

/// The read part of the new-thread form, "only these people can read it"
/// (DESIGN.md §3.4): what a member starting a thread may name.
pub struct ReadPart {
    pub form: PartForm,
    pub choices: Vec<GroupChoice>,
}

/// The form's two parts with what each offers.
struct AudienceForm {
    read: PartForm,
    reply: PartForm,
    read_choices: Vec<GroupChoice>,
    reply_choices: Vec<GroupChoice>,
}

#[derive(Template)]
#[template(path = "thread_audience.html")]
struct AudienceTemplate {
    layout: Layout,
    audience: ThreadAudience,
    /// The form, for those who may change the restriction.
    form: Option<AudienceForm>,
    /// Who is always named, for the form's hint.
    always: String,
    notice: Option<&'static str>,
    errors: Vec<String>,
}

impl AudienceTemplate {
    fn heading(&self) -> String {
        match &self.audience.target.thread {
            Some(thread) => format!("Who can take part in {}", thread.title),
            None => format!(
                "Who can take part in a thread in {}",
                self.audience.target.space.space.name
            ),
        }
    }

    fn space_name(&self) -> &str {
        &self.audience.target.space.space.name
    }

    /// A site group as "Organisers group", a list as it is named to the
    /// viewer, with a capital.
    fn group_heading(&self, group: &NamedGroup) -> String {
        if group.list {
            format::capitalised(&group.label)
        } else {
            format::group_name(&group.label)
        }
    }
}

/// Who always reads and replies, whatever the restriction says: the
/// thread's author and, in a feed, its owner (DESIGN.md §3.4), as the
/// form's hint says it to the viewer.
fn always_named(audience: &ThreadAudience, viewer: &Viewer) -> String {
    let me = viewer.user_id();
    let author = &audience.author;
    let mut text = if me == Some(author.id) {
        "You started it, so you always can.".to_owned()
    } else {
        format!("{} started it, so they always can.", author.display_name)
    };
    if let Some(owner) = &audience.target.space.owner
        && owner.user.id != author.id
    {
        if me == Some(owner.user.id) {
            text.push_str(" So can you: it is your feed.");
        } else {
            text.push_str(&format!(
                " So can {}, whose feed it is.",
                owner.user.display_name
            ));
        }
    }
    text
}

/// The audience of a thread the viewer may see it for: one they can read,
/// or one whose restriction they may change.
async fn audience_for(
    state: &AppState,
    viewer: &Viewer,
    id: &str,
) -> Result<Option<ThreadAudience>, AppError> {
    let Some(id) = parse_id(id) else {
        return Ok(None);
    };
    Ok(content::thread_audience(state.db(), viewer, id)
        .await?
        .filter(|audience| {
            audience.target.readable || writing::may_restrict(viewer, &audience.target)
        }))
}

async fn render_page(
    state: &AppState,
    viewer: &Viewer,
    status: StatusCode,
    audience: ThreadAudience,
    typed: Option<(PartForm, PartForm)>,
    notice: Option<&'static str>,
    errors: Vec<String>,
) -> Result<Response, AppError> {
    let restriction = &audience.target.restriction;
    let form = if writing::may_restrict(viewer, &audience.target) {
        let mut always = vec![audience.target.author_id];
        if let Some(owner) = &audience.target.space.owner {
            always.push(owner.user.id);
        }
        let (read, reply) = match typed {
            Some(typed) => typed,
            None => (
                PartForm::from_grant(state, restriction.read.as_ref(), &always).await?,
                PartForm::from_grant(state, restriction.reply.as_ref(), &always).await?,
            ),
        };
        let offered = async |grant: Option<&Grant>| {
            let named = grant.map(Grant::group_ids).unwrap_or_default();
            groups::choosable(state.db(), viewer.user_id(), &named).await
        };
        Some(AudienceForm {
            read,
            reply,
            read_choices: offered(restriction.read.as_ref()).await?,
            reply_choices: offered(restriction.reply.as_ref()).await?,
        })
    } else {
        None
    };
    render(
        status,
        &AudienceTemplate {
            layout: Layout::load(state, viewer, Nav::None).await?,
            always: always_named(&audience, viewer),
            audience,
            form,
            notice,
            errors,
        },
    )
}

/// `/t/{id}/audience`: who may read and reply in a thread, and for its
/// author and the people who run its space, the form to change that.
pub async fn page(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
    Query(query): Query<DoneQuery>,
) -> Result<Response, AppError> {
    if viewer.user().is_none() {
        return Ok(web::login_first(&format!("/t/{id}/audience")));
    }
    let Some(audience) = audience_for(&state, &viewer, &id).await? else {
        return web::not_found(&state, &viewer).await;
    };
    let notice = (query.done == "saved").then_some("Saved.");
    render_page(
        &state,
        &viewer,
        StatusCode::OK,
        audience,
        None,
        notice,
        Vec::new(),
    )
    .await
}

/// Saves the form: sets, changes or lifts the thread's restriction.
pub async fn save(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
    Form(fields): Form<Fields>,
) -> Result<Response, AppError> {
    let back = format!("/t/{id}/audience");
    if viewer.user().is_none() {
        return Ok(web::login_first(&back));
    }
    let Some(audience) = audience_for(&state, &viewer, &id).await? else {
        return web::not_found(&state, &viewer).await;
    };
    let thread_id = audience.target.thread_id;
    if !writing::may_restrict(&viewer, &audience.target) {
        return not_allowed(
            &state,
            &viewer,
            "Only the thread’s author and the people who run its space can change who may read it and reply in it.",
        )
        .await;
    }
    let read = PartForm::from_fields(&fields, "read");
    let reply = PartForm::from_fields(&fields, "reply");
    let now = &audience.target.restriction;
    let offered = async |grant: Option<&Grant>| {
        let named = grant.map(Grant::group_ids).unwrap_or_default();
        groups::choosable(state.db(), viewer.user_id(), &named).await
    };
    let read_choices = offered(now.read.as_ref()).await?;
    let reply_choices = offered(now.reply.as_ref()).await?;
    let mut problems = Vec::new();
    let read_grant = part_grant(&state, &read, &read_choices, now.read.as_ref())
        .await?
        .unwrap_or_else(|more| {
            problems.extend(more);
            None
        });
    let reply_grant = part_grant(&state, &reply, &reply_choices, now.reply.as_ref())
        .await?
        .unwrap_or_else(|more| {
            problems.extend(more);
            None
        });
    let restriction = ThreadRestriction {
        read: read_grant,
        reply: reply_grant,
    };
    let glass = fields
        .iter()
        .find(|(key, _)| key == glass_pages::REASON_FIELD)
        .map(|(_, value)| value.as_str());
    let mut outcome = if problems.is_empty() {
        writing::restrict_thread_breaking_glass(&state, &viewer, thread_id, &restriction, glass)
            .await?
    } else {
        problems.dedup();
        Err(Refused::Invalid(problems))
    };
    // Letting themselves read it is breaking glass, for staff, which the
    // confirmation page asks about first (DESIGN.md §8).
    if matches!(outcome, Err(Refused::BreaksGlass))
        || (glass.is_some() && matches!(&outcome, Err(Refused::Invalid(_))))
    {
        let opens = content::access::closed(
            state.db(),
            &viewer,
            crate::staff_access::Scope::Thread(thread_id),
        )
        .await?
        .into_iter()
        .collect();
        let action = format!("/t/{thread_id}/audience");
        match glass_pages::ask_change(
            &state,
            &viewer,
            opens,
            crate::glass_pages::Change {
                action: &action,
                fields: &fields,
                says: "You can’t read this thread, and saving this would let you.".to_owned(),
                button: "Break glass and save",
                back: &action,
            },
        )
        .await?
        {
            glass_pages::Confirmation::Ask(response) => return Ok(response),
            glass_pages::Confirmation::Given(reason) => {
                outcome = writing::restrict_thread_breaking_glass(
                    &state,
                    &viewer,
                    thread_id,
                    &restriction,
                    Some(&reason),
                )
                .await?;
            }
        }
    }
    match outcome {
        Ok(()) => Ok(see_other(&format!("/t/{thread_id}/audience?done=saved"))),
        Err(Refused::Invalid(problems)) => {
            render_page(
                &state,
                &viewer,
                StatusCode::BAD_REQUEST,
                audience,
                Some((read, reply)),
                None,
                problems,
            )
            .await
        }
        Err(Refused::LogIn) => Ok(web::login_first(&back)),
        Err(Refused::NotFound) => web::not_found(&state, &viewer).await,
        Err(Refused::NotAllowed(reason)) => not_allowed(&state, &viewer, reason).await,
        Err(Refused::NotAllowedBecause(reason)) => not_allowed(&state, &viewer, &reason).await,
        Err(Refused::RateLimited) => web::rate_limited(&state, &viewer).await,
        Err(Refused::BreaksGlass) => not_allowed(&state, &viewer, writing::BREAKS_GLASS).await,
    }
}
