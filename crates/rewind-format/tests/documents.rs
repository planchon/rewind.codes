//! The cassette, pinned action set, workflow version and test documents v1 are frozen like events:
//! these fixtures were written once and must decode forever, to the same domain values, and
//! re-encode to the same bytes.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::time::Duration;

use rewind_core::{
    Action, ActionAuth, ActionFailure, ActionFailureKind, ActionOutcome, ActionPolicy, ActionRoute,
    ActionRule, ActionSet, Api, ApiOperation, ApiParameter, Builtin, CallOrder, Cassette, Digest,
    Expectations, ExpectedCall, ExpectedError, HttpMethod, Matching, McpServer, McpTransport,
    ParameterLocation, RecordedCall, RecordedSignal, RetiredTest, RunStatus, SecretPlacement,
    SecretRef, Seed, SignalWhen, Suite, SuiteTest, Test, TestOrigin, TestSignal, Timestamp,
    UpdatedTest, Workflow, WorkflowSchemas, World,
};
use rewind_format::{
    FormatError, decode_action_set, decode_cassette, decode_test, decode_workflow,
    encode_action_set, encode_cassette, encode_test, encode_workflow, test_digest,
};
use serde_json::json;

const ACTION_SET: &str = include_str!("fixtures/action-set.v1.json");
const CASSETTE: &str = include_str!("fixtures/cassette.v1.json");

/// Every field and variant of the pinned action set.
fn action_set() -> ActionSet {
    let github = McpServer {
        name: "github".into(),
        transport: McpTransport::Http {
            url: "https://mcp.example.com/github".into(),
        },
        concurrency: 4,
        auth: ActionAuth::None,
    };
    let files = McpServer::new(
        "files",
        McpTransport::Stdio {
            command: "mcp-files".into(),
            args: vec!["--root".into(), "/srv".into()],
            env: BTreeMap::from([("LOG".to_owned(), "warn".to_owned())]),
        },
    );
    let mcp = |server: &str| ActionRoute::Mcp {
        server: server.into(),
    };
    let mut list_issues = Action::new(
        "github.list_issues",
        mcp("github"),
        json!({
            "type": "object",
            "properties": { "repo": { "type": "string" }, "state": { "enum": ["open", "closed"] } },
            "required": ["repo"],
        }),
    );
    list_issues.description = Some("List the issues of a repository.".into());
    list_issues.output_schema = Some(json!({ "type": "array" }));
    let mut close_issue = Action::new(
        "github.close_issue",
        mcp("github"),
        json!({ "type": "object" }),
    );
    close_issue.policy = ActionPolicy {
        idempotent: false,
        approval: true,
        execution_timeout: Some(Duration::from_secs(30)),
        reconnect_timeout: Duration::from_secs(20),
        concurrency: 2,
    };
    let read_file = Action::new("files.read", mcp("files"), json!({ "type": "object" }));
    let mut ask_user = Action::new(
        "harness.ask_user",
        ActionRoute::Harness {
            group: Some("harness".into()),
        },
        json!({ "type": "object", "properties": { "question": { "type": "string" } } }),
    );
    ask_user.policy.concurrency = 1;
    let notify = Action::new("notify", ActionRoute::Harness { group: None }, json!({}));
    let http = Action::new(
        "http",
        ActionRoute::Builtin(Builtin::Http {
            allowed_hosts: vec!["api.example.com".into(), "hooks.example.com".into()],
        }),
        json!({}),
    );
    let webhook = Action::new("webhook", ActionRoute::Builtin(Builtin::Webhook), json!({}));
    ActionSet::new(
        [github, files],
        [
            list_issues,
            close_issue,
            read_file,
            ask_user,
            notify,
            http,
            webhook,
        ],
    )
    .unwrap()
}

