//! The moderation pages (DESIGN.md §8; ModQueue.dc.html):
//!
//! - for everyone: reporting a post or member, the contact form (which
//!   works without an account), the public moderation log at
//!   `/moderation`, a held post's page for its author, and contesting an
//!   action;
//! - for staff: the queue at `/mod/queue`, the full log, members with
//!   their notes and addresses, and the forms for every action.
//!
//! Every form works without JavaScript. The decisions are made in
//! [`crate::moderation`]; staff pages are never cached.

use askama::Template;
use axum::Form;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, Uri};
use axum::response::Response;

use crate::AppState;
use crate::client::ClientIp;
use crate::content::moderation::{ENTRIES_PER_PAGE, LogEntry, Scope};
use crate::content::{self, Deps, PostTarget, SpaceTarget, ThreadTarget, page_count};
use crate::error::AppError;
use crate::mod_records::{
    self, HeldPost, IpRecord, MemberRecord, MemberRow, Note, REASONS, Restriction,
};
use crate::moderation::{
    self, Destination, PostAction, QueueItem, QueueKind, Reversal, ThreadAction,
};
use crate::pages::parse_id;
use crate::pagination::{PageItem, Pagination, page_href};
use crate::permissions::Audience;
use crate::settings::{self, ModerationSettings};
use crate::viewer::Viewer;
use crate::web::{self, Layout, Nav, cacheable, redirect_unless_canonical, render, see_other};
use crate::write_pages::not_allowed;
use crate::writing::Refused;

type Fields = Vec<(String, String)>;

fn field<'a>(fields: &'a Fields, name: &str) -> &'a str {
    fields
        .iter()
        .find(|(key, _)| key == name)
        .map_or("", |(_, value)| value.as_str())
}

/// Where to go after an action: the `back` field if it is a path on this
/// site, otherwise `default`.
fn back_to(fields: &Fields, default: &str) -> String {
    web::safe_next(field(fields, "back"))
        .unwrap_or(default)
        .to_owned()
}

/// The report a decision taken from the queue answers.
fn report_field(fields: &Fields) -> Option<i64> {
    parse_id(field(fields, "report"))
}

/// Shows a refusal from a one-click form: there is no form to show again,
/// so problems are listed with a link back.
pub(crate) async fn refused(
    state: &AppState,
    viewer: &Viewer,
    refusal: Refused,
    back: &str,
) -> Result<Response, AppError> {
    match refusal {
        Refused::LogIn => Ok(web::login_first(back)),
        Refused::NotFound => web::not_found(state, viewer).await,
        Refused::NotAllowed(why) => not_allowed(state, viewer, why).await,
        Refused::NotAllowedBecause(why) => not_allowed(state, viewer, &why).await,
        Refused::Invalid(problems) => {
            let problems: Vec<&str> = problems.iter().map(String::as_str).collect();
            web::message(
                state,
                viewer,
                StatusCode::BAD_REQUEST,
                "That didn’t work",
                &problems,
                Some((back, "Go back")),
            )
            .await
        }
        Refused::RateLimited => web::rate_limited(state, viewer).await,
        Refused::BreaksGlass => not_allowed(state, viewer, crate::writing::BREAKS_GLASS).await,
    }
}

/// The `?page=` asked for, alongside other parameters.
fn page_param(params: &Fields) -> Option<i64> {
    match params.iter().find(|(k, _)| k == "page") {
        None => Some(1),
        Some((_, v)) => v
            .parse()
            .ok()
            .filter(|&n: &i64| n >= 1 && *v == n.to_string()),
    }
}

fn params_of(uri: &Uri) -> Fields {
    Query::<Fields>::try_from_uri(uri)
        .map(|Query(p)| p)
        .unwrap_or_default()
}

/// The choices of how long to silence or suspend for.
pub const DURATIONS: [(u32, &str); 6] = [
    (1, "1 day"),
    (3, "3 days"),
    (7, "7 days"),
    (30, "30 days"),
    (365, "a year"),
    (moderation::MAX_DAYS, "indefinitely"),
];

// ---------------------------------------------------------------------------
// Reporting

#[derive(Template)]
#[template(path = "report.html")]
struct ReportTemplate {
    layout: Layout,
    /// What is being reported, as a heading.
    heading: String,
    action: String,
    /// The post being reported, if it is one.
    post: Option<PostTarget>,
    /// Whether what is reported is private, so that the form says what
    /// opening it would mean.
    private: Private,
    reasons: &'static [(&'static str, &'static str)],
    reason: String,
    details: String,
    errors: Vec<String>,
    cancel: String,
}

async fn render_report(
    state: &AppState,
    viewer: &Viewer,
    status: StatusCode,
    about: ReportPage,
    reason: String,
    details: String,
    errors: Vec<String>,
) -> Result<Response, AppError> {
    let (heading, action, private, cancel, post) = match about {
        ReportPage::Post(post) => (
            format!("Report № {} by {}", post.seq, post.author.display_name),
            format!("/p/{}/report", post.post_id),
            Private::of(&post.thread),
            format!("/p/{}", post.post_id),
            Some(*post),
        ),
        ReportPage::Member(username, name) => (
            format!("Report {name}"),
            format!("/u/{username}/report"),
            Private::No,
            format!("/u/{username}"),
            None,
        ),
    };
    render(
        status,
        &ReportTemplate {
            layout: Layout::load(state, viewer, Nav::None).await?,
            heading,
            action,
            post,
            private,
            reasons: &REASONS,
            reason,
            details,
            errors,
            cancel,
        },
    )
}

/// Whether a reported post is somewhere not every moderator may read
/// (DESIGN.md §8): the report form tells its reporter that, if the
/// moderators open it, everyone who can read it will be told.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Private {
    /// Every moderator reads it, as a member or by a grant to staff.
    No,
    /// A thread whose audience leaves some of them out.
    Thread,
    /// A conversation, which only the people in it read.
    Conversation,
}

impl Private {
    fn of(thread: &ThreadTarget) -> Private {
        match &thread.place {
            content::Place::Conversation(_) => Private::Conversation,
            content::Place::Space(space) => match thread.restriction.audience(&space.rules) {
                Audience::Public | Audience::Members | Audience::Staff => Private::No,
                _ => Private::Thread,
            },
            content::Place::Unreadable => Private::Thread,
        }
    }
}

