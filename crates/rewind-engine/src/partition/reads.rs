//! The reads an edge asks the owner of a partition (`archi/05-testing-and-observability.md`,
//! "Read model", "Statistics"; `archi/06-storage-and-scale.md`, "Retention and deletion").
//!
//! A read answers what the partition's last durable batch left: a run's head and summary, its
//! history (from its partition while its attempt is live, from the archive once sealed), its
//! tasks, a page of a search, counters and gauges, shapes. Each read is a job of its own.

use std::sync::Arc;

use crate::node::message::{Message, ReplyTo, job};
use crate::partition::Owner;
use crate::plane::Plane;
use crate::ports::link::{NodeReply, PartitionReply, PartitionRequest, Refusal, RunView};
use crate::ports::{PartitionLog, RunArchive};

/// A refusal in the shape of the request's reply.
pub(crate) fn refused(request: &PartitionRequest, refusal: Refusal) -> PartitionReply {
    match request {
        PartitionRequest::Start(_) => PartitionReply::Started(Err(refusal)),
        PartitionRequest::Signal(_)
        | PartitionRequest::Cancel { .. }
        | PartitionRequest::Webhook { .. } => PartitionReply::Enqueued(Err(refusal)),
        PartitionRequest::Release { .. }
        | PartitionRequest::GiveUp { .. }
        | PartitionRequest::Erase { .. } => PartitionReply::Done(Err(refusal)),
        PartitionRequest::Answer { .. } | PartitionRequest::Decide { .. } => {
            PartitionReply::Answered(Err(refusal))
        }
        PartitionRequest::GetRun { .. } => PartitionReply::Run(Err(refusal)),
        PartitionRequest::History { .. } => PartitionReply::History(Err(refusal)),
        PartitionRequest::Tasks { .. } => PartitionReply::Tasks(Err(refusal)),
        PartitionRequest::Search { .. } => PartitionReply::Found(Err(refusal)),
        PartitionRequest::Counters { .. } => PartitionReply::Counted(Err(refusal)),
        PartitionRequest::Shapes { .. } => PartitionReply::Shapes(Err(refusal)),
    }
}

fn unavailable(error: impl ToString) -> Refusal {
    Refusal::Unavailable(error.to_string())
}

impl<P: Plane> Owner<P> {
    pub(crate) fn read(&mut self, request: PartitionRequest, reply: ReplyTo) {
        let Some(log) = self.log.clone() else {
            return;
        };
        let plane = Arc::clone(&self.plane);
        self.out.push(job(async move {
            let answer = match request {
                PartitionRequest::GetRun { run } => PartitionReply::Run(
                    async {
                        let Some(head) = log.run(&run).await.map_err(unavailable)? else {
                            return Ok(None);
                        };
                        let summary = log.summary(&run).await.map_err(unavailable)?;
                        Ok(Some(RunView {
                            head,
                            summary: summary.map(|indexed| indexed.summary),
                        }))
                    }
                    .await,
                ),
                PartitionRequest::History { run, from } => PartitionReply::History(
                    async {
                        let Some(head) = log.run(&run).await.map_err(unavailable)? else {
                            return Ok(None);
                        };
                        if head.sealed == Some(head.attempt) {
                            let sealed = plane
                                .archive()
                                .get(&run, head.attempt)
                                .await
                                .map_err(unavailable)?;
                            return Ok(sealed.map(|sealed| {
                                sealed
                                    .entries
                                    .into_iter()
                                    .filter(|entry| entry.index >= from)
                                    .collect()
                            }));
                        }
                        log.history(&run, head.attempt, from)
                            .await
                            .map(Some)
                            .map_err(unavailable)
                    }
                    .await,
                ),
                PartitionRequest::Tasks { run } => PartitionReply::Tasks(
                    async {
                        if log.run(&run).await.map_err(unavailable)?.is_none() {
                            return Ok(None);
                        }
                        log.tasks(&run).await.map(Some).map_err(unavailable)
                    }
                    .await,
                ),
                PartitionRequest::Search {
                    scope,
                    query,
                    order,
                    page,
                } => PartitionReply::Found(
                    log.search(&scope, &query, order, &page)
                        .await
                        .map_err(unavailable),
                ),
                PartitionRequest::Counters { scope, window } => PartitionReply::Counted(
                    async {
                        let counters = log.counters(&scope, window).await.map_err(unavailable)?;
                        let gauges = log.gauges(&scope).await.map_err(unavailable)?;
                        Ok((counters, gauges))
                    }
                    .await,
                ),
                PartitionRequest::Shapes {
                    tenant,
                    namespace,
                    tools,
                } => PartitionReply::Shapes(
                    log.shapes(&tenant, namespace.as_ref(), &tools)
                        .await
                        .map_err(unavailable),
                ),
                other => refused(&other, Refusal::Unavailable("not a read".into())),
            };
            Message::Reply(reply, NodeReply::Partition(Box::new(answer)))
        }));
    }
}
