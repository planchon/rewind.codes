//! Secrets attached to runs (`archi/04-tools.md`, "Secrets" and "Signed calls"): the checks at
//! start, at a fork and at a workflow's save; the secret a call reads in its run's namespace,
//! never another; a deleted secret parks the call until it is put again; signed calls.

#![allow(clippy::unwrap_used)]

mod support;

use std::collections::BTreeMap;

use rewind_core::{
    Attachments, Event, JsonValue, McpServer, McpTransport, RunMode, RunStatus, Script, SecretName,
    SecretPlacement, SecretRef, Seq, Tool, ToolAuth, ToolFailureKind, ToolRoute, Toolset,
    WorkflowSchemas,
};
use rewind_engine::adapters::StaticTypeChecker;
use rewind_engine::attachments::{AttachRights, AttachmentProblem, RefusedAttachments};
use rewind_engine::ports::{
    CallAuth, CallClaims, Clock, CredentialsExpiredReason, Day, Delivery, Dispatched, JournalStore,
    NamespaceName, NewSecret, ParkKind, PutSecret, RunFilter, RunRef, Secret, SecretId,
    SecretNamespace, SecretStore, Subscription, TaskState, TenantId, ToolDeclarations,
    WebhookEventKind, WorkflowDeclarations,
};
use rewind_engine::suites::SuiteChanges;
use rewind_engine::{
    DriveOptions, EngineConfig, ForkError, ForkRun, RegisterError, RegisterWorkflow, StartError,
    StartRun, StartWorkflow,
};
use serde_json::json;
use support::*;

// -------------------------------------------------------------------------------------------
// Helpers

/// `github` takes a secret as a bearer token, `linear` in `X-Api-Key`, `dax` checks Rewind's
/// signature, and `ask_user` is a host tool.
fn toolset() -> Toolset {
    let server = |name: &str, auth: ToolAuth| {
        let mut server = McpServer::new(
            name,
            McpTransport::Http {
                url: format!("https://mcp.example.com/{name}"),
            },
        );
        server.auth = auth;
        server
    };
    let tool = |name: &str, server: &str| {
        Tool::new(
            name,
            ToolRoute::Mcp {
                server: server.into(),
            },
            json!({}),
        )
    };
    Toolset::new(
        [
            server("github", ToolAuth::Secret(SecretPlacement::bearer())),
            server(
                "linear",
                ToolAuth::Secret(SecretPlacement::header("X-Api-Key")),
            ),
            server("dax", ToolAuth::Rewind),
        ],
        [
            tool("github.get_issue", "github"),
            tool("linear.search", "linear"),
            tool("dax.ask", "dax"),
            Tool::new("ask_user", ToolRoute::Host { group: None }, json!({})),
        ],
    )
    .unwrap()
}

/// Calls the tool its input names, and completes with the result or the failure.
fn calling(run: &mut FakeRun) {
    let tool = run.input["tool"].as_str().unwrap().to_owned();
    run.call(&tool, json!({}));
}

fn answered(run: &mut FakeRun, delivery: &Delivery) {
    match delivery {
        Delivery::ToolCallCompleted { result, .. } => {
            run.complete(json!({ "result": result }));
        }
        Delivery::ToolCallFailed { error, .. } => run.complete(json!({
            "error": error.kind.as_str(),
            "message": error.message,
        })),
        other => panic!("unexpected {other:?}"),
    }
}

/// `caller` and `signer` call the tool their input names and complete with what came back;
/// `quiet` calls nothing. `caller` imports `github` and `dax`, `signer` `dax`, `quiet` nothing.
fn programs() -> FakeExecutor {
    FakeExecutor::new()
        .with("caller", calling, answered)
        .importing("caller", &["github", "dax"])
        .with("signer", calling, answered)
        .importing("signer", &["dax"])
        .with("quiet", |run| run.complete(json!(null)), |_, _| {})
        .importing("quiet", &[])
}

fn alice() -> NamespaceName {
    NamespaceName::new("alice").unwrap()
}

fn bob() -> NamespaceName {
    NamespaceName::new("bob").unwrap()
}

