//! Secrets attached to runs, in the driver (`archi/04-tools.md`, "Secrets" and "Signed calls"):
//! the checks at start, at a fork and at a workflow's save; the secret a call reads in its
//! run's namespace, never another; a deleted secret parks the call until it is put again;
//! signed calls.

#![allow(clippy::unwrap_used)]

mod support;

use std::collections::BTreeMap;

use rewind_core::{
    ActionSet, Attachments, Command, Event, JsonValue, McpServer, McpTransport, RunMode, RunStatus,
    Script, SecretName, SecretPlacement, SecretRef, Seq, Tool, ToolAuth, ToolFailure,
    ToolFailureKind, ToolRoute, WorkflowSchemas,
};
use rewind_engine::StartWorkflow;
use rewind_engine::adapters::StaticTypeChecker;
use rewind_engine::attachments::{AttachRights, AttachmentProblem, RefusedAttachments};
use rewind_engine::plane::NodeConfig;
use rewind_engine::ports::{
    CallAuth, CallClaims, CatalogStore, Created, CredentialsExpiredReason, Day, Delivery,
    Dispatched, NamespaceName, NewSecret, Page, ParkKind, PutSecret, RunOrder, RunQuery, RunRef,
    Scope, Secret, SecretId, SecretReader, SecretStore, StoredWorkflow, Subscription, TaskState,
    TenantId, ToolDeclarations, WebhookEventKind, WorkflowDeclarations,
};
use rewind_engine::suites::SuiteChanges;
use rewind_engine::{ForkError, ForkRun, RegisterError, RegisterWorkflow, StartError, StartRun};
use serde_json::json;
use support::plane::{World, config};
use support::{FakeExecutor, FakeRun, RecordingDispatcher, kit};

// -------------------------------------------------------------------------------------------
// Helpers