enum ReportPage {
    Post(Box<PostTarget>),
    Member(String, String),
}

async fn reportable_post(
    state: &AppState,
    viewer: &Viewer,
    post_id: i64,
) -> Result<Option<PostTarget>, AppError> {
    Ok(content::post_target(state.db(), viewer, post_id)
        .await?
        .filter(PostTarget::is_live))
}

pub async fn report_post_page(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let back = format!("/p/{id}/report");
    if viewer.user().is_none() {
        return Ok(web::login_first(&back));
    }
    let Some(post_id) = parse_id(&id) else {
        return web::not_found(&state, &viewer).await;
    };
    let Some(post) = reportable_post(&state, &viewer, post_id).await? else {
        return web::not_found(&state, &viewer).await;
    };
    render_report(
        &state,
        &viewer,
        StatusCode::OK,
        ReportPage::Post(Box::new(post)),
        String::new(),
        String::new(),
        Vec::new(),
    )
    .await
}

pub async fn report_post(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
    Form(fields): Form<Fields>,
) -> Result<Response, AppError> {
    let back = format!("/p/{id}/report");
    let Some(post_id) = parse_id(&id) else {
        return web::not_found(&state, &viewer).await;
    };
    let (reason, details) = (field(&fields, "reason"), field(&fields, "details"));
    match moderation::report_post(&state, &viewer, post_id, reason, details).await? {
        Ok(()) => reported(&state, &viewer, &format!("/p/{post_id}")).await,
        Err(Refused::Invalid(problems)) => match reportable_post(&state, &viewer, post_id).await? {
            Some(post) => {
                render_report(
                    &state,
                    &viewer,
                    StatusCode::BAD_REQUEST,
                    ReportPage::Post(Box::new(post)),
                    reason.to_owned(),
                    details.to_owned(),
                    problems,
                )
                .await
            }
            None => web::not_found(&state, &viewer).await,
        },
        Err(refusal) => refused(&state, &viewer, refusal, &back).await,
    }
}

async fn reported(state: &AppState, viewer: &Viewer, back: &str) -> Result<Response, AppError> {
    web::message(
        state,
        viewer,
        StatusCode::OK,
        "Thank you",
        &["The moderators will look at your report. Nobody else is told who sent it."],
        Some((back, "Go back")),
    )
    .await
}

pub async fn report_member_page(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(username): Path<String>,
) -> Result<Response, AppError> {
    let back = format!("/u/{username}/report");
    if viewer.user().is_none() {
        return Ok(web::login_first(&back));
    }
    let Some(member) = crate::social::members_named(state.db(), std::slice::from_ref(&username))
        .await?
        .into_iter()
        .next()
    else {
        return web::not_found(&state, &viewer).await;
    };
    render_report(
        &state,
        &viewer,
        StatusCode::OK,
        ReportPage::Member(member.user.username, member.user.display_name),
        String::new(),
        String::new(),
        Vec::new(),
    )
    .await
}

pub async fn report_member(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(username): Path<String>,
    Form(fields): Form<Fields>,
) -> Result<Response, AppError> {
    let back = format!("/u/{username}/report");
    let (reason, details) = (field(&fields, "reason"), field(&fields, "details"));
    match moderation::report_member(&state, &viewer, &username, reason, details).await? {
        Ok(member) => reported(&state, &viewer, &format!("/u/{}", member.username)).await,
        Err(Refused::Invalid(problems)) => {
            let Some(member) =
                crate::social::members_named(state.db(), std::slice::from_ref(&username))
                    .await?
                    .into_iter()
                    .next()
            else {
                return web::not_found(&state, &viewer).await;
            };
            render_report(
                &state,
                &viewer,
                StatusCode::BAD_REQUEST,
                ReportPage::Member(member.user.username, member.user.display_name),
                reason.to_owned(),
                details.to_owned(),
                problems,
            )
            .await
        }
        Err(refusal) => refused(&state, &viewer, refusal, &back).await,
    }
}

// ---------------------------------------------------------------------------
// The contact form

#[derive(Template)]
#[template(path = "contact.html")]
struct ContactTemplate {
    layout: Layout,
    email: String,
    message: String,
    errors: Vec<String>,
    sent: bool,
    logged_in: bool,
}

/// The contact form. For a visitor it is the same page for everyone, so it
/// is cached like any other anonymous page; `?post=` (from a post's
/// "report" link) starts the message with a link to the post. Only posts
/// the viewer can see have such a page: otherwise every made-up id would
/// put another page in the anonymous cache.
pub async fn contact_page(
    State(state): State<AppState>,
    viewer: Viewer,
    uri: Uri,
) -> Result<Response, AppError> {
    let params = params_of(&uri);
    let (message, canonical) = match params.as_slice() {
        [] => (String::new(), "/contact".to_owned()),
        [(key, value)] if key == "post" => {
            let post = match parse_id(value) {
                Some(id) => content::post_location(state.db(), &viewer, id)
                    .await?
                    .map(|_| id),
                None => None,
            };
            let Some(id) = post else {
                return web::not_found(&state, &viewer).await;
            };
            (
                format!(
                    "About this post: {}\n\n",
                    state.config().url(&format!("/p/{id}"))
                ),
                format!("/contact?post={id}"),
            )
        }
        [(key, value)] if key == "sent" && value == "1" => {
            (String::new(), "/contact?sent=1".to_owned())
        }
        _ => return web::not_found(&state, &viewer).await,
    };
    if let Some(redirect) = redirect_unless_canonical(&uri, &canonical) {
        return Ok(redirect);
    }
    let deps = Deps::new();
    cacheable(
        &ContactTemplate {
            layout: Layout::load(&state, &viewer, Nav::None).await?,
            email: String::new(),
            message,
            errors: Vec::new(),
            sent: canonical == "/contact?sent=1",
            logged_in: viewer.user().is_some(),
        },
        &deps,
    )
}

