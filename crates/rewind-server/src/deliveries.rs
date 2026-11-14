//! When lifecycle webhooks are sent: concurrently, a few at a time per subscription, each within
//! a window that the claim of its feed item outlasts.
//!
//! | Events | Sent by |
//! | --- | --- |
//! | Projected from the feed: `run.completed`, `run.failed`, `run.cancelled`, `run.aborted`, `run.suspended` | [`Outbox`]: claims feed items for longer than their deliveries can last, sends each item's deliveries at once, acknowledges the item when they end |
//! | Reported by the engine: `run.quarantined`, `tool.schema_changed`, `credentials.expired` | [`Background`], the engine's `WebhookSender`: it queues the delivery and returns, so that no activation and no tool call waits for a receiver |
//! | Recorded by the store: `quota.alert` | The engine's `QuotaAlerts`, through [`Waiting`]: claims alerts for longer than their deliveries can last, sends them at once, acknowledges them when they end |
//!
//! A delivery waits for one of its subscription's [`PER_SUBSCRIPTION`] slots, then is sent with
//! the sender's retries, cut at its deadline: the end of its window, twice the sender's retry
//! window. A delivery given up is logged, and its feed item is acknowledged all the same. A
//! receiver that is down holds its own slots, never another subscription's.
//!
//! Delivery is at least once: a feed item is acknowledged only once its deliveries ended. When
//! the server stops, or dies, the deliveries in progress are dropped and their items are sent
//! again, with the same `webhook-id`, once their claims expire.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rewind_engine::ports::{
    Clock, FeedConsumer, JournalStore, Subscription, WebhookError, WebhookEvent, WebhookSender,
};
use rewind_engine::projector::{WebhookDelivery, webhook_deliveries};
use tokio::sync::{Semaphore, watch};
use tokio::task::{JoinError, JoinSet};
use tokio::time::Instant;

use crate::wake::{POLL, wait};
use crate::wiring::webhooks::{HttpWebhooks, Render};

/// Deliveries in flight to one subscription at once.
pub(crate) const PER_SUBSCRIPTION: usize = 4;

/// What a feed item's or a quota alert's claim lasts beyond the window of its deliveries:
/// reading the run, rendering the events, acknowledging.
pub(crate) const LEASE_MARGIN: Duration = Duration::from_secs(30);

/// Feed items an outbox holds at once.
const HELD: usize = 256;

/// How long the outbox waits after a failure of the store.
const BACKOFF: Duration = Duration::from_secs(1);

/// The deliveries of one process: the sender, the slots of each subscription, and the
/// deliveries sent in the background.
#[derive(Debug)]
pub(crate) struct Deliveries<R> {
    sender: HttpWebhooks<R>,
    per_subscription: usize,
    window: Duration,
    /// By subscription id; an entry lives while a delivery holds or waits for its slots.
    slots: Mutex<HashMap<String, Arc<Semaphore>>>,
    background: Mutex<JoinSet<()>>,
}

