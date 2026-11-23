//! `rewind server migrate-runs`: the runs of the tables the schema migration set aside, moved
//! onto the run plane once (`archi/08-decisions.md`, "Taken for the object-storage rewrite": C2;
//! "Taken while writing the object-storage spec": the migration; `archi/06-storage-and-scale.md`,
//! "Where data lives").
//!
//! ```text
//!  for each set-aside run, by id:
//!      in legacy_runs already ─────────────────────────────► skipped
//!      its namespace's partition (ensure_namespace), opened once: the only writer
//!      in the partition already (moved, not recorded) ─────► recorded, skipped
//!      its history: from the tables, or its archived journal
//!      its payloads above 64 KiB ─────────────────────────► blobs under the run
//!      ended: its attempt sealed in the archive, then one batch:
//!             head, history, payload records, summary, counters; Seal; its retention's timer
//!      live:  one batch: head, history, payload records, summary, counters; its inbox, signal
//!             keys, tool tasks (in flight: ready), timers (its script's, its deadline, a
//!             backoff), watchers, webhook tokens
//!      its legacy_runs row: the router finds its version 7 id
//!  every run moved ───────────────────────────────────────► the tables dropped
//! ```
//!
//! A run keeps its version 7 id. Its head and summary are derived from its history as an
//! activation derives them; the counters it moves (created, started, ended by status, its calls,
//! its script, its failure group, the shapes of its results) are written with it, so statistics
//! and quotas count it; the CPU time it used is not known per run and is not counted. A task in
//! flight (claimed, or deferred with a token the run plane cannot route) is written `ready`:
//! recovery sends it again with attempt + 1, or fails it under `at_most_once`. A run that cannot
//! be moved (its script, its toolset, a blob or its journal missing, its history corrupt) is
//! reported and left in the tables, which are then not dropped; the command can run again: what
//! moved is skipped.
//!
//! Opening a partition fences its writer: the command runs while no node runs.

use std::collections::BTreeMap;
use std::num::NonZeroU16;
use std::time::Duration;

use rewind_core::{
    Digest, HistoryEntry, JsonValue, PartitionId, Payload, RunState, Timestamp, Toolset,
};

use crate::executors::derive;
use crate::payloads::payload_mut;
use crate::ports::legacy::{LegacyBlobs, LegacyError, LegacyHistory, LegacyRun, LegacyRuns};
use crate::ports::partition::CreateRun;
use crate::ports::{
    ArchiveError, Attempt, Batch, BlobError, BlobOwner, BlobStore, CatalogError, CatalogStore,
    Change, DueTimer, NamespaceWrite, NodeId, PartitionError, PartitionLog, PartitionStore,
    PayloadRecord, RunArchive, RunHead, RunRef, SealedAttempt, TaskChange, TaskRecord, TaskRef,
    TaskState, Tenant, TimerChange, TimerKind, ToolTask, namespace_partition,
};

/// The runs read from the tables at once.
pub const PAGE: usize = 100;

/// What stops the migration: a backend that cannot be reached or refuses. Running the command
/// again goes on from where it stopped.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MigrateError {
    #[error(transparent)]
    Legacy(#[from] LegacyError),
    #[error(transparent)]
    Catalog(#[from] CatalogError),
    #[error(transparent)]
    Partition(#[from] PartitionError),
    #[error(transparent)]
    Archive(#[from] ArchiveError),
    #[error(transparent)]
    Blob(#[from] BlobError),
}

/// How a run moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MovedAs {
    /// Ended: its attempt sealed in the archive, its head and summary in its partition.
    Ended,
    /// Live: everything it holds in its partition.
    Live,
}

/// A run moved into a partition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Moved {
    pub run: RunRef,
    pub partition: PartitionId,
    pub how: MovedAs,
}

/// What the migration did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub moved: Vec<Moved>,
    /// The runs a previous migration moved.
    pub skipped: Vec<RunRef>,
    /// The runs that could not be moved, and why: they stay in the tables.
    pub failed: Vec<(RunRef, String)>,
    /// Every run moved, and the tables were dropped.
    pub dropped: bool,
}

/// The adapters the migration reads and writes, and where it writes.
#[derive(Debug)]
pub struct Migration<'a, C, P, A, B, L, O> {
    pub catalog: &'a C,
    pub partitions: &'a P,
    pub archive: &'a A,
    pub blobs: &'a B,
    pub legacy: &'a L,
    pub legacy_blobs: &'a O,
    /// The partitions of the deployment.
    pub partition_count: NonZeroU16,
    /// The writer the partitions are opened for.
    pub node: NodeId,
    /// When the migration runs: the time of the counters' and the tasks' writes.
    pub now: Timestamp,
}

