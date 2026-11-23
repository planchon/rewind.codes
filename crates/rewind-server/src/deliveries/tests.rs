//! The deliveries against HTTP receivers on 127.0.0.1: one that answers, one that is down.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::Path;
use axum::http::{HeaderMap, StatusCode};
use rewind_api::webhooks::WebhookSecret;
use rewind_core::{
    Command, Digest, Epoch, Event, HistoryEntry, Lifecycle, RunId, RunMode, RunStarted, RunStatus,
    Seed, Timestamp,
};
use rewind_engine::adapters::{MemoryJournal, SystemClock};
use rewind_engine::ports::{
    Activation, Clock, FeedConsumer, JournalStore, NewRun, RunMetadata, RunRef, Secret,
    Subscription, TenantId, WebhookError, WebhookEvent, WebhookEventKind, WebhookSender,
};
use tokio::sync::watch;

use super::{Background, Deliveries, Outbox};
use crate::wake::Wake;
use crate::wiring::webhooks::{HttpWebhooks, Render, Retries, WebhookEgress};

/// A delivery to the receiver that is down takes 3 attempts of 200 ms, 100 ms and 400 ms apart:
/// 1.1 s at least.
const RETRIES: Retries = Retries {
    attempts: 3,
    first_delay: Duration::from_millis(100),
    timeout: Duration::from_secs(1),
};

/// Writes the event as `{"id", "type"}`.
#[derive(Debug, Clone, Copy)]
struct Plain;

impl Render for Plain {
    async fn render(
        &self,
        _subscription: &Subscription,
        event: &WebhookEvent,
    ) -> Result<Vec<u8>, WebhookError> {
        Ok(serde_json::to_vec(&serde_json::json!({
            "id": event.id,
            "type": event.kind.event_type().as_str(),
        }))
        .unwrap_or_default())
    }
}

/// What a receiver got: the subscription and the `webhook-id` of each request.
#[derive(Debug, Default, Clone)]
struct Log(Arc<Mutex<Vec<(String, String)>>>);

impl Log {
    fn push(&self, subscription: String, id: String) {
        if let Ok(mut log) = self.0.lock() {
            log.push((subscription, id));
        }
    }