impl<R: Render + 'static> Deliveries<R> {
    pub(crate) fn new(sender: HttpWebhooks<R>, per_subscription: usize) -> Self {
        let window = sender.retries().window().saturating_mul(2);
        Self {
            sender,
            per_subscription: per_subscription.max(1),
            window,
            slots: Mutex::new(HashMap::new()),
            background: Mutex::new(JoinSet::new()),
        }
    }

    /// How long a delivery may take, waiting for a slot included: twice the sender's retry
    /// window, so that a delivery that waited a whole one still makes every attempt.
    pub(crate) fn window(&self) -> Duration {
        self.window
    }

    /// Sends `event` to `subscription` before `deadline`: waits for one of the subscription's
    /// slots, then sends with the sender's retries, cut at the deadline.
    pub(crate) async fn send(
        &self,
        subscription: &Subscription,
        event: &WebhookEvent,
        deadline: Instant,
    ) -> Result<(), WebhookError> {
        let slots = self.slots_of(&subscription.id);
        let sent = match tokio::time::timeout_at(deadline, slots.clone().acquire_owned()).await {
            Ok(Ok(_slot)) => self.sender.send_before(subscription, event, deadline).await,
            _ => Err(WebhookError::Delivery {
                url: subscription.url.clone(),
                message: "not sent: the subscription's earlier deliveries took its whole window"
                    .into(),
            }),
        };
        self.forget(&subscription.id, slots);
        sent
    }

    /// Sends every delivery at once, each within the same deadline; logs those given up.
    pub(crate) async fn send_all(
        self: &Arc<Self>,
        deliveries: Vec<WebhookDelivery>,
        deadline: Instant,
    ) {
        let mut sending = JoinSet::new();
        for delivery in deliveries {
            let this = self.clone();
            sending.spawn(async move {
                this.send_logged(&delivery.subscription, &delivery.event, deadline)
                    .await;
            });
        }
        while sending.join_next().await.is_some() {}
    }

    /// Sends in the background, within the window.
    pub(crate) fn spawn(self: &Arc<Self>, subscription: Subscription, event: WebhookEvent) {
        let this = self.clone();
        let deadline = Instant::now() + self.window;
        let mut background = self
            .background
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while background.try_join_next().is_some() {}
        background.spawn(async move {
            this.send_logged(&subscription, &event, deadline).await;
        });
    }

    /// Drops the deliveries sent in the background: the server stops.
    pub(crate) fn abort(&self) {
        self.background
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .abort_all();
    }

    async fn send_logged(
        &self,
        subscription: &Subscription,
        event: &WebhookEvent,
        deadline: Instant,
    ) {
        if let Err(error) = self.send(subscription, event, deadline).await {
            tracing::warn!(
                subscription = %subscription.id,
                event = %event.id,
                kind = event.kind.event_type().as_str(),
                %error,
                "a lifecycle event was not delivered"
            );
        }
    }

    fn slots_of(&self, subscription: &str) -> Arc<Semaphore> {
        let mut slots = self
            .slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        slots
            .entry(subscription.to_owned())
            .or_insert_with(|| Arc::new(Semaphore::new(self.per_subscription)))
            .clone()
    }

    /// Drops the subscription's entry when no other delivery holds it.
    fn forget(&self, subscription: &str, held: Arc<Semaphore>) {
        let mut slots = self
            .slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // The map and `held`: nobody else holds or waits for these slots.
        if Arc::strong_count(&held) == 2 {
            slots.remove(subscription);
        }
    }
}

/// The engine's `WebhookSender`: queues each delivery and returns at once. The delivery is sent
/// in the background, within its window, and logged if it is given up: the engine only logs a
/// delivery that fails, so it never needs to wait for one.
#[derive(Debug)]
pub(crate) struct Background<R>(pub(crate) Arc<Deliveries<R>>);

impl<R: Render + 'static> WebhookSender for Background<R> {
    async fn send(
        &self,
        subscription: &Subscription,
        event: &WebhookEvent,
    ) -> Result<(), WebhookError> {
        self.0.spawn(subscription.clone(), event.clone());
        Ok(())
    }
}

/// A `WebhookSender` that waits for the delivery, sent within its window: the sender of quota
/// alerts, which acknowledges an alert once its deliveries ended.
#[derive(Debug)]
pub(crate) struct Waiting<R>(pub(crate) Arc<Deliveries<R>>);

impl<R: Render + 'static> WebhookSender for Waiting<R> {
    async fn send(
        &self,
        subscription: &Subscription,
        event: &WebhookEvent,
    ) -> Result<(), WebhookError> {
        let deadline = Instant::now() + self.0.window();
        self.0.send(subscription, event, deadline).await
    }
}

/// Sends the lifecycle webhooks of the feed. It claims `webhooks` items for longer than their
/// deliveries can last, so that no other worker claims an item while its deliveries are in
/// progress, sends each item's deliveries at once, and acknowledges each item when they end.
#[derive(Debug)]
pub(crate) struct Outbox<'a, J, C, R> {
    pub(crate) journal: &'a J,
    pub(crate) clock: &'a C,
    pub(crate) deliveries: Arc<Deliveries<R>>,
    /// The owner of the claims.
    pub(crate) owner: String,
    /// Feed items claimed at once.
    pub(crate) batch: usize,
}

