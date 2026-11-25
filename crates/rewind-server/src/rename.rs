//! `rewind server rename-data`: the one-time offline rewrite of what a deployment stored under the
//! names it had before the rename release (`archi/06-storage-and-scale.md`, "Rewriting the stored
//! names"; `archi/07-code-architecture.md`, "The server's commands"; `archi/08-decisions.md`, R2
//! and R6).
//!
//! ```text
//! rewind server rename-data [--dry-run] --store postgres://… --bucket s3://… [the server's options]
//! rewind server rename-data [--dry-run] --embedded [--dir .rewind]
//! ```
//!
//! It opens the control plane, whose schema migration renames what SQL can rename (tables,
//! columns, principals, permissions, subscriptions), and refuses to go on while a node serves the
//! deployment or runs still wait in the tables `migrate-runs` moves. Then, in order:
//!
//! ```text
//! the partitions, read          ─► each run: its code, action set, secrets, labels, version,
//!                                   whether a trigger waits in its inbox
//! the tests, the versions, the  ─► renamed and read; each gets the digest of its new form,
//! action sets of the catalog        every reference to it follows (to a fixed point)
//! rename::workflows             ─► every run's workflow and version, each code's workflow
//! the partitions, rewritten     ─► every record in the new names, active runs running or
//!                                   waiting, the gauges counted again
//! the archive, rewritten        ─► every sealed attempt in the new names
//! ```
//!
//! A code's text is never rewritten, nor the prefixes `rewind_wh_` and `rewind_tk_`, nor the
//! handles given. Run again, it finds every record in the new names and changes nothing, and says
//! so. `--dry-run` reads everything, counts what it would change by kind, and writes nothing.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::process::ExitCode;

use clap::Parser;
use rewind_core::{
    ActionSet, Digest, PartitionId, RunId, SecretRef, ServerSecrets, Shape, Test, Timestamp,
    WorkflowVersion,
};
use rewind_engine::adapters::{SystemClock, SystemEntropy};
use rewind_engine::ports::cluster::Membership;
use rewind_engine::ports::legacy::LegacyRuns;
use rewind_engine::ports::{
    ActionDeclarations, BlobOwner, BlobStore, CatalogDocuments, Clock, Cluster, NamespaceName,
    NodeId, NodeInfo, PartitionLog, PartitionStore, TenantId,
};
use rewind_engine::rename::{CodeUse, DroppedTitle, WorkflowsRenamed};
use rewind_format::rename::{PinnedVersion, Renames};
use rewind_store_slatedb::SlateLog;
use rewind_store_slatedb::rename::{StoredRun, rename_partition, stored_runs};
use serde_json::{Value, json};

use crate::ServerArgs;
use crate::config::MasterKeyRing;
use crate::embedded::Directory;
use crate::migrate::Legacy;
use crate::serve::StartupError;
use crate::wiring::bucket::{self, Archives};
use crate::wiring::control::ControlPlane;

/// `rewind server rename-data`.
#[derive(Debug, Clone, Parser)]
#[command(
    name = "rewind server rename-data",
    about = "Rewrites, once, what the deployment stored under the names it had before the rename \
             release: the partitions and the archive, the catalog's action sets, workflow \
             versions and tests (each with its new digest), every run's workflow. Run it while no \
             node serves, after `migrate-runs`. Run again, it changes nothing.",
    version
)]
pub struct RenameArgs {
    /// Read everything and print what would change, by kind, without writing anything.
    #[arg(long)]
    pub dry_run: bool,

    #[command(flatten)]
    pub server: ServerArgs,
}

/// What `rename-data` changed, or would change with `--dry-run`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RenameReport {
    pub dry_run: bool,
    /// The catalog's content-addressed documents given a new digest, by kind.
    pub documents: BTreeMap<&'static str, u64>,
    /// The partition records rewritten, by kind.
    pub records: BTreeMap<&'static str, u64>,
    /// The sealed attempts of the archive rewritten.
    pub sealed_attempts: u64,
    /// The runs the partitions hold.
    pub runs: u64,
    /// What `rename::workflows` did; `None` with `--dry-run`, which writes no workflow.
    pub workflows: Option<WorkflowsSummary>,
    /// The titles people gave codes that did not become names, and why.
    pub dropped: Vec<DroppedTitle>,
}