pub async fn contact(
    State(state): State<AppState>,
    viewer: Viewer,
    ClientIp(ip): ClientIp,
    Form(fields): Form<Fields>,
) -> Result<Response, AppError> {
    let (email, message) = (field(&fields, "email"), field(&fields, "message"));
    match moderation::contact(&state, &viewer, &ip, email, message).await? {
        Ok(()) => Ok(see_other("/contact?sent=1")),
        Err(refusal) => {
            let (status, errors) = match refusal {
                Refused::Invalid(problems) => (StatusCode::BAD_REQUEST, problems),
                Refused::RateLimited => (
                    StatusCode::TOO_MANY_REQUESTS,
                    vec!["Too many messages have come from here recently. Wait a while and try again.".to_owned()],
                ),
                other => return refused(&state, &viewer, other, "/contact").await,
            };
            render(
                status,
                &ContactTemplate {
                    layout: Layout::load(&state, &viewer, Nav::None).await?,
                    email: email.to_owned(),
                    message: message.to_owned(),
                    errors,
                    sent: false,
                    logged_in: viewer.user().is_some(),
                },
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Held posts and appeals

#[derive(Template)]
#[template(path = "held.html")]
struct HeldTemplate {
    layout: Layout,
    held: HeldPost,
    place: (String, String),
    /// What it says, unless the viewer is staff who couldn't read it once
    /// it is up.
    html: Option<String>,
    staff: bool,
}

pub async fn held_page(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let back = format!("/held/{id}");
    let Some(id) = parse_id(&id) else {
        return web::not_found(&state, &viewer).await;
    };
    let held = match moderation::held_for(&state, &viewer, id).await? {
        Ok(held) => held,
        Err(refusal) => return refused(&state, &viewer, refusal, &back).await,
    };
    let shown = moderation::held_shown(&state, &viewer, &held).await?;
    let place = moderation::held_place(&state, &viewer, &held.place, shown).await?;
    let html = if shown {
        Some(
            crate::store::render_post(state.db(), &held.body_md)
                .await?
                .html,
        )
    } else {
        None
    };
    render(
        StatusCode::OK,
        &HeldTemplate {
            layout: Layout::load(&state, &viewer, Nav::None).await?,
            place,
            html,
            staff: viewer.is_staff(),
            held,
        },
    )
}

#[derive(Template)]
#[template(path = "appeal.html")]
struct AppealTemplate {
    layout: Layout,
    entry: Option<LogEntry>,
    action_id: i64,
    details: String,
    errors: Vec<String>,
}

async fn render_appeal(
    state: &AppState,
    viewer: &Viewer,
    status: StatusCode,
    action_id: i64,
    details: String,
    errors: Vec<String>,
) -> Result<Response, AppError> {
    let entry = content::moderation::entry_for_member(state.db(), viewer, action_id).await?;
    render(
        status,
        &AppealTemplate {
            layout: Layout::load(state, viewer, Nav::None).await?,
            entry,
            action_id,
            details,
            errors,
        },
    )
}

pub async fn appeal_page(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let back = format!("/appeal/{id}");
    let Some(action_id) = parse_id(&id) else {
        return web::not_found(&state, &viewer).await;
    };
    match moderation::appealable(&state, &viewer, action_id).await? {
        Ok(_) => {
            render_appeal(
                &state,
                &viewer,
                StatusCode::OK,
                action_id,
                String::new(),
                Vec::new(),
            )
            .await
        }
        Err(refusal) => refused(&state, &viewer, refusal, &back).await,
    }
}

pub async fn appeal(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
    Form(fields): Form<Fields>,
) -> Result<Response, AppError> {
    let back = format!("/appeal/{id}");
    let Some(action_id) = parse_id(&id) else {
        return web::not_found(&state, &viewer).await;
    };
    let details = field(&fields, "details");
    match moderation::appeal(&state, &viewer, action_id, details).await? {
        Ok(()) => {
            web::message(
                &state,
                &viewer,
                StatusCode::OK,
                "Sent to the moderators",
                &["A moderator will look at it again and let you know what they decide."],
                Some(("/notifications", "Back to your notifications")),
            )
            .await
        }
        Err(Refused::Invalid(problems)) => {
            render_appeal(
                &state,
                &viewer,
                StatusCode::BAD_REQUEST,
                action_id,
                details.to_owned(),
                problems,
            )
            .await
        }
        Err(refusal) => refused(&state, &viewer, refusal, &back).await,
    }
}

// ---------------------------------------------------------------------------
// The logs

#[derive(Template)]
#[template(path = "mod_log.html")]
struct LogTemplate {
    layout: Layout,
    entries: Vec<LogEntry>,
    pagination: Pagination,
    /// The staff view, with every entry.
    staff_view: bool,
    /// Whether the viewer is staff (who get the moderation tabs).
    staff: bool,
    /// On the first page of the public log, how many times staff broke
    /// glass (DESIGN.md §8): in all, and in each of the last months.
    glass: Option<GlassCounts>,
}

/// How often staff have broken glass (DESIGN.md §8): in all, and in each
/// of the last months, newest first.
struct GlassCounts {
    total: i64,
    months: Vec<(chrono::DateTime<chrono::Utc>, i64)>,
}

impl LogTemplate {
    /// How often staff have broken glass, as the public log says it.
    fn glass_total(&self) -> String {
        match self.glass.as_ref().map(|counts| counts.total) {
            Some(0) | None => "It hasn’t happened yet.".to_owned(),
            Some(n) => format!(
                "It has happened {} in all.",
                crate::format::plural(n, "time", "times")
            ),
        }
    }
}

/// How many months of break-glass counts `/moderation` shows.
const GLASS_MONTHS: i32 = 12;

/// `/moderation`: the public log, the same for everyone (DESIGN.md §8).
pub async fn public_log(
    State(state): State<AppState>,
    viewer: Viewer,
    uri: Uri,
) -> Result<Response, AppError> {
    render_log(&state, &viewer, &uri, "/moderation", Scope::Public).await
}

/// `/mod/log`: every entry, for staff.
pub async fn staff_log(
    State(state): State<AppState>,
    viewer: Viewer,
    uri: Uri,
) -> Result<Response, AppError> {
    if let Err(refusal) = moderation::staff(&viewer) {
        return refused(&state, &viewer, refusal, "/mod/log").await;
    }
    render_log(&state, &viewer, &uri, "/mod/log", Scope::All).await
}

async fn render_log(
    state: &AppState,
    viewer: &Viewer,
    uri: &Uri,
    base: &str,
    scope: Scope,
) -> Result<Response, AppError> {
    let params = params_of(uri);
    let Some(page) = page_param(&params).filter(|_| params.iter().all(|(k, _)| k == "page")) else {
        return web::not_found(state, viewer).await;
    };
    if let Some(redirect) = redirect_unless_canonical(uri, &page_href(base, page)) {
        return Ok(redirect);
    }
    let mut deps = Deps::new();
    let log = content::moderation::log(state.db(), viewer, scope, page, &mut deps).await?;
    let total = page_count(log.total, ENTRIES_PER_PAGE);
    if page > total {
        return web::not_found(state, viewer).await;
    }
    let glass = if scope == Scope::Public && page == 1 {
        let months = crate::staff_access::counts_by_month(state.db(), GLASS_MONTHS).await?;
        // The months shown change when a month begins.
        if let Some((this_month, _)) = months.first() {
            deps.expires_at(
                this_month
                    .checked_add_months(chrono::Months::new(1))
                    .unwrap_or(*this_month),
            );
        }
        Some(GlassCounts {
            total: crate::staff_access::total(state.db()).await?,
            months,
        })
    } else {
        None
    };
    cacheable(
        &LogTemplate {
            layout: Layout::load(
                state,
                viewer,
                if scope == Scope::Public {
                    Nav::None
                } else {
                    Nav::Moderation
                },
            )
            .await?,
            entries: log.entries,
            pagination: Pagination::new(base, page, total),
            staff_view: scope != Scope::Public,
            staff: viewer.is_staff(),
            glass,
        },
        &deps,
    )
}

// ---------------------------------------------------------------------------
// The queue

#[derive(Template)]
#[template(path = "mod_queue.html")]
struct QueueTemplate {
    layout: Layout,
    items: Vec<QueueItem>,
    recent: Vec<LogEntry>,
    durations: &'static [(u32, &'static str)],
    notice: Option<&'static str>,
}

impl QueueTemplate {
    fn reporters(&self, reports: &[mod_records::OpenReport]) -> String {
        let names: Vec<String> = reports
            .iter()
            .filter_map(|r| r.reporter.as_ref().map(|u| u.username.clone()))
            .collect();
        match names.as_slice() {
            [] => "reported".to_owned(),
            [one] => format!("reported by {one}"),
            [a, b] => format!("reported by {a} and {b}"),
            [a, b, rest @ ..] => format!(
                "reported by {a}, {b} and {}",
                crate::format::plural(rest.len() as i64, "other", "others")
            ),
        }
    }

    /// The most common reason among reports, for the item's heading and
    /// as the default reason for acting on it.
    fn main_reason(&self, reports: &[mod_records::OpenReport]) -> String {
        reports
            .first()
            .map(|r| mod_records::reason_label(&r.reason).to_owned())
            .unwrap_or_default()
    }

    fn excerpt(&self, post: &PostTarget) -> String {
        post.body
            .as_ref()
            .map_or_else(String::new, |b| excerpt(&b.markdown))
    }
}

/// Up to 400 characters of a post's text, on one line.
pub(crate) fn excerpt(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= 400 {
        flat
    } else {
        format!("{}…", flat.chars().take(399).collect::<String>().trim_end())
    }
}

pub async fn queue(
    State(state): State<AppState>,
    viewer: Viewer,
    uri: Uri,
) -> Result<Response, AppError> {
    if let Err(refusal) = moderation::staff(&viewer) {
        return refused(&state, &viewer, refusal, "/mod/queue").await;
    }
    let params = params_of(&uri);
    let notice = match params.as_slice() {
        [] => None,
        [(k, v)] if k == "done" => Some(match v.as_str() {
            "superseded" => {
                "Reversed, but a later action has since changed some or all of what that decision did, and was left alone. Change it from the member’s, the post’s or the thread’s page if you mean to."
            }
            "nothing" => {
                "Reversed. There was nothing left to undo: it had run out or been undone already, or (for an edit or a redaction) it can’t be undone here."
            }
            _ => "Done.",
        }),
        _ => return web::not_found(&state, &viewer).await,
    };
    let items = moderation::queue(&state, &viewer).await?;
    let mut deps = Deps::new();
    let mut recent = content::moderation::log(state.db(), &viewer, Scope::All, 1, &mut deps)
        .await?
        .entries;
    recent.truncate(8);
    render(
        StatusCode::OK,
        &QueueTemplate {
            layout: Layout::load(&state, &viewer, Nav::Moderation).await?,
            items,
            recent,
            durations: &DURATIONS,
            notice,
        },
    )
}

pub async fn mod_index(
    State(state): State<AppState>,
    viewer: Viewer,
) -> Result<Response, AppError> {
    if let Err(refusal) = moderation::staff(&viewer) {
        return refused(&state, &viewer, refusal, "/mod/queue").await;
    }
    Ok(see_other("/mod/queue"))
}

pub async fn dismiss(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
    Form(fields): Form<Fields>,
) -> Result<Response, AppError> {
    let back = back_to(&fields, "/mod/queue?done=1");
    let Some(id) = parse_id(&id) else {
        return web::not_found(&state, &viewer).await;
    };
    match moderation::dismiss(&state, &viewer, id, field(&fields, "reason")).await? {
        Ok(()) => Ok(see_other(&back)),
        Err(refusal) => refused(&state, &viewer, refusal, &back).await,
    }
}

pub async fn decide_appeal(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
    Form(fields): Form<Fields>,
) -> Result<Response, AppError> {
    let back = back_to(&fields, "/mod/queue?done=1");
    let Some(id) = parse_id(&id) else {
        return web::not_found(&state, &viewer).await;
    };
    let overturn = match field(&fields, "decision") {
        "overturn" => true,
        "uphold" => false,
        _ => {
            return refused(
                &state,
                &viewer,
                Refused::Invalid(vec![
                    "Choose whether to keep or reverse the decision.".to_owned(),
                ]),
                &back,
            )
            .await;
        }
    };
    match moderation::decide_appeal(&state, &viewer, id, overturn, field(&fields, "reason")).await?
    {
        Ok(Reversal::Superseded) => Ok(see_other("/mod/queue?done=superseded")),
        Ok(Reversal::NothingToUndo) => Ok(see_other("/mod/queue?done=nothing")),
        Ok(Reversal::Kept | Reversal::Undone) => Ok(see_other(&back)),
        Err(refusal) => refused(&state, &viewer, refusal, &back).await,
    }
}

pub async fn approve(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
    Form(fields): Form<Fields>,
) -> Result<Response, AppError> {
    let back = back_to(&fields, "/mod/queue?done=1");
    let Some(id) = parse_id(&id) else {
        return web::not_found(&state, &viewer).await;
    };
    match moderation::approve(&state, &viewer, id).await? {
        Ok(_) => Ok(see_other(&back)),
        Err(refusal) => refused(&state, &viewer, refusal, &back).await,
    }
}

pub async fn reject(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
    Form(fields): Form<Fields>,
) -> Result<Response, AppError> {
    let back = back_to(&fields, "/mod/queue?done=1");
    let Some(id) = parse_id(&id) else {
        return web::not_found(&state, &viewer).await;
    };
    match moderation::reject(&state, &viewer, id, field(&fields, "reason")).await? {
        Ok(()) => Ok(see_other(&back)),
        Err(refusal) => refused(&state, &viewer, refusal, &back).await,
    }
}

// ---------------------------------------------------------------------------
// Posts

#[derive(Template)]
#[template(path = "mod_post.html")]
struct ModPostTemplate {
    layout: Layout,
    post: PostTarget,
    deleted: bool,
    /// Earlier versions that can still be redacted.
    revisions: Vec<i32>,
    /// The addresses of the images it shows.
    images: Vec<String>,
}

pub async fn post_page(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let back = format!("/mod/p/{id}");
    let Some(post_id) = parse_id(&id) else {
        return web::not_found(&state, &viewer).await;
    };
    let post = match moderation::post_for_staff(&state, &viewer, post_id).await? {
        Ok(post) => post,
        Err(refusal) => return refused(&state, &viewer, refusal, &back).await,
    };
    let revisions = if post.body.is_some() {
        mod_records::unredacted_revisions(state.db(), post_id).await?
    } else {
        Vec::new()
    };
    render(
        StatusCode::OK,
        &ModPostTemplate {
            layout: Layout::load(&state, &viewer, Nav::Moderation).await?,
            deleted: post.body.is_none(),
            images: if post.body.is_some() {
                crate::upload_records::in_post(state.db(), post_id).await?
            } else {
                Vec::new()
            },
            post,
            revisions,
        },
    )
}

pub async fn moderate_post(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
    Form(fields): Form<Fields>,
) -> Result<Response, AppError> {
    let Some(post_id) = parse_id(&id) else {
        return web::not_found(&state, &viewer).await;
    };
    let back = back_to(&fields, &format!("/mod/p/{post_id}"));
    let Some(action) = PostAction::parse(field(&fields, "action")) else {
        return refused(
            &state,
            &viewer,
            Refused::Invalid(vec!["Choose what to do.".to_owned()]),
            &back,
        )
        .await;
    };
    match moderation::moderate_post(
        &state,
        &viewer,
        post_id,
        action,
        field(&fields, "reason"),
        report_field(&fields),
    )
    .await?
    {
        Ok(_) => Ok(see_other(&back)),
        Err(refusal) => refused(&state, &viewer, refusal, &back).await,
    }
}

pub async fn redact(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((id, rev)): Path<(String, String)>,
    Form(fields): Form<Fields>,
) -> Result<Response, AppError> {
    let (Some(post_id), Some(rev)) = (
        parse_id(&id),
        parse_id(&rev).and_then(|r| i32::try_from(r).ok()),
    ) else {
        return web::not_found(&state, &viewer).await;
    };
    let back = back_to(&fields, &format!("/p/{post_id}/revisions?v={rev}"));
    match moderation::redact(&state, &viewer, post_id, rev, field(&fields, "reason")).await? {
        Ok(_) => Ok(see_other(&back)),
        Err(refusal) => refused(&state, &viewer, refusal, &back).await,
    }
}

// ---------------------------------------------------------------------------
// Threads

#[derive(Template)]
#[template(path = "mod_thread.html")]
struct ModThreadTemplate {
    layout: Layout,
    thread: ThreadTarget,
    pinned: bool,
    spaces: Vec<SpaceTarget>,
    notice: Option<&'static str>,
}

impl ModThreadTemplate {
    fn current_space(&self, slug: &str) -> bool {
        self.thread.space().is_some_and(|c| c.space.slug == slug)
    }
}

#[derive(Template)]
#[template(path = "mod_deleted_thread.html")]
struct ModDeletedThreadTemplate {
    layout: Layout,
    thread: content::moderation::DeletedThread,
    notice: Option<&'static str>,
}

/// A thread the moderator can't read: what they may do to it, and
/// nothing of what it says (DESIGN.md §3.1).
#[derive(Template)]
#[template(path = "mod_closed_thread.html")]
struct ModClosedThreadTemplate {
    layout: Layout,
    thread: content::moderation::ClosedThread,
    /// Where it may be moved: every space but its own.
    spaces: Vec<SpaceTarget>,
    notice: Option<&'static str>,
}

pub async fn thread_page(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
    uri: Uri,
) -> Result<Response, AppError> {
    let back = format!("/mod/t/{id}");
    if let Err(refusal) = moderation::staff(&viewer) {
        return refused(&state, &viewer, refusal, &back).await;
    }
    let Some(thread_id) = parse_id(&id) else {
        return web::not_found(&state, &viewer).await;
    };
    let notice = params_of(&uri)
        .iter()
        .any(|(k, _)| k == "done")
        .then_some("Done.");
    let Some(thread) = content::thread_target(state.db(), &viewer, thread_id)
        .await?
        .filter(|t| t.space().is_some())
    else {
        // A deleted thread can be restored from here.
        if let Some(thread) =
            content::moderation::deleted_thread(state.db(), &viewer, thread_id).await?
        {
            return render(
                StatusCode::OK,
                &ModDeletedThreadTemplate {
                    layout: Layout::load(&state, &viewer, Nav::Moderation).await?,
                    thread,
                    notice,
                },
            );
        }
        // One they can't read, as staff run every space.
        return match content::moderation::closed_thread(state.db(), &viewer, thread_id).await? {
            Some(thread) => {
                let spaces = content::administered_spaces(state.db(), &viewer)
                    .await?
                    .into_iter()
                    .filter(|c| c.space.id != thread.space.space.id)
                    .collect();
                render(
                    StatusCode::OK,
                    &ModClosedThreadTemplate {
                        layout: Layout::load(&state, &viewer, Nav::Moderation).await?,
                        thread,
                        spaces,
                        notice,
                    },
                )
            }
            None => web::not_found(&state, &viewer).await,
        };
    };
    let mut deps = Deps::new();
    let pinned = content::thread_page(state.db(), &viewer, thread_id, 1, &mut deps)
        .await?
        .is_some_and(|t| t.pinned);
    render(
        StatusCode::OK,
        &ModThreadTemplate {
            layout: Layout::load(&state, &viewer, Nav::Moderation).await?,
            spaces: content::readable_spaces(state.db(), &viewer).await?,
            pinned,
            thread,
            notice,
        },
    )
}

pub async fn moderate_thread(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
    Form(fields): Form<Fields>,
) -> Result<Response, AppError> {
    let Some(thread_id) = parse_id(&id) else {
        return web::not_found(&state, &viewer).await;
    };
    let back = back_to(&fields, &format!("/mod/t/{thread_id}?done=1"));
    let action = match field(&fields, "action") {
        "lock" => ThreadAction::Lock,
        "unlock" => ThreadAction::Unlock,
        "pin" => ThreadAction::Pin,
        "unpin" => ThreadAction::Unpin,
        "rename" => ThreadAction::Rename(field(&fields, "title").to_owned()),
        "move" => ThreadAction::Move(field(&fields, "space").to_owned()),
        "delete" => ThreadAction::Delete,
        "restore" => ThreadAction::Restore,
        _ => {
            return refused(
                &state,
                &viewer,
                Refused::Invalid(vec!["Choose what to do.".to_owned()]),
                &back,
            )
            .await;
        }
    };
    let glass = fields
        .iter()
        .find(|(key, _)| key == crate::glass_pages::REASON_FIELD)
        .map(|(_, value)| value.as_str());
    let reason = field(&fields, "reason");
    let mut outcome = moderation::moderate_thread_breaking_glass(
        &state,
        &viewer,
        thread_id,
        action.clone(),
        reason,
        glass,
    )
    .await?;
    // Moving a thread they can't read where they could is breaking glass,
    // which the confirmation page asks about first (DESIGN.md §8).
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
        let action_path = format!("/mod/t/{thread_id}");
        match crate::glass_pages::ask_change(
            &state,
            &viewer,
            opens,
            crate::glass_pages::Change {
                action: &action_path,
                fields: &fields,
                says: "You can’t read this thread, and moving it there would let you.".to_owned(),
                button: "Break glass and move it",
                back: &action_path,
            },
        )
        .await?
        {
            crate::glass_pages::Confirmation::Ask(response) => return Ok(response),
            crate::glass_pages::Confirmation::Given(why) => {
                outcome = moderation::moderate_thread_breaking_glass(
                    &state,
                    &viewer,
                    thread_id,
                    action,
                    reason,
                    Some(&why),
                )
                .await?;
            }
        }
    }
    match outcome {
        // A deleted thread's page here offers to restore it.
        Ok(_) => Ok(see_other(&back)),
        Err(refusal) => refused(&state, &viewer, refusal, &back).await,
    }
}

pub async fn move_posts(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
    Form(fields): Form<Fields>,
) -> Result<Response, AppError> {
    let Some(thread_id) = parse_id(&id) else {
        return web::not_found(&state, &viewer).await;
    };
    let back = format!("/mod/t/{thread_id}");
    let destination = match field(&fields, "to") {
        "existing" => Destination::Existing(field(&fields, "thread").to_owned()),
        "new" => Destination::New {
            space: field(&fields, "space").to_owned(),
            title: field(&fields, "title").to_owned(),
        },
        _ => {
            return refused(
                &state,
                &viewer,
                Refused::Invalid(vec!["Choose where to move them.".to_owned()]),
                &back,
            )
            .await;
        }
    };
    match moderation::move_posts(
        &state,
        &viewer,
        thread_id,
        field(&fields, "numbers"),
        &destination,
        field(&fields, "reason"),
    )
    .await?
    {
        Ok(to) => match content::thread_target(state.db(), &viewer, to).await? {
            Some(t) => Ok(see_other(&t.href())),
            None => Ok(see_other("/mod/log")),
        },
        Err(refusal) => refused(&state, &viewer, refusal, &back).await,
    }
}

// ---------------------------------------------------------------------------
// Members

#[derive(Template)]
#[template(path = "mod_users.html")]
struct ModUsersTemplate {
    layout: Layout,
    members: Vec<MemberRow>,
    search: String,
    restricted: bool,
    total: i64,
    pagination: Pagination,
}

const MEMBERS_PER_PAGE: i64 = 50;

pub async fn users_page(
    State(state): State<AppState>,
    viewer: Viewer,
    uri: Uri,
) -> Result<Response, AppError> {
    if let Err(refusal) = moderation::staff(&viewer) {
        return refused(&state, &viewer, refusal, "/mod/users").await;
    }
    let params = params_of(&uri);
    let Some(page) = page_param(&params) else {
        return web::not_found(&state, &viewer).await;
    };
    let search = params
        .iter()
        .find(|(k, _)| k == "q")
        .map_or("", |(_, v)| v.as_str())
        .chars()
        .take(50)
        .collect::<String>();
    let restricted = params.iter().any(|(k, v)| k == "restricted" && v == "1");
    let offset = (page - 1).saturating_mul(MEMBERS_PER_PAGE);
    let (total, members) =
        mod_records::members(state.db(), &search, restricted, MEMBERS_PER_PAGE, offset).await?;
    let mut base = format!("/mod/users?q={}", web::query_encode(&search));
    if restricted {
        base.push_str("&restricted=1");
    }
    render(
        StatusCode::OK,
        &ModUsersTemplate {
            layout: Layout::load(&state, &viewer, Nav::Moderation).await?,
            members,
            search,
            restricted,
            total,
            pagination: Pagination::new(base, page, page_count(total, MEMBERS_PER_PAGE)),
        },
    )
}

#[derive(Template)]
#[template(path = "mod_user.html")]
struct ModUserTemplate {
    layout: Layout,
    member: MemberRecord,
    notes: Vec<Note>,
    ips: Vec<IpRecord>,
    retention_days: u32,
    actions: Vec<LogEntry>,
    branch: Vec<crate::content::UserRef>,
    durations: &'static [(u32, &'static str)],
    notice: Option<&'static str>,
    /// The address of their picture.
    avatar: Option<String>,
}