fn cassette() -> Cassette {
    Cassette {
        calls: vec![
            RecordedCall {
                action: "github.list_issues".into(),
                args: json!({ "state": "open", "repo": "acme/api" }),
                outcome: ActionOutcome::Result(json!([{ "number": 4, "title": "Crash" }])),
            },
            RecordedCall {
                action: "slack.post_message".into(),
                args: json!({ "channel": "#triage", "text": "1 stale issue" }),
                outcome: ActionOutcome::Failure(ActionFailure {
                    kind: ActionFailureKind::Unavailable,
                    message: "rate limited".into(),
                    data: Some(json!({ "retry_after": 30 })),
                }),
            },
            RecordedCall {
                action: "github.close_issue".into(),
                args: json!({ "number": 4 }),
                outcome: ActionOutcome::Failure(ActionFailure {
                    kind: ActionFailureKind::OutcomeUnknown,
                    message: "connection reset".into(),
                    data: None,
                }),
            },
        ],
        signals: vec![RecordedSignal {
            name: "approval".into(),
            payload: json!({ "close": true }),
            elapsed_ms: 3_600_000,
        }],
        matching: Matching::Sequential,
    }
}

#[test]
fn the_action_set_fixture_decodes_and_re_encodes_to_the_same_bytes() {
    let decoded = decode_action_set(ACTION_SET).unwrap();
    assert_eq!(decoded, action_set());
    assert_eq!(encode_action_set(&decoded), ACTION_SET);
}

/// The bytes of a pinned action set document are what its digest is computed on: a store can
/// check a stored action set against its key.
#[test]
fn an_action_set_document_hashes_to_the_action_set_digest() {
    assert_eq!(Digest::of(ACTION_SET.as_bytes()), action_set().digest());
    for action_set in [
        ActionSet::empty(),
        ActionSet::from_harness_actions(["a", "g.b"]).unwrap(),
        action_set(),
    ] {
        assert_eq!(
            Digest::of(encode_action_set(&action_set).as_bytes()),
            action_set.digest()
        );
        assert_eq!(
            decode_action_set(&encode_action_set(&action_set)).unwrap(),
            action_set
        );
    }
}

#[test]
fn an_action_set_document_may_list_actions_in_any_order() {
    let reordered = r#"{"v":1,"actions":[
        {"name":"b","route":{"type":"harness"},"schemas":{"input":{}},"policy":{"idempotent":true,"approval":false,"reconnect_timeout_ms":60000,"concurrency":16}},
        {"name":"a","route":{"type":"harness"},"schemas":{"input":{}},"policy":{"idempotent":true,"approval":false,"reconnect_timeout_ms":60000,"concurrency":16}}
    ],"servers":[]}"#;
    assert_eq!(
        decode_action_set(reordered).unwrap(),
        ActionSet::from_harness_actions(["a", "b"]).unwrap()
    );
}

#[test]
fn the_cassette_fixture_decodes_and_re_encodes_to_the_same_bytes() {
    let decoded = decode_cassette(CASSETTE).unwrap();
    assert_eq!(decoded, cassette());
    assert_eq!(encode_cassette(&decoded), CASSETTE);
    let empty = Cassette {
        calls: vec![],
        signals: vec![],
        matching: Matching::ByArgs,
    };
    assert_eq!(
        encode_cassette(&empty),
        "{\"v\":1,\"matching\":\"by_args\",\"calls\":[\n],\"signals\":[\n]}\n"
    );
    assert_eq!(decode_cassette(&encode_cassette(&empty)).unwrap(), empty);
}

#[test]
fn a_hand_written_cassette_decodes() {
    let written = r#"{
      "v": 1,
      "matching": "by_args",
      "calls": [
        { "action": "github.get_issue", "args": { "number": 4 }, "outcome": { "result": { "title": "Crash" } } },
        { "action": "slack.post", "args": {}, "outcome": { "error": { "kind": "error", "message": "no" } } }
      ],
      "signals": []
    }"#;
    let cassette = decode_cassette(written).unwrap();
    assert_eq!(cassette.matching, Matching::ByArgs);
    assert_eq!(cassette.calls.len(), 2);
    assert_eq!(
        cassette.calls[0].outcome,
        ActionOutcome::Result(json!({ "title": "Crash" }))
    );
}

