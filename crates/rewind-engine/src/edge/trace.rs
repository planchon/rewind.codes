//! The LLM trace of a run at the edge (`archi/05-testing-and-observability.md`, "LLM trace and
//! metrics"): `GET /v1/runs/{id}/trace?format=llm`, `rewind trace <run> [--entry n]`, and the
//! trace of a test's report.
//!
//! The run's head and the history of its attempt come from its owner, from the partition while
//! the attempt is live, from its sealed journal once it is sealed; every payload up to 64 KiB is
//! a value there. A payload above is a blob of the run: the trace shows its size and preview,
//! and an entry shown in full loads its value.

use std::collections::BTreeMap;

use rewind_core::{Digest, HistoryEntry, Payload};

use super::{Edge, EdgeError, Gateway, unavailable};
use crate::executors::activation::Carried;
use crate::plane::Plane;
use crate::ports::link::Refusal;
use crate::ports::{BlobOwner, BlobStore, PayloadRecord, RunRef};
use crate::{payloads, trace};

impl<P: Plane> Edge<P> {
    /// The run's LLM trace.
    pub async fn trace<G: Gateway>(
        &self,
        gateway: &mut G,
        run: &RunRef,
    ) -> Result<String, EdgeError> {
        let view = self
            .get_run(gateway, run)
            .await?
            .ok_or(Refusal::RunNotFound(run.id))?;
        let history = self.traced_history(gateway, run).await?;
        let rows = self.blob_rows(run, &history).await?;
        Ok(trace::render_trace(trace::TraceInput {
            head: &view.head,
            history: &history,
            payloads: &rows,
            now: self.now(),
        }))
    }

    /// One entry of the run's history in full, its payload loaded (`rewind trace <run> --entry
    /// n`). `None` past the end of the history.
    pub async fn trace_entry<G: Gateway>(
        &self,
        gateway: &mut G,
        run: &RunRef,
        index: u64,
    ) -> Result<Option<String>, EdgeError> {
        let history = self.traced_history(gateway, run).await?;
        let Some(entry) = history.iter().find(|entry| entry.index == index) else {
            return Ok(None);
        };
        let value = match payloads::payload_of(&entry.event) {
            Some(payload) => Some(
                payloads::load(&self.run_blobs(run), &run.tenant, payload)
                    .await
                    .map_err(unavailable)?,
            ),
            None => None,
        };
        let started_at = history
            .first()
            .map_or(entry.recorded_at, |first| first.recorded_at);
        Ok(Some(trace::render_entry(entry, value.as_ref(), started_at)))
    }

    /// The history of the run's attempt, the payloads up to 64 KiB as values.
    async fn traced_history<G: Gateway>(
        &self,
        gateway: &mut G,
        run: &RunRef,
    ) -> Result<Vec<HistoryEntry>, EdgeError> {
        Ok(self
            .history(gateway, run, 0, true)
            .await?
            .ok_or(Refusal::RunNotFound(run.id))?)
    }

    /// Where the values of a run's payloads above 64 KiB are: its blobs.
    pub(crate) fn run_blobs(&self, run: &RunRef) -> Carried<'_, P::Blobs> {
        Carried {
            records: BTreeMap::new(),
            blobs: self.plane.blobs(),
            owners: vec![BlobOwner::Run(run.clone())],
        }
    }

    /// The size and preview of the payloads the history holds by digest, read from the run's
    /// blobs.
    async fn blob_rows(
        &self,
        run: &RunRef,
        history: &[HistoryEntry],
    ) -> Result<BTreeMap<Digest, PayloadRecord>, EdgeError> {
        let owner = BlobOwner::Run(run.clone());
        let mut rows = BTreeMap::new();
        for entry in history {
            let Some(Payload::Digest(digest)) = payloads::payload_of(&entry.event) else {
                continue;
            };
            if rows.contains_key(digest) {
                continue;
            }
            let Some(bytes) = self
                .plane
                .blobs()
                .get(&owner, *digest)
                .await
                .map_err(unavailable)?
            else {
                continue;
            };
            let preview = serde_json::from_slice(&bytes)
                .map(|value| Payload::Value(value).preview(payloads::PREVIEW_CHARS))
                .unwrap_or_default();
            rows.insert(
                *digest,
                PayloadRecord {
                    digest: *digest,
                    size: bytes.len() as u64,
                    preview,
                    value: None,
                },
            );
        }
        Ok(rows)
    }
}