/// Why one run was not moved: it stays in the tables.
enum Refused {
    Run(String),
    Fatal(MigrateError),
}

impl From<LegacyError> for Refused {
    fn from(error: LegacyError) -> Self {
        match error {
            LegacyError::Corrupt(why) => Refused::Run(why),
            other => Refused::Fatal(other.into()),
        }
    }
}

impl From<BlobError> for Refused {
    fn from(error: BlobError) -> Self {
        match error {
            BlobError::Missing(_) | BlobError::Corrupt { .. } => Refused::Run(error.to_string()),
            other => Refused::Fatal(other.into()),
        }
    }
}

macro_rules! fatal {
    ($($error:ty),*) => {
        $(impl From<$error> for Refused {
            fn from(error: $error) -> Self {
                Refused::Fatal(error.into())
            }
        })*
    };
}

fatal!(CatalogError, PartitionError, ArchiveError);

impl<C, P, A, B, L, O> Migration<'_, C, P, A, B, L, O>
where
    C: CatalogStore,
    P: PartitionStore,
    A: RunArchive,
    B: BlobStore,
    L: LegacyRuns,
    O: LegacyBlobs,
{
    /// Moves every set-aside run, then drops the tables if each one moved.
    pub async fn run(&self) -> Result<Report, MigrateError> {
        let mut report = Report::default();
        let mut logs: BTreeMap<PartitionId, P::Log> = BTreeMap::new();
        let mut after = None;
        loop {
            let page = self.legacy.runs(after, PAGE).await?;
            let Some(last) = page.last() else {
                break;
            };
            after = Some(last.run.id);
            for legacy in page {
                let run = legacy.run.clone();
                match self.move_run(&mut logs, legacy).await {
                    Ok(Some(moved)) => {
                        tracing::info!(%run, partition = %moved.partition, how = ?moved.how, "run moved");
                        report.moved.push(moved);
                    }
                    Ok(None) => report.skipped.push(run),
                    Err(Refused::Run(why)) => {
                        tracing::error!(%run, %why, "run not moved");
                        report.failed.push((run, why));
                    }
                    Err(Refused::Fatal(error)) => return Err(error),
                }
            }
        }
        if report.failed.is_empty() {
            self.legacy.drop_tables().await?;
            report.dropped = true;
        }
        Ok(report)
    }

    async fn move_run(
        &self,
        logs: &mut BTreeMap<PartitionId, P::Log>,
        legacy: LegacyRun,
    ) -> Result<Option<Moved>, Refused> {
        let run = legacy.run.clone();
        if self.catalog.legacy_partition(run.id).await?.is_some() {
            return Ok(None);
        }
        let partition = match self
            .catalog
            .ensure_namespace(
                &run.tenant,
                &legacy.namespace,
                self.partition_count,
                self.now,
            )
            .await?
        {
            NamespaceWrite::Written(namespace) => namespace.partition,
            NamespaceWrite::Deleting => {
                namespace_partition(&run.tenant, &legacy.namespace, self.partition_count)
            }
        };
        let log = match logs.entry(partition) {
            std::collections::btree_map::Entry::Occupied(open) => open.into_mut(),
            std::collections::btree_map::Entry::Vacant(closed) => {
                closed.insert(self.partitions.open(partition, &self.node).await?)
            }
        };
        let log = &*log;
        if log.run(&run).await?.is_some() {
            self.catalog.put_legacy_run(&run, partition).await?;
            return Ok(None);
        }
        let how = self.write(log, legacy).await?;
        self.catalog.put_legacy_run(&run, partition).await?;
        Ok(Some(Moved {
            run,
            partition,
            how,
        }))
    }

    /// Writes the run into its partition, and an ended run's attempt into the archive.
    async fn write(&self, log: &P::Log, legacy: LegacyRun) -> Result<MovedAs, Refused> {
        let run = legacy.run.clone();
        let tenant = &run.tenant;
        let history = match &legacy.history {
            LegacyHistory::Entries(entries) => entries.clone(),
            LegacyHistory::Archived(document) => self
                .legacy_blobs
                .journal(tenant, *document)
                .await?
                .ok_or_else(|| {
                    Refused::Run(format!("its archived journal {document} is missing"))
                })?,
        };
        let script = self
            .catalog
            .get_script(tenant, legacy.script)
            .await?
            .ok_or_else(|| Refused::Run(format!("its script {} is missing", legacy.script)))?
            .script;
        let toolset = self
            .catalog
            .get_toolset(tenant, legacy.toolset)
            .await?
            .ok_or_else(|| Refused::Run(format!("its toolset {} is missing", legacy.toolset)))?
            .toolset;
        if !history.is_empty() {
            RunState::from_history(&history)
                .map_err(|error| Refused::Run(format!("its history is corrupt: {error}")))?;
        }

        // Every payload belongs to the run: the values above 64 KiB become its blobs.
        let mut values: BTreeMap<Digest, JsonValue> = BTreeMap::new();
        for record in &legacy.payloads {
            match &record.value {
                Some(value) => {
                    values.insert(record.digest, value.clone());
                }
                None => {
                    let bytes = self
                        .legacy_blobs
                        .payload(tenant, record.digest)
                        .await?
                        .ok_or_else(|| {
                            Refused::Run(format!(
                                "the blob of its payload {} is missing",
                                record.digest
                            ))
                        })?;
                    let value = serde_json::from_slice(&bytes).map_err(|error| {
                        Refused::Run(format!(
                            "its payload {} is not JSON: {error}",
                            record.digest
                        ))
                    })?;
                    self.blobs
                        .put(&BlobOwner::Run(run.clone()), record.digest, bytes)
                        .await?;
                    values.insert(record.digest, value);
                }
            }
        }

        let tasks = tasks_of(&legacy, &toolset, self.now)?;
        let by_seq: BTreeMap<_, _> = tasks
            .iter()
            .map(|task| (task.task.task.seq, task.clone()))
            .collect();
        let mut head = RunHead {
            run: run.clone(),
            namespace: legacy.namespace.clone(),
            script: legacy.script,
            toolset: legacy.toolset,
            status: rewind_core::RunStatus::Pending,
            epoch: legacy.epoch,
            attempt: Attempt::FIRST,
            sealed: None,
            failures: legacy.failures,
            attempts: legacy.attempts,
            quarantine: legacy.quarantine.clone(),
            waits: Vec::new(),
            expires_at: legacy.expires_at,
            history_len: history.len() as u64,
            metadata: legacy.metadata.clone(),
            idempotency_key: legacy.idempotency_key.clone(),
            created_at: legacy.created_at,
            updated_at: history
                .last()
                .map_or(legacy.updated_at, |entry| entry.recorded_at),
            ended_at: None,
        };
        let headline = crate::projector::headline(script.source());
        let summary = derive::summary_of(&head, &headline, &history, &toolset, &by_seq);
        head.status = summary.summary.status;
        let ended = head.is_ended();
        if ended {
            head.ended_at = summary.summary.ended_at.or(legacy.ended_at);
            head.quarantine = None;
        } else {
            head.waits = summary.summary.waits.clone();
        }
        let summary = derive::with_quarantine(summary, head.quarantine.as_ref());

        let mut inlined = history.clone();
        inline(&mut inlined, &values);
        let mut counters = vec![derive::created_counters(&head)];
        counters.extend(derive::counters_of(
            &head,
            &history,
            0,
            &summary,
            Duration::ZERO,
            self.now,
        ));
        counters.extend(derive::shapes_of(&head, &inlined, 0));

        let mut batch = Batch::new();
        batch.push(CreateRun {
            run: run.clone(),
            head: head.clone(),
            start_key: legacy.idempotency_key.clone(),
            entries: history.clone(),
            payloads: legacy.payloads.clone(),
            summary,
        });
        batch.push(Change::Counters(counters));
        let how = if ended {
            let mut sealed = history;
            inline(&mut sealed, &table_values(&legacy.payloads));
            self.archive
                .put(&SealedAttempt {
                    run: run.clone(),
                    attempt: Attempt::FIRST,
                    script,
                    toolset,
                    entries: sealed,
                })
                .await?;
            let retention = self
                .catalog
                .get_tenant(tenant)
                .await?
                .unwrap_or_else(|| Tenant::new(tenant.clone()))
                .retention
                .runs;
            let ended_at = head.ended_at.unwrap_or(head.updated_at);
            batch.push(Change::Seal {
                run: run.clone(),
                attempt: Attempt::FIRST,
            });
            batch.push(Change::Timer(TimerChange::Set(DueTimer {
                at: ended_at.saturating_add(retention),
                kind: TimerKind::RetentionEnd { run: run.clone() },
            })));
            MovedAs::Ended
        } else {
            live(&mut batch, &legacy, tasks, self.now);
            MovedAs::Live
        };
        match log.commit(batch).await {
            Ok(()) => Ok(how),
            Err(PartitionError::Conflict(why) | PartitionError::Corrupt(why)) => {
                Err(Refused::Run(format!("its partition refused it: {why}")))
            }
            Err(error) => Err(Refused::Fatal(error.into())),
        }
    }
}