    /// The `webhook-id`s sent to `subscription`.
    fn ids(&self, subscription: &str) -> Vec<String> {
        self.0
            .lock()
            .map(|log| {
                log.iter()
                    .filter(|(sub, _)| sub == subscription)
                    .map(|(_, id)| id.clone())
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Counts the requests in progress at the receiver that is down, and the most at once.
#[derive(Debug, Default)]
struct InFlight {
    now: AtomicUsize,
    most: AtomicUsize,
}

struct Request(Arc<InFlight>);

impl Request {
    fn start(in_flight: &Arc<InFlight>) -> Self {
        let now = in_flight.now.fetch_add(1, Ordering::SeqCst) + 1;
        in_flight.most.fetch_max(now, Ordering::SeqCst);
        Self(in_flight.clone())
    }
}

impl Drop for Request {
    fn drop(&mut self) {
        self.0.now.fetch_sub(1, Ordering::SeqCst);
    }
}

/// `/up/{sub}` answers 204 at once; `/down/{sub}` answers 503 after 200 ms; `/silent/{sub}`
/// never answers.
struct Receiver {
    base: String,
    log: Log,
    down: Arc<InFlight>,
}

impl Receiver {
    async fn start() -> Self {
        let log = Log::default();
        let down = Arc::new(InFlight::default());
        let header = |headers: &HeaderMap| {
            headers
                .get("webhook-id")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_owned()
        };
        let (up_log, down_log, in_flight) = (log.clone(), log.clone(), down.clone());
        let app = axum::Router::new()
            .route(
                "/up/{sub}",
                axum::routing::post(move |Path(sub): Path<String>, headers: HeaderMap| {
                    let log = up_log.clone();
                    async move {
                        log.push(sub, header(&headers));
                        StatusCode::NO_CONTENT
                    }
                }),
            )
            .route(
                "/down/{sub}",
                axum::routing::post(move |Path(sub): Path<String>, headers: HeaderMap| {
                    let (log, in_flight) = (down_log.clone(), in_flight.clone());
                    async move {
                        let _request = Request::start(&in_flight);
                        log.push(sub, header(&headers));
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        StatusCode::SERVICE_UNAVAILABLE
                    }
                }),
            )
            .route(
                "/silent/{sub}",
                axum::routing::post(std::future::pending::<StatusCode>),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap_or_else(|error| panic!("{error}"));
        let addr = listener
            .local_addr()
            .unwrap_or_else(|error| panic!("{error}"));
        tokio::spawn(async move { axum::serve(listener, app).await });
        Self {
            base: format!("http://{addr}"),
            log,
            down,
        }
    }

    fn subscription(&self, id: &str, path: &str) -> Subscription {
        Subscription {
            tenant: TenantId::local(),
            id: id.into(),
            url: format!("{}/{path}/{id}", self.base),
            secret: Secret::new(
                WebhookSecret::from_bytes([3u8; 32])
                    .unwrap_or_else(|error| panic!("{error}"))
                    .expose(),
            ),
            events: BTreeSet::new(),
            labels: BTreeMap::new(),
        }
    }
}

fn deliveries(per_subscription: usize) -> Arc<Deliveries<Plain>> {
    Arc::new(Deliveries::new(
        HttpWebhooks::new(Plain, RETRIES, WebhookEgress::Open),
        per_subscription,
    ))
}

fn run_ref(n: u128) -> RunRef {
    RunRef::new(
        TenantId::local(),
        RunId::from_u128(0x0193_0000_0000_7000_8000_0000_0000_0000 | n),
    )
}

/// Commits a run that starts and completes in one activation: one feed item per consumer, whose
/// lifecycle event is `run.completed`, `{run}/1`.
async fn completed_run(journal: &MemoryJournal, n: u128) -> String {
    let now = SystemClock.now();
    let run = run_ref(n);
    let started = RunStarted {
        script: Digest::of(b"script"),
        toolset: Digest::of(b"toolset"),
        input: serde_json::json!({ "n": 1 }).into(),
        seed: Seed::from_bytes([9; 32]),
        epoch: Epoch(1),
        expires_at: Timestamp(now.0 + 86_400_000),
        mode: RunMode::Live,
    };
    journal
        .create_run(
            NewRun {
                run: run.clone(),
                namespace: rewind_engine::ports::NamespaceName::migrated(),
                origin: rewind_core::SCRIPT_ORIGIN.to_owned(),
                started: started.clone(),
                metadata: RunMetadata::default(),
                created_at: now,
            },
            None,
        )
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    let lease = journal
        .acquire_run(&run, "test", now, Duration::from_secs(30))
        .await
        .unwrap_or_else(|error| panic!("{error}"))
        .unwrap_or_else(|| panic!("the run can be leased"));
    let consumed = journal
        .take_inbox(&lease)
        .await
        .unwrap_or_else(|error| panic!("{error}"))
        .iter()
        .map(|item| item.id)
        .collect();
    let entries = vec![
        HistoryEntry {
            index: 0,
            recorded_at: now,
            event: Event::Lifecycle(Lifecycle::RunStarted(started)),
        },
        HistoryEntry {
            index: 1,
            recorded_at: now,
            event: Event::Command(Command::CompleteRun {
                output: serde_json::json!({ "done": true }).into(),
            }),
        },
    ];
    journal
        .commit_activation(
            &lease,
            Activation::new(entries, consumed, RunStatus::Completed),
            now,
        )
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    format!("{}/1", run.id)
}

/// The `webhooks` items nobody acknowledged, claimed long after every claim expired.
async fn unacknowledged(journal: &MemoryJournal) -> usize {
    journal
        .claim_feed(
            FeedConsumer::Webhooks,
            "probe",
            Timestamp(SystemClock.now().0 + 86_400_000),
            Duration::from_secs(1),
            1000,
        )
        .await
        .unwrap_or_else(|error| panic!("{error}"))
        .len()
}

/// Waits until `check` holds, for up to 20 seconds.
async fn eventually(mut check: impl FnMut() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while !check() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the condition did not hold within 20 s"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn outbox<'a>(
    journal: &'a MemoryJournal,
    deliveries: Arc<Deliveries<Plain>>,
    owner: &str,
) -> Outbox<'a, MemoryJournal, SystemClock, Plain> {
    Outbox {
        journal,
        clock: &SystemClock,
        deliveries,
        owner: owner.into(),
        batch: 100,
    }
}

fn once_each(mut ids: Vec<String>) -> Vec<String> {
    ids.sort();
    ids
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_receiver_that_is_down_holds_up_no_other_subscription() {
    let journal = MemoryJournal::default();
    let receiver = Receiver::start().await;
    for (id, path) in [("live", "up"), ("dead", "down")] {
        journal
            .put_subscription(&receiver.subscription(id, path))
            .await
            .unwrap_or_else(|error| panic!("{error}"));
    }
    let mut events = Vec::new();
    for n in 1..=6 {
        events.push(completed_run(&journal, n).await);
    }
    let wake = Wake::new();
    let (stop, stopped) = watch::channel(false);
    let outbox = outbox(&journal, deliveries(2), "w1");
    let started = std::time::Instant::now();
    let check = async {
        eventually(|| receiver.log.ids("live").len() == 6).await;
        let live = started.elapsed();
        assert!(
            live < Duration::from_millis(1500),
            "the live subscription got its 6 events in {live:?}: sent one after the other, each \
             would wait for a delivery to the dead one (1.1 s at least)"
        );
        assert_eq!(
            once_each(receiver.log.ids("live")),
            once_each(events.clone())
        );

        // Three attempts per event, two at a time.
        eventually(|| receiver.log.ids("dead").len() == 18).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        let _ = stop.send(true);
    };
    tokio::join!(outbox.run(wake.subscribe(), stopped), check);
    let mut expected: Vec<String> = events
        .iter()
        .flat_map(|id| std::iter::repeat_n(id.clone(), 3))
        .collect();
    expected.sort();
    assert_eq!(once_each(receiver.log.ids("dead")), expected);
    assert!(receiver.down.most.load(Ordering::SeqCst) <= 2);
    assert_eq!(
        unacknowledged(&journal).await,
        0,
        "every item was acknowledged"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_workers_send_each_event_once_while_a_receiver_is_down() {
    let journal = MemoryJournal::default();
    let receiver = Receiver::start().await;
    for (id, path) in [("live", "up"), ("dead", "down")] {
        journal
            .put_subscription(&receiver.subscription(id, path))
            .await
            .unwrap_or_else(|error| panic!("{error}"));
    }
    let mut events = Vec::new();
    for n in 1..=3 {
        events.push(completed_run(&journal, n).await);
    }
    let wake = Wake::new();
    let (stop, stopped) = watch::channel(false);
    let first = outbox(&journal, deliveries(2), "w1");
    let second = outbox(&journal, deliveries(2), "w2");
    let check = async {
        // More commits while the first deliveries to the dead receiver are in progress.
        tokio::time::sleep(Duration::from_millis(300)).await;
        for n in 4..=6 {
            events.push(completed_run(&journal, n).await);
        }
        wake.bump();
        eventually(|| receiver.log.ids("dead").len() >= 18).await;
        // Long enough for a worker to claim again what the other is still sending.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let _ = stop.send(true);
    };
    tokio::join!(
        first.run(wake.subscribe(), stopped.clone()),
        second.run(wake.subscribe(), stopped),
        check
    );
    assert_eq!(
        once_each(receiver.log.ids("live")),
        once_each(events.clone()),
        "each event reached the live receiver once"
    );
    let mut expected: Vec<String> = events
        .iter()
        .flat_map(|id| std::iter::repeat_n(id.clone(), 3))
        .collect();
    expected.sort();
    assert_eq!(
        once_each(receiver.log.ids("dead")),
        expected,
        "one worker's three attempts per event, never the other's too"
    );
    assert_eq!(unacknowledged(&journal).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_claim_outlasts_every_delivery_and_a_delivery_its_window() {
    let journal = MemoryJournal::default();
    let all = deliveries(1);
    let outbox = outbox(&journal, all.clone(), "w1");
    assert_eq!(all.window(), RETRIES.window() * 2);
    assert!(outbox.lease() > all.window());

    // One slot: the second delivery waits for the first, and its window ends first.
    let receiver = Receiver::start().await;
    let silent = receiver.subscription("silent", "silent");
    let event = |n: u128| WebhookEvent {
        id: format!("e/{n}"),
        tenant: TenantId::local(),
        namespace: None,
        run: Some(run_ref(n).id),
        labels: BTreeMap::new(),
        occurred_at: Timestamp(0),
        kind: WebhookEventKind::RunQuarantined { reason: "x".into() },
    };
    let now = tokio::time::Instant::now();
    let started = std::time::Instant::now();
    let (one, two) = (event(1), event(2));
    let (first, second) = tokio::join!(
        all.send(&silent, &one, now + Duration::from_millis(700)),
        async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            all.send(&silent, &two, now + Duration::from_millis(400))
                .await
        }
    );
    assert!(started.elapsed() < Duration::from_millis(1200));
    let Err(WebhookError::Delivery { message, .. }) = first else {
        panic!("the receiver never answers");
    };
    assert!(message.ends_with("its window ended"), "{message}");
    let Err(WebhookError::Delivery { message, .. }) = second else {
        panic!("no slot was free");
    };
    assert!(message.contains("took its whole window"), "{message}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stop_leaves_the_deliveries_in_progress_to_be_sent_again() {
    let journal = MemoryJournal::default();
    let receiver = Receiver::start().await;
    journal
        .put_subscription(&receiver.subscription("dead", "down"))
        .await
        .unwrap_or_else(|error| panic!("{error}"));
    completed_run(&journal, 1).await;
    let wake = Wake::new();
    let (stop, stopped) = watch::channel(false);
    let outbox = outbox(&journal, deliveries(2), "w1");
    let stopping = async {
        eventually(|| !receiver.log.ids("dead").is_empty()).await;
        let _ = stop.send(true);
        std::time::Instant::now()
    };
    let ((), asked) = tokio::join!(outbox.run(wake.subscribe(), stopped), stopping);
    assert!(
        asked.elapsed() < Duration::from_millis(500),
        "the stop waits for no receiver"
    );
    assert_eq!(
        unacknowledged(&journal).await,
        1,
        "sent again once its claim expires"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_engine_never_waits_for_a_receiver() {
    let receiver = Receiver::start().await;
    let all = deliveries(2);
    let background = Background(all.clone());
    let event = WebhookEvent {
        id: "r/quarantined/1".into(),
        tenant: TenantId::local(),
        namespace: None,
        run: Some(run_ref(1).id),
        labels: BTreeMap::new(),
        occurred_at: Timestamp(0),
        kind: WebhookEventKind::RunQuarantined { reason: "x".into() },
    };
    let started = std::time::Instant::now();
    for subscription in [
        receiver.subscription("silent", "silent"),
        receiver.subscription("live", "up"),
    ] {
        background
            .send(&subscription, &event)
            .await
            .unwrap_or_else(|error| panic!("{error}"));
    }
    assert!(started.elapsed() < Duration::from_millis(100));
    eventually(|| receiver.log.ids("live") == ["r/quarantined/1"]).await;
    all.abort();
}