fn tenant() -> TenantId {
    TenantId::local()
}

fn secret_ref(text: &str) -> SecretRef {
    SecretRef::parse(text).unwrap()
}

fn attaching(pairs: &[(&str, &str)]) -> Attachments {
    pairs
        .iter()
        .map(|(server, secret)| ((*server).to_owned(), secret_ref(secret)))
        .collect()
}

fn organization(name: &str) -> SecretId {
    SecretId::organization(SecretName::new(name).unwrap())
}

fn of(namespace: &NamespaceName, name: &str) -> SecretId {
    SecretId::of_namespace(namespace.clone(), SecretName::new(name).unwrap())
}

fn with_secrets() -> RightsTo {
    RightsTo::Organization(BTreeMap::new())
}

/// What the caller of a test may attach.
enum RightsTo {
    Namespace,
    Organization(BTreeMap<String, String>),
}

impl RightsTo {
    fn rights(&self) -> AttachRights {
        match self {
            RightsTo::Namespace => AttachRights::NamespaceOnly,
            RightsTo::Organization(labels) => AttachRights::Organization {
                labels: labels.clone(),
            },
        }
    }
}

/// A live run of `program` in `namespace`, calling `tool`, attaching `secrets`.
fn start(
    program: &str,
    tool: &str,
    namespace: &NamespaceName,
    secrets: &[(&str, &str)],
    rights: &RightsTo,
) -> StartRun {
    let mut request = StartRun::new(Script::new(program), toolset(), json!({ "tool": tool }));
    request.namespace = namespace.clone();
    request.metadata.secrets = attaching(secrets);
    request.attach = rights.rights();
    request
}

async fn put(engine: &TestEngine, id: SecretId, value: &str, labels: &[(&str, &str)]) -> usize {
    let secret = NewSecret {
        id,
        value: Secret::new(value),
        labels: labels
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect(),
        by: "user:u_1".into(),
    };
    engine.put_secret(&tenant(), &secret).await.unwrap().1
}

fn refused(error: StartError) -> RefusedAttachments {
    match error {
        StartError::SecretRequired(refused) => refused,
        other => panic!("expected secret-required, got {other:?}"),
    }
}

async fn no_run(engine: &TestEngine) {
    let runs = engine.list_runs(&RunFilter::new(tenant())).await.unwrap();
    assert!(runs.is_empty(), "no run exists: {runs:?}");
}

async fn drive(engine: &TestEngine) {
    let report = engine
        .run_until_idle_virtual(&DriveOptions::default())
        .await
        .unwrap();
    assert!(report.failures.is_empty(), "{:?}", report.failures);
}

async fn output(engine: &TestEngine, run: &RunRef) -> JsonValue {
    let history = engine.history(run).await.unwrap();
    match &history.last().unwrap().event {
        Event::Command(rewind_core::Command::CompleteRun { output }) => {
            output.value().unwrap().clone()
        }
        other => panic!("the run has not completed: {other:?}"),
    }
}

// -------------------------------------------------------------------------------------------
// The checks at start

#[tokio::test]
async fn a_start_without_the_secret_an_imported_server_needs_is_refused() {
    let engine = engine(programs(), RecordingDispatcher::echo());
    put(&engine, of(&alice(), "github"), "ghp_alice", &[]).await;
    put(&engine, organization("zendesk"), "zd", &[]).await;
    put(&engine, of(&bob(), "linear"), "lin_bob", &[]).await;

    let error = engine
        .start_run(start(
            "caller",
            "github.get_issue",
            &alice(),
            &[],
            &with_secrets(),
        ))
        .await
        .unwrap_err();
    let message = error.to_string();
    assert_eq!(
        refused(error),
        RefusedAttachments {
            problems: vec![AttachmentProblem::Missing {
                server: "github".into()
            }],
            attachable: vec![secret_ref("org/zendesk"), secret_ref("ns/github")],
        },
        "github, which the script imports; not linear, which it does not; the organization's \
         secrets and alice's, not bob's"
    );
    assert!(
        message.contains("the script imports github, which needs a secret")
            && message.ends_with("The secrets you may attach: org/zendesk, ns/github"),
        "{message}"
    );
    no_run(&engine).await;

    // Without secrets:use, the organization's secrets are not offered.
    let error = engine
        .start_run(start(
            "caller",
            "github.get_issue",
            &bob(),
            &[],
            &RightsTo::Namespace,
        ))
        .await
        .unwrap_err();
    assert_eq!(refused(error).attachable, [secret_ref("ns/linear")]);

    // A script that imports none of them needs nothing.
    engine
        .start_run(start("quiet", "", &alice(), &[], &RightsTo::Namespace))
        .await
        .unwrap();
}

