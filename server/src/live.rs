//! Live updates over server-sent events (DESIGN.md §5, §9).
//!
//! Writes publish [`Event`]s to one in-process broadcast channel; there is
//! one process, so that reaches every open stream. Each stream checks, as
//! its viewer, that they may know about an event before passing it on:
//!
//! - `/events/t/{id}` tells anyone who can read a thread or conversation
//!   about new posts in it, for the "N new posts" bar. The check is made
//!   when the stream opens and again for every post, and the stream ends
//!   if the viewer can no longer read the thread.
//! - `/events/notifications` tells a logged-in member their unread
//!   notification and conversation counts whenever they change.
//!
//! Events carry only numbers, never text. Pages work without these
//! streams; the scripts that use them come later.

use std::convert::Infallible;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::stream::{self, Stream};
use tokio::sync::broadcast;

use crate::AppState;
use crate::content;
use crate::error::AppError;
use crate::pages::parse_id;
use crate::viewer::Viewer;
use crate::web;

/// Something that happened, for open streams to pass on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// A post was added to a thread or conversation.
    Post {
        thread_id: i64,
        post_id: i64,
        seq: i64,
    },
    /// A member's notifications or conversations changed.
    Counts { user_id: i64 },
}

/// The broadcast channel.
#[derive(Clone)]
pub struct Live {
    sender: broadcast::Sender<Event>,
}

impl Default for Live {
    fn default() -> Self {
        // Streams that fall this far behind skip what they missed; the
        // events they would have sent are only hints.
        let (sender, _) = broadcast::channel(1024);
        Live { sender }
    }
}

impl Live {
    pub fn publish(&self, event: Event) {
        // An error only means nobody is listening.
        let _ = self.sender.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.sender.subscribe()
    }
}

const KEEP_ALIVE: Duration = Duration::from_secs(25);

fn sse(stream: impl Stream<Item = Result<SseEvent, Infallible>> + Send + 'static) -> Response {
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(KEEP_ALIVE))
        .into_response()
}

/// Waits for the next event that `wanted` picks out. `None` when the
/// channel has closed.
async fn next_event<T>(
    receiver: &mut broadcast::Receiver<Event>,
    wanted: impl Fn(Event) -> Option<T>,
) -> Option<T> {
    loop {
        match receiver.recv().await {
            Ok(event) => {
                if let Some(found) = wanted(event) {
                    return Some(found);
                }
            }
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => return None,
        }
    }
}

/// `/events/t/{id}`: new posts in a thread or conversation the viewer can
/// read.
pub async fn thread_events(
    State(state): State<AppState>,
    viewer: Viewer,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let Some(thread_id) = parse_id(&id) else {
        return web::not_found(&state, &viewer).await;
    };
    // Subscribe before checking, so nothing posted in between is missed.
    let receiver = state.live().subscribe();
    if content::thread_target(state.db(), &viewer, thread_id)
        .await?
        .is_none()
    {
        return web::not_found(&state, &viewer).await;
    }
    let stream = stream::unfold(
        (receiver, state, viewer),
        move |(mut receiver, state, viewer)| async move {
            let (post_id, seq) = next_event(&mut receiver, |event| match event {
                Event::Post {
                    thread_id: t,
                    post_id,
                    seq,
                } if t == thread_id => Some((post_id, seq)),
                _ => None,
            })
            .await?;
            // The thread may have moved, the viewer may have left the
            // conversation or a group, since the stream opened.
            let current = match &viewer {
                Viewer::Anonymous => Ok(Some(Viewer::Anonymous)),
                Viewer::User(user) => crate::sessions::user_ctx(state.db(), user.id)
                    .await
                    .map(|user| user.map(Viewer::User)),
            };
            let readable = match current {
                Ok(Some(current)) => content::thread_target(state.db(), &current, thread_id)
                    .await
                    .map(|t| t.is_some()),
                Ok(None) => Ok(false),
                Err(err) => Err(err),
            };
            match readable {
                Ok(true) => {}
                Ok(false) => return None,
                Err(err) => {
                    tracing::error!(%err, "could not check a live stream's thread");
                    return None;
                }
            }
            let event = SseEvent::default().event("post").data(format!(
                r#"{{"thread":{thread_id},"post":{post_id},"seq":{seq}}}"#
            ));
            Some((Ok(event), (receiver, state, viewer)))
        },
    );
    Ok(sse(stream))
}

/// The counts on the notification badge and the messages link.
async fn counts_event(state: &AppState, viewer: &Viewer) -> sqlx::Result<SseEvent> {
    let notifications = content::notifications::unread_count(state.db(), viewer).await?;
    let messages = content::messages::unread_conversations(state.db(), viewer).await?;
    Ok(SseEvent::default().event("counts").data(format!(
        r#"{{"notifications":{notifications},"messages":{messages}}}"#
    )))
}

/// `/events/notifications`: the logged-in member's unread counts, now and
/// whenever they change.
pub async fn notification_events(
    State(state): State<AppState>,
    viewer: Viewer,
) -> Result<Response, AppError> {
    let Some(user_id) = viewer.user_id() else {
        return web::not_found(&state, &viewer).await;
    };
    let receiver = state.live().subscribe();
    let first = counts_event(&state, &viewer).await?;
    let rest = stream::unfold(
        (receiver, state, viewer),
        move |(mut receiver, state, viewer)| async move {
            next_event(&mut receiver, |event| match event {
                Event::Counts { user_id: u } if u == user_id => Some(()),
                _ => None,
            })
            .await?;
            match counts_event(&state, &viewer).await {
                Ok(event) => Some((Ok(event), (receiver, state, viewer))),
                Err(err) => {
                    tracing::error!(%err, "could not count notifications for a live stream");
                    None
                }
            }
        },
    );
    use futures_util::StreamExt;
    Ok(sse(stream::once(async move { Ok(first) }).chain(rest)))
}