pub async fn user_page(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(username): Path<String>,
    uri: Uri,
) -> Result<Response, AppError> {
    let back = format!("/mod/users/{username}");
    let member = match moderation::member(&state, &viewer, &username).await? {
        Ok(member) => member,
        Err(refusal) => return refused(&state, &viewer, refusal, &back).await,
    };
    let retention_days = settings::moderation_settings(state.db())
        .await?
        .ip_retention_days;
    let retention = i32::try_from(retention_days).unwrap_or(i32::MAX);
    let mut deps = Deps::new();
    let actions = content::moderation::log(
        state.db(),
        &viewer,
        Scope::Member(member.user.id),
        1,
        &mut deps,
    )
    .await?
    .entries;
    let notice = params_of(&uri)
        .iter()
        .any(|(k, _)| k == "done")
        .then_some("Done.");
    render(
        StatusCode::OK,
        &ModUserTemplate {
            layout: Layout::load(&state, &viewer, Nav::Moderation).await?,
            notes: mod_records::notes(state.db(), member.user.id).await?,
            ips: mod_records::ips(state.db(), member.user.id, retention).await?,
            retention_days,
            actions,
            branch: mod_records::invite_branch(state.db(), member.user.id).await?,
            avatar: crate::upload_records::avatar_url(state.db(), member.user.id).await?,
            durations: &DURATIONS,
            member,
            notice,
        },
    )
}

