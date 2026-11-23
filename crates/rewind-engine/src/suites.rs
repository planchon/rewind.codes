//! Suites: the tests a workflow version carries, inherited from its parent, and tests recorded
//! from runs (`archi/05-testing-and-observability.md`, "Suites and the gate", "Tests recorded
//! from runs", "Where tests are stored").
//!
//! ```text
//! parent's suite ── load (blobs) ── erased? → retired ─┐
//!                                  retire (reason) ────┤
//!                                  update (reason) ────┼─► suite ─► run_tests ─► all pass?
//!                                  tests (inline,      │                          │
//!                                    digest, from) ────┘                 reserve rows, put blobs,
//!                                                                         put_workflow
//! runs, newest first ─► Recorder (one per RunPath, Test::record) ─► store_tests ─► digests
//! ```
//!
//! Test documents are blobs of the `BlobStore` under the digest of their canonical JSON; the
//! store indexes them. The engine does not know the stored format: the composition root gives
//! it as a [`TestFormat`] (`rewind_format::encode_test`, `decode_test`).

use std::collections::{BTreeMap, BTreeSet};

use rewind_core::{
    Digest, ERASED_TEST_REASON, Expectations, HistoryEntry, RecordError, RetiredTest, RunId,
    RunPath, Suite, SuiteTest, Test, TestOrigin, Timestamp, UpdatedTest, Workflow,
};

use crate::ports::{
    BlobError, BlobOwner, BlobStore, CatalogStore, NamespaceName, StoreError, TenantId, TestRecord,
};
use crate::testing::TestOptions;

/// A test document is at most this many bytes: recording skips a run whose test would be larger.
pub const MAX_TEST_BYTES: usize = 4 * 1024 * 1024;

/// Tests recorded at once, by default and at most.
pub const DEFAULT_RECORDED: usize = 20;
pub const MAX_RECORDED: usize = 100;

/// Runs a recording looks at, at most.
pub const MAX_RECORDING_CANDIDATES: usize = 1_000;

/// The stored form of a test, which the composition root gives: the engine does not know the
/// stored format.
#[derive(Debug, Clone, Copy)]
pub struct TestFormat {
    /// The canonical document: its bytes hash to the test's digest.
    pub encode: fn(&Test) -> String,
    pub decode: fn(&str) -> Result<Test, String>,
}

impl TestFormat {
    /// The test's canonical document and its digest.
    pub fn document(&self, test: &Test) -> (Digest, String) {
        let document = (self.encode)(test);
        (Digest::of(document.as_bytes()), document)
    }
}

/// What running a suite needs besides the engine: the mock handlers' loader, the stored format
/// of tests, and the options of the runs.
#[derive(Debug)]
pub struct TestKit<'a, L> {
    pub loader: &'a L,
    pub format: TestFormat,
    pub options: TestOptions,
}

/// A test given to a save, or to a run of tests.
#[derive(Debug, Clone, PartialEq)]
pub enum TestInput {
    Inline(Box<Test>),
    /// A stored test, by digest: one recorded from a run.
    Stored(Digest),
    /// A stored test with a new expectation.
    From {
        from: Digest,
        expect: Expectations,
    },
}

/// An inherited test replaced, and why.
#[derive(Debug, Clone, PartialEq)]
pub struct TestUpdate {
    pub name: String,
    pub reason: String,
    pub with: UpdateWith,
}

#[derive(Debug, Clone, PartialEq)]
pub enum UpdateWith {
    /// A whole new test, which takes the inherited test's name.
    Test(TestInput),
    /// The inherited test with a new expectation.
    Expect(Expectations),
}

/// Inherited tests dropped, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestRetirement {
    pub names: Vec<String>,
    pub reason: String,
}

/// What a save says about its suite.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SuiteChanges {
    /// The version it derives from: the latest version of the same name saved under the
    /// caller's labels when `None`.
    pub parent: Option<Digest>,
    pub tests: Vec<TestInput>,
    pub update: Vec<TestUpdate>,
    pub retire: Vec<TestRetirement>,
}

impl SuiteChanges {
    pub fn is_empty(&self) -> bool {
        self.parent.is_none()
            && self.tests.is_empty()
            && self.update.is_empty()
            && self.retire.is_empty()
    }
}