/// What a live run holds beside its head and history, as changes of its batch.
fn live(batch: &mut Batch, legacy: &LegacyRun, tasks: Vec<TaskRecord>, now: Timestamp) {
    let run = &legacy.run;
    for item in &legacy.inbox {
        batch.push(Change::Enqueue {
            run: run.clone(),
            item: item.clone(),
            signal_key: None,
        });
    }
    for key in &legacy.signal_keys {
        batch.push(Change::SignalKey {
            run: run.clone(),
            key: key.clone(),
        });
    }
    for task in tasks {
        let seq = task.task.task.seq;
        if let TaskState::Ready { next_attempt_at } = task.task.state
            && next_attempt_at > now
        {
            batch.push(Change::Timer(TimerChange::Set(DueTimer {
                at: next_attempt_at,
                kind: TimerKind::RetryBackoff {
                    run: run.clone(),
                    seq,
                },
            })));
        }
        batch.push(Change::Task {
            run: run.clone(),
            seq,
            change: TaskChange::Put(Box::new(task)),
        });
    }
    for (seq, at) in &legacy.timers {
        batch.push(Change::Timer(TimerChange::Set(DueTimer {
            at: *at,
            kind: TimerKind::Script {
                run: run.clone(),
                seq: *seq,
            },
        })));
    }
    if !legacy.expiry_enqueued {
        batch.push(Change::Timer(TimerChange::Set(DueTimer {
            at: legacy.expires_at,
            kind: TimerKind::RunDeadline { run: run.clone() },
        })));
    }
    for watcher in &legacy.watchers {
        batch.push(Change::Watch {
            run: run.clone(),
            watcher: *watcher,
        });
    }
    for webhook in &legacy.webhooks {
        batch.push(Change::WebhookToken {
            run: run.clone(),
            name: webhook.name.clone(),
            hash: webhook.hash,
        });
    }
}