#[derive(Template)]
#[template(path = "mod_spammer.html")]
struct ModSpammerTemplate {
    layout: Layout,
    preview: moderation::SpammerPreview,
    reason: String,
    /// Where to go once it is done.
    back: String,
    /// Where to go instead.
    cancel: String,
}

/// `POST /mod/users/{username}/{action}`: silence, suspend, lift either,
/// suspend the invite branch, delete as a spammer (once confirmed), or add
/// a note.
pub async fn user_action(
    State(state): State<AppState>,
    viewer: Viewer,
    Path((username, action)): Path<(String, String)>,
    Form(fields): Form<Fields>,
) -> Result<Response, AppError> {
    let page = format!("/mod/users/{username}");
    let back = back_to(&fields, &format!("{page}?done=1"));
    let reason = field(&fields, "reason");
    let report = report_field(&fields);
    let days = || moderation::parse_days(field(&fields, "days"));
    let no_days = || {
        Refused::Invalid(vec![format!(
            "Choose how many days, from 1 to {}.",
            moderation::MAX_DAYS
        )])
    };
    let outcome = match action.as_str() {
        "silence" | "suspend" => {
            let restriction = if action == "silence" {
                Restriction::Silence
            } else {
                Restriction::Suspend
            };
            match days() {
                Some(days) => moderation::restrict_member(
                    &state,
                    &viewer,
                    &username,
                    restriction,
                    Some(days),
                    reason,
                    report,
                )
                .await?
                .map(|_| ()),
                None => Err(no_days()),
            }
        }
        "unsilence" => moderation::restrict_member(
            &state,
            &viewer,
            &username,
            Restriction::Silence,
            None,
            reason,
            report,
        )
        .await?
        .map(|_| ()),
        "unsuspend" => moderation::restrict_member(
            &state,
            &viewer,
            &username,
            Restriction::Suspend,
            None,
            reason,
            report,
        )
        .await?
        .map(|_| ()),
        "suspend-branch" => match days() {
            Some(days) => {
                moderation::suspend_branch(&state, &viewer, &username, days, reason, report)
                    .await?
                    .map(|_| ())
            }
            None => Err(no_days()),
        },
        // It can't be undone, so the first press asks.
        "spammer" if field(&fields, "confirm") != "yes" => {
            return match moderation::spammer_preview(&state, &viewer, &username).await? {
                Ok(preview) => {
                    let back = back_to(&fields, &page);
                    render(
                        StatusCode::OK,
                        &ModSpammerTemplate {
                            layout: Layout::load(&state, &viewer, Nav::Moderation).await?,
                            cancel: back.split('?').next().unwrap_or(&page).to_owned(),
                            back,
                            reason: reason.to_owned(),
                            preview,
                        },
                    )
                }
                Err(refusal) => refused(&state, &viewer, refusal, &page).await,
            };
        }
        "spammer" => {
            return match moderation::delete_spammer(&state, &viewer, &username, reason).await? {
                Ok(_) => Ok(see_other(&back_to(&fields, "/mod/queue?done=1"))),
                Err(refusal) => refused(&state, &viewer, refusal, &page).await,
            };
        }
        "notes" => moderation::add_note(&state, &viewer, &username, field(&fields, "body"))
            .await?
            .map(|_| ()),
        _ => return web::not_found(&state, &viewer).await,
    };
    match outcome {
        Ok(()) => Ok(see_other(&back)),
        Err(refusal) => refused(&state, &viewer, refusal, &page).await,
    }
}

