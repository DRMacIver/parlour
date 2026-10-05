//! The break-glass confirmation page (DESIGN.md §8, §11): `/mod/break-glass`
//! with `?report=`, `?thread=` or `?space=`, and the same page shown in
//! answer to a form whose change would let the member of staff sending it
//! read what they can't ([`ask_change`]). The decisions are in
//! [`crate::glass`].
//!
//! The page is for staff, depends on who is looking, and is never cached.

use askama::Template;
use axum::Form;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Response;

use crate::AppState;
use crate::content::access::{Closed, What};
use crate::error::AppError;
use crate::glass::{self, Ask, Proposal, Prospect};
use crate::mod_pages::refused;
use crate::pages::parse_id;
use crate::viewer::Viewer;
use crate::web::{self, Layout, Nav, render, see_other};
use crate::writing::Refused;

type Fields = Vec<(String, String)>;

/// The field a confirmation page sends its reason in.
pub const REASON_FIELD: &str = "glass_reason";

#[derive(Template)]
#[template(path = "glass.html")]
struct GlassTemplate {
    layout: Layout,
    proposal: Proposal,
    /// Where the form goes, and the fields it carries along.
    action: String,
    hidden: Fields,
    /// For a change: what saving it would do, and the button's words.
    change: Option<String>,
    button: &'static str,
    reason: String,
    problems: Vec<String>,
    back: String,
    moderator: String,
}

impl GlassTemplate {
    fn expires(&self) -> String {
        crate::format::datetime(&self.proposal.expires_at)
    }

    fn one(&self) -> Option<&Closed> {
        match self.proposal.opens.as_slice() {
            [one] => Some(one),
            _ => None,
        }
    }

    /// Who will be told, as the page says it.
    fn told(&self) -> String {
        let n = self.proposal.readers as i64;
        let who = match self.one().map(|c| c.what) {
            Some(What::Conversation) => "Everyone else in it".to_owned(),
            Some(What::Space) => "Everyone who can read the space".to_owned(),
            Some(What::Thread) => "Everyone who can read the thread".to_owned(),
            None => "Everyone who can read any of them".to_owned(),
        };
        if n == 0 {
            "Nobody else can read it now, so nobody will be told.".to_owned()
        } else {
            format!(
                "{who} ({}) will be told at once, on the site and by email, that a moderator ({}) viewed it and when, and its pages will say so from then on. The notice won’t say why or who reported it, but they can read your reason in the access log.",
                crate::format::plural(n, "person", "people"),
                self.moderator
            )
        }
    }
}

async fn show(
    state: &AppState,
    viewer: &Viewer,
    status: StatusCode,
    page: GlassPage,
) -> Result<Response, AppError> {
    render(
        status,
        &GlassTemplate {
            layout: Layout::load(state, viewer, Nav::Moderation).await?,
            moderator: viewer
                .user()
                .map(|user| user.display_name.clone())
                .unwrap_or_default(),
            proposal: page.proposal,
            action: page.action,
            hidden: page.hidden,
            change: page.change,
            button: page.button,
            reason: page.reason,
            problems: page.problems,
            back: page.back,
        },
    )
}

struct GlassPage {
    proposal: Proposal,
    action: String,
    hidden: Fields,
    change: Option<String>,
    button: &'static str,
    reason: String,
    problems: Vec<String>,
    back: String,
}

/// What the query or form names, if it names one thing.
fn ask_of(fields: &Fields) -> Option<Ask> {
    let mut asks = fields.iter().filter_map(|(key, value)| match key.as_str() {
        "report" => parse_id(value).map(Ask::Report),
        "thread" => parse_id(value).map(Ask::Thread),
        "space" => Some(Ask::Space(value.clone())),
        _ => None,
    });
    let ask = asks.next()?;
    asks.next().is_none().then_some(ask)
}

fn fields_of(ask: &Ask) -> Fields {
    let (key, value) = match ask {
        Ask::Report(id) => ("report", id.to_string()),
        Ask::Thread(id) => ("thread", id.to_string()),
        Ask::Space(slug) => ("space", slug.clone()),
    };
    vec![(key.to_owned(), value)]
}

