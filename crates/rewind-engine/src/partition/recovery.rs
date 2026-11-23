//! The open and the recovery of a partition (`archi/06-storage-and-scale.md`, "Partitions,
//! owners and nodes": recovery; `archi/04-tools.md`, "Tool task lifecycle").
//!
//! `open` makes the owner the only writer: it fences the previous one. The owner then reads,
//! once, what the previous owner held in memory and rebuilds it from the records:
//!
//! - the runnable runs, queued with one more started activation each (`Interrupted`), since an
//!   activation may have been running when the previous owner died;
//! - the timers due before the horizon, to the wheel;
//! - the calls in flight: every `ready` task that is due is interrupted, sent again with
//!   attempt + 1 under `retry`, failed `outcome_unknown` under `at_most_once`, in the recovery
//!   batch;
//! - the outbox, to the projections; the ended attempts not sealed, to the sealer;
//! - the counters, reported to their organization actors; the organizations homed here;
//! - the quarantined runs of the epochs this build runs that no node of this build tried, tried
//!   once;
//! - the namespaces of the partition being erased.

use std::sync::Arc;



use crate::node::NodeMsg;
use crate::node::message::{Effect, Message, job};
use crate::partition::commit::{After, Unit};
use crate::partition::{Internal, Owner, OwnerMsg, Phase};
use crate::plane::{LogOf, Plane};
use crate::ports::partition::Change;
use crate::ports::{
    CatalogStore, NamespaceName, PartitionLog, PartitionStore, Recovery, TaskState,
    TenantId, namespace_partition,
};

/// A partition opened and recovered.
pub(crate) struct Opened<P: Plane> {
    pub(crate) log: Arc<LogOf<P>>,
    pub(crate) recovery: Recovery,
    /// The highest inbox item id of the runs whose inbox is not empty.
    pub(crate) inbox_max: u64,
    /// The namespaces of the partition being erased.
    pub(crate) deleting: Vec<(TenantId, NamespaceName)>,
}

/// The job that opens the partition and reads what its owner rebuilds.
pub(crate) fn open_job<P: Plane>(owner: &Owner<P>) -> Effect<P> {
    let plane = Arc::clone(&owner.plane);
    let partition = owner.partition;
    let node = owner.node.clone();
    let partitions = owner.config.partitions;
    let horizon = owner.config.horizon;
    job(async move {
        let opened = async {
            let log = plane
                .partitions()
                .open(partition, &node)
                .await
                .map_err(|error| error.to_string())?;
            let now = crate::ports::Clock::now(plane.clock());
            let recovery = log
                .recover(now.saturating_add(horizon))
                .await
                .map_err(|error| error.to_string())?;
            let mut inbox_max = 0;
            for head in recovery.runnable.iter().chain(&recovery.quarantined) {
                for item in log
                    .inbox(&head.run)
                    .await
                    .map_err(|error| error.to_string())?
                {
                    inbox_max = inbox_max.max(item.id);
                }
            }
            let deleting = plane
                .catalog()
                .namespaces_to_erase(1_000)
                .await
                .map_err(|error| error.to_string())?
                .into_iter()
                .filter(|(tenant, name)| namespace_partition(tenant, name, partitions) == partition)
                .collect();
            Ok::<_, String>(Opened {
                log: Arc::new(log),
                recovery,
                inbox_max,
                deleting,
            })
        }
        .await;
        Message::Owner(
            partition,
            match opened {
                Ok(opened) => OwnerMsg::Opened(Box::new(opened)),
                Err(error) => OwnerMsg::OpenFailed(error),
            },
        )
    })
}

impl<P: Plane> Owner<P> {
    pub(crate) fn opened(&mut self, opened: Opened<P>) {
        let Opened {
            log,
            recovery,
            inbox_max,
            deleting,
        } = opened;
        let Recovery {
            runnable,
            quarantined,
            timers,
            tasks,
            unsealed,
            outbox,
            counters,
            organizations,
        } = recovery;
        self.log = Some(log);
        self.phase = Phase::Open;
        self.next_inbox = inbox_max.saturating_add(1).max(1);
        self.next_outbox = outbox
            .iter()
            .map(|entry| entry.id.0)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        self.wheel.load(timers);
        self.org_states = organizations;
        let now = self.now();
        tracing::info!(
            partition = %self.partition,
            runnable = runnable.len(),
            tasks = tasks.len(),
            "partition opened"
        );

        let mut unit = Unit::new();
        for head in &runnable {
            unit.change(Change::Interrupted {
                run: head.run.clone(),
            });
            unit.then(After::Queue(head.run.clone()));
        }
        // The calls that may have been in flight, as their policy says.
        let mut interrupted = Vec::new();
        for record in tasks {
            if let TaskState::Ready { next_attempt_at } = record.task.state
                && next_attempt_at <= now
            {
                interrupted.push(record);
            }
        }
        for record in interrupted {
            let task = record.task.task.clone();
            let next = record.task.attempt.saturating_add(1);
            match crate::tasks::calls::interrupted(
                &record.task.tool,
                record.task.policy.on_interruption,
                next,
                self.config.max_tool_attempts,
            ) {
                Some(failure) => self.finish_in(&mut unit, &task, &record, Err(failure)),
                None => {
                    let mut record = record;
                    record.task.attempt = next;
                    unit.change(Change::Task {
                        run: task.run.clone(),
                        seq: task.seq,
                        change: crate::ports::TaskChange::Put(Box::new(record)),
                    });
                    unit.then(After::Dispatch(task));
                }
            }
        }
        for (run, _) in unsealed {
            unit.then(After::Seal(run));
        }
        for entry in outbox {
            unit.then(After::Project(entry));
        }
        for counted in counters {
            unit.then(After::Report {
                tenant: counted.tenant,
                namespace: counted.namespace,
                included: Vec::new(),
                abandoned: Vec::new(),
            });
        }
        for head in quarantined {
            let retried = head
                .quarantine
                .as_ref()
                .and_then(|quarantine| quarantine.retried_by.as_deref())
                == Some(self.config.build.as_str());
            if self.epochs.contains(&head.epoch) && !retried {
                unit.then(After::Owner(OwnerMsg::Internal(Internal::RetryQuarantined(
                    head.run.clone(),
                ))));
            }
        }
        for (tenant, name) in deleting {
            unit.then(After::Owner(OwnerMsg::ControlPlane(
                crate::ports::ControlPlaneChange::Namespace { tenant, name },
            )));
        }
        self.push(unit);
        for message in std::mem::take(&mut self.opening) {
            self.dispatch(message);
        }
    }

    pub(crate) fn open_failed(&mut self, error: String) {
        tracing::error!(partition = %self.partition, %error, "the partition could not be opened");
        self.close();
        self.out.push(Effect::Local(Message::Node(NodeMsg::OpenFailed(
            self.partition,
            error,
        ))));
    }
}