// ---------------------------------------------------------------------------
// Settings

#[derive(Template)]
#[template(path = "admin_moderation.html")]
struct AdminModerationTemplate {
    layout: Layout,
    settings: ModerationSettings,
    notice: Option<&'static str>,
    errors: Vec<String>,
}

pub async fn admin_page(
    State(state): State<AppState>,
    viewer: Viewer,
    uri: Uri,
) -> Result<Response, AppError> {
    if !viewer.is_admin() {
        return refused(&state, &viewer, admin_refusal(&viewer), "/admin/moderation").await;
    }
    let notice = params_of(&uri)
        .iter()
        .any(|(k, _)| k == "done")
        .then_some("Saved.");
    render(
        StatusCode::OK,
        &AdminModerationTemplate {
            layout: Layout::load(&state, &viewer, Nav::Admin).await?,
            settings: settings::moderation_settings(state.db()).await?,
            notice,
            errors: Vec::new(),
        },
    )
}

fn admin_refusal(viewer: &Viewer) -> Refused {
    if viewer.user().is_none() {
        Refused::LogIn
    } else {
        Refused::NotAllowed("Only admins can change this.")
    }
}

pub async fn save_admin(
    State(state): State<AppState>,
    viewer: Viewer,
    Form(fields): Form<Fields>,
) -> Result<Response, AppError> {
    if !viewer.is_admin() {
        return refused(&state, &viewer, admin_refusal(&viewer), "/admin/moderation").await;
    }
    let mut errors = Vec::new();
    let number =
        |name: &str, max: u32, label: &str, errors: &mut Vec<String>| match field(&fields, name)
            .trim()
            .parse::<u32>()
        {
            Ok(n) if n <= max => n,
            _ => {
                errors.push(format!("{label} should be a whole number from 0 to {max}."));
                0
            }
        };
    let new = ModerationSettings {
        hold_posts: number(
            "hold_posts",
            100,
            "The number of posts to hold",
            &mut errors,
        ),
        ip_retention_days: number(
            "ip_retention_days",
            3650,
            "The days to keep addresses",
            &mut errors,
        )
        .max(1),
    };
    if !errors.is_empty() {
        return render(
            StatusCode::BAD_REQUEST,
            &AdminModerationTemplate {
                layout: Layout::load(&state, &viewer, Nav::Admin).await?,
                settings: new,
                notice: None,
                errors,
            },
        );
    }
    settings::set(state.db(), "moderation", &new).await?;
    mod_records::forget_old_ips(
        state.db(),
        i32::try_from(new.ip_retention_days).unwrap_or(i32::MAX),
    )
    .await?;
    Ok(see_other("/admin/moderation?done=1"))
}

