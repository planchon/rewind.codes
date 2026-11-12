//! Exports of a run's journal (`GET /v1/runs/{id}/journal`): the journal document, which
//! replays on its own (`ptc replay` of an exported file), and the cassette of its calls and
//! signals (`?as=cassette`, replayed by `ptc test`).

use ptc_core::{Cassette, CassetteError, JournalDocument, Matching};

use crate::engine::{Engine, EngineError};
use crate::payloads;
use crate::ports::{
    BlobStore, Clock, Entropy, JournalStore, RunRef, ScriptExecutor, SecretStore, ToolDispatcher,
    WebhookSender,
};

impl<J, B, X, D, C, E, S, W> Engine<J, B, X, D, C, E, S, W>
where
    J: JournalStore,
    B: BlobStore,
    X: ScriptExecutor,
    D: ToolDispatcher,
    C: Clock,
    E: Entropy,
    S: SecretStore,
    W: WebhookSender,
{
    /// The run's journal document: its id, its script, its toolset, and its history with every
    /// payload loaded. It replays on its own, without the store.
    pub async fn export_journal(&self, run: &RunRef) -> Result<JournalDocument, EngineError> {
        let record = self.run_record(run).await?;
        let script = self.stored_script(&run.tenant, record.script).await?.script;
        let toolset = self.toolset(&run.tenant, record.toolset).await?.toolset;
        let mut entries = self.journal.load_history(run, 0).await?;
        payloads::load_all(&self.payloads(), &run.tenant, &mut entries).await?;
        Ok(JournalDocument {
            run: run.id,
            script,
            toolset,
            entries,
        })
    }

    /// The cassette of the run: its calls with their args and outcomes, and its signals.
    pub async fn export_cassette(
        &self,
        run: &RunRef,
        matching: Matching,
    ) -> Result<Cassette, EngineError> {
        let history = self.export_history(run).await?;
        Cassette::from_history(&history, matching).map_err(|err| match err {
            CassetteError::History(error) => EngineError::CorruptHistory {
                run_id: run.id,
                error,
            },
            // Every payload was loaded above.
            CassetteError::PayloadNotLoaded { digest, .. } => {
                EngineError::Blob(crate::ports::BlobError::Missing(digest))
            }
        })
    }
}
