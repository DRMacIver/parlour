//! Live updates over server-sent events (DESIGN.md §5, §9).
//!
//! Writes publish [`Event`]s to one in-process broadcast channel; there is
//! one process, so that reaches every open stream. Each stream checks, as
//! its viewer, that they may know about an event before passing it on:
//!
//! - `/events/t/{id}` tells anyone who can read a thread or conversation
//!   about new posts in it, for the "N new posts" bar. The check is made
//!   when the stream opens and again for every post, and the stream ends
//!   if the viewer can no longer read the thread. For visitors the check
//!   is made once per post, when it is published, not once per stream, so
//!   that a crowd of visitors following a thread costs nothing per post.
//! - `/events/notifications` tells a logged-in member their unread
//!   notification and conversation counts whenever they change.
//!
//! A member's streams look their session up again for each event, so they
//! end once the session does (logging out, or being logged out everywhere
//! else) and always count with the member's groups as they are now.
//!
//! Streams are limited per client address and per member
//! ([`MAX_STREAMS_PER_ADDRESS`], [`MAX_STREAMS_PER_MEMBER`]).
//!
//! Events carry only numbers, never text. Pages work without these
//! streams; the scripts that use them come later.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_util::stream::{self, Stream};
use tokio::sync::broadcast;

use crate::AppState;
use crate::client::ClientIp;
use crate::content;
use crate::error::AppError;
use crate::pages::parse_id;
use crate::sessions;
use crate::viewer::{Viewer, session_cookie};
use crate::web;

/// Something that happened, for open streams to pass on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// A post was added to a thread or conversation.
    Post {
        thread_id: i64,
        post_id: i64,
        seq: i64,
        /// Whether visitors may read the thread, as of this post.
        public: bool,
    },
    /// A member's notifications or conversations changed.
    Counts { user_id: i64 },
}

/// How many streams one client address may have open at once.
pub const MAX_STREAMS_PER_ADDRESS: usize = 32;
/// How many streams one member may have open at once, wherever from.
pub const MAX_STREAMS_PER_MEMBER: usize = 24;

/// The broadcast channel, and the count of open streams.
#[derive(Clone)]
pub struct Live {
    sender: broadcast::Sender<Event>,
    open: Arc<Mutex<HashMap<String, usize>>>,
}

impl Default for Live {
    fn default() -> Self {
        // Streams that fall this far behind skip what they missed; the
        // events they would have sent are only hints.
        let (sender, _) = broadcast::channel(1024);
        Live {
            sender,
            open: Arc::default(),
        }
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

    /// Whether any stream is open, so publishers can skip work for nobody.
    pub fn has_listeners(&self) -> bool {
        self.sender.receiver_count() > 0
    }

    /// Counts a new stream against each of `limits` (a key and how many
    /// streams it may have), if none is at its limit. The stream holds the
    /// permit while open; dropping it frees the place.
    fn admit(&self, limits: &[(String, usize)]) -> Option<Permit> {
        let mut open = self.open.lock().unwrap_or_else(|e| e.into_inner());
        if limits
            .iter()
            .any(|(key, max)| open.get(key).copied().unwrap_or(0) >= *max)
        {
            return None;
        }
        for (key, _) in limits {
            *open.entry(key.clone()).or_insert(0) += 1;
        }
        Some(Permit {
            open: self.open.clone(),
            keys: limits.iter().map(|(key, _)| key.clone()).collect(),
        })
    }
}

/// A place taken by an open stream.
struct Permit {
    open: Arc<Mutex<HashMap<String, usize>>>,
    keys: Vec<String>,
}

impl Drop for Permit {
    fn drop(&mut self) {
        let mut open = self.open.lock().unwrap_or_else(|e| e.into_inner());
        for key in &self.keys {
            if let Some(n) = open.get_mut(key) {
                *n -= 1;
                if *n == 0 {
                    open.remove(key);
                }
            }
        }
    }
}

/// A place for a new stream from this client, if it may open one.
fn admit(state: &AppState, ip: &ClientIp, viewer: &Viewer) -> Option<Permit> {
    let mut limits = vec![(format!("address:{}", ip.0), MAX_STREAMS_PER_ADDRESS)];
    if let Some(user_id) = viewer.user_id() {
        limits.push((format!("member:{user_id}"), MAX_STREAMS_PER_MEMBER));
    }
    state.live().admit(&limits)
}

/// The answer to a client with too many streams open.
fn too_many() -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        "Too many live updates are open. Close some pages and try again.",
    )
        .into_response()
}