/// The run's tool tasks, their route and policy from the pinned toolset. A task in flight is
/// `ready` from now: what was in flight is in no node's memory any more.
fn tasks_of(
    legacy: &LegacyRun,
    toolset: &Toolset,
    now: Timestamp,
) -> Result<Vec<TaskRecord>, Refused> {
    legacy
        .tasks
        .iter()
        .map(|task| {
            let pinned = toolset
                .tool(&task.tool)
                .ok_or_else(|| Refused::Run(format!("its toolset has no tool {:?}", task.tool)))?;
            let state = match &task.state {
                TaskState::Claimed { .. } | TaskState::Deferred { .. } => TaskState::Ready {
                    next_attempt_at: now,
                },
                other => other.clone(),
            };
            Ok(TaskRecord {
                task: ToolTask {
                    task: TaskRef {
                        run: legacy.run.clone(),
                        seq: task.seq,
                    },
                    toolset: legacy.toolset,
                    tool: task.tool.clone(),
                    route: pinned.route.clone(),
                    args: task.args.clone(),
                    policy: pinned.policy,
                    state,
                    // The run plane counts the first attempt from the task's creation.
                    attempt: task.attempt.max(1),
                    schema_change_approved: task.schema_change_approved,
                    created_at: task.created_at,
                },
                token_hash: None,
            })
        })
        .collect()
}

/// The values of the payload records that hold one: those up to 64 KiB, which a sealed journal
/// inlines.
fn table_values(records: &[PayloadRecord]) -> BTreeMap<Digest, JsonValue> {
    records
        .iter()
        .filter_map(|record| Some((record.digest, record.value.clone()?)))
        .collect()
}

/// Puts the known values back in the entries' payloads.
fn inline(entries: &mut [HistoryEntry], values: &BTreeMap<Digest, JsonValue>) {
    for entry in entries {
        if let Some(payload) = payload_mut(&mut entry.event)
            && let Payload::Digest(digest) = payload
            && let Some(value) = values.get(digest)
        {
            *payload = Payload::Value(value.clone());
        }
    }
}
