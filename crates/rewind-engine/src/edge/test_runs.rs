//! Tests at the edge (`archi/05-testing-and-observability.md`, "Running tests", "Tests recorded
//! from runs"): a run of tests on this process's executor, and tests recorded from the runs of a
//! workflow.
//!
//! ```text
//! run_tests ───── each test on a driver of its own, on this node's executor ──► reports
//! record_tests ── search (the workflow's completed and failed runs, newest first) or runs named
//!                 ─► get_run + history (every payload loaded) ─► Recorder ─► store_tests
//! ```

use std::collections::BTreeSet;
use std::sync::Arc;

use rewind_core::{ActionSet, Digest, Epoch, RunId, RunStatus, Script, Test, TestOrigin};

use super::{Edge, EdgeError, Gateway, unavailable};
use crate::plane::{InstanceOf, Plane};
use crate::ports::{
    Clock, CompiledScript, Emitted, ExecutorError, InvalidScript, Page, Predicate, RunOrder,
    RunQuery, RunRef, Scope, ScriptExecutor, StartScript, TenantId, TextMatch,
};
use crate::suites::{
    MAX_RECORDED, MAX_RECORDING_CANDIDATES, RecordedTest, Recorder, SkippedRun, SuiteLoadError,
    TestFormat,
};
use crate::testing::{MockLoader, Subject, TestError, TestOptions, TestReport};
use crate::{payloads, testing};

/// Where a recording takes its runs from.
#[derive(Debug, Clone, PartialEq)]
pub enum RecordFrom {
    /// The runs of the scope the query selects, newest first, the [`MAX_RECORDING_CANDIDATES`]
    /// newest at most: the query is narrowed to the workflow's completed and failed runs. The
    /// caller's visibility is part of the query (its namespace, its labels).
    Search { scope: Scope, query: RunQuery },
    /// These runs, in this order, each a run of the workflow, which the caller sees.
    Runs(Vec<RunRef>),
}

/// What a recording made: the tests it stored, with their digests in the same order, and the
/// runs it left out.
#[derive(Debug, Clone, PartialEq)]
pub struct Recording {
    pub recorded: Vec<RecordedTest>,
    pub digests: Vec<Digest>,
    pub skipped: Vec<SkippedRun>,
}