#[tokio::test]
async fn an_attachment_names_a_server_taking_a_secret_and_a_secret_that_exists() {
    let engine = engine(programs(), RecordingDispatcher::echo());
    // The organization's github exists, alice's does not: ns/github never falls back on it.
    put(&engine, organization("github"), "ghp_org", &[]).await;
    let error = engine
        .start_run(start(
            "caller",
            "github.get_issue",
            &alice(),
            &[
                ("github", "ns/github"),
                ("dax", "org/github"),
                ("ask_user", "org/github"),
                ("nowhere", "org/github"),
            ],
            &with_secrets(),
        ))
        .await
        .unwrap_err();
    let refused = refused(error);
    assert_eq!(
        refused.problems,
        [
            AttachmentProblem::TakesNone {
                server: "ask_user".into()
            },
            AttachmentProblem::TakesNone {
                server: "dax".into()
            },
            AttachmentProblem::Unknown {
                server: "github".into(),
                secret: secret_ref("ns/github"),
            },
            AttachmentProblem::TakesNone {
                server: "nowhere".into()
            },
        ],
        "a host tool, a signed server and an unknown name take no secret"
    );
    assert_eq!(refused.attachable, [secret_ref("org/github")]);
    no_run(&engine).await;

    put(&engine, of(&alice(), "github"), "ghp_alice", &[]).await;
    engine
        .start_run(start(
            "caller",
            "github.get_issue",
            &alice(),
            &[("github", "ns/github")],
            &RightsTo::Namespace,
        ))
        .await
        .unwrap();
}

#[tokio::test]
async fn the_organization_s_secrets_need_secrets_use_within_the_caller_s_labels() {
    let engine = engine(programs(), RecordingDispatcher::echo());
    put(&engine, organization("prod"), "p", &[("env", "prod")]).await;
    put(&engine, organization("dev"), "d", &[("env", "dev")]).await;
    let attach = |secret: &str, rights: RightsTo| {
        start(
            "caller",
            "github.get_issue",
            &alice(),
            &[("github", secret)],
            &rights,
        )
    };
    let prod = || BTreeMap::from([("env".to_owned(), "prod".to_owned())]);

    let error = engine
        .start_run(attach("org/prod", RightsTo::Namespace))
        .await
        .unwrap_err();
    assert_eq!(
        refused(error).problems,
        [AttachmentProblem::Forbidden {
            server: "github".into(),
            secret: secret_ref("org/prod"),
        }],
        "an organization's secret without secrets:use"
    );
    let error = engine
        .start_run(attach("org/dev", RightsTo::Organization(prod())))
        .await
        .unwrap_err();
    let refused = refused(error);
    assert_eq!(
        refused.problems,
        [AttachmentProblem::Unknown {
            server: "github".into(),
            secret: secret_ref("org/dev"),
        }],
        "a secret outside the caller's labels is one it does not see"
    );
    assert_eq!(refused.attachable, [secret_ref("org/prod")]);
    engine
        .start_run(attach("org/prod", RightsTo::Organization(prod())))
        .await
        .unwrap();
    engine
        .start_run(attach("org/dev", with_secrets()))
        .await
        .unwrap();
}

#[tokio::test]
async fn mock_mode_checks_no_attachment_and_reads_no_secret() {
    let dispatcher = RecordingDispatcher::echo();
    let engine = engine(programs(), dispatcher.clone());
    let mut request = start(
        "caller",
        "github.get_issue",
        &alice(),
        &[],
        &RightsTo::Namespace,
    );
    request.mode = RunMode::Mock;
    let run = engine.start_run(request).await.unwrap().run;
    drive(&engine).await;
    assert_eq!(output(&engine, &run).await, json!({ "result": {} }));
    assert_eq!(dispatcher.auth(), [CallAuth::None]);
}