/// `GET /mod/break-glass`: straight to what is asked for, if the viewer
/// reads it already, and otherwise the confirmation page.
pub async fn page(
    State(state): State<AppState>,
    viewer: Viewer,
    Query(query): Query<Fields>,
) -> Result<Response, AppError> {
    let Some(ask) = ask_of(&query) else {
        return web::not_found(&state, &viewer).await;
    };
    match glass::prospect(&state, &viewer, &ask).await? {
        Ok(Prospect::Open(href)) => Ok(see_other(&href)),
        Ok(Prospect::Confirm(proposal)) => {
            show(
                &state,
                &viewer,
                StatusCode::OK,
                GlassPage {
                    proposal,
                    action: "/mod/break-glass".to_owned(),
                    hidden: fields_of(&ask),
                    change: None,
                    button: "Break glass and read it",
                    reason: String::new(),
                    problems: Vec::new(),
                    back: "/mod/queue".to_owned(),
                },
            )
            .await
        }
        Err(refusal) => refused(&state, &viewer, refusal, "/mod/queue").await,
    }
}

/// `POST /mod/break-glass`: opens the grant and goes to what it opens.
pub async fn confirm(
    State(state): State<AppState>,
    viewer: Viewer,
    Form(fields): Form<Fields>,
) -> Result<Response, AppError> {
    let Some(ask) = ask_of(&fields) else {
        return web::not_found(&state, &viewer).await;
    };
    let reason = fields
        .iter()
        .find(|(key, _)| key == REASON_FIELD)
        .map(|(_, value)| value.clone())
        .unwrap_or_default();
    match glass::break_glass(&state, &viewer, &ask, &reason).await? {
        Ok(href) => Ok(see_other(&href)),
        Err(Refused::Invalid(problems)) => match glass::prospect(&state, &viewer, &ask).await? {
            Ok(Prospect::Confirm(proposal)) => {
                show(
                    &state,
                    &viewer,
                    StatusCode::BAD_REQUEST,
                    GlassPage {
                        proposal,
                        action: "/mod/break-glass".to_owned(),
                        hidden: fields_of(&ask),
                        change: None,
                        button: "Break glass and read it",
                        reason,
                        problems,
                        back: "/mod/queue".to_owned(),
                    },
                )
                .await
            }
            Ok(Prospect::Open(href)) => Ok(see_other(&href)),
            Err(refusal) => refused(&state, &viewer, refusal, "/mod/queue").await,
        },
        Err(refusal) => refused(&state, &viewer, refusal, "/mod/queue").await,
    }
}

/// What a form that would let a member of staff read what they can't
/// does with its confirmation.
pub enum Confirmation {
    /// They confirmed, for this reason: make the change, then
    /// [`glass::record`] it, with the readers asked before the change.
    Given(String),
    /// Show them this page first.
    Ask(Response),
}

/// A form whose change would let the member of staff sending it read what
/// they can't, as its confirmation page shows it.
pub struct Change<'a> {
    /// Where the form goes, and what it sends.
    pub action: &'a str,
    pub fields: &'a Fields,
    /// What saving it would do ("You can’t read Circle, and saving these
    /// settings would let you.").
    pub says: String,
    /// What the button says.
    pub button: &'static str,
    /// Where cancelling goes.
    pub back: &'a str,
}

/// The reason a form carries for breaking glass, if it carries one and it
/// will do; otherwise the confirmation page, which sends the same fields
/// back to the form's action with a reason.
pub async fn ask_change(
    state: &AppState,
    viewer: &Viewer,
    opens: Vec<Closed>,
    change: Change<'_>,
) -> Result<Confirmation, AppError> {
    let Change {
        action,
        fields,
        says,
        button,
        back,
    } = change;
    let reason = fields
        .iter()
        .find(|(key, _)| key == REASON_FIELD)
        .map(|(_, value)| value.clone());
    let problems = match &reason {
        Some(reason) => match glass::reason_problem(reason) {
            None => return Ok(Confirmation::Given(reason.clone())),
            Some(problem) => vec![problem],
        },
        None => Vec::new(),
    };
    let Some(user) = viewer.user() else {
        return Ok(Confirmation::Ask(web::login_first(back)));
    };
    let proposal = glass::proposal(state, user, opens, None).await?;
    let hidden = fields
        .iter()
        .filter(|(key, _)| key != REASON_FIELD)
        .cloned()
        .collect();
    Ok(Confirmation::Ask(
        show(
            state,
            viewer,
            if problems.is_empty() {
                StatusCode::OK
            } else {
                StatusCode::BAD_REQUEST
            },
            GlassPage {
                proposal,
                action: action.to_owned(),
                hidden,
                change: Some(says),
                button,
                reason: reason.unwrap_or_default(),
                problems,
                back: back.to_owned(),
            },
        )
        .await?,
    ))
}
