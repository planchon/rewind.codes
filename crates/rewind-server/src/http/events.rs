//! The event stream of a run (`GET /v1/runs/{id}/events`): server-sent events of its history
//! by index, from `from`, then, with `follow`, of each batch its owner commits, pushed once
//! durable (`archi/06-storage-and-scale.md`, "Messages after facts").

use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::Stream;
use rewind_api::access::Permission;
use rewind_api::events::{EventsQuery, StreamEvent};
use rewind_api::headers::LAST_EVENT_ID;
use rewind_core::{HistoryEvent, RunStatus};
use rewind_engine::ports::RunRef;
use rewind_engine::ports::link::{Changed, SubscriptionKey};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::app::App;
use crate::auth::{Caller, TokenExpiry};
use crate::error::ApiError;
use crate::extract::Query;
use crate::follow;
use crate::idle::Busy;
use crate::streams::Rights;

/// `GET /v1/runs/{id}/events?follow=1&from=…`. A stream opened with a member's access token is
/// closed at the token's expiry: the dashboard resumes it with a new token and `from`. A stream
/// opened with an end user's token is closed once the token no longer authenticates: revoked,
/// expired, its key deleted or its namespace erased; one opened by a connection, at its access
/// token's expiry or once the connection is revoked or no longer opens the organization. A key
/// revoked, or a grant that no longer gives `runs:read` with the same labels, closes it too: the
/// caller's rights are read again at each change of the control plane that concerns it.
pub(super) async fn stream(
    State(app): State<Arc<App>>,
    caller: Caller,
    TokenExpiry(expiry): TokenExpiry,
    Path(id): Path<String>,
    Query(query): Query<EventsQuery>,
    headers: HeaderMap,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    caller.require(Permission::RunsRead)?;
    let view = app.run(&caller, &id).await?;
    let resumed = headers
        .get(LAST_EVENT_ID)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(|last| last.saturating_add(1));
    let follow = query.follow.unwrap_or(false);
    let run = view.head.run.clone();
    // Followed from before the first read: nothing committed in between is missed.
    let changes = follow.then(|| follow::follow(&app, SubscriptionKey::Run(run.clone())));
    let tail = Tail {
        busy: follow.then(|| app.idle.busy()),
        rights: Rights::new(&app, caller, Permission::RunsRead, expiry),
        app,
        run,
        next: query.from.or(resumed).unwrap_or(0),
        changes,
        pending: VecDeque::new(),
        read: false,
        done: false,
    };
    let events = futures_util::stream::unfold(tail, |mut tail| async move {
        tail.next_event().await.map(|event| (Ok(event), tail))
    });
    Ok(Sse::new(events).keep_alive(KeepAlive::default()))
}

/// Where a stream stands.
struct Tail {
    app: Arc<App>,
    rights: Rights,
    run: RunRef,
    /// The index of the next event to send.
    next: u64,
    /// The run's changes, when followed.
    changes: Option<UnboundedReceiver<Changed>>,
    pending: VecDeque<Event>,
    /// The history was read once.
    read: bool,
    done: bool,
    /// A followed stream keeps the embedded server from stopping on idle.
    #[allow(dead_code)]
    busy: Option<Busy>,
}

impl Tail {
    async fn next_event(&mut self) -> Option<Event> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Some(event);
            }
            if self.done {
                return None;
            }
            if !self.read {
                self.read = true;
                if let Err(error) = self.catch_up().await {
                    tracing::warn!(run = %self.run, detail = %error.0.detail, "the event stream stops");
                    return None;
                }
                continue;
            }
            let changes = self.changes.as_mut()?;
            tokio::select! {
                () = self.rights.lost() => return None,
                changed = changes.recv() => {
                    let changed = changed?;
                    if let Err(error) = self.changed(changed).await {
                        tracing::warn!(run = %self.run, detail = %error.0.detail, "the event stream stops");
                        return None;
                    }
                }
            }
        }
    }

    /// Reads the history from `next` and the run's state, as the owner holds them now.
    async fn catch_up(&mut self) -> Result<(), ApiError> {
        let Some(view) = self.app.read_run(&self.run).await? else {
            self.done = true;
            return Ok(());
        };
        let history = self.app.views().history_from(&self.run, self.next).await?;
        self.push(&history).await?;
        let caught_up = self.next >= view.head.history_len;
        if caught_up && (view.head.is_ended() || self.changes.is_none()) {
            self.end(view.head.status);
        }
        Ok(())
    }

    /// A batch the owner committed: its events, read again from the history when the batch
    /// starts past `next`.
    async fn changed(&mut self, changed: Changed) -> Result<(), ApiError> {
        let Some(summary) = &changed.summary else {
            // The run was erased.
            self.done = true;
            return Ok(());
        };
        let starts_after = changed
            .events
            .first()
            .is_some_and(|first| first.index > self.next);
        if starts_after {
            return self.catch_up().await;
        }
        let events: Vec<HistoryEvent> = changed
            .events
            .into_iter()
            .filter(|entry| entry.index >= self.next)
            .collect();
        self.push(&events).await?;
        if summary.status.is_terminal() && self.next >= summary.history_len {
            self.end(summary.status);
        }
        Ok(())
    }

    async fn push(&mut self, history: &[HistoryEvent]) -> Result<(), ApiError> {
        let events = self
            .app
            .views()
            .events(&self.run, history, rewind_api::common::PAYLOAD_VALUE_LIMIT)
            .await?;
        for entry in events {
            if entry.idx < self.next {
                continue;
            }
            self.next = entry.idx + 1;
            self.pending
                .push_back(sse(&StreamEvent::Event(Box::new(entry))));
        }
        Ok(())
    }

    fn end(&mut self, status: RunStatus) {
        self.pending.push_back(sse(&StreamEvent::End {
            status: status.into(),
        }));
        self.done = true;
    }
}

fn sse(event: &StreamEvent) -> Event {
    let data = serde_json::to_string(event).unwrap_or_default();
    let mut sse = Event::default().event(event.sse_event()).data(data);
    if let Some(id) = event.sse_id() {
        sse = sse.id(id);
    }
    sse
}