// -------------------------------------------------------------------------------------------
// Reading at dispatch

#[tokio::test]
async fn a_call_reads_the_secret_its_run_attached_in_its_own_namespace() {
    let dispatcher = RecordingDispatcher::echo();
    let engine = engine(programs(), dispatcher.clone());
    put(&engine, of(&alice(), "github"), "ghp_alice", &[]).await;
    put(&engine, of(&bob(), "github"), "ghp_bob", &[]).await;
    put(&engine, organization("github"), "ghp_org", &[]).await;
    let alices = engine
        .start_run(start(
            "caller",
            "github.get_issue",
            &alice(),
            &[("github", "ns/github")],
            &RightsTo::Namespace,
        ))
        .await
        .unwrap()
        .run;
    drive(&engine).await;
    let bobs = engine
        .start_run(start(
            "caller",
            "github.get_issue",
            &bob(),
            &[("github", "ns/github")],
            &RightsTo::Namespace,
        ))
        .await
        .unwrap()
        .run;
    drive(&engine).await;

    let bearer = |value: &str| CallAuth::Secret {
        value: Secret::new(value),
        placement: SecretPlacement::bearer(),
    };
    assert_eq!(
        dispatcher.auth(),
        [bearer("ghp_alice"), bearer("ghp_bob")],
        "each run's own namespace's, never the organization's"
    );
    let today = Day::of(engine.clock().now());
    let read = engine
        .secrets()
        .meta(&tenant(), &of(&bob(), "github"), today)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(read.reads.iter().map(|day| day.reads).sum::<u64>(), 1);
    let last = read.last_read.unwrap();
    assert_eq!((last.run, last.namespace), (bobs.id, bob()));
    let org = engine
        .secrets()
        .meta(&tenant(), &organization("github"), today)
        .await
        .unwrap()
        .unwrap();
    assert!(org.last_read.is_none(), "nothing fell back on it");
    assert_eq!(output(&engine, &alices).await, json!({ "result": {} }));
}

#[tokio::test]
async fn a_secret_a_server_echoes_never_reaches_the_journal_nor_the_script() {
    let dispatcher = RecordingDispatcher::dispatching(|call, auth| {
        let CallAuth::Secret { value, .. } = auth else {
            panic!("the call carries its secret");
        };
        let value = value.expose();
        if call.attempt == 1 {
            // An error that quotes the key, retried.
            return Dispatched::Transient(rewind_core::ToolFailure {
                kind: ToolFailureKind::Unavailable,
                message: format!("invalid key {value}"),
                data: Some(json!({ "body": format!("token={value}") })),
            });
        }
        Dispatched::Result(json!({ "echo": format!("Bearer {value}"), value: [value] }))
    });
    let engine = engine(programs(), dispatcher.clone());
    put(&engine, of(&alice(), "github"), "ghp_alice_secret", &[]).await;
    let run = engine
        .start_run(start(
            "caller",
            "github.get_issue",
            &alice(),
            &[("github", "ns/github")],
            &RightsTo::Namespace,
        ))
        .await
        .unwrap()
        .run;
    drive(&engine).await;

    assert_eq!(
        output(&engine, &run).await,
        json!({ "result": { "echo": "Bearer [secret]", "[secret]": ["[secret]"] } })
    );
    let journal = format!("{:?}", engine.history(&run).await.unwrap());
    assert!(!journal.contains("ghp_alice_secret"), "{journal}");
}