/// What the workflows step changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkflowsSummary {
    pub created: usize,
    pub scoped: usize,
    pub kept: usize,
    pub named: usize,
}

impl RenameReport {
    /// Whether anything was, or would be, changed.
    pub fn changed(&self) -> bool {
        self.documents.values().any(|n| *n > 0)
            || self.records.values().any(|n| *n > 0)
            || self.sealed_attempts > 0
            || self
                .workflows
                .as_ref()
                .is_some_and(|workflows| workflows.created + workflows.scoped + workflows.named > 0)
    }
}

/// `rewind server rename-data …`: `args` are the arguments after `rename-data`. Exits 0 once
/// everything is in the new names (or, with `--dry-run`, once it has counted), 70 when a store
/// failed or the deployment is not ready for it, 64 for a usage error, 75 when an embedded server
/// holds the directory.
pub(crate) fn main(args: Vec<OsString>) -> ExitCode {
    let args = match RenameArgs::try_parse_from(
        std::iter::once(OsString::from("rewind server rename-data")).chain(args),
    ) {
        Ok(args) => args,
        Err(error) => {
            let _ = error.print();
            return ExitCode::from(if error.use_stderr() {
                crate::exit::USAGE
            } else {
                0
            });
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("rewind-rename-data")
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("rewind server rename-data: cannot start the async runtime: {error}");
            return ExitCode::from(crate::exit::FAILED);
        }
    };
    runtime.block_on(async move {
        let _telemetry = match crate::wiring::telemetry::install(&args.server) {
            Ok(guard) => guard,
            Err(error) => {
                eprintln!("rewind server rename-data: telemetry: {error}");
                return ExitCode::from(crate::exit::USAGE);
            }
        };
        match rename_data(args).await {
            Ok(report) => {
                print(&report);
                ExitCode::from(crate::exit::STOPPED)
            }
            Err(error) => {
                eprintln!("rewind server rename-data: {error}");
                ExitCode::from(crate::exit_code(&error))
            }
        }
    })
}

/// The summary: what changed by kind, the workflows, the titles that did not become names.
fn print(report: &RenameReport) {
    let verb = if report.dry_run {
        "would rewrite"
    } else {
        "rewrote"
    };
    if !report.changed() {
        println!(
            "nothing to rename: the {} run(s) and the catalog are in the new names already",
            report.runs
        );
    }
    for (kind, count) in &report.documents {
        if *count > 0 {
            println!("{verb} {count} {kind} of the catalog, each under its new digest");
        }
    }
    for (kind, count) in &report.records {
        if *count > 0 {
            println!("{verb} {count} {kind} of the partitions");
        }
    }
    if report.sealed_attempts > 0 {
        println!(
            "{verb} {} sealed attempt(s) of the archive",
            report.sealed_attempts
        );
    }
    match &report.workflows {
        Some(workflows) => println!(
            "workflows: {} created for the codes run without one, {} moved into the namespace \
             that ran them, {} kept by the organization (the namespace has one of its name), {} \
             named after a title",
            workflows.created, workflows.scoped, workflows.kept, workflows.named
        ),
        None => println!("workflows: not given with --dry-run"),
    }
    for dropped in &report.dropped {
        println!(
            "title {:?} of code {} ({}) is not a name: {}",
            dropped.title, dropped.code, dropped.tenant, dropped.reason
        );
    }
    println!(
        "{} run(s); {}",
        report.runs,
        if report.dry_run {
            "nothing was written (--dry-run)"
        } else {
            "the data is in the new names: start the new build"
        }
    );
}