#[test]
fn rejects_what_v1_does_not_define() {
    let policy = r#""policy":{"idempotent":true,"approval":false,"reconnect_timeout_ms":60000,"concurrency":16}"#;
    let action = |route: &str, extra: &str| {
        format!(
            r#"{{"v":1,"servers":[],"actions":[{{"name":"webhook","route":{route},"schemas":{{"input":{{}}}},{policy}{extra}}}]}}"#
        )
    };
    assert!(decode_action_set(&action(r#"{"type":"builtin","builtin":"webhook"}"#, "")).is_ok());

    let invalid_action_sets = [
        action(r#"{"type":"builtin","builtin":"webhook"}"#, r#","extra":1"#),
        action(r#"{"type":"builtin","builtin":"ftp"}"#, ""),
        action(r#"{"type":"teleport"}"#, ""),
        action(r#"{"type":"builtin","builtin":"webhook","allowed_hosts":["a"]}"#, ""),
        action(r#"{"type":"builtin","builtin":"http"}"#, ""),
        action(r#"{"type":"harness","group":"g"}"#, ""),
        action(r#"{"type":"mcp","server":"github"}"#, ""),
        r#"{"v":1,"servers":[],"actions":[],"extra":[]}"#.to_owned(),
        r#"{"v":1,"servers":[]}"#.to_owned(),
        r#"{"v":1,"servers":[{"name":"s","transport":{"type":"ws","url":"x"},"concurrency":1}],"actions":[]}"#.to_owned(),
        r#"{"v":1,"servers":[{"name":"s","transport":{"type":"http","url":"x"},"concurrency":0}],"actions":[]}"#.to_owned(),
    ];
    for json in &invalid_action_sets {
        assert!(
            matches!(
                decode_action_set(json),
                Err(FormatError::InvalidActionSet(_))
            ),
            "{json} must be rejected, got {:?}",
            decode_action_set(json)
        );
    }
    assert_eq!(
        decode_action_set(r#"{"v":2,"servers":[],"actions":[]}"#),
        Err(FormatError::UnsupportedVersion(json!(2)))
    );
    assert_eq!(
        decode_action_set(r#"{"servers":[],"actions":[]}"#),
        Err(FormatError::MissingVersion)
    );

    let invalid_cassettes = [
        r#"{"v":1,"matching":"sequential","calls":[],"signals":[],"extra":1}"#,
        r#"{"v":1,"matching":"random","calls":[],"signals":[]}"#,
        r#"{"v":1,"calls":[],"signals":[]}"#,
        r#"{"v":1,"matching":"sequential","calls":[{"action":"t","args":{},"outcome":{"result":1,"error":{"kind":"error","message":"m"}}}],"signals":[]}"#,
        r#"{"v":1,"matching":"sequential","calls":[{"action":"t","args":{}}],"signals":[]}"#,
        r#"{"v":1,"matching":"sequential","calls":[{"action":"t","args":{},"outcome":{"error":{"kind":"bad","message":"m"}}}],"signals":[]}"#,
        r#"{"v":1,"matching":"sequential","calls":[],"signals":[{"name":"s","payload":1}]}"#,
    ];
    for json in invalid_cassettes {
        assert!(
            matches!(decode_cassette(json), Err(FormatError::InvalidCassette(_))),
            "{json} must be rejected, got {:?}",
            decode_cassette(json)
        );
    }
    assert_eq!(
        decode_cassette(r#"{"v":3,"matching":"sequential","calls":[],"signals":[]}"#),
        Err(FormatError::UnsupportedVersion(json!(3)))
    );
}

/// Prints the fixtures; used once to write them.
#[test]
#[ignore = "writes nothing: prints the fixtures for a human to review"]
fn print_fixtures() {
    println!("{}", encode_action_set(&action_set()));
    print!("{}", encode_cassette(&cassette()));
}

const WORKFLOW: &str = include_str!("fixtures/workflow.v1.json");
const WORKFLOW_WITH_LABELS: &str = include_str!("fixtures/workflow-labels.v1.json");
const ACTION_SET_WITH_WORKFLOWS: &str = include_str!("fixtures/action-set-workflows.v1.json");

/// Every field of a workflow version.
fn workflow() -> Workflow {
    Workflow::new(
        "triage",
        Some("Triage the stale issues of a repository, after approval.".into()),
        digest_of("script"),
        action_set().digest(),
        WorkflowSchemas {
            input: json!({
                "type": "object",
                "properties": { "repo": { "type": "string" } },
                "required": ["repo"],
            }),
            output: json!({ "type": "object", "properties": { "closed": { "type": "integer" } } }),
            signals: BTreeMap::from([
                (
                    "approval".to_owned(),
                    json!({ "type": "object", "properties": { "close": { "type": "boolean" } } }),
                ),
                ("pr merged".to_owned(), json!(true)),
            ]),
        },
    )
    .unwrap()
}

fn digest_of(text: &str) -> Digest {
    Digest::of(text.as_bytes())
}

/// An action set pinning two workflows, one without a description or signals.
fn action_set_with_workflows() -> ActionSet {
    let digest = Workflow::new(
        "digest",
        None,
        digest_of("digest"),
        ActionSet::empty().digest(),
        WorkflowSchemas {
            input: json!({}),
            output: json!({ "type": "string" }),
            signals: BTreeMap::new(),
        },
    )
    .unwrap();
    ActionSet::from_harness_actions(["notify"])
        .unwrap()
        .with_workflows([workflow().pinned(), digest.pinned()])
        .unwrap()
}

#[test]
fn the_workflow_fixture_decodes_re_encodes_and_hashes_to_its_version() {
    let decoded = decode_workflow(WORKFLOW).unwrap();
    assert_eq!(decoded, workflow());
    assert_eq!(encode_workflow(&decoded), WORKFLOW);
    assert_eq!(Digest::of(WORKFLOW.as_bytes()), workflow().version());
}

/// A version saved under a grant limited to `env=dev`: its labels are part of its document, so
/// it is another version than the same workflow saved tenant-wide.
#[test]
fn the_workflow_with_labels_fixture_decodes_re_encodes_and_hashes_to_its_version() {
    let labelled = workflow().with_labels(BTreeMap::from([("env".to_owned(), "dev".to_owned())]));
    let decoded = decode_workflow(WORKFLOW_WITH_LABELS).unwrap();
    assert_eq!(decoded, labelled);
    assert_eq!(encode_workflow(&decoded), WORKFLOW_WITH_LABELS);
    assert_eq!(
        Digest::of(WORKFLOW_WITH_LABELS.as_bytes()),
        labelled.version()
    );
    assert_ne!(labelled.version(), workflow().version());
    assert!(
        decode_workflow(&WORKFLOW_WITH_LABELS.replace(r#""dev""#, "1")).is_err(),
        "a label's value is a string"
    );
}

#[test]
fn the_action_set_with_workflows_fixture_decodes_and_hashes_to_its_digest() {
    let decoded = decode_action_set(ACTION_SET_WITH_WORKFLOWS).unwrap();
    assert_eq!(decoded, action_set_with_workflows());
    assert_eq!(encode_action_set(&decoded), ACTION_SET_WITH_WORKFLOWS);
    assert_eq!(
        Digest::of(ACTION_SET_WITH_WORKFLOWS.as_bytes()),
        action_set_with_workflows().digest()
    );
    assert!(decoded.contains("workflows.triage.signal"));
}

const ACTION_SET_WITH_APIS: &str = include_str!("fixtures/action-set-apis.v1.json");
const WORKFLOW_WITH_SECRETS: &str = include_str!("fixtures/workflow-secrets.v1.json");

/// Every kind of `auth`, an API with every place of a parameter, and an API action.
fn action_set_with_apis() -> ActionSet {
    let mut github = McpServer::new(
        "github",
        McpTransport::Http {
            url: "https://mcp.example.com/github".into(),
        },
    );
    github.auth = ActionAuth::Secret(SecretPlacement::bearer());
    let mut crm = McpServer::new(
        "crm",
        McpTransport::Http {
            url: "https://crm.example.com/mcp".into(),
        },
    );
    crm.auth = ActionAuth::Rewind;
    let mut files = McpServer::new(
        "files",
        McpTransport::Stdio {
            command: "mcp-files".into(),
            args: vec![],
            env: BTreeMap::new(),
        },
    );
    files.auth = ActionAuth::Secret(SecretPlacement::Env {
        variable: "FILES_TOKEN".into(),
    });
    let mut dax = Api::new("dax", "https://api.dax.example.com/v1");
    dax.auth = ActionAuth::Rewind;
    dax.concurrency = 4;
    let mut zendesk = Api::new("zendesk", "https://acme.zendesk.example.com/api");
    zendesk.auth = ActionAuth::Secret(SecretPlacement::basic());
    let mut keyed = Api::new("weather", "https://weather.example.com");
    keyed.auth = ActionAuth::Secret(SecretPlacement::header("X-Api-Key"));
    let parameter = |name: &str, location| ApiParameter {
        name: name.into(),
        location,
    };
    let mut create_measure = Action::api(
        "dax.createMeasure",
        "dax",
        ApiOperation {
            method: HttpMethod::Post,
            path: "/projects/{project}/measures".into(),
            parameters: vec![
                parameter("project", ParameterLocation::Path),
                parameter("dryRun", ParameterLocation::Query),
                parameter("X-Request-Source", ParameterLocation::Header),
            ],
            body: true,
            idempotency_key: true,
        },
        json!({ "type": "object", "required": ["project", "body"] }),
    );
    create_measure.description = Some("Create a measure.".into());
    create_measure.output_schema = Some(json!({ "type": "object" }));
    let get_ticket = Action::api(
        "zendesk.get_ticket",
        "zendesk",
        ApiOperation {
            method: HttpMethod::Get,
            path: "/tickets/{id}".into(),
            parameters: vec![parameter("id", ParameterLocation::Path)],
            body: false,
            idempotency_key: false,
        },
        json!({ "type": "object" }),
    );
    let mcp = |server: &str, action: &str| {
        Action::new(
            format!("{server}.{action}"),
            ActionRoute::Mcp {
                server: server.into(),
            },
            json!({}),
        )
    };
    ActionSet::from_parts(
        [github, crm, files],
        [dax, zendesk, keyed],
        [
            mcp("github", "list_issues"),
            mcp("crm", "find_contact"),
            mcp("files", "read"),
            create_measure,
            get_ticket,
        ],
    )
    .unwrap()
}

#[test]
fn the_action_set_with_apis_fixture_decodes_and_hashes_to_its_digest() {
    let decoded = decode_action_set(ACTION_SET_WITH_APIS).unwrap();
    assert_eq!(decoded, action_set_with_apis());
    assert_eq!(encode_action_set(&decoded), ACTION_SET_WITH_APIS);
    assert_eq!(
        Digest::of(ACTION_SET_WITH_APIS.as_bytes()),
        action_set_with_apis().digest()
    );
}

/// A version that attaches an end user's secret and an organization's.
fn workflow_with_secrets() -> Workflow {
    workflow().with_secrets(
        [
            ("github".to_owned(), SecretRef::parse("ns/github").unwrap()),
            (
                "zendesk".to_owned(),
                SecretRef::parse("org/zendesk").unwrap(),
            ),
        ]
        .into_iter()
        .collect(),
    )
}

#[test]
fn the_workflow_with_secrets_fixture_decodes_re_encodes_and_hashes_to_its_version() {
    let decoded = decode_workflow(WORKFLOW_WITH_SECRETS).unwrap();
    assert_eq!(decoded, workflow_with_secrets());
    assert_eq!(encode_workflow(&decoded), WORKFLOW_WITH_SECRETS);
    assert_eq!(
        Digest::of(WORKFLOW_WITH_SECRETS.as_bytes()),
        workflow_with_secrets().version()
    );
    assert_ne!(workflow_with_secrets().version(), workflow().version());
    for invalid in [r#""ns/github""#, r#""org/zendesk""#] {
        for replacement in [r#""github""#, r#""user/x""#, r#""ns/GitHub""#, "1"] {
            assert!(
                decode_workflow(&WORKFLOW_WITH_SECRETS.replace(invalid, replacement)).is_err(),
                "{replacement}"
            );
        }
    }
}

#[test]
fn rejects_auth_and_apis_v1_does_not_define() {
    let server = |auth: &str| {
        format!(
            r#"{{"v":1,"servers":[{{"name":"s","transport":{{"type":"http","url":"https://x"}},"concurrency":1,"auth":{auth}}}],"actions":[]}}"#
        )
    };
    assert!(decode_action_set(&server(r#"{"type":"rewind"}"#)).is_ok());
    assert!(decode_action_set(&server(r#"{"type":"secret","header":"X-Key"}"#)).is_ok());
    let api = |fields: &str| {
        format!(
            r#"{{"v":1,"servers":[],"apis":[{{"name":"a","concurrency":1,{fields}}}],"actions":[]}}"#
        )
    };
    assert!(decode_action_set(&api(r#""base_url":"https://x""#)).is_ok());
    let invalid = [
        server(r#"{"type":"none"}"#),
        server(r#"{"type":"rewind","header":"X-Key"}"#),
        server(r#"{"type":"secret"}"#),
        server(r#"{"type":"secret","header":"X-Key","env":"KEY"}"#),
        server(r#"{"type":"secret","env":"KEY","format":"{secret}"}"#),
        server(r#"{"type":"secret","header":"X-Key","encoding":"rot13"}"#),
        server(r#"{"type":"secret","env":"KEY"}"#),
        api(r#""baseUrl":"https://x""#),
        api(r#""base_url":"https://x","extra":1"#),
        api(r#""base_url":"x""#),
        r#"{"v":1,"servers":[],"actions":[{"name":"a.b","route":{"type":"api","api":"a"},"schemas":{"input":{}},"policy":{"idempotent":true,"approval":false,"reconnect_timeout_ms":60000,"concurrency":16}}]}"#.to_owned(),
    ];
    for json in &invalid {
        assert!(
            matches!(
                decode_action_set(json),
                Err(FormatError::InvalidActionSet(_))
            ),
            "{json} must be rejected, got {:?}",
            decode_action_set(json)
        );
    }
}

/// Prints the fixtures of APIs and attachments; used once to write them.
#[test]
#[ignore = "writes nothing: prints the fixtures for a human to review"]
fn print_secrets_fixtures() {
    println!("{}", encode_action_set(&action_set_with_apis()));
    println!("{}", encode_workflow(&workflow_with_secrets()));
}

#[test]
fn rejects_workflows_v1_does_not_define() {
    let schemas = r#""schemas":{"input":{},"output":{},"signals":{}}"#;
    let digest = "0".repeat(64);
    let document = |fields: &str| format!(r#"{{"v":1,{fields}}}"#);
    assert!(
        decode_workflow(&document(&format!(
            r#""name":"triage","code":"{digest}","action_set":"{digest}",{schemas}"#
        )))
        .is_ok()
    );
    let invalid = [
        document(&format!(
            r#""name":"triage","code":"{digest}","action_set":"{digest}",{schemas},"x":1"#
        )),
        document(&format!(r#""name":"triage","code":"{digest}",{schemas}"#)),
        document(&format!(
            r#""name":"triage","code":"abc","action_set":"{digest}",{schemas}"#
        )),
        document(&format!(
            r#""name":"default","code":"{digest}","action_set":"{digest}",{schemas}"#
        )),
        document(&format!(
            r#""name":"triage","code":"{digest}","action_set":"{digest}","schemas":{{"input":{{}},"output":{{}},"signals":{{"rewind:x":{{}}}}}}"#
        )),
        document(&format!(
            r#""name":"triage","code":"{digest}","action_set":"{digest}","schemas":{{"input":{{}},"output":{{}}}}"#
        )),
    ];
    for json in &invalid {
        assert!(
            matches!(decode_workflow(json), Err(FormatError::InvalidWorkflow(_))),
            "{json} must be rejected, got {:?}",
            decode_workflow(json)
        );
    }
    assert_eq!(
        decode_workflow(r#"{"v":2}"#),
        Err(FormatError::UnsupportedVersion(json!(2)))
    );
    let pinned_twice = format!(
        r#"{{"v":1,"servers":[],"actions":[],"workflows":[{w},{w}]}}"#,
        w = format_args!(r#"{{"name":"a","version":"{digest}",{schemas}}}"#)
    );
    assert!(matches!(
        decode_action_set(&pinned_twice),
        Err(FormatError::InvalidActionSet(_))
    ));
}

// -------------------------------------------------------------------------------------------
// Suites and tests

const WORKFLOW_WITH_SUITE: &str = include_str!("fixtures/workflow-suite.v1.json");
const TEST: &str = include_str!("fixtures/test.v1.json");

/// A version with every field of a suite.
fn workflow_with_suite() -> Workflow {
    workflow()
        .with_suite(Suite {
            parent: Some(digest_of("parent")),
            tests: vec![
                SuiteTest {
                    name: "closes once approved".into(),
                    test: digest_of("a"),
                },
                SuiteTest {
                    name: "closes nothing without approval".into(),
                    test: digest_of("b2"),
                },
            ],
            retired: vec![RetiredTest {
                name: "asks before closing".into(),
                test: digest_of("c"),
                reason: "under 5 stale issues, it closes without asking".into(),
            }],
            updated: vec![UpdatedTest {
                name: "closes nothing without approval".into(),
                from: digest_of("b"),
                to: digest_of("b2"),
                reason: "the message names the repository".into(),
            }],
        })
        .unwrap()
}

/// Every field and variant of a test.
fn test_document() -> Test {
    let mut actions = BTreeMap::new();
    actions.insert(
        "github.list_issues".to_owned(),
        vec![
            ActionRule {
                args: Some(json!({ "state": "open" })),
                answer: ActionOutcome::Result(json!([{ "number": 4 }])),
                times: Some(1),
            },
            ActionRule {
                args: None,
                answer: ActionOutcome::Result(json!(null)),
                times: None,
            },
        ],
    );
    actions.insert(
        "slack.post_message".to_owned(),
        vec![ActionRule {
            args: None,
            answer: ActionOutcome::Failure(ActionFailure {
                kind: ActionFailureKind::Unavailable,
                message: "rate limited".into(),
                data: Some(json!({ "retryAfter": 30 })),
            }),
            times: None,
        }],
    );
    actions.insert(
        "workflows.notify.start".to_owned(),
        vec![ActionRule::always(ActionOutcome::Failure(ActionFailure {
            kind: ActionFailureKind::Error,
            message: "the child failed".into(),
            data: None,
        }))],
    );
    Test {
        name: "closes once approved".into(),
        input: json!({ "repo": "acme/api" }),
        start: Some(Timestamp(1_767_225_600_000)),
        seed: Some(Seed::from_bytes([7; 32])),
        world: World {
            actions,
            handlers: Some("export default {};\n".into()),
            cassette: Some(Cassette {
                calls: vec![RecordedCall {
                    action: "github.close_issue".into(),
                    args: json!({ "number": 4 }),
                    outcome: ActionOutcome::Result(json!({})),
                }],
                signals: vec![RecordedSignal {
                    name: "approval".into(),
                    payload: json!({ "close": true }),
                    elapsed_ms: 3_600_000,
                }],
                matching: Matching::ByArgs,
            }),
        },
        signals: vec![
            TestSignal {
                name: "approval".into(),
                payload: json!({ "close": true }),
                when: SignalWhen::Blocked,
            },
            TestSignal {
                name: "pr merged".into(),
                payload: json!(null),
                when: SignalWhen::After(Duration::from_secs(7_200)),
            },
        ],
        expect: Expectations {
            status: Some(RunStatus::Completed),
            output: Some(json!({ "closed": 1 })),
            error: Some(ExpectedError {
                name: "ActionError".into(),
                message: Some("rate limited".into()),
            }),
            calls: vec![
                ExpectedCall {
                    action: "slack.post_message".into(),
                    args: Some(json!({ "channel": "#triage" })),
                    times: Some(1),
                },
                ExpectedCall {
                    action: "github.delete_repo".into(),
                    args: None,
                    times: Some(0),
                },
                ExpectedCall {
                    action: "github.close_issue".into(),
                    args: None,
                    times: None,
                },
            ],
            order: CallOrder::Listed,
        },
        origin: Some(TestOrigin {
            run: "0193a5c4-7d2e-7c1a-9b1e-3f6a2d9c8e10".parse().unwrap(),
            version: digest_of("version"),
            namespace: "alice".into(),
        }),
    }
}

#[test]
fn a_version_without_a_suite_keeps_its_digest() {
    // The fixture of before suites existed decodes to a version with an empty suite.
    let decoded = decode_workflow(WORKFLOW).unwrap();
    assert!(decoded.suite().is_empty());
    assert_eq!(decoded.version(), Digest::of(WORKFLOW.as_bytes()));
}

#[test]
fn the_workflow_suite_fixture_decodes_re_encodes_and_hashes_to_its_version() {
    let decoded = decode_workflow(WORKFLOW_WITH_SUITE).unwrap();
    assert_eq!(decoded, workflow_with_suite());
    assert_eq!(encode_workflow(&decoded), WORKFLOW_WITH_SUITE);
    assert_eq!(
        Digest::of(WORKFLOW_WITH_SUITE.as_bytes()),
        workflow_with_suite().version()
    );
    assert_ne!(workflow_with_suite().version(), workflow().version());
}

#[test]
fn the_test_fixture_decodes_re_encodes_and_hashes_to_its_digest() {
    let decoded = decode_test(TEST).unwrap();
    assert_eq!(decoded, test_document());
    assert_eq!(encode_test(&decoded), TEST);
    assert_eq!(test_digest(&decoded), Digest::of(TEST.as_bytes()));
}

#[test]
fn a_test_is_written_one_way() {
    // A single rule without its array, the default `when`, `order` and input: the same test,
    // written back in one way.
    let given = r#"{"v":1,"name":"t","input":{},"world":{"actions":{"a.b":{"returns":1}}},
        "signals":[{"name":"go","when":"blocked"}],"expect":{"order":"any"}}"#;
    let decoded = decode_test(given).unwrap();
    assert_eq!(
        encode_test(&decoded),
        r#"{"name":"t","signals":[{"name":"go"}],"v":1,"world":{"actions":{"a.b":[{"returns":1}]}}}"#
    );
    // A result of `null` is a result.
    let null =
        decode_test(r#"{"v":1,"name":"t","world":{"actions":{"a.b":{"returns":null}}}}"#).unwrap();
    assert_eq!(
        null.world.actions["a.b"][0].answer,
        ActionOutcome::Result(json!(null))
    );
}

#[test]
fn rejects_tests_v1_does_not_define() {
    let invalid = [
        r#"{"name":"t"}"#,
        r#"{"v":1}"#,
        r#"{"v":1,"name":"t","x":1}"#,
        r#"{"v":1,"name":""}"#,
        r#"{"v":1,"name":"t","world":{"actions":{"a.b":{}}}}"#,
        r#"{"v":1,"name":"t","world":{"actions":{"a.b":{"returns":1,"error":{"message":"m"}}}}}"#,
        r#"{"v":1,"name":"t","world":{"actions":{"a.b":{"returns":1,"times":0}}}}"#,
        r#"{"v":1,"name":"t","world":{"cassette":{"v":1,"matching":"by_args","calls":[],"signals":[]}}}"#,
        r#"{"v":1,"name":"t","signals":[{"name":"rewind:ended:x"}]}"#,
        r#"{"v":1,"name":"t","signals":[{"name":"go","when":"soon"}]}"#,
        r#"{"v":1,"name":"t","seed":"ABC"}"#,
        r#"{"v":1,"name":"t","expect":{"status":"waiting"}}"#,
        r#"{"v":1,"name":"t","expect":{"order":"sorted"}}"#,
    ];
    for document in invalid {
        assert!(
            decode_test(document).is_err(),
            "{document} should be refused"
        );
    }
    assert!(matches!(
        decode_test(r#"{"v":2,"name":"t"}"#),
        Err(FormatError::UnsupportedVersion(_))
    ));
}

#[test]
fn rejects_suites_v1_does_not_define() {
    let digest = "0".repeat(64);
    let base = format!(
        r#""name":"triage","code":"{digest}","action_set":"{digest}","schemas":{{"input":{{}},"output":{{}},"signals":{{}}}}"#
    );
    let document = |fields: &str| format!(r#"{{"v":1,{base},{fields}}}"#);
    assert!(
        decode_workflow(&document(&format!(
            r#""tests":[{{"name":"a","test":"{digest}"}}]"#
        )))
        .is_ok()
    );
    let invalid = [
        document(&format!(
            r#""tests":[{{"name":"a","test":"{digest}"}},{{"name":"a","test":"{digest}"}}]"#
        )),
        document(&format!(
            r#""retired":[{{"name":"a","test":"{digest}","reason":" "}}]"#
        )),
        document(r#""parent":"abc""#),
        document(&format!(
            r#""tests":[{{"name":"a","test":"{digest}","x":1}}]"#
        )),
    ];
    for document in invalid {
        assert!(
            decode_workflow(&document).is_err(),
            "{document} should be refused"
        );
    }
}
