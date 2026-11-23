//! Runs named by their handle, `<namespace>/<origin>/<suffix>` (`archi/06-storage-and-scale.md`,
//! "Run handles"): the API takes a handle wherever it takes a run's id.

use rewind_core::RunHandle;

use crate::ports::{JournalStore, RunRecord, StoreError, TenantId};

/// The most runs one suffix of [`rewind_core::SUFFIX_LEN`] characters can name in a tenant:
/// the store gives a longer suffix to a run that shares its first characters with another, so
/// they are rare, and this bound is never reached.
const RUNS_PER_SUFFIX: usize = 64;

/// The tenant's run `handle` names: the one whose id its suffix names, whose suffix has the
/// handle's length, and of its namespace and origin. `None` when no run of the tenant has this
/// handle.
pub async fn run_by_handle<J: JournalStore>(
    journal: &J,
    tenant: &TenantId,
    handle: &RunHandle,
) -> Result<Option<RunRecord>, StoreError> {
    let Some((low, high)) = handle.range() else {
        return Ok(None);
    };
    let len = handle.suffix.len();
    Ok(journal
        .runs_in_range(tenant, low, high, RUNS_PER_SUFFIX)
        .await?
        .into_iter()
        .find(|record| {
            usize::from(record.suffix_len) == len
                && record.namespace.as_str() == handle.namespace
                && record.origin == handle.origin
        }))
}

/// The run `id` names in the tenant: a UUID, or a handle.
pub async fn find_run<J: JournalStore>(
    journal: &J,
    tenant: &TenantId,
    id: &str,
) -> Result<Option<RunRecord>, StoreError> {
    if let Ok(run_id) = id.parse() {
        return journal
            .get_run(&crate::ports::RunRef::new(tenant.clone(), run_id))
            .await;
    }
    match id.parse::<RunHandle>() {
        Ok(handle) => run_by_handle(journal, tenant, &handle).await,
        Err(_) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use rewind_core::{
        Digest, Epoch, Payload, RunId, RunMode, RunStarted, SUFFIX_LEN, Seed, Timestamp,
    };
    use serde_json::json;

    use super::*;
    use crate::adapters::MemoryJournal;
    use crate::ports::{NamespaceName, NewRun, RunMetadata, RunRef};

    async fn create(journal: &MemoryJournal, id: RunId, namespace: &str, origin: &str) {
        journal
            .create_run(
                NewRun {
                    run: RunRef::new(TenantId::local(), id),
                    namespace: NamespaceName::new(namespace).expect("a namespace"),
                    origin: origin.into(),
                    started: RunStarted {
                        script: Digest::of(b"script"),
                        toolset: Digest::of(b"toolset"),
                        input: Payload::Value(json!(null)),
                        seed: Seed::from_bytes([1; 32]),
                        epoch: Epoch(1),
                        expires_at: Timestamp(86_400_000),
                        mode: RunMode::Mock,
                    },
                    metadata: RunMetadata::default(),
                    created_at: Timestamp(0),
                },
                None,
            )
            .await
            .expect("create_run");
    }

    #[tokio::test]
    async fn a_handle_finds_its_run_and_no_other() {
        let journal = MemoryJournal::new();
        let tenant = TenantId::local();
        let first = RunId::from_v7_parts(1_795_383_896_000, 0x0c5, 1);
        let twin = RunId::from_v7_parts(1_795_383_896_000, 0x0c5, 2);
        create(&journal, first, "dana", "triage").await;
        create(&journal, twin, "dana", "triage").await;
        let first_record = find_run(&journal, &tenant, &first.to_string())
            .await
            .expect("find_run")
            .expect("by its UUID");
        let twin_record = find_run(&journal, &tenant, &twin.to_string())
            .await
            .expect("find_run")
            .expect("by its UUID");
        assert_eq!(usize::from(first_record.suffix_len), SUFFIX_LEN);
        assert!(usize::from(twin_record.suffix_len) > SUFFIX_LEN);
        for record in [&first_record, &twin_record] {
            let found = find_run(&journal, &tenant, &record.handle().to_string())
                .await
                .expect("find_run")
                .expect("by its handle");
            assert_eq!(found.run.id, record.run.id);
        }
        for wrong in [
            format!("bob/triage/{}", first_record.handle().suffix),
            format!("dana/digest/{}", first_record.handle().suffix),
            "dana/triage/zzzzzzzzzzzz".to_owned(),
            "not a run".to_owned(),
        ] {
            assert_eq!(
                find_run(&journal, &tenant, &wrong).await.expect("find_run"),
                None,
                "{wrong}"
            );
        }
    }
}
