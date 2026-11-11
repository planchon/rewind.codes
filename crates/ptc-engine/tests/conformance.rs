//! `MemoryJournal` passes the store conformance suite.

use ptc_engine::adapters::MemoryJournal;

#[tokio::test]
async fn memory_journal_conforms() {
    ptc_engine::conformance::run_all(MemoryJournal::new).await;
}
