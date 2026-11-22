//! The change stream (`GET /v1/changes`): server-sent events of the runs of the organization
//! whose summary changes, as the caller may see them. The loop that reads the changes is the
//! process's own (`crate::changes`); each stream keeps the runs its caller may open.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::Stream;
use rewind_api::access::Permission;
use rewind_api::events::{ChangeEvent, Resync};
use rewind_engine::adapters::SystemClock;
use rewind_engine::ports::Clock;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::watch;

use crate::app::App;
use crate::auth::{self, Caller, TokenExpiry};
use crate::changes::{Batch, Changes, Subscription};
use crate::error::ApiError;
use crate::wake::Busy;

/// How often the caller's token and grant are checked again while its stream is open.
const RECHECK: Duration = Duration::from_secs(1);

/// `GET /v1/changes`. Each `run` event holds the run's summary as `GET /v1/runs` lists it, for
/// the runs the caller may see: of its tenant, in the namespace its request is bound to (an end
/// user's token's own), carrying every label of its grant. No other run shows, not even its id.
/// `resync` says the client should read again what it shows. There is no resume. Closed at a
/// member's or a connection's access token's expiry (an access token already expired is refused
/// with 401, though the identity provider's leeway let it through); once an end user's token or
/// a connection no longer authenticates; once a key is revoked or the grant of a key, a member
/// or a connection is removed, changes its labels or no longer gives `runs:read`; and after
/// `resync` when the server cannot read the changes any more.
pub(super) async fn stream(
    State(app): State<Arc<App>>,
    caller: Caller,
    TokenExpiry(expiry): TokenExpiry,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    caller.require(Permission::RunsRead)?;
    // Tokens expire in the world's time, whatever the server's clock.
    let now = SystemClock.now().as_millis();
    if let Some(expires_at) = expiry
        && expires_at.as_millis() <= now
    {
        return Err(auth::unauthorized(
            "the access token has expired: POST /auth/session gives a new one, then open the \
             stream again",
        ));
    }
    let deadline = expiry.map(|expires_at| {
        let left = expires_at.as_millis().saturating_sub(now);
        tokio::time::Instant::now() + Duration::from_millis(left)
    });
    let rechecked =
        caller.token.is_some() || caller.connection.is_some() || caller.principal.is_some();
    let follow = Follow {
        checked: rechecked.then(tokio::time::Instant::now),
        subscription: Changes::subscribe(&app, &caller.tenant),
        stop: app.stop.subscribe(),
        busy: app.activity.busy(),
        caller,
        app,
        pending: VecDeque::new(),
        closing: false,
        deadline,
    };
    let events = futures_util::stream::unfold(follow, |mut follow| async move {
        follow.next_event().await.map(|event| (Ok(event), follow))
    });
    Ok(Sse::new(events).keep_alive(KeepAlive::default()))
}

/// Where a stream stands.
struct Follow {
    app: Arc<App>,
    caller: Caller,
    /// When the caller was last found to authenticate with the same rights; `None` over the
    /// socket.
    checked: Option<tokio::time::Instant>,
    subscription: Subscription,
    stop: watch::Receiver<bool>,
    pending: VecDeque<Event>,
    /// The loop cannot read the changes: the stream ends once `pending` is sent.
    closing: bool,
    /// A stream keeps the embedded server from stopping on idle, as a followed run's does.
    #[allow(dead_code)]
    busy: Busy,
    /// The expiry of the access token that opened the stream, which ends it.
    deadline: Option<tokio::time::Instant>,
}

impl Follow {
    async fn next_event(&mut self) -> Option<Event> {
        loop {
            let now = tokio::time::Instant::now();
            if self.deadline.is_some_and(|deadline| now >= deadline) || self.app.stopping() {
                return None;
            }
            if let Some(checked) = self.checked
                && now >= checked + RECHECK
            {
                if !auth::still_holds(&self.app, &self.caller, Permission::RunsRead).await {
                    return None;
                }
                self.checked = Some(tokio::time::Instant::now());
            }
            if let Some(event) = self.pending.pop_front() {
                return Some(event);
            }
            if self.closing {
                return None;
            }
            let mut until = now + RECHECK;
            if let Some(deadline) = self.deadline {
                until = until.min(deadline);
            }
            tokio::select! {
                received = self.subscription.batches.recv() => match received {
                    Ok(batch) => self.keep(&batch),
                    // Batches were missed: the client reads again what it shows.
                    Err(RecvError::Lagged(_)) => self.pending.push_back(resync()),
                    Err(RecvError::Closed) => return None,
                },
                () = tokio::time::sleep_until(until) => {}
                _ = self.stop.changed() => {}
            }
        }
    }

    /// Queues what of a batch the caller may see.
    fn keep(&mut self, batch: &Batch) {
        let caller = &self.caller;
        if batch.close {
            self.pending.push_back(resync());
            self.closing = true;
            return;
        }
        if batch.resync.contains(&caller.tenant) {
            self.pending.push_back(resync());
        }
        for run in &batch.runs {
            if run.tenant == caller.tenant
                && caller.reaches(&run.namespace)
                && caller.sees(&run.labels)
            {
                self.pending
                    .push_back(Event::default().event(run.event).data(run.data.as_str()));
            }
        }
    }
}

fn resync() -> Event {
    let event = ChangeEvent::Resync(Resync {});
    let data = serde_json::to_string(&event).unwrap_or_else(|_| "{}".to_owned());
    Event::default().event(event.sse_event()).data(data)
}