// ---------------------------------------------------------------------------
// Images

#[derive(Template)]
#[template(path = "mod_images.html")]
struct ModImagesTemplate {
    layout: Layout,
    address: String,
    error: Option<&'static str>,
}

/// The file name in an image's address, which may be a whole URL or just
/// the path.
fn image_name(address: &str) -> Option<&str> {
    let address = address.trim();
    let (_, rest) = address.rsplit_once("/uploads/")?;
    let name = rest.split(['?', '#']).next()?;
    crate::uploads::parse_name(name).map(|_| name)
}

/// `/mod/uploads`: finds an image by its address, to review or remove it.
pub async fn images_page(
    State(state): State<AppState>,
    viewer: Viewer,
    uri: Uri,
) -> Result<Response, AppError> {
    if let Err(refusal) = moderation::staff(&viewer) {
        return refused(&state, &viewer, refusal, "/mod/uploads").await;
    }
    let address = params_of(&uri)
        .into_iter()
        .find(|(k, _)| k == "image")
        .map(|(_, v)| v)
        .unwrap_or_default();
    let (status, error) = if address.trim().is_empty() {
        (StatusCode::OK, None)
    } else if let Some(name) = image_name(&address) {
        return Ok(see_other(&format!("/mod/uploads/{name}")));
    } else {
        (
            StatusCode::BAD_REQUEST,
            Some(
                "That isn’t the address of an image uploaded here. Copy the image’s address and paste it in.",
            ),
        )
    };
    render(
        status,
        &ModImagesTemplate {
            layout: Layout::load(&state, &viewer, Nav::Moderation).await?,
            address: address.chars().take(500).collect(),
            error,
        },
    )
}

