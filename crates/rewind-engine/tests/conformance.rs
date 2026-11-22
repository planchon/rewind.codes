//! The in-memory adapters pass the conformance suites of their ports: they are the reference
//! implementations of the contracts.

use std::collections::{BTreeMap, BTreeSet};

use rewind_engine::adapters::{
    MemoryAccess, MemoryArchive, MemoryBlobs, MemoryCatalog, MemoryCluster, MemoryJournal,
    MemoryPartitions, MemoryReadModel, MemorySecrets, MemorySpanExporter, MemoryWebhooks,
};
use rewind_engine::conformance;
use rewind_engine::ports::{Secret, Subscription, TenantId, WebhookEventType};

#[tokio::test]
async fn memory_catalog_conforms() {
    conformance::catalog::run_all(|| {
        let cluster = MemoryCluster::new(conformance::cluster::PARTITIONS);
        (
            MemoryCatalog::with_changes(cluster.changes().clone()),
            cluster,
        )
    })
    .await;
}

#[tokio::test]
async fn memory_partitions_conform() {
    conformance::partition::run_all(MemoryPartitions::new).await;
}

#[tokio::test]
async fn memory_archive_conforms() {
    conformance::archive::run_all(|| {
        let blobs = MemoryBlobs::new();
        (MemoryArchive::with_blobs(blobs.clone()), blobs)
    })
    .await;
}

#[tokio::test]
async fn memory_cluster_conforms() {
    conformance::cluster::run_all(|| MemoryCluster::new(conformance::cluster::PARTITIONS)).await;
}

#[tokio::test]
async fn memory_journal_conforms() {
    conformance::journal::run_all(MemoryJournal::new).await;
}

#[tokio::test]
async fn memory_access_conforms() {
    conformance::access::run_all(MemoryAccess::new).await;
}

#[tokio::test]
async fn memory_blobs_conform() {
    conformance::blobs::run_all(MemoryBlobs::new).await;
}

#[tokio::test]
async fn memory_read_model_conforms() {
    conformance::read_model::run_all(MemoryReadModel::new).await;
}

#[tokio::test]
async fn memory_secrets_conform() {
    conformance::secrets::run_all(MemorySecrets::new).await;
}

/// The receiving side of the memory sender: what it recorded, by subscription.
struct MemoryReceiver(MemoryWebhooks);

impl conformance::webhooks::Receiver for MemoryReceiver {
    fn subscription(&self, tenant: &TenantId, id: &str, accepting: bool) -> Subscription {
        let url = format!("memory://{id}");
        if !accepting {
            self.0.refuse(url.clone());
        }
        Subscription {
            tenant: tenant.clone(),
            id: id.to_owned(),
            url,
            secret: Secret::new("whsec_test"),
            events: BTreeSet::new(),
            labels: BTreeMap::new(),
        }
    }

    async fn received(&self, id: &str) -> Vec<(String, WebhookEventType)> {
        self.0
            .deliveries()
            .into_iter()
            .filter(|(subscription, _)| subscription.id == id)
            .map(|(_, event)| (event.id, event.kind.event_type()))
            .collect()
    }
}

#[tokio::test]
async fn memory_webhooks_conform() {
    conformance::webhooks::run_all(|| {
        let sender = MemoryWebhooks::new();
        (sender.clone(), MemoryReceiver(sender))
    })
    .await;
}

#[tokio::test]
async fn memory_span_exporter_conforms() {
    conformance::spans::run_all(MemorySpanExporter::new).await;
}
