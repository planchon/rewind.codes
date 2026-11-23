//! The router of a process (`archi/06-storage-and-scale.md`, "Partitions, owners and nodes":
//! the edge; `archi/07-code-architecture.md`, "Inside a node").
//!
//! It finds the partition of a run from its id: the 16 bits a run id holds, the control plane's
//! `legacy_runs` for a run `migrate-runs` moved from the `legacy` schema (whose version 7 id
//! names no partition), and partition 0 in a deployment of one partition. It finds the owner of
//! a partition from the partition map, cached, dropped on `ControlPlaneChange::PartitionMap`;
//! an owner that answers `NotOwner` makes it read the map again and try once more.

use std::num::NonZeroU16;
use std::sync::{Mutex, PoisonError};

use rewind_core::{PartitionId, RunId};

use crate::ports::{CatalogError, CatalogStore, NodeId, PartitionMap};

/// Where a partition's owner is, for an edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// An owner of this process.
    Local,
    /// The owner on another node, through the link.
    Remote(NodeId),
    /// No node owns it now.
    Unowned,
}

/// The partition of a run among `partitions`: the 16 bits a run id holds, the control plane's
/// `legacy_runs` for a run `migrate-runs` moved, or partition 0 when the deployment has one. `None`: no partition holds the run.
pub async fn partition_of<C: CatalogStore>(
    catalog: &C,
    partitions: NonZeroU16,
    run: RunId,
) -> Result<Option<PartitionId>, CatalogError> {
    if let Some(partition) = run.partition() {
        return Ok(Some(partition));
    }
    if let Some(partition) = catalog.legacy_partition(run).await? {
        return Ok(Some(partition));
    }
    Ok((partitions.get() == 1).then_some(PartitionId::new(0)))
}

/// The router of a process: the partition map it caches.
#[derive(Debug)]
pub struct Router {
    node: NodeId,
    partitions: NonZeroU16,
    map: Mutex<Option<PartitionMap>>,
}

impl Router {
    pub fn new(node: NodeId, partitions: NonZeroU16) -> Self {
        Self {
            node,
            partitions,
            map: Mutex::new(None),
        }
    }

    /// The partition of a run: from its id, from the control plane for a run of the previous
    /// design, or the only one.
    pub async fn partition_of<C: CatalogStore>(
        &self,
        catalog: &C,
        run: RunId,
    ) -> Result<Option<PartitionId>, CatalogError> {
        partition_of(catalog, self.partitions, run).await
    }

    /// The map cached, if any.
    pub fn map(&self) -> Option<PartitionMap> {
        self.map
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Caches a map read from the cluster, or received as a change.
    pub fn set_map(&self, map: PartitionMap) {
        *self.map.lock().unwrap_or_else(PoisonError::into_inner) = Some(map);
    }

    /// Drops the map cached: the next route reads it again.
    pub fn forget(&self) {
        *self.map.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }

    /// Where the owner of `partition` is, by the map cached; `None` when no map is cached.
    pub fn route(&self, partition: PartitionId) -> Option<Route> {
        let map = self.map.lock().unwrap_or_else(PoisonError::into_inner);
        let map = map.as_ref()?;
        Some(match map.owner(partition) {
            Some(node) if *node == self.node => Route::Local,
            Some(node) => Route::Remote(node.clone()),
            None => Route::Unowned,
        })
    }
}

#[cfg(test)]
mod tests {
    use rewind_core::Timestamp;

    use super::*;
    use crate::adapters::MemoryCatalog;
    use crate::ports::PartitionOwner;

    #[tokio::test]
    async fn a_run_is_routed_from_its_id_its_legacy_row_or_the_only_partition() {
        let node = NodeId::new("a").unwrap();
        let catalog = MemoryCatalog::new();
        let router = Router::new(node.clone(), NonZeroU16::new(64).unwrap());
        let run = RunId::in_partition(PartitionId::new(7), Timestamp(1), 1);
        assert_eq!(
            router.partition_of(&catalog, run).await.unwrap(),
            Some(PartitionId::new(7))
        );
        let legacy = RunId::from_u128(0x0190_0000_0000_7000_8000_0000_0000_0001);
        assert_eq!(router.partition_of(&catalog, legacy).await.unwrap(), None);
        catalog
            .put_legacy_run(
                &crate::ports::RunRef::new(crate::ports::TenantId::local(), legacy),
                PartitionId::new(3),
            )
            .await
            .unwrap();
        assert_eq!(
            router.partition_of(&catalog, legacy).await.unwrap(),
            Some(PartitionId::new(3))
        );
        let embedded = Router::new(node.clone(), NonZeroU16::new(1).unwrap());
        let other = RunId::from_u128(0x0190_0000_0000_7000_8000_0000_0000_0002);
        assert_eq!(
            embedded.partition_of(&catalog, other).await.unwrap(),
            Some(PartitionId::new(0))
        );

        assert_eq!(router.route(PartitionId::new(0)), None, "no map cached");
        let mut map = PartitionMap::unowned(2);
        map.partitions.insert(
            PartitionId::new(1),
            PartitionOwner {
                node: Some(node),
                epoch: crate::ports::PartitionEpoch(1),
            },
        );
        router.set_map(map);
        assert_eq!(router.route(PartitionId::new(1)), Some(Route::Local));
        assert_eq!(router.route(PartitionId::new(0)), Some(Route::Unowned));
        router.forget();
        assert_eq!(router.route(PartitionId::new(1)), None);
    }
}