#[derive(Template)]
#[template(path = "mod_image.html")]
struct ModImageTemplate {
    layout: Layout,
    name: String,
    url: String,
    /// `None` once it has been removed.
    image: Option<content::uploads::ImageUse>,
}

/// `/mod/uploads/{name}`: an image, where it is used, and the form to
/// remove it.
pub async fn image_page(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(name): Path<String>,
) -> Result<Response, AppError> {
    let back = format!("/mod/uploads/{name}");
    if let Err(refusal) = moderation::staff(&viewer) {
        return refused(&state, &viewer, refusal, &back).await;
    }
    let Some((sha256, ext)) = crate::uploads::parse_name(&name) else {
        return web::not_found(&state, &viewer).await;
    };
    let image = match content::uploads::image_use(state.db(), &viewer, sha256, ext).await? {
        content::uploads::ImageLookup::NotFound => {
            return web::not_found(&state, &viewer).await;
        }
        content::uploads::ImageLookup::Removed => None,
        content::uploads::ImageLookup::Found(image) => Some(image),
    };
    render(
        StatusCode::OK,
        &ModImageTemplate {
            layout: Layout::load(&state, &viewer, Nav::Moderation).await?,
            url: crate::uploads::url(sha256, ext),
            name,
            image,
        },
    )
}

pub async fn remove_image(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(name): Path<String>,
    Form(fields): Form<Fields>,
) -> Result<Response, AppError> {
    let back = format!("/mod/uploads/{name}");
    match moderation::remove_image(&state, &viewer, &name, field(&fields, "reason")).await? {
        Ok(()) => Ok(see_other(&back)),
        Err(refusal) => refused(&state, &viewer, refusal, &back).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_addresses() {
        let hash = "ab".repeat(32);
        let name = format!("{hash}.png");
        for address in [
            format!("/uploads/{name}"),
            format!("https://forum.example/uploads/{name}"),
            format!("  https://forum.example/uploads/{name}?x=1#y "),
        ] {
            assert_eq!(image_name(&address), Some(name.as_str()), "{address}");
        }
        for address in [
            "",
            "/uploads/",
            "/uploads/nothing.png",
            &format!("/uploads/{hash}.svg"),
        ] {
            assert_eq!(image_name(address), None, "{address}");
        }
    }

    #[test]
    fn excerpts_are_short_and_flat() {
        assert_eq!(excerpt("a\n\nb"), "a b");
        let long = "word ".repeat(200);
        let cut = excerpt(&long);
        assert!(cut.chars().count() <= 400 && cut.ends_with('…'), "{cut}");
    }
}