#[tokio::test]
async fn a_secret_deleted_mid_run_parks_the_call_until_it_is_put_again() {
    let dispatcher = RecordingDispatcher::dispatching(|call, _| {
        rewind_engine::ports::Dispatched::Result(json!({ "key": call.idempotency_key }))
    });
    let engine = engine(programs(), dispatcher.clone());
    engine
        .journal()
        .put_subscription(&Subscription {
            tenant: tenant(),
            id: "sub".into(),
            url: "https://harness.example.com/hooks".into(),
            secret: Secret::new("whsec_test"),
            events: Default::default(),
            labels: BTreeMap::new(),
        })
        .await
        .unwrap();
    put(&engine, of(&alice(), "github"), "ghp_old", &[]).await;
    let run = engine
        .start_run(start(
            "caller",
            "github.get_issue",
            &alice(),
            &[("github", "ns/github")],
            &RightsTo::Namespace,
        ))
        .await
        .unwrap()
        .run;
    assert!(
        engine
            .delete_secret(&tenant(), &of(&alice(), "github"))
            .await
            .unwrap()
    );
    drive(&engine).await;

    assert!(dispatcher.calls().is_empty(), "nothing was sent");
    let tasks = engine.tool_tasks(&run).await.unwrap();
    let TaskState::Parked(reason) = &tasks[0].state else {
        panic!("the call is parked: {:?}", tasks[0].state);
    };
    assert_eq!(reason.kind, ParkKind::SecretMissing);
    assert!(
        reason
            .message
            .contains("the secret ns/github the run attached for github no longer exists"),
        "{}",
        reason.message
    );
    assert!(
        reason
            .message
            .contains("PUT /v1/namespaces/alice/secrets/github")
    );
    let deliveries = engine.webhooks().deliveries();
    assert_eq!(deliveries.len(), 1);
    let WebhookEventKind::CredentialsExpired { reason, secret, .. } = &deliveries[0].1.kind else {
        panic!("credentials.expired: {:?}", deliveries[0].1.kind);
    };
    assert_eq!(*reason, CredentialsExpiredReason::Missing);
    assert_eq!(secret.as_ref(), Some(&secret_ref("ns/github")));
    assert_eq!(
        engine.get_run(&run).await.unwrap().unwrap().status,
        RunStatus::Active
    );

    // Bob's github put is not alice's: the call keeps waiting.
    assert_eq!(put(&engine, of(&bob(), "github"), "ghp_bob", &[]).await, 0);
    assert_eq!(
        put(&engine, of(&alice(), "github"), "ghp_new", &[]).await,
        1
    );
    drive(&engine).await;
    assert_eq!(
        output(&engine, &run).await,
        json!({ "result": { "key": format!("{}/1", run.id) } }),
        "sent again with the same idempotency key"
    );
    assert_eq!(
        dispatcher.auth(),
        [CallAuth::Secret {
            value: Secret::new("ghp_new"),
            placement: SecretPlacement::bearer(),
        }]
    );
}

#[tokio::test]
async fn a_signed_call_carries_the_tenant_s_key_and_the_claims_of_the_call() {
    let dispatcher = RecordingDispatcher::echo();
    let config = EngineConfig {
        public_url: Some("https://rewind.example.com/".into()),
        ..EngineConfig::default()
    };
    let engine = engine_with(programs(), dispatcher.clone(), config);
    let mut request = start("signer", "dax.ask", &alice(), &[], &RightsTo::Namespace);
    request.metadata.labels = BTreeMap::from([("env".to_owned(), "prod".to_owned())]);
    let run = engine.start_run(request).await.unwrap().run;
    drive(&engine).await;

    let auth = dispatcher.auth();
    let [CallAuth::Signed { key, claims }] = auth.as_slice() else {
        panic!("a signed call: {auth:?}");
    };
    let published = engine
        .secrets()
        .public_keys(&tenant(), engine.clock().now())
        .await
        .unwrap();
    assert_eq!(published.len(), 1, "made at the first signed call");
    assert_eq!(key.public, published[0]);
    assert_eq!(
        *claims,
        CallClaims {
            issuer: "https://rewind.example.com/organizations/local".into(),
            subject: alice(),
            labels: BTreeMap::from([("env".to_owned(), "prod".to_owned())]),
            run: run.id,
            tool: "dax.ask".into(),
            idempotency_key: format!("{}/1", run.id),
            seq: Seq(1),
            attempt: 1,
        }
    );
    assert_eq!(claims.jti(), format!("{}/1/1", run.id));
}

