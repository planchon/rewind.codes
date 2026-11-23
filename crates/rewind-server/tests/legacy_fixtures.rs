//! Builds the fixtures of `migrate-runs` with the engine over the journal store. Run once with
//! `cargo test -p rewind-server --test legacy_fixtures -- --ignored --nocapture`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

mod support;

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use rewind_blob::FileBlobs;
use rewind_core::{
    Builtin, Script, Timestamp, Tool, ToolFailure, ToolFailureKind, ToolRoute, Toolset,
    WorkflowSchemas,
};
use rewind_engine::adapters::{
    MemorySecrets, MemoryWebhooks, SeededEntropy, StaticTypeChecker, VirtualClock,
};
use rewind_engine::ports::{
    BlobOwner, BlobStore, CallAuth, Clock, Dispatched, JournalStore, NamespaceName, QuarantineKind,
    ReadModel, Retention, RunRef, SecretStore, TaskFilter, Tenant, TenantId, TenantScope,
    TestRecord, ToolCall, ToolDeclarations, ToolDispatcher, VirtualTime, WorkflowDeclarations,
};
use rewind_engine::retention::Housekeeper;
use rewind_engine::{
    Adapters, DriveOptions, Engine, EngineConfig, ForkRun, RegisterWorkflow, StartRun,
};
use rewind_runtime::{SandboxLimits, V8Executor};
use serde_json::{Value, json};

const T0: Timestamp = Timestamp(1_767_225_600_000); // 2026-01-01T00:00:00Z
const DAY: u64 = 86_400_000;
const LONG_TTL: Duration = Duration::from_secs(50 * 365 * 86_400);

#[derive(Debug, Clone)]
struct Shared(Arc<VirtualClock>);

impl Clock for Shared {
    fn now(&self) -> Timestamp {
        self.0.now()
    }
}

impl VirtualTime for Shared {
    fn advance_to(&self, at: Timestamp) {
        self.0.advance_to(at);
    }
}

#[derive(Debug, Clone)]
struct Tools;

impl ToolDispatcher for Tools {
    async fn dispatch(&self, call: ToolCall, _auth: CallAuth) -> Dispatched {
        match call.tool.name.as_str() {
            "small" => Dispatched::Result(json!({ "ok": true })),
            "medium" => Dispatched::Result(json!({ "text": "m".repeat(2_000) })),
            "big" => Dispatched::Result(json!({ "text": "b".repeat(100_000) })),
            "slow" => Dispatched::Deferred,
            "flaky" => Dispatched::Failed(ToolFailure {
                kind: ToolFailureKind::Error,
                message: "it broke".into(),
                data: None,
            }),
            _ => Dispatched::Result(json!({ "ok": true })),
        }
    }
}

type Old<J> =
    Engine<J, FileBlobs, V8Executor, Tools, Shared, SeededEntropy, MemorySecrets, MemoryWebhooks>;

fn engine<J: JournalStore>(
    journal: J,
    blobs: FileBlobs,
    clock: Shared,
    seed: u64,
    tool_tasks: TaskFilter,
) -> Old<J> {
    Engine::new(
        Adapters {
            journal,
            blobs,
            executor: V8Executor::new(SandboxLimits::default()).expect("TZ=UTC"),
            dispatcher: Tools,
            clock,
            entropy: SeededEntropy::new(seed),
            secrets: MemorySecrets::new(),
            webhooks: MemoryWebhooks::new(),
        },
        EngineConfig {
            public_url: Some("https://rewind.example.com".into()),
            max_ttl: LONG_TTL,
            tool_tasks,
            ..EngineConfig::default()
        },
    )
}

fn toolset() -> Toolset {
    let mut tools: Vec<Tool> = ["small", "medium", "big", "slow", "flaky", "ask_human"]
        .into_iter()
        .map(|name| Tool::new(name, ToolRoute::Host { group: None }, json!({})))
        .collect();
    tools.push(Tool::new(
        "webhook",
        ToolRoute::Builtin(Builtin::Webhook),
        rewind_runtime::builtins::webhook_input_schema(),
    ));
    Toolset::new([], tools).unwrap()
}

const TRIAGE: &str = r#"
import { signal } from "rewind";

export default async function main(input: { repo: string }) {
  const approval = await signal<{ close: boolean }>("approval");
  return { closed: approval.close ? 3 : 0, repo: input.repo };
}
"#;

