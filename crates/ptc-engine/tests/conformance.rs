//! The in-memory adapters pass the conformance suites of their ports: they are the reference
//! implementations of the contracts.

use ptc_engine::adapters::{
    MemoryBlobs, MemoryJournal, MemoryReadModel, MemorySecrets, MemorySpanExporter,
};
use ptc_engine::conformance;

#[tokio::test]
async fn memory_journal_conforms() {
    conformance::journal::run_all(MemoryJournal::new).await;
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

#[tokio::test]
async fn memory_span_exporter_conforms() {
    conformance::spans::run_all(MemorySpanExporter::new).await;
}