#[tokio::test]
async fn without_a_public_url_a_signed_call_fails_unsent() {
    let dispatcher = RecordingDispatcher::echo();
    let engine = engine(programs(), dispatcher.clone());
    let run = engine
        .start_run(start(
            "signer",
            "dax.ask",
            &alice(),
            &[],
            &RightsTo::Namespace,
        ))
        .await
        .unwrap()
        .run;
    drive(&engine).await;
    assert!(dispatcher.calls().is_empty());
    let output = output(&engine, &run).await;
    assert_eq!(output["error"], ToolFailureKind::Unavailable.as_str());
    assert!(
        output["message"]
            .as_str()
            .unwrap()
            .contains("no public URL"),
        "{output}"
    );
}

// -------------------------------------------------------------------------------------------
// Forks

#[tokio::test]
async fn a_fork_inherits_its_parent_s_attachments_checked_against_its_caller() {
    let engine = engine(programs(), RecordingDispatcher::echo());
    put(&engine, organization("github"), "ghp_org", &[]).await;
    put(&engine, of(&alice(), "github"), "ghp_alice", &[]).await;
    let parent = engine
        .start_run(start(
            "caller",
            "github.get_issue",
            &alice(),
            &[("github", "org/github")],
            &with_secrets(),
        ))
        .await
        .unwrap()
        .run;
    drive(&engine).await;
    let fork = |secrets: Option<&[(&str, &str)]>, rights: RightsTo| {
        let mut request = ForkRun::new(parent.clone(), Script::new("caller"));
        request.secrets = secrets.map(attaching);
        request.attach = rights.rights();
        request
    };

    // The script is new code: the parent's org/github is checked against the caller.
    let error = engine
        .fork(fork(None, RightsTo::Namespace))
        .await
        .unwrap_err();
    let ForkError::Start(error) = error else {
        panic!("refused at start: {error:?}");
    };
    assert_eq!(
        refused(error).problems,
        [AttachmentProblem::Forbidden {
            server: "github".into(),
            secret: secret_ref("org/github"),
        }]
    );

    let inherited = engine.fork(fork(None, with_secrets())).await.unwrap().run;
    let record = engine.get_run(&inherited).await.unwrap().unwrap();
    assert_eq!(
        record.metadata.secrets,
        attaching(&[("github", "org/github")])
    );
    assert_eq!(
        record.namespace,
        alice(),
        "a fork never leaves its namespace"
    );

    let own = engine
        .fork(fork(Some(&[("github", "ns/github")]), RightsTo::Namespace))
        .await
        .unwrap()
        .run;
    let record = engine.get_run(&own).await.unwrap().unwrap();
    assert_eq!(
        record.metadata.secrets,
        attaching(&[("github", "ns/github")])
    );
}

// -------------------------------------------------------------------------------------------
// Workflow versions

fn save(secrets: &[(&str, &str)], rights: &RightsTo) -> RegisterWorkflow {
    RegisterWorkflow {
        tenant: tenant(),
        name: "triage".into(),
        description: None,
        script: Script::new("caller"),
        toolset: toolset().digest(),
        schemas: WorkflowSchemas {
            input: json!({ "type": "object" }),
            output: json!({}),
            signals: BTreeMap::new(),
        },
        labels: BTreeMap::new(),
        secrets: attaching(secrets),
        attach: rights.rights(),
        declarations: WorkflowDeclarations::default(),
        suite: SuiteChanges::default(),
    }
}

async fn register(
    engine: &TestEngine,
    request: RegisterWorkflow,
) -> Result<rewind_engine::ports::StoredWorkflow, RegisterError> {
    engine
        .register_workflow(request, &StaticTypeChecker::default(), &kit())
        .await
}