fn schemas() -> WorkflowSchemas {
    WorkflowSchemas {
        input: json!({
            "type": "object",
            "properties": { "repo": { "type": "string" } },
            "required": ["repo"],
        }),
        output: json!({ "type": "object" }),
        signals: BTreeMap::from([(
            "approval".to_owned(),
            json!({
                "type": "object",
                "properties": { "close": { "type": "boolean" } },
                "required": ["close"],
            }),
        )]),
    }
}

struct Scenario<'a, J> {
    engine: &'a Old<J>,
    tenant: TenantId,
    toolset: Toolset,
}

impl<J: JournalStore> Scenario<'_, J> {
    async fn start(
        &self,
        name: &str,
        namespace: &NamespaceName,
        source: &str,
        input: Value,
        key: Option<&str>,
    ) -> RunRef {
        let mut request = StartRun::new(Script::new(source), self.toolset.clone(), input);
        request.tenant = self.tenant.clone();
        request.namespace = namespace.clone();
        request.ttl = Some(LONG_TTL);
        request.idempotency_key = key.map(str::to_owned);
        request.metadata.labels = BTreeMap::from([
            ("fixture".to_owned(), name.to_owned()),
            ("agent".to_owned(), "triage".to_owned()),
        ]);
        let created = self.engine.start_run(request).await.unwrap();
        eprintln!("{name}: {}", created.run.id);
        created.run
    }

    async fn drive(&self) {
        let report = self
            .engine
            .run_until_idle(&DriveOptions::default())
            .await
            .unwrap();
        assert!(report.failures.is_empty(), "{report:?}");
    }
}