impl<J, C, R> Outbox<'_, J, C, R>
where
    J: JournalStore,
    C: Clock,
    R: Render + 'static,
{
    /// How long a feed item is claimed: the window of its deliveries, and a margin.
    pub(crate) fn lease(&self) -> Duration {
        self.deliveries.window().saturating_add(LEASE_MARGIN)
    }

    /// Claims, sends and acknowledges until `stop`. The deliveries in progress then are dropped:
    /// their items are sent again once their claims expire.
    pub(crate) async fn run(
        &self,
        mut changes: watch::Receiver<u64>,
        mut stop: watch::Receiver<bool>,
    ) {
        let mut sending = Sending::default();
        while !*stop.borrow() {
            let mut done = Vec::new();
            let mut failed = false;
            let room = HELD.saturating_sub(sending.held.len()).min(self.batch);
            if room > 0 {
                match self.claim(room, &mut sending).await {
                    Ok(empty) => done.extend(empty),
                    Err(()) => failed = true,
                }
            }
            let poll = if failed { BACKOFF } else { POLL * 4 };
            tokio::select! {
                Some(ended) = sending.tasks.join_next_with_id(), if !sending.tasks.is_empty() => {
                    done.extend(sending.ended(ended));
                }
                () = wait(&mut changes, poll, None) => {}
                _ = stop.wait_for(|stop| *stop) => break,
            }
            while let Some(ended) = sending.tasks.try_join_next_with_id() {
                done.extend(sending.ended(ended));
            }
            if done.is_empty() {
                continue;
            }
            if let Err(error) = self.journal.ack_feed(&done).await {
                tracing::error!(%error, "delivered lifecycle events were not acknowledged: they are sent again");
            }
        }
        sending.tasks.abort_all();
    }

    /// Claims up to `room` items and starts sending their deliveries. Returns the items that
    /// call for none, to acknowledge.
    async fn claim(&self, room: usize, sending: &mut Sending) -> Result<Vec<u64>, ()> {
        let items = self
            .journal
            .claim_feed(
                FeedConsumer::Webhooks,
                &self.owner,
                self.clock.now(),
                self.lease(),
                room,
            )
            .await
            .map_err(|error| tracing::error!(%error, "the lifecycle webhooks cannot be claimed"))?;
        let mut empty = Vec::new();
        for item in items {
            // An item claimed again while its deliveries are in progress (its claim expired in
            // virtual time) is not sent twice.
            if !sending.held.insert(item.id) {
                continue;
            }
            let deadline = Instant::now() + self.deliveries.window();
            match webhook_deliveries(self.journal, &item).await {
                Ok(deliveries) if deliveries.is_empty() => {
                    sending.held.remove(&item.id);
                    empty.push(item.id);
                }
                Ok(deliveries) => {
                    let all = self.deliveries.clone();
                    let task = sending.tasks.spawn(async move {
                        all.send_all(deliveries, deadline).await;
                    });
                    sending.items.insert(task.id(), item.id);
                }
                Err(error) => {
                    sending.held.remove(&item.id);
                    tracing::warn!(run = %item.run, %error, "the lifecycle events of a commit cannot be read: they are sent once its claim expires");
                }
            }
        }
        Ok(empty)
    }
}

/// The feed items an outbox holds, and the tasks sending their deliveries.
#[derive(Debug, Default)]
struct Sending {
    held: HashSet<u64>,
    tasks: JoinSet<()>,
    /// The item each task sends.
    items: HashMap<tokio::task::Id, u64>,
}

impl Sending {
    /// The item whose deliveries ended, to acknowledge. A task that did not end (it panicked)
    /// leaves its item to be sent again once its claim expires.
    fn ended(&mut self, ended: Result<(tokio::task::Id, ()), JoinError>) -> Option<u64> {
        let (task, finished) = match ended {
            Ok((task, ())) => (task, true),
            Err(error) => {
                tracing::error!(%error, "a delivery of lifecycle events failed: it is sent again");
                (error.id(), false)
            }
        };
        let item = self.items.remove(&task)?;
        self.held.remove(&item);
        finished.then_some(item)
    }
}

#[cfg(test)]
mod tests;