/// `github` takes a secret as a bearer token, `linear` in `X-Api-Key`, `dax` checks Rewind's
/// signature, and `ask_user` is a host tool.
fn action_set() -> ActionSet {
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
    ActionSet::new(
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

fn with_secrets() -> RightsTo {
    RightsTo::Organization(BTreeMap::new())
}

/// A live run of `program` in `namespace`, calling `tool`, attaching `secrets`.
fn start(
    program: &str,
    tool: &str,
    namespace: &NamespaceName,
    secrets: &[(&str, &str)],
    rights: &RightsTo,
) -> StartRun {
    let mut request = StartRun::new(Script::new(program), action_set(), json!({ "tool": tool }));
    request.namespace = namespace.clone();
    request.metadata.secrets = attaching(secrets);
    request.attach = rights.rights();
    request
}

async fn world() -> World {
    World::new(programs(), RecordingDispatcher::echo()).await
}

async fn put(world: &World, id: SecretId, value: &str, labels: &[(&str, &str)]) -> PutSecret {
    let secret = NewSecret {
        id,
        value: Secret::new(value),
        labels: labels
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect(),
        by: "user:u_1".into(),
    };
    world
        .plane
        .secrets
        .put(&tenant(), &secret, world.now())
        .await
        .unwrap()
}

fn refused(error: StartError) -> RefusedAttachments {
    match error {
        StartError::SecretRequired(refused) => refused,
        other => panic!("expected secret-required, got {other:?}"),
    }
}

async fn runs(world: &mut World) -> usize {
    let mut gateway = world.driver.gateway(&world.at);
    world
        .edge
        .search(
            &mut gateway,
            &Scope::Tenant(tenant()),
            &RunQuery::new(tenant()),
            RunOrder::Started,
            &Page::first(100),
        )
        .await
        .unwrap()
        .summaries
        .len()
}

async fn output(world: &mut World, run: &RunRef) -> JsonValue {
    let history = world.history(run).await;
    match &history.last().unwrap().event {
        Event::Command(Command::CompleteRun { output }) => output.value().unwrap().clone(),
        other => panic!("the run has not completed: {other:?}"),
    }
}

async fn fork(world: &mut World, request: ForkRun) -> Result<Created, ForkError> {
    let mut gateway = world.driver.gateway(&world.at);
    world.edge.fork(&mut gateway, request).await
}

async fn start_workflow(world: &mut World, request: StartWorkflow) -> Result<Created, StartError> {
    let mut gateway = world.driver.gateway(&world.at);
    world.edge.start_workflow(&mut gateway, request).await
}

// -------------------------------------------------------------------------------------------
// The checks at start

#[tokio::test]
async fn a_start_without_the_secret_an_imported_server_needs_is_refused() {
    let mut world = world().await;
    put(&world, of(&alice(), "github"), "ghp_alice", &[]).await;
    put(&world, organization("zendesk"), "zd", &[]).await;
    put(&world, of(&bob(), "linear"), "lin_bob", &[]).await;

    let error = world
        .start(start(
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
    assert_eq!(runs(&mut world).await, 0, "no run exists");

    // Without secrets:use, the organization's secrets are not offered.
    let error = world
        .start(start(
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
    world
        .start(start("quiet", "", &alice(), &[], &RightsTo::Namespace))
        .await
        .unwrap();
}

#[tokio::test]
async fn an_attachment_names_a_server_taking_a_secret_and_a_secret_that_exists() {
    let mut world = world().await;
    // The organization's github exists, alice's does not: ns/github never falls back on it.
    put(&world, organization("github"), "ghp_org", &[]).await;
    let error = world
        .start(start(
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
    assert_eq!(runs(&mut world).await, 0, "no run exists");

    put(&world, of(&alice(), "github"), "ghp_alice", &[]).await;
    world
        .start(start(
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
    let mut world = world().await;
    put(&world, organization("prod"), "p", &[("env", "prod")]).await;
    put(&world, organization("dev"), "d", &[("env", "dev")]).await;
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

    let error = world
        .start(attach("org/prod", RightsTo::Namespace))
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
    let error = world
        .start(attach("org/dev", RightsTo::Organization(prod())))
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
    world
        .start(attach("org/prod", RightsTo::Organization(prod())))
        .await
        .unwrap();
    world
        .start(attach("org/dev", with_secrets()))
        .await
        .unwrap();
}

#[tokio::test]
async fn mock_mode_checks_no_attachment_and_reads_no_secret() {
    let mut world = world().await;
    let mut request = start(
        "caller",
        "github.get_issue",
        &alice(),
        &[],
        &RightsTo::Namespace,
    );
    request.mode = RunMode::Mock;
    let run = world.run(request).await;
    assert_eq!(output(&mut world, &run).await, json!({ "result": {} }));
    assert_eq!(world.plane.dispatcher.auth(), [CallAuth::None]);
}

// -------------------------------------------------------------------------------------------
// Reading at dispatch

#[tokio::test]
async fn a_call_reads_the_secret_its_run_attached_in_its_own_namespace() {
    let mut world = world().await;
    put(&world, of(&alice(), "github"), "ghp_alice", &[]).await;
    put(&world, of(&bob(), "github"), "ghp_bob", &[]).await;
    put(&world, organization("github"), "ghp_org", &[]).await;
    let alices = world
        .run(start(
            "caller",
            "github.get_issue",
            &alice(),
            &[("github", "ns/github")],
            &RightsTo::Namespace,
        ))
        .await;
    let bobs = world
        .run(start(
            "caller",
            "github.get_issue",
            &bob(),
            &[("github", "ns/github")],
            &RightsTo::Namespace,
        ))
        .await;

    let bearer = |value: &str| CallAuth::Secret {
        value: Secret::new(value),
        placement: SecretPlacement::bearer(),
    };
    assert_eq!(
        world.plane.dispatcher.auth(),
        [bearer("ghp_alice"), bearer("ghp_bob")],
        "each run's own namespace's, never the organization's"
    );
    let today = Day::of(world.now());
    let read = world
        .plane
        .secrets
        .meta(&tenant(), &of(&bob(), "github"), today)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(read.reads.iter().map(|day| day.reads).sum::<u64>(), 1);
    assert_eq!(
        read.last_read.unwrap().by,
        SecretReader::Run {
            run: bobs.id,
            namespace: bob(),
        }
    );
    let org = world
        .plane
        .secrets
        .meta(&tenant(), &organization("github"), today)
        .await
        .unwrap()
        .unwrap();
    assert!(org.last_read.is_none(), "nothing fell back on it");
    assert_eq!(output(&mut world, &alices).await, json!({ "result": {} }));
}

#[tokio::test]
async fn a_secret_a_server_echoes_never_reaches_the_history_nor_the_script() {
    let dispatcher = RecordingDispatcher::dispatching(|call, auth| {
        let CallAuth::Secret { value, .. } = auth else {
            panic!("the call carries its secret");
        };
        let value = value.expose();
        if call.attempt == 1 {
            // An error that quotes the key, retried.
            return Dispatched::Transient(ToolFailure {
                kind: ToolFailureKind::Unavailable,
                message: format!("invalid key {value}"),
                data: Some(json!({ "body": format!("token={value}") })),
            });
        }
        Dispatched::Result(json!({ "echo": format!("Bearer {value}"), value: [value] }))
    });
    let mut world = World::new(programs(), dispatcher).await;
    put(&world, of(&alice(), "github"), "ghp_alice_secret", &[]).await;
    let run = world
        .run(start(
            "caller",
            "github.get_issue",
            &alice(),
            &[("github", "ns/github")],
            &RightsTo::Namespace,
        ))
        .await;

    assert_eq!(
        output(&mut world, &run).await,
        json!({ "result": { "echo": "Bearer [secret]", "[secret]": ["[secret]"] } })
    );
    let history = format!("{:?}", world.history(&run).await);
    assert!(!history.contains("ghp_alice_secret"), "{history}");
}

#[tokio::test]
async fn a_secret_deleted_mid_run_parks_the_call_until_it_is_put_again() {
    let dispatcher = RecordingDispatcher::dispatching(|call, _| {
        Dispatched::Result(json!({ "key": call.idempotency_key }))
    });
    let mut world = World::new(programs(), dispatcher).await;
    world
        .plane
        .catalog
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
    put(&world, of(&alice(), "github"), "ghp_old", &[]).await;
    let run = world
        .start(start(
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
        world
            .plane
            .secrets
            .delete(&tenant(), &of(&alice(), "github"))
            .await
            .unwrap()
    );
    world.idle().await;

    assert!(
        world.plane.dispatcher.calls().is_empty(),
        "nothing was sent"
    );
    let tasks = world.tasks(&run).await;
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
    let deliveries = world.plane.webhooks.deliveries();
    let expired: Vec<_> = deliveries
        .iter()
        .filter_map(|(_, event)| match &event.kind {
            WebhookEventKind::CredentialsExpired { reason, secret, .. } => Some((reason, secret)),
            _ => None,
        })
        .collect();
    let [(reason, secret)] = expired.as_slice() else {
        panic!("one credentials.expired: {deliveries:?}");
    };
    assert_eq!(**reason, CredentialsExpiredReason::Missing);
    assert_eq!(secret.as_ref(), Some(&secret_ref("ns/github")));
    assert_eq!(world.view(&run).await.head.status, RunStatus::Active);

    // Bob's github put is not alice's: the call keeps waiting.
    put(&world, of(&bob(), "github"), "ghp_bob", &[]).await;
    world.idle().await;
    assert!(world.plane.dispatcher.calls().is_empty(), "still parked");
    put(&world, of(&alice(), "github"), "ghp_new", &[]).await;
    world.idle().await;
    assert_eq!(
        output(&mut world, &run).await,
        json!({ "result": { "key": format!("{}/1", run.id) } }),
        "sent again with the same idempotency key"
    );
    assert_eq!(
        world.plane.dispatcher.auth(),
        [CallAuth::Secret {
            value: Secret::new("ghp_new"),
            placement: SecretPlacement::bearer(),
        }]
    );
}

// -------------------------------------------------------------------------------------------
// Signed calls

/// A node that receives calls back at `https://rewind.example.com/`.
fn public() -> NodeConfig {
    let mut node = config("node-a", 1);
    node.public_url = Some("https://rewind.example.com/".into());
    node
}

#[tokio::test]
async fn a_signed_call_carries_the_tenant_s_key_and_the_claims_of_the_call() {
    let mut world = World::with(programs(), RecordingDispatcher::echo(), 1, &[public()]).await;
    let mut request = start("signer", "dax.ask", &alice(), &[], &RightsTo::Namespace);
    request.metadata.labels = BTreeMap::from([("env".to_owned(), "prod".to_owned())]);
    let run = world.run(request).await;

    let auth = world.plane.dispatcher.auth();
    let [CallAuth::Signed { key, claims }] = auth.as_slice() else {
        panic!("a signed call: {auth:?}");
    };
    let published = world
        .plane
        .secrets
        .public_keys(&tenant(), world.now())
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
    let mut world = world().await;
    let run = world
        .run(start(
            "signer",
            "dax.ask",
            &alice(),
            &[],
            &RightsTo::Namespace,
        ))
        .await;
    assert!(world.plane.dispatcher.calls().is_empty());
    let output = output(&mut world, &run).await;
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
    let mut world = world().await;
    put(&world, organization("github"), "ghp_org", &[]).await;
    put(&world, of(&alice(), "github"), "ghp_alice", &[]).await;
    let parent = world
        .run(start(
            "caller",
            "github.get_issue",
            &alice(),
            &[("github", "org/github")],
            &with_secrets(),
        ))
        .await;
    let request = |secrets: Option<&[(&str, &str)]>, rights: RightsTo| {
        let mut request = ForkRun::new(parent.clone(), Script::new("caller"));
        request.secrets = secrets.map(attaching);
        request.attach = rights.rights();
        request
    };

    // The script is new code: the parent's org/github is checked against the caller.
    let error = fork(&mut world, request(None, RightsTo::Namespace))
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

    let inherited = fork(&mut world, request(None, with_secrets()))
        .await
        .unwrap()
        .run;
    let head = world.view(&inherited).await.head;
    assert_eq!(
        head.metadata.secrets,
        attaching(&[("github", "org/github")])
    );
    assert_eq!(head.namespace, alice(), "a fork never leaves its namespace");

    let own = fork(
        &mut world,
        request(Some(&[("github", "ns/github")]), RightsTo::Namespace),
    )
    .await
    .unwrap()
    .run;
    assert_eq!(
        world.view(&own).await.head.metadata.secrets,
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
        action_set: action_set().digest(),
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

/// A world whose catalog holds the action set the versions are saved over.
async fn saving() -> World {
    let world = world().await;
    world
        .plane
        .catalog
        .put_action_set(&tenant(), &action_set(), &ToolDeclarations::default())
        .await
        .unwrap();
    world
}

async fn register(
    world: &World,
    request: RegisterWorkflow,
) -> Result<StoredWorkflow, RegisterError> {
    world
        .edge
        .register_workflow(request, &StaticTypeChecker::default(), &kit())
        .await
}

#[tokio::test]
async fn a_version_is_saved_with_attachments_its_saver_may_attach() {
    let world = saving().await;
    put(&world, organization("github"), "ghp_org", &[]).await;

    let saver_refused = |error: RegisterError| match error {
        RegisterError::SecretRequired(refused) => refused.problems,
        other => panic!("expected secret-required, got {other:?}"),
    };
    assert_eq!(
        saver_refused(
            register(&world, save(&[], &with_secrets()))
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
                &world,
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
            register(&world, save(&[("github", "org/gone")], &with_secrets()))
                .await
                .unwrap_err()
        ),
        [AttachmentProblem::Unknown {
            server: "github".into(),
            secret: secret_ref("org/gone"),
        }]
    );
    let org = register(&world, save(&[("github", "org/github")], &with_secrets()))
        .await
        .unwrap();
    assert_eq!(
        org.workflow.secrets(),
        &attaching(&[("github", "org/github")])
    );
    // An ns/ secret has no namespace yet: only its name is checked.
    let per_user = register(
        &world,
        save(&[("github", "ns/github")], &RightsTo::Namespace),
    )
    .await
    .unwrap();
    assert_ne!(per_user.workflow.version(), org.workflow.version());
}

#[tokio::test]
async fn a_run_of_a_version_checks_only_that_its_namespace_s_secrets_exist() {
    let mut world = saving().await;
    let version = register(
        &world,
        save(&[("github", "ns/github")], &RightsTo::Namespace),
    )
    .await
    .unwrap()
    .workflow
    .version();
    let request = |namespace: NamespaceName| StartWorkflow {
        tenant: tenant(),
        namespace,
        version,
        input: json!({ "tool": "github.get_issue" }),
        ttl: None,
        metadata: Default::default(),
        idempotency_key: None,
    };
    let error = start_workflow(&mut world, request(bob()))
        .await
        .unwrap_err();
    assert_eq!(
        refused(error).problems,
        [AttachmentProblem::Unknown {
            server: "github".into(),
            secret: secret_ref("ns/github"),
        }]
    );
    put(&world, of(&bob(), "github"), "ghp_bob", &[]).await;
    let run = start_workflow(&mut world, request(bob()))
        .await
        .unwrap()
        .run;
    assert_eq!(
        world.view(&run).await.head.metadata.secrets,
        attaching(&[("github", "ns/github")]),
        "the version's attachments"
    );
}