/// Fills `journal` and the blobs under `blobs_root` with the runs of every kind.
async fn fill<J>(journal: J, blobs_root: &Path, tenant: TenantId, namespaces: [&str; 2])
where
    J: JournalStore + ReadModel + SecretStore + Clone,
{
    let clock = Shared(Arc::new(VirtualClock::new(T0)));
    let blobs = FileBlobs::open(blobs_root).unwrap();
    let all = engine(
        journal.clone(),
        blobs.clone(),
        clock.clone(),
        7,
        TaskFilter::All,
    );
    let toolset = toolset();
    let [first, second] = namespaces.map(|name| NamespaceName::new(name).unwrap());
    journal
        .put_tenant(&Tenant {
            retention: Retention {
                archive: LONG_TTL,
                ..Retention::default()
            },
            ..Tenant::new(tenant.clone())
        })
        .await
        .unwrap();

    // A workflow whose runs a script starts and watches.
    let child_toolset = Toolset::empty();
    journal
        .put_toolset(&tenant, &child_toolset, &ToolDeclarations::default())
        .await
        .unwrap();
    let stored = all
        .register_workflow(
            RegisterWorkflow {
                tenant: tenant.clone(),
                name: "triage".into(),
                description: None,
                script: Script::new(TRIAGE),
                toolset: child_toolset.digest(),
                schemas: schemas(),
                labels: BTreeMap::new(),
                secrets: Default::default(),
                attach: Default::default(),
                declarations: WorkflowDeclarations::default(),
                suite: rewind_engine::suites::SuiteChanges::default(),
            },
            &StaticTypeChecker::default(),
            &rewind_engine::suites::TestKit {
                loader: &rewind_engine::testing::NoMockLoader,
                format: rewind_engine::suites::TestFormat {
                    encode: |_| String::new(),
                    decode: |_| Err("no test is read here".into()),
                },
                options: rewind_engine::testing::TestOptions::default(),
            },
        )
        .await
        .unwrap();
    let toolset = toolset.with_workflows([stored.workflow.pinned()]).unwrap();
    let scenario = Scenario {
        engine: &all,
        tenant: tenant.clone(),
        toolset,
    };

    // Ended a month ago, its journal archived by the retention.
    scenario
        .start(
            "archived",
            &first,
            r#"
import { small, big, medium } from "rewind:tools";
export default async function main(input: { n: number }) {
  const a = await small({});
  const m = await medium({});
  const b = await big({});
  return { a, m: m.text.length, b: b.text.length, n: input.n };
}
"#,
            json!({ "n": 1 }),
            None,
        )
        .await;
    scenario.drive().await;
    project(&journal, clock.now()).await;
    clock.advance_to(T0.saturating_add(Duration::from_millis(31 * DAY)));
    let archived = Housekeeper {
        journal: &journal,
        blobs: &blobs,
        read_model: &journal,
        secrets: &journal,
        batch: 100,
    }
    .archive(clock.now(), rewind_format::encode_journal)
    .await
    .unwrap();
    assert_eq!(archived, 1);

    let completed = scenario
        .start(
            "completed",
            &first,
            r#"
import { small, medium, big } from "rewind:tools";
export default async function main(input: { repo: string }) {
  const a = await small({ repo: input.repo });
  const m = await medium({});
  const b = await big({});
  return { a, m: m.text.length, b: b.text.length, tail: b.text.slice(0, 5) };
}
"#,
            json!({ "repo": "acme/api" }),
            Some("start-completed"),
        )
        .await;
    let _ = completed;
    let failed = scenario
        .start(
            "failed",
            &first,
            r#"
import { small, flaky } from "rewind:tools";
export default async function main() {
  await small({});
  await flaky({});
  return "unreachable";
}
"#,
            json!({}),
            None,
        )
        .await;
    let cancelled = scenario
        .start(
            "cancelled",
            &second,
            r#"
import { signal } from "rewind";
export default async function main() {
  await signal("never");
}
"#,
            json!({}),
            None,
        )
        .await;
    scenario
        .start(
            "timer",
            &first,
            r#"
import { sleep } from "rewind";
import { small } from "rewind:tools";
export default async function main(input: { text: string }) {
  await small({});
  await sleep("1h");
  return { slept: true, size: input.text.length };
}
"#,
            json!({ "text": "i".repeat(3_000) }),
            None,
        )
        .await;
    scenario
        .start(
            "signal",
            &second,
            r#"
import { signal } from "rewind";
export default async function main() {
  const approval = await signal<{ ok: boolean }>("approval");
  return { approval };
}
"#,
            json!({}),
            Some("start-signal"),
        )
        .await;
    let inbox = scenario
        .start(
            "inbox",
            &first,
            r#"
import { signal } from "rewind";
export default async function main() {
  const first = await signal("first");
  const second = await signal("second");
  return { first, second };
}
"#,
            json!({}),
            None,
        )
        .await;
    scenario
        .start(
            "big-live",
            &second,
            r#"
import { signal } from "rewind";
import { big, medium } from "rewind:tools";
export default async function main() {
  const b = await big({});
  const m = await medium({});
  const done = await signal("done");
  return { b: b.text.length, m: m.text.length, done };
}
"#,
            json!({}),
            None,
        )
        .await;
    scenario
        .start(
            "webhook",
            &first,
            r#"
import { webhook, signal } from "rewind";
export default async function main() {
  const hook = await webhook("pr_merged");
  const event = await signal<{ merged: boolean }>("pr_merged");
  return { url: hook.url, merged: event.merged };
}
"#,
            json!({}),
            None,
        )
        .await;
    scenario
        .start(
            "parent",
            &first,
            r#"
import { triage } from "rewind:workflows";
export default async function main() {
  const run = await triage.start({ repo: "acme/api" });
  const result = await run.result();
  return { result };
}
"#,
            json!({}),
            None,
        )
        .await;
    let quarantined = scenario
        .start(
            "quarantined",
            &second,
            r#"
import { signal } from "rewind";
export default async function main() {
  return await signal("never");
}
"#,
            json!({}),
            None,
        )
        .await;
    scenario
        .start(
            "deferred",
            &first,
            r#"
import { slow } from "rewind:tools";
export default async function main() {
  return await slow({});
}
"#,
            json!({}),
            None,
        )
        .await;
    scenario.drive().await;

    // A fork of the failed run, fixed.
    let mut fork = ForkRun::new(
        failed.clone(),
        Script::new(
            r#"
import { small, flaky } from "rewind:tools";
export default async function main() {
  await small({});
  try {
    await flaky({});
  } catch (error) {
    return { fixed: true };
  }
  return "unreachable";
}
"#,
        ),
    );
    fork.ttl = Some(LONG_TTL);
    fork.labels = Some(BTreeMap::from([("fixture".to_owned(), "fork".to_owned())]));
    let forked = all.fork(fork).await.unwrap();
    eprintln!("fork: {}", forked.run.id);
    all.signal(&inbox, "first", json!({ "n": 1 }), Some("key-first".into()))
        .await
        .unwrap();
    all.cancel(&cancelled, Some("not needed".into()))
        .await
        .unwrap();
    scenario.drive().await;
    let lease = journal
        .acquire_run(&quarantined, "worker", clock.now(), Duration::from_secs(30))
        .await
        .unwrap()
        .unwrap();
    journal
        .quarantine(
            &lease,
            QuarantineKind::Failures,
            "its activations never finished".into(),
            clock.now(),
        )
        .await
        .unwrap();

    // A host tool's task, which no worker took.
    let hosts = engine(
        journal.clone(),
        blobs.clone(),
        clock.clone(),
        8,
        TaskFilter::Dispatched {
            tenants: TenantScope::All,
        },
    );
    let host_scenario = Scenario {
        engine: &hosts,
        tenant: tenant.clone(),
        toolset: scenario.toolset.clone(),
    };
    host_scenario
        .start(
            "host",
            &second,
            r#"
import { ask_human } from "rewind:tools";
export default async function main() {
  const answer = await ask_human({ question: "ship it?" });
  return { answer };
}
"#,
            json!({}),
            None,
        )
        .await;
    host_scenario.drive().await;

    // A signal in the inbox, not yet delivered.
    all.signal(
        &inbox,
        "second",
        json!({ "n": 2 }),
        Some("key-second".into()),
    )
    .await
    .unwrap();

    // A test recorded by hand, as the suites keep them.
    let document = br#"{"v":1,"name":"by hand"}"#.to_vec();
    let digest = rewind_core::Digest::of(&document);
    journal
        .reserve_tests(
            &tenant,
            &[TestRecord {
                digest,
                namespace: None,
                size: document.len() as u64,
            }],
            clock.now(),
        )
        .await
        .unwrap();
    blobs
        .put_in(&BlobOwner::Tests(tenant.clone()), digest, document)
        .await
        .unwrap();
    project(&journal, clock.now()).await;
    eprintln!("now: {}", clock.now().as_millis());
}