/// A suite that cannot be composed. The messages are written for a model.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SuiteError {
    #[error("the parent version {0} does not exist, or the caller does not see it")]
    ParentNotFound(Digest),
    #[error("{verb} {name:?}, which the parent's suite does not have{known}")]
    NotInherited {
        verb: &'static str,
        name: String,
        known: String,
    },
    #[error("the test {0:?} is retired or updated without a reason: say why")]
    NoReason(String),
    #[error("the test {0:?} is both retired and updated: do one")]
    RetiredAndUpdated(String),
    #[error(
        "the suite already has a test named {0:?}: give the new test another name, or update \
         the inherited one with a reason"
    )]
    Duplicate(String),
    #[error("no stored test has the digest {0}: record it again, or give the test inline")]
    UnknownTest(Digest),
    #[error("{0}")]
    Invalid(String),
    #[error("the test {name:?} is {size} bytes as a document, above the limit of {max} bytes")]
    TooLarge {
        name: String,
        size: usize,
        max: usize,
    },
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SuiteLoadError {
    #[error(transparent)]
    Suite(#[from] SuiteError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Blob(#[from] BlobError),
    /// A stored document the format cannot read: the store is corrupt.
    #[error("the stored test {digest} cannot be read: {message}")]
    Corrupt { digest: Digest, message: String },
}

/// A test of a version's suite as loaded: `None` once its namespace was erased.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadedTest {
    pub name: String,
    pub digest: Digest,
    pub test: Option<Test>,
}

/// A suite composed for a new version: the tests to run, and the suite to write.
#[derive(Debug, Clone, PartialEq)]
pub struct ComposedSuite {
    /// Every test of the suite, by name.
    pub tests: Vec<Test>,
    pub suite: Suite,
}

// -------------------------------------------------------------------------------------------
// Suites in the control plane

/// The tests of a tenant where the control plane keeps them: their rows in the catalog, their
/// documents as blobs of the tenant's tests ([`BlobOwner::Tests`]), in the stored format the
/// composition root gives.
#[derive(Debug, Clone, Copy)]
pub struct Suites<'a, C, B> {
    pub catalog: &'a C,
    pub blobs: &'a B,
    pub format: TestFormat,
}