/// Renames the data of the deployment `args` name, or of the embedded directory; see the module.
pub async fn rename_data(args: RenameArgs) -> Result<RenameReport, StartupError> {
    let RenameArgs { dry_run, server } = args;
    if server.embedded {
        let dir = server.dir.clone();
        let directory = Directory::open(&dir)?;
        if dir.join("journal.db").exists() {
            return Err(StartupError::Setup(format!(
                "{} still holds the runs of the old tables (journal.db): move them with \
                 `rewind server migrate-runs --embedded` of the build before the rename release \
                 first",
                dir.display()
            )));
        }
        let config = server.resolve()?;
        let keys = MasterKeyRing::new(directory.master_key);
        let store = ControlPlane::open(&config.store, &keys).await?;
        let report = renamed(&store, &config, dry_run).await;
        store.close().await;
        return report;
    }
    let config = server.resolve()?;
    let keys = config
        .master_keys
        .clone()
        .ok_or_else(|| StartupError::Config("rename-data needs --master-key".into()))?;
    let store = ControlPlane::open(&config.store, &keys).await?;
    let report = renamed(&store, &config, dry_run).await;
    store.close().await;
    report
}

fn setup(error: impl std::fmt::Display) -> StartupError {
    StartupError::Setup(error.to_string())
}

/// Refuses a deployment a node serves, or whose old tables still hold runs.
async fn ready(store: &ControlPlane, config: &crate::config::Config) -> Result<(), StartupError> {
    let legacy = Legacy::of(store)?;
    match legacy.runs(None, 1).await {
        Ok(runs) if runs.is_empty() => {}
        Ok(_) | Err(_) => {
            return Err(setup(
                "runs still wait in the tables `migrate-runs` moves: move them with \
                 `rewind server migrate-runs` of the build before the rename release first",
            ));
        }
    }
    let cluster = store
        .cluster(config.partitions, &config.cluster_database)
        .await
        .map_err(setup)?;
    let me = NodeInfo {
        id: NodeId::new(format!("rename-data-{}", std::process::id())).map_err(setup)?,
        address: String::new(),
        build: crate::VERSION.to_owned(),
        epochs: BTreeSet::new(),
        pool: None,
        executors_only: true,
    };
    let membership = cluster.join(me.clone()).await.map_err(setup)?;
    let serving: Vec<NodeId> = membership
        .nodes()
        .await
        .map_err(setup)?
        .into_iter()
        .map(|node| node.id)
        .filter(|id| *id != me.id)
        .collect();
    membership.leave().await.map_err(setup)?;
    if !serving.is_empty() {
        let names: Vec<String> = serving.iter().map(ToString::to_string).collect();
        return Err(setup(format!(
            "the node(s) {} serve the deployment: stop them first, rename-data runs offline",
            names.join(", ")
        )));
    }
    Ok(())
}

/// The context of the renaming: the new digests of the catalog's documents, the workflow each
/// pinned version belongs to, each run's workflow, each code's, the runs with a trigger waiting.
#[derive(Debug, Default)]
struct Context {
    digests: BTreeMap<Digest, Digest>,
    pinned: BTreeMap<(String, Digest), PinnedVersion>,
    runs: BTreeMap<RunId, Value>,
    codes: BTreeMap<(String, String, Digest), String>,
    stepping: BTreeSet<RunId>,
}

impl Renames for Context {
    fn digest(&self, old: &Digest) -> Option<Digest> {
        self.digests.get(old).copied()
    }

    fn pinned(&self, tenant: &str, _name: &str, version: &Digest) -> Option<PinnedVersion> {
        self.pinned.get(&(tenant.to_owned(), *version)).cloned()
    }

    fn run_workflow(&self, run: &RunId) -> Option<Value> {
        self.runs.get(run).cloned()
    }

    fn code_workflow(&self, tenant: &str, namespace: &str, code: &Digest) -> Option<String> {
        self.codes
            .get(&(tenant.to_owned(), namespace.to_owned(), *code))
            .cloned()
    }

    fn stepping(&self, run: &RunId) -> bool {
        self.stepping.contains(run)
    }
}