#[tokio::test]
async fn a_version_is_saved_with_attachments_its_saver_may_attach() {
    let engine = engine(programs(), RecordingDispatcher::echo());
    engine
        .journal()
        .put_toolset(&tenant(), &toolset(), &ToolDeclarations::default())
        .await
        .unwrap();
    put(&engine, organization("github"), "ghp_org", &[]).await;

    let saver_refused = |error: RegisterError| match error {
        RegisterError::SecretRequired(refused) => refused.problems,
        other => panic!("expected secret-required, got {other:?}"),
    };
    assert_eq!(
        saver_refused(
            register(&engine, save(&[], &with_secrets()))
                .await
                .unwrap_err()
        ),
        [AttachmentProblem::Missing {
            server: "github".into()
        }],
        "its script imports github: every run of it would need one"
    );
    assert_eq!(
        saver_refused(
            register(
                &engine,
                save(&[("github", "org/github")], &RightsTo::Namespace)
            )
            .await
            .unwrap_err()
        ),
        [AttachmentProblem::Forbidden {
            server: "github".into(),
            secret: secret_ref("org/github"),
        }]
    );
    assert_eq!(
        saver_refused(
            register(&engine, save(&[("github", "org/gone")], &with_secrets()))
                .await
                .unwrap_err()
        ),
        [AttachmentProblem::Unknown {
            server: "github".into(),
            secret: secret_ref("org/gone"),
        }]
    );
    let org = register(&engine, save(&[("github", "org/github")], &with_secrets()))
        .await
        .unwrap();
    assert_eq!(
        org.workflow.secrets(),
        &attaching(&[("github", "org/github")])
    );
    // An ns/ secret has no namespace yet: only its name is checked.
    let per_user = register(
        &engine,
        save(&[("github", "ns/github")], &RightsTo::Namespace),
    )
    .await
    .unwrap();
    assert_ne!(per_user.workflow.version(), org.workflow.version());
}

#[tokio::test]
async fn a_run_of_a_version_checks_only_that_its_namespace_s_secrets_exist() {
    let engine = engine(programs(), RecordingDispatcher::echo());
    engine
        .journal()
        .put_toolset(&tenant(), &toolset(), &ToolDeclarations::default())
        .await
        .unwrap();
    let version = register(
        &engine,
        save(&[("github", "ns/github")], &RightsTo::Namespace),
    )
    .await
    .unwrap()
    .workflow
    .version();
    let start = |namespace: NamespaceName| StartWorkflow {
        tenant: tenant(),
        namespace,
        version,
        input: json!({ "tool": "github.get_issue" }),
        ttl: None,
        metadata: Default::default(),
        idempotency_key: None,
    };
    let error = engine.start_workflow(start(bob())).await.unwrap_err();
    assert_eq!(
        refused(error).problems,
        [AttachmentProblem::Unknown {
            server: "github".into(),
            secret: secret_ref("ns/github"),
        }]
    );
    put(&engine, of(&bob(), "github"), "ghp_bob", &[]).await;
    let run = engine.start_workflow(start(bob())).await.unwrap().run;
    let record = engine.get_run(&run).await.unwrap().unwrap();
    assert_eq!(
        record.metadata.secrets,
        attaching(&[("github", "ns/github")]),
        "the version's attachments"
    );
}

#[tokio::test]
async fn the_secrets_of_a_namespace_and_of_the_organization_stay_apart() {
    let engine = engine(programs(), RecordingDispatcher::echo());
    put(&engine, of(&alice(), "github"), "a", &[]).await;
    put(&engine, organization("github"), "o", &[]).await;
    let list = |namespace: SecretNamespace| {
        let engine = &engine;
        async move {
            engine
                .secrets()
                .list(&tenant(), &namespace, &BTreeMap::new(), Day(0))
                .await
                .unwrap()
                .into_iter()
                .map(|meta| meta.id.to_string())
                .collect::<Vec<_>>()
        }
    };
    assert_eq!(list(SecretNamespace::Organization).await, ["org/github"]);
    assert_eq!(
        list(SecretNamespace::Namespace(alice())).await,
        ["ns/alice/github"]
    );
    let again = NewSecret {
        id: of(&alice(), "github"),
        value: Secret::new("b"),
        labels: BTreeMap::new(),
        by: "token:alice".into(),
    };
    assert_eq!(
        engine.put_secret(&tenant(), &again).await.unwrap(),
        (PutSecret::Replaced, 0)
    );
}
