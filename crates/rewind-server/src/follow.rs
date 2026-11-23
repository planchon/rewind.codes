//! Following what changes in the run plane (`archi/06-storage-and-scale.md`, "Messages after
//! facts"; `archi/07-code-architecture.md`, "`Cluster` and `NodeLink`"): a subscription to a
//! run, a namespace or a tenant at the owners of the partitions that hold it, here through the
//! node or elsewhere through the node link, taken again at the new owner when a partition moves.
//!
//! ```text
//!  follow(key) ── owners of key's partitions (the partition map) ──┬─ this node: NodeHandle::subscribe
//!       ▲                                                         └─ another: NodeLink::subscribe
//!       └── the map changed: subscribe again where the owners are now
//! ```
//!
//! Nothing is read on a timer: the task waits on the owners' streams and on the map.

use std::collections::BTreeSet;
use std::sync::Arc;

use futures_util::StreamExt;
use futures_util::stream::{BoxStream, SelectAll};
use rewind_core::PartitionId;
use rewind_engine::ports::link::{Changed, SubscriptionKey};
use rewind_engine::ports::{NodeId, NodeLink as _, PartitionMap, Scope};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use crate::app::App;

/// The changes of `key`, from wherever its owners are, until the receiver is dropped or the
/// server stops. The stream ends when the owners cannot be reached: the caller reads again what
/// it shows.
pub(crate) fn follow(app: &Arc<App>, key: SubscriptionKey) -> UnboundedReceiver<Changed> {
    let (sink, changes) = unbounded_channel();
    let app = Arc::clone(app);
    tokio::spawn(async move {
        let Some(partitions) = partitions(&app, &key).await else {
            return;
        };
        let mut map = app.map.clone();
        let mut stop = app.stop.subscribe();
        loop {
            let owners = owners_of(&map.borrow_and_update(), &partitions);
            let mut streams: SelectAll<BoxStream<'static, Changed>> = SelectAll::new();
            for owner in &owners {
                let subscribed = if owner == app.node.node() {
                    app.node.subscribe(key.clone(), None).map(StreamExt::boxed)
                } else {
                    app.link.subscribe(owner, key.clone(), None).await
                };
                match subscribed {
                    Ok(stream) => streams.push(stream),
                    Err(error) => {
                        tracing::debug!(%owner, %error, "a subscription could not be taken");
                    }
                }
            }
            if streams.is_empty() && !owners.is_empty() {
                return;
            }
            loop {
                tokio::select! {
                    () = sink.closed() => return,
                    _ = stop.changed() => return,
                    moved = map.changed() => {
                        if moved.is_err() {
                            return;
                        }
                        if owners_of(&map.borrow(), &partitions) != owners {
                            break;
                        }
                    }
                    changed = streams.next(), if !streams.is_empty() => match changed {
                        Some(changed) => {
                            if sink.send(changed).is_err() {
                                return;
                            }
                        }
                        // Every owner's stream ended: one gave its partition up, or was cut.
                        None => return,
                    },
                }
            }
        }
    });
    changes
}

/// The partitions that hold a key.
async fn partitions(app: &App, key: &SubscriptionKey) -> Option<Vec<PartitionId>> {
    let scope = match key {
        SubscriptionKey::Run(run) => {
            return app.edge.partition_of(run.id).await.ok().map(|p| vec![p]);
        }
        SubscriptionKey::Namespace { tenant, namespace } => Scope::Namespace {
            tenant: tenant.clone(),
            namespace: namespace.clone(),
        },
        SubscriptionKey::Tenant(tenant) => Scope::Tenant(tenant.clone()),
    };
    app.edge.partitions_of(&scope).await.ok()
}

/// The nodes that own these partitions now.
fn owners_of(map: &PartitionMap, partitions: &[PartitionId]) -> BTreeSet<NodeId> {
    partitions
        .iter()
        .filter_map(|partition| map.owner(*partition).cloned())
        .collect()
}