/// The node rename-data opens the partitions as: an open fences the writer before it.
fn node() -> Result<NodeId, StartupError> {
    NodeId::new("rename-data").map_err(setup)
}

/// Renames everything; see the module.
async fn renamed(
    store: &ControlPlane,
    config: &crate::config::Config,
    dry_run: bool,
) -> Result<RenameReport, StartupError> {
    ready(store, config).await?;
    let opened = bucket::open(&config.bucket)?;
    let catalog = store.catalog();
    let mut report = RenameReport {
        dry_run,
        ..RenameReport::default()
    };

    // The runs, as the partitions hold them.
    let node = node()?;
    let mut runs: Vec<StoredRun> = Vec::new();
    let mut logs: Vec<SlateLog> = Vec::new();
    for partition in 0..config.partitions.get() {
        let log = opened
            .partitions
            .open(PartitionId::new(partition), &node)
            .await
            .map_err(setup)?;
        runs.extend(stored_runs(&log).await.map_err(setup)?);
        logs.push(log);
    }
    report.runs = runs.len() as u64;
    let mut context = Context {
        stepping: runs
            .iter()
            .filter(|run| run.stepping)
            .map(|run| run.run)
            .collect(),
        ..Context::default()
    };

    // The catalog's documents, each under the digest of its new form, to a fixed point: a
    // version names its tests and its action set, an action set the versions it pins.
    let tests: Vec<(TenantId, Digest, String)> = {
        let mut tests = Vec::new();
        for (tenant, digest) in catalog.raw_tests().await.map_err(setup)? {
            let owner = BlobOwner::Tests(tenant.clone());
            if let Some(bytes) = opened.blobs.get(&owner, digest).await.map_err(setup)? {
                let json = String::from_utf8(bytes).map_err(setup)?;
                tests.push((tenant, digest, json));
            }
        }
        tests
    };
    let versions = catalog.raw_versions().await.map_err(setup)?;
    let action_sets = catalog.raw_action_sets().await.map_err(setup)?;
    let mut new_tests: BTreeMap<(TenantId, Digest), Test> = BTreeMap::new();
    let mut new_versions: BTreeMap<usize, WorkflowVersion> = BTreeMap::new();
    let mut new_action_sets: BTreeMap<usize, ActionSet> = BTreeMap::new();
    for _ in 0..16 {
        let before = context.digests.clone();
        for (tenant, digest, json) in &tests {
            let test = rewind_format::rename::test(json, &context)
                .map_err(|error| setup(format!("test {digest} of {tenant}: {error}")))?;
            let new = rewind_format::test_digest(&test);
            if new != *digest {
                context.digests.insert(*digest, new);
            }
            new_tests.insert((tenant.clone(), *digest), test);
        }
        for (index, version) in versions.iter().enumerate() {
            let document = rewind_format::rename::workflow_version(&version.json, &context)
                .map_err(|error| {
                    setup(format!(
                        "version {} of workflow {} ({}): {error}",
                        version.version, version.workflow, version.tenant
                    ))
                })?;
            let new = document.digest();
            if new != version.digest {
                context.digests.insert(version.digest, new);
            }
            let pinned = PinnedVersion {
                id: version.workflow.to_string(),
                version: version.version,
                digest: new,
            };
            for digest in [version.digest, new] {
                context
                    .pinned
                    .insert((version.tenant.to_string(), digest), pinned.clone());
            }
            new_versions.insert(index, document);
        }
        for (index, document) in action_sets.iter().enumerate() {
            let action_set = rewind_format::rename::action_set(
                &document.json,
                document.tenant.as_str(),
                &context,
            )
            .map_err(|error| {
                setup(format!(
                    "action set {} of {}: {error}",
                    document.digest, document.tenant
                ))
            })?;
            let new = action_set.digest();
            if new != document.digest {
                context.digests.insert(document.digest, new);
            }
            new_action_sets.insert(index, action_set);
        }
        if context.digests == before {
            break;
        }
    }
    let changed_tests: Vec<&(TenantId, Digest, String)> = tests
        .iter()
        .filter(|(_, digest, _)| context.digests.contains_key(digest))
        .collect();
    report.documents.insert("tests", changed_tests.len() as u64);
    let changed_versions: Vec<usize> = versions
        .iter()
        .enumerate()
        .filter(|(index, version)| {
            new_versions
                .get(index)
                .is_some_and(|document| document.digest() != version.digest)
        })
        .map(|(index, _)| index)
        .collect();
    report
        .documents
        .insert("workflow versions", changed_versions.len() as u64);
    let changed_action_sets: Vec<usize> = action_sets
        .iter()
        .enumerate()
        .filter(|(index, document)| {
            new_action_sets
                .get(index)
                .is_some_and(|action_set| action_set.digest() != document.digest)
        })
        .map(|(index, _)| index)
        .collect();
    report
        .documents
        .insert("action sets", changed_action_sets.len() as u64);

    if !dry_run {
        for (tenant, old, _) in &changed_tests {
            let test = &new_tests[&(tenant.clone(), *old)];
            let bytes = rewind_format::encode_test(test).into_bytes();
            let new = rewind_format::test_digest(test);
            let owner = BlobOwner::Tests(tenant.clone());
            let size = bytes.len() as u64;
            opened.blobs.put(&owner, new, bytes).await.map_err(setup)?;
            catalog
                .replace_test(tenant, *old, new, size)
                .await
                .map_err(setup)?;
            opened.blobs.delete(&owner, *old).await.map_err(setup)?;
        }
        for index in &changed_versions {
            let version = &versions[*index];
            catalog
                .replace_version(
                    &version.tenant,
                    &version.workflow,
                    version.version,
                    &new_versions[index],
                )
                .await
                .map_err(setup)?;
        }
        for index in &changed_action_sets {
            let document = &action_sets[*index];
            let action_set = &new_action_sets[index];
            let observed = observed(&logs, &document.tenant, action_set).await?;
            let generated = rewind_codegen::generate(action_set, &observed);
            let declarations = ActionDeclarations {
                typescript: generated.typescript,
                prompt: generated.prompt,
            };
            catalog
                .replace_action_set(&document.tenant, document.digest, action_set, &declarations)
                .await
                .map_err(setup)?;
        }
    }

    // Every run's workflow and version, every code's workflow.
    if dry_run {
        report.workflows = None;
    } else {
        let uses = code_uses(&runs, &context)?;
        let renamed: WorkflowsRenamed =
            rewind_engine::rename::workflows(&catalog, &SystemEntropy, &uses, SystemClock.now())
                .await
                .map_err(setup)?;
        for (run, workflow) in &renamed.runs {
            context.runs.insert(
                *run,
                json!({
                    "id": workflow.id.as_str(),
                    "name": workflow.name,
                    "version": workflow.version,
                    "digest": workflow.digest.to_hex(),
                    "interface": workflow.interface,
                }),
            );
            if let Some(Value::Object(object)) = context.runs.get_mut(run)
                && object.get("name").is_some_and(Value::is_null)
            {
                object.remove("name");
            }
        }
        for ((tenant, namespace, code), id) in &renamed.codes {
            context.codes.insert(
                (tenant.to_string(), namespace.to_string(), *code),
                id.to_string(),
            );
        }
        report.workflows = Some(WorkflowsSummary {
            created: renamed.created.len(),
            scoped: renamed.scoped.len(),
            kept: renamed.kept.len(),
            named: renamed.named.len(),
        });
        report.dropped = renamed.dropped;
    }

    // The partitions, in the new names.
    for log in &logs {
        let rewritten = rename_partition(log, &context, !dry_run)
            .await
            .map_err(setup)?;
        for (kind, count) in rewritten.records {
            *report.records.entry(kind).or_default() += count;
        }
    }
    for log in &logs {
        let _ = log.close().await;
    }

    // The archive's sealed attempts, in the new names.
    let archive = opened.archive.inner();
    for run in &runs {
        let Some(sealed) = run.sealed else {
            continue;
        };
        let tenant = TenantId::new(run.tenant.clone()).map_err(setup)?;
        let run_ref = rewind_engine::ports::RunRef::new(tenant, run.run);
        for attempt in 1..=sealed {
            let attempt = rewind_engine::ports::Attempt(attempt);
            let Some(bytes) = raw(archive, &run_ref, attempt).await? else {
                continue;
            };
            let json = String::from_utf8(bytes.clone()).map_err(setup)?;
            let export = rewind_format::rename::run_export(&json, &run.tenant, &context)
                .map_err(|error| setup(format!("run {} attempt {attempt}: {error}", run.run)))?;
            let renamed = rewind_format::encode_run_export(&export).into_bytes();
            if renamed != bytes {
                report.sealed_attempts += 1;
                if !dry_run {
                    replace(archive, &run_ref, attempt, renamed).await?;
                }
            }
        }
    }
    Ok(report)
}

