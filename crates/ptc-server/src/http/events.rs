//! The event stream of a run (`GET /v1/runs/{id}/events`): server-sent events tailing its
//! history by index, woken by commits. Not the feed: a stream of one run needs no projector
//! (`archi/05-testing-and-observability.md`, "The feed and the projector").

use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::Stream;
use ptc_api::events::{EventsQuery, StreamEvent};
use ptc_api::headers::LAST_EVENT_ID;
use ptc_engine::ports::{JournalStore, RunRef};
use tokio::sync::watch;

use crate::app::App;
use crate::auth::{Access, Caller};
use crate::error::ApiError;
use crate::extract::Query;
use crate::wake::{Busy, POLL, wait};

/// `GET /v1/runs/{id}/events?follow=1&from=…`.
pub(super) async fn stream(
    State(app): State<Arc<App>>,
    caller: Caller,
    Path(id): Path<String>,
    Query(query): Query<EventsQuery>,
    headers: HeaderMap,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    caller.allow(Access::Read)?;
    let record = app.run(&caller, &id).await?;
    let resumed = headers
        .get(LAST_EVENT_ID)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(|last| last.saturating_add(1));
    let tail = Tail {
        changes: app.wake.subscribe(),
        busy: query.follow.unwrap_or(false).then(|| app.activity.busy()),
        app,
        run: record.run,
        next: query.from.or(resumed).unwrap_or(0),
        follow: query.follow.unwrap_or(false),
        pending: VecDeque::new(),
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
    run: RunRef,
    /// The index of the next entry to send.
    next: u64,
    follow: bool,
    pending: VecDeque<Event>,
    done: bool,
    changes: watch::Receiver<u64>,
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
            if self.done || self.app.stopping() {
                return None;
            }
            match self.read().await {
                Ok(true) => {}
                Ok(false) => wait(&mut self.changes, POLL, None).await,
                Err(error) => {
                    tracing::warn!(run = %self.run, detail = %error.0.detail, "the event stream stops");
                    return None;
                }
            }
        }
    }

    /// Reads what was committed since the last read. Returns whether there is something to
    /// send.
    async fn read(&mut self) -> Result<bool, ApiError> {
        let Some(record) = self.app.store.get_run(&self.run).await? else {
            self.done = true;
            return Ok(false);
        };
        let views = self.app.views();
        let history = views.history_from(&record, self.next).await?;
        let entries = views
            .entries(
                &self.run.tenant,
                &history,
                ptc_api::common::PAYLOAD_VALUE_LIMIT,
            )
            .await?;
        for entry in entries {
            self.next = entry.idx + 1;
            self.pending
                .push_back(sse(&StreamEvent::Entry(Box::new(entry))));
        }
        let caught_up = self.next >= record.history_len;
        if caught_up && (record.status.is_terminal() || !self.follow) {
            self.pending.push_back(sse(&StreamEvent::End {
                status: record.status.into(),
            }));
            self.done = true;
        }
        Ok(!self.pending.is_empty())
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