impl<C: CatalogStore, B: BlobStore> Suites<'_, C, B> {
    /// A stored test, by digest; `None` for one never stored, erased, or being deleted.
    pub async fn load_test(
        &self,
        tenant: &TenantId,
        digest: Digest,
    ) -> Result<Option<Test>, SuiteLoadError> {
        if self
            .catalog
            .get_test(tenant, digest)
            .await
            .map_err(StoreError::from)?
            .is_none()
        {
            return Ok(None);
        }
        let owner = BlobOwner::Tests(tenant.clone());
        let Some(bytes) = self.blobs.get(&owner, digest).await? else {
            return Ok(None);
        };
        let text = String::from_utf8(bytes).map_err(|error| SuiteLoadError::Corrupt {
            digest,
            message: error.to_string(),
        })?;
        (self.format.decode)(&text)
            .map(Some)
            .map_err(|message| SuiteLoadError::Corrupt { digest, message })
    }

    /// The tests of a version's suite, in its order; an erased test is `None`.
    pub async fn load_suite(
        &self,
        tenant: &TenantId,
        workflow: &Workflow,
    ) -> Result<Vec<LoadedTest>, SuiteLoadError> {
        let mut loaded = Vec::with_capacity(workflow.suite().tests.len());
        for entry in &workflow.suite().tests {
            loaded.push(LoadedTest {
                name: entry.name.clone(),
                digest: entry.test,
                test: self.load_test(tenant, entry.test).await?,
            });
        }
        Ok(loaded)
    }

    /// The test a [`TestInput`] gives.
    pub async fn resolve_test(
        &self,
        tenant: &TenantId,
        input: &TestInput,
    ) -> Result<Test, SuiteLoadError> {
        let test = match input {
            TestInput::Inline(test) => (**test).clone(),
            TestInput::Stored(digest) => self
                .load_test(tenant, *digest)
                .await?
                .ok_or(SuiteError::UnknownTest(*digest))?,
            TestInput::From { from, expect } => {
                let mut test = self
                    .load_test(tenant, *from)
                    .await?
                    .ok_or(SuiteError::UnknownTest(*from))?;
                test.expect = expect.clone();
                test
            }
        };
        test.check()
            .map_err(|error| SuiteError::Invalid(error.to_string()))?;
        Ok(test)
    }

    /// Composes the suite of a new version: the parent's tests, those of an erased namespace
    /// left out, minus `retire`, with `update` applied, plus `tests`.
    pub async fn compose_suite(
        &self,
        tenant: &TenantId,
        parent: Option<&Workflow>,
        changes: &SuiteChanges,
    ) -> Result<ComposedSuite, SuiteLoadError> {
        let mut retired = Vec::new();
        let mut updated = Vec::new();
        // name → (digest, test)
        let mut suite: BTreeMap<String, (Digest, Test)> = BTreeMap::new();
        if let Some(parent) = parent {
            for loaded in self.load_suite(tenant, parent).await? {
                match loaded.test {
                    Some(test) => {
                        suite.insert(loaded.name, (loaded.digest, test));
                    }
                    None => retired.push(RetiredTest {
                        name: loaded.name,
                        test: loaded.digest,
                        reason: ERASED_TEST_REASON.to_owned(),
                    }),
                }
            }
        }
        let inherited: BTreeSet<String> = suite.keys().cloned().collect();
        let known = || {
            if inherited.is_empty() {
                " (it has no test)".to_owned()
            } else {
                format!(
                    " (it has {})",
                    inherited
                        .iter()
                        .map(|name| format!("{name:?}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
        };
        let mut retiring = BTreeSet::new();
        for retirement in &changes.retire {
            if retirement.reason.trim().is_empty() {
                let name = retirement.names.first().cloned().unwrap_or_default();
                return Err(SuiteError::NoReason(name).into());
            }
            for name in &retirement.names {
                let Some((digest, _)) = suite.remove(name) else {
                    return Err(SuiteError::NotInherited {
                        verb: "retire names",
                        name: name.clone(),
                        known: known(),
                    }
                    .into());
                };
                retiring.insert(name.clone());
                retired.push(RetiredTest {
                    name: name.clone(),
                    test: digest,
                    reason: retirement.reason.clone(),
                });
            }
        }
        for update in &changes.update {
            if update.reason.trim().is_empty() {
                return Err(SuiteError::NoReason(update.name.clone()).into());
            }
            if retiring.contains(&update.name) {
                return Err(SuiteError::RetiredAndUpdated(update.name.clone()).into());
            }
            let Some((from, inherited_test)) = suite.remove(&update.name) else {
                return Err(SuiteError::NotInherited {
                    verb: "update names",
                    name: update.name.clone(),
                    known: known(),
                }
                .into());
            };
            let mut replacement = match &update.with {
                UpdateWith::Expect(expect) => Test {
                    expect: expect.clone(),
                    ..inherited_test
                },
                UpdateWith::Test(input) => self.resolve_test(tenant, input).await?,
            };
            replacement.name = update.name.clone();
            let (to, _) = self.format.document(&replacement);
            updated.push(UpdatedTest {
                name: update.name.clone(),
                from,
                to,
                reason: update.reason.clone(),
            });
            suite.insert(update.name.clone(), (to, replacement));
        }
        for input in &changes.tests {
            let test = self.resolve_test(tenant, input).await?;
            if suite.contains_key(&test.name) {
                return Err(SuiteError::Duplicate(test.name).into());
            }
            let (digest, _) = self.format.document(&test);
            suite.insert(test.name.clone(), (digest, test));
        }
        let mut tests = Vec::with_capacity(suite.len());
        let mut entries = Vec::with_capacity(suite.len());
        for (name, (digest, test)) in suite {
            let (_, document) = self.format.document(&test);
            if document.len() > MAX_TEST_BYTES {
                return Err(SuiteError::TooLarge {
                    name,
                    size: document.len(),
                    max: MAX_TEST_BYTES,
                }
                .into());
            }
            entries.push(SuiteTest { name, test: digest });
            tests.push(test);
        }
        Ok(ComposedSuite {
            tests,
            suite: Suite {
                parent: parent.map(Workflow::version),
                tests: entries,
                retired,
                updated,
            },
        })
    }

    /// Stores tests: reserves their rows, then writes their blobs. A test whose deletion has
    /// started cannot be stored until its row is gone (`Conflict`).
    pub async fn store_tests(
        &self,
        tenant: &TenantId,
        tests: &[Test],
        now: Timestamp,
    ) -> Result<Vec<Digest>, SuiteLoadError> {
        let documents = documents(tests, self.format);
        let records = records(&documents);
        let deleting = self
            .catalog
            .reserve_tests(tenant, &records, now)
            .await
            .map_err(StoreError::from)?;
        if let Some(digest) = deleting.first() {
            return Err(StoreError::Conflict(format!(
                "the test {digest} is being deleted: save again in a few minutes"
            ))
            .into());
        }
        let owner = BlobOwner::Tests(tenant.clone());
        for (digest, document, _) in &documents {
            self.blobs
                .put(&owner, *digest, document.clone().into_bytes())
                .await?;
        }
        Ok(documents.into_iter().map(|(digest, _, _)| digest).collect())
    }
}

/// How many tests [`clean_tests`] takes at once.
pub const CLEANED_AT_ONCE: usize = 500;

/// The control plane's daily job of tests (`archi/06-storage-and-scale.md`, "Retention and
/// deletion"; `archi/05-testing-and-observability.md`, "Where tests are stored"): deletes the
/// tests no version references, stored more than [`TEST_GRACE`](crate::ports::TEST_GRACE)
/// before `now`. It marks their rows, deletes their blobs, then drops the rows still marked, a
/// batch at a time until none is left. Returns how many rows it dropped.
///
/// A round that stops halfway leaves rows whose deletion started: a later round marks them again
/// after [`TEST_DELETION_TIMEOUT`](crate::ports::TEST_DELETION_TIMEOUT). The tests of an erased
/// namespace are the owner of its partition's to erase, with the namespace.
pub async fn clean_tests<C: CatalogStore, B: BlobStore>(
    catalog: &C,
    blobs: &B,
    now: Timestamp,
) -> Result<usize, SuiteLoadError> {
    let mut dropped = 0;
    loop {
        let marked = catalog
            .unreferenced_tests(now, CLEANED_AT_ONCE)
            .await
            .map_err(StoreError::from)?;
        if marked.is_empty() {
            return Ok(dropped);
        }
        for (tenant, digest) in &marked {
            blobs
                .delete(&BlobOwner::Tests(tenant.clone()), *digest)
                .await?;
        }
        dropped += catalog
            .drop_tests(&marked, false)
            .await
            .map_err(StoreError::from)?;
        if marked.len() < CLEANED_AT_ONCE {
            return Ok(dropped);
        }
    }
}

/// The documents of tests: their digest, their canonical document, and the namespace a recorded
/// test came from.
fn documents(tests: &[Test], format: TestFormat) -> Vec<(Digest, String, Option<NamespaceName>)> {
    tests
        .iter()
        .map(|test| {
            let (digest, document) = format.document(test);
            let namespace = test
                .origin
                .as_ref()
                .and_then(|origin| NamespaceName::new(&origin.namespace).ok());
            (digest, document, namespace)
        })
        .collect()
}

fn records(documents: &[(Digest, String, Option<NamespaceName>)]) -> Vec<TestRecord> {
    documents
        .iter()
        .map(|(digest, document, namespace)| TestRecord {
            digest: *digest,
            namespace: namespace.clone(),
            size: document.len() as u64,
        })
        .collect()
}

// -------------------------------------------------------------------------------------------
// Recording

/// A test recorded from a run, and the path it stands for.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordedTest {
    pub run: RunId,
    pub path: RunPath,
    pub test: Test,
}

/// A run recording left out, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedRun {
    pub run: RunId,
    pub reason: String,
}

/// Turns runs, offered newest first, into tests: the first run of each path, up to `limit`
/// (`archi/05-testing-and-observability.md`, "Tests recorded from runs").
#[derive(Debug)]
pub struct Recorder {
    limit: usize,
    format: TestFormat,
    paths: BTreeSet<Digest>,
    pub recorded: Vec<RecordedTest>,
    pub skipped: Vec<SkippedRun>,
}

impl Recorder {
    pub fn new(limit: usize, format: TestFormat) -> Self {
        Self {
            limit: limit.clamp(1, MAX_RECORDED),
            format,
            paths: BTreeSet::new(),
            recorded: Vec::new(),
            skipped: Vec::new(),
        }
    }

    /// Whether it has as many tests as it may record.
    pub fn is_full(&self) -> bool {
        self.recorded.len() >= self.limit
    }

    /// Offers a run, its history with every payload loaded. A run whose path a newer run
    /// already stands for adds nothing.
    pub fn offer(&mut self, history: &[HistoryEntry], origin: TestOrigin) {
        if self.is_full() {
            return;
        }
        let run = origin.run;
        let path = match RunPath::of(history) {
            Ok(path) => path,
            Err(error) => {
                self.skipped.push(SkippedRun {
                    run,
                    reason: format!("its history is invalid: {error}"),
                });
                return;
            }
        };
        if self.paths.contains(&path.digest()) {
            return;
        }
        let test = match Test::record(history, origin) {
            Ok(test) => test,
            Err(RecordError::Status(status)) => {
                self.skipped.push(SkippedRun {
                    run,
                    reason: format!(
                        "it is {status}: only a run that completed or failed is recorded"
                    ),
                });
                return;
            }
            Err(error) => {
                self.skipped.push(SkippedRun {
                    run,
                    reason: error.to_string(),
                });
                return;
            }
        };
        let (_, document) = self.format.document(&test);
        if document.len() > MAX_TEST_BYTES {
            self.skipped.push(SkippedRun {
                run,
                reason: format!(
                    "its test would be {} bytes, above the limit of {MAX_TEST_BYTES}",
                    document.len()
                ),
            });
            return;
        }
        self.paths.insert(path.digest());
        self.recorded.push(RecordedTest { run, path, test });
    }
}