/// The use of each run, for `rename::workflows`, its digests the new ones.
fn code_uses(runs: &[StoredRun], context: &Context) -> Result<Vec<CodeUse>, StartupError> {
    runs.iter()
        .map(|run| {
            let mut secrets = ServerSecrets::new();
            for (server, secret) in &run.secrets {
                secrets.insert(server.clone(), SecretRef::parse(secret).map_err(setup)?);
            }
            Ok(CodeUse {
                tenant: TenantId::new(run.tenant.clone()).map_err(setup)?,
                namespace: NamespaceName::new(run.namespace.clone()).map_err(setup)?,
                run: run.run,
                code: run.code,
                action_set: context.digest(&run.action_set).unwrap_or(run.action_set),
                secrets,
                labels: run.labels.clone(),
                version: run
                    .version
                    .map(|version| context.digest(&version).unwrap_or(version)),
                started_at: Timestamp(run.created_at),
            })
        })
        .collect()
}

/// The shapes the partitions observed of an action set's actions, across the tenant's
/// namespaces: what its declarations type the results with.
async fn observed(
    logs: &[SlateLog],
    tenant: &TenantId,
    action_set: &ActionSet,
) -> Result<BTreeMap<String, Shape>, StartupError> {
    let names: Vec<String> = action_set
        .actions()
        .iter()
        .map(|action| action.name.clone())
        .collect();
    let mut shapes: BTreeMap<String, Shape> = BTreeMap::new();
    for log in logs {
        for (action, shape) in log.shapes(tenant, None, &names).await.map_err(setup)? {
            let merged = match shapes.remove(&action) {
                Some(seen) => seen.merge(shape),
                None => shape,
            };
            shapes.insert(action, merged);
        }
    }
    Ok(shapes)
}