/// Applies the feed: the read model, as the server's projector kept it.
async fn project<J: JournalStore + ReadModel>(journal: &J, now: Timestamp) {
    let projector = rewind_engine::projector::Projector {
        journal,
        read_model: journal,
        exporter: &rewind_engine::adapters::MemorySpanExporter::new(),
        webhooks: &MemoryWebhooks::new(),
        config: rewind_engine::projector::ProjectorConfig::default(),
    };
    projector.project_all(now).await.unwrap();
}

fn fixtures() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "builds the fixtures of migrate-runs once"]
async fn build_the_turso_fixture() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join(".rewind");
    std::fs::create_dir_all(&dir).unwrap();
    let store = rewind_store_turso::TursoStore::open(
        dir.join("journal.db"),
        rewind_store_turso::MasterKey::from_bytes([3; 32]),
    )
    .await
    .unwrap();
    fill(
        store.clone(),
        &dir.join("blobs"),
        TenantId::local(),
        ["local", "other"],
    )
    .await;
    drop(store);
    let target = fixtures().join("embedded");
    let _ = std::fs::remove_dir_all(&target);
    std::fs::create_dir_all(&target).unwrap();
    let status = Command::new("cp")
        .arg("-R")
        .arg(&dir)
        .arg(target.join("rewind"))
        .status()
        .unwrap();
    assert!(status.success());
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "builds the fixtures of migrate-runs once"]
async fn build_the_postgres_fixture() {
    let Some(postgres) = support::Postgres::start() else {
        return;
    };
    let mut config = rewind_store_postgres::Config::new(postgres.url());
    config.max_connections = 8;
    let store = rewind_store_postgres::PostgresStore::connect(
        &config,
        rewind_store_postgres::MasterKey::from_bytes([3; 32]),
    )
    .await
    .unwrap();
    let temp = tempfile::tempdir().unwrap();
    let blobs = temp.path().join("bucket");
    fill(
        store.clone(),
        &blobs,
        TenantId::new("acme").unwrap(),
        ["alice", "bob"],
    )
    .await;
    store.close().await;
    let target = fixtures().join("postgres");
    let _ = std::fs::remove_dir_all(&target);
    std::fs::create_dir_all(&target).unwrap();
    let dump = postgres.pg_dump();
    std::fs::write(target.join("dump.sql"), dump).unwrap();
    let status = Command::new("cp")
        .arg("-R")
        .arg(&blobs)
        .arg(target.join("bucket"))
        .status()
        .unwrap();
    assert!(status.success());
}