/// Who a stream is for: a visitor, or the member a session belongs to.
#[derive(Clone)]
enum Follower {
    Visitor,
    Session(String),
}

impl Follower {
    fn of(viewer: &Viewer, headers: &HeaderMap) -> Option<Follower> {
        match viewer {
            Viewer::Anonymous => Some(Follower::Visitor),
            Viewer::User(_) => session_cookie(headers).map(|t| Follower::Session(t.to_owned())),
        }
    }

    /// The viewer as they are now: `None` once their session has ended.
    async fn current(&self, state: &AppState) -> sqlx::Result<Option<Viewer>> {
        match self {
            Follower::Visitor => Ok(Some(Viewer::Anonymous)),
            Follower::Session(token) => Ok(sessions::still_current(state.db(), token)
                .await?
                .map(Viewer::User)),
        }
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

/// Whether a follower may still hear about posts in a thread.
async fn still_readable(
    state: &AppState,
    follower: &Follower,
    thread_id: i64,
    public: bool,
) -> sqlx::Result<bool> {
    if let Follower::Visitor = follower {
        // Checked once for everyone when the post was published.
        return Ok(public);
    }
    match follower.current(state).await? {
        Some(viewer) => Ok(content::thread_target(state.db(), &viewer, thread_id)
            .await?
            .is_some()),
        None => Ok(false),
    }
}

/// `/events/t/{id}`: new posts in a thread or conversation the viewer can
/// read.
pub async fn thread_events(
    State(state): State<AppState>,
    viewer: Viewer,
    ip: ClientIp,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let (Some(thread_id), Some(follower)) = (parse_id(&id), Follower::of(&viewer, &headers)) else {
        return web::not_found(&state, &viewer).await;
    };
    let Some(permit) = admit(&state, &ip, &viewer) else {
        return Ok(too_many());
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
        (receiver, state, follower, permit),
        move |(mut receiver, state, follower, permit)| async move {
            let (post_id, seq, public) = next_event(&mut receiver, |event| match event {
                Event::Post {
                    thread_id: t,
                    post_id,
                    seq,
                    public,
                } if t == thread_id => Some((post_id, seq, public)),
                _ => None,
            })
            .await?;
            // The thread may have moved, the viewer may have left the
            // conversation or a group, or logged out, since the stream
            // opened.
            match still_readable(&state, &follower, thread_id, public).await {
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
            Some((Ok(event), (receiver, state, follower, permit)))
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
    ip: ClientIp,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let (Some(user_id), Some(follower)) = (viewer.user_id(), Follower::of(&viewer, &headers))
    else {
        return web::not_found(&state, &viewer).await;
    };
    let Some(permit) = admit(&state, &ip, &viewer) else {
        return Ok(too_many());
    };
    let receiver = state.live().subscribe();
    let first = counts_event(&state, &viewer).await?;
    let rest = stream::unfold(
        (receiver, state, follower, permit),
        move |(mut receiver, state, follower, permit)| async move {
            next_event(&mut receiver, |event| match event {
                Event::Counts { user_id: u } if u == user_id => Some(()),
                _ => None,
            })
            .await?;
            // Looked up again, so that the stream ends with the session
            // and counts with the member's groups as they are now.
            let counted = match follower.current(&state).await {
                Ok(Some(viewer)) if viewer.user_id() == Some(user_id) => {
                    counts_event(&state, &viewer).await.map(Some)
                }
                Ok(_) => Ok(None),
                Err(err) => Err(err),
            };
            match counted {
                Ok(Some(event)) => Some((Ok(event), (receiver, state, follower, permit))),
                Ok(None) => None,
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