async fn raw(
    archive: &Archives,
    run: &rewind_engine::ports::RunRef,
    attempt: rewind_engine::ports::Attempt,
) -> Result<Option<Vec<u8>>, StartupError> {
    match archive {
        Archives::Files(files) => files.raw(run, attempt).await,
        Archives::S3(s3) => s3.raw(run, attempt).await,
    }
    .map_err(setup)
}

async fn replace(
    archive: &Archives,
    run: &rewind_engine::ports::RunRef,
    attempt: rewind_engine::ports::Attempt,
    bytes: Vec<u8>,
) -> Result<(), StartupError> {
    match archive {
        Archives::Files(files) => files.replace(run, attempt, bytes).await,
        Archives::S3(s3) => s3.replace(run, attempt, bytes).await,
    }
    .map_err(setup)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_command_takes_the_servers_options_and_a_dry_run() {
        let args = RenameArgs::try_parse_from([
            "rewind server rename-data",
            "--dry-run",
            "--store",
            "postgres://localhost/rewind",
            "--bucket",
            "s3://runs",
        ])
        .unwrap();
        assert!(args.dry_run);
        let args = RenameArgs::try_parse_from(["rewind server rename-data", "--embedded"]).unwrap();
        assert!(!args.dry_run && args.server.embedded);
    }
}