/// Why a recording failed.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum RecordingError {
    /// A run named is not found.
    #[error("run {0} not found")]
    RunNotFound(RunId),
    /// A run named is not a run of the workflow.
    #[error("run {run} is not a run of the workflow {workflow}")]
    NotOfWorkflow { run: RunId, workflow: String },
    #[error(transparent)]
    Edge(#[from] EdgeError),
    /// The tests could not be stored.
    #[error(transparent)]
    Suite(#[from] SuiteLoadError),
}

impl<P: Plane> Edge<P> {
    /// Runs tests against a subject on this node's executor: [`testing::run_tests`], each test
    /// on a run plane of its own.
    pub async fn run_tests<L: MockLoader>(
        &self,
        loader: &L,
        subject: &Subject<'_>,
        tests: &[Test],
        options: &TestOptions,
    ) -> Result<Vec<TestReport>, TestError> {
        let executor = PlaneExecutor(Arc::clone(&self.plane));
        testing::run_tests(&executor, loader, subject, tests, options).await
    }

    /// Records tests from the completed and failed runs of `workflow`, newest first, one per
    /// path, up to `limit` (at most [`MAX_RECORDED`]), and stores them
    /// (`POST /v1/workflows/{name}/record`). A run that cannot be a test is skipped, and named.
    pub async fn record_tests<G: Gateway>(
        &self,
        gateway: &mut G,
        workflow: &str,
        from: RecordFrom,
        limit: usize,
        format: TestFormat,
    ) -> Result<Recording, RecordingError> {
        let (tenant, candidates) = match from {
            RecordFrom::Search { scope, query } => (
                scope.tenant().clone(),
                self.recordable(gateway, workflow, &scope, query).await?,
            ),
            RecordFrom::Runs(runs) => match runs.first() {
                Some(first) => (first.tenant.clone(), runs),
                None => (TenantId::local(), runs),
            },
        };
        let mut recorder = Recorder::new(limit.min(MAX_RECORDED), format);
        for run in &candidates {
            if recorder.is_full() {
                break;
            }
            let view = self
                .get_run(gateway, run)
                .await?
                .ok_or(RecordingError::RunNotFound(run.id))?;
            let head = view.head;
            let Some(of) = head
                .metadata
                .workflow
                .as_ref()
                .filter(|of| of.name == workflow)
            else {
                return Err(RecordingError::NotOfWorkflow {
                    run: run.id,
                    workflow: workflow.to_owned(),
                });
            };
            let version = of.version;
            let mut history = self
                .history(gateway, run, 0, true)
                .await?
                .ok_or(RecordingError::RunNotFound(run.id))?;
            payloads::load_all(&self.run_blobs(run), &run.tenant, &mut history)
                .await
                .map_err(|error| RecordingError::Edge(unavailable(error)))?;
            recorder.offer(
                &history,
                TestOrigin {
                    run: run.id,
                    version,
                    namespace: head.namespace.to_string(),
                },
            );
        }
        let tests: Vec<Test> = recorder
            .recorded
            .iter()
            .map(|recorded| recorded.test.clone())
            .collect();
        let digests = if tests.is_empty() {
            Vec::new()
        } else {
            self.suites(format)
                .store_tests(&tenant, &tests, self.plane.clock().now())
                .await?
        };
        Ok(Recording {
            recorded: recorder.recorded,
            digests,
            skipped: recorder.skipped,
        })
    }

    /// The runs a recording looks at: the workflow's completed and failed runs the query
    /// selects, newest first, the [`MAX_RECORDING_CANDIDATES`] newest at most.
    async fn recordable<G: Gateway>(
        &self,
        gateway: &mut G,
        workflow: &str,
        scope: &Scope,
        query: RunQuery,
    ) -> Result<Vec<RunRef>, EdgeError> {
        let query = query
            .with(Predicate::Workflow(vec![TextMatch::Exact(
                workflow.to_owned(),
            )]))
            .with(Predicate::Status(BTreeSet::from([
                RunStatus::Completed,
                RunStatus::Failed,
            ])));
        let mut runs = Vec::new();
        let mut page = Page::first(MAX_RECORDING_CANDIDATES);
        loop {
            let found = self
                .search(gateway, scope, &query, RunOrder::Started, &page)
                .await?;
            runs.extend(found.summaries.into_iter().map(|summary| summary.run));
            match found.cursor {
                Some(cursor) if runs.len() < MAX_RECORDING_CANDIDATES => {
                    page = Page::after(cursor, MAX_RECORDING_CANDIDATES - runs.len());
                }
                _ => break,
            }
        }
        runs.truncate(MAX_RECORDING_CANDIDATES);
        Ok(runs)
    }
}

/// The executor of a plane, shared: what a test's own plane runs its script with.
struct PlaneExecutor<P>(Arc<P>);

impl<P> Clone for PlaneExecutor<P> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<P: Plane> ScriptExecutor for PlaneExecutor<P> {
    type Instance = InstanceOf<P>;

    fn epochs(&self) -> &[Epoch] {
        self.0.executor().epochs()
    }

    fn compile(
        &self,
        script: &Script,
        action_set: &ActionSet,
    ) -> Result<CompiledScript, InvalidScript> {
        self.0.executor().compile(script, action_set)
    }

    fn action_imports(&self, script: &Script) -> Option<BTreeSet<String>> {
        self.0.executor().action_imports(script)
    }

    async fn start(
        &self,
        start: StartScript,
    ) -> Result<(Self::Instance, Vec<Emitted>), ExecutorError> {
        self.0.executor().start(start).await
    }
}
