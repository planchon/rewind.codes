//! The cassette, pinned toolset, workflow version and test documents v1 are frozen like events: these
//! fixtures were written once and must decode forever, to the same domain values, and re-encode
//! to the same bytes.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::time::Duration;

use rewind_core::{
    Builtin, CallOrder, Cassette, Digest, Expectations, ExpectedCall, ExpectedError, Interruption,
    Matching, McpServer, McpTransport, RecordedCall, RecordedSignal, RetiredTest, RunStatus, Seed,
    SignalWhen, Suite, SuiteTest, Test, TestOrigin, TestSignal, Timestamp, Tool, ToolFailure,
    ToolFailureKind, ToolOutcome, ToolPolicy, ToolRoute, ToolRule, Toolset, UpdatedTest, Workflow,
    WorkflowSchemas, World,
};
use rewind_format::{
    FormatError, decode_cassette, decode_test, decode_toolset, decode_workflow, encode_cassette,
    encode_test, encode_toolset, encode_workflow, test_digest,
};
use serde_json::json;

const TOOLSET: &str = include_str!("fixtures/toolset.v1.json");
const CASSETTE: &str = include_str!("fixtures/cassette.v1.json");

/// Every field and variant of the pinned toolset.
fn toolset() -> Toolset {
    let github = McpServer {
        name: "github".into(),
        transport: McpTransport::Http {
            url: "https://mcp.example.com/github".into(),
        },
        concurrency: 4,
    };
    let files = McpServer::new(
        "files",
        McpTransport::Stdio {
            command: "mcp-files".into(),
            args: vec!["--root".into(), "/srv".into()],
            env: BTreeMap::from([("LOG".to_owned(), "warn".to_owned())]),
        },
    );
    let mcp = |server: &str| ToolRoute::Mcp {
        server: server.into(),
    };
    let mut list_issues = Tool::new(
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
    let mut close_issue = Tool::new(
        "github.close_issue",
        mcp("github"),
        json!({ "type": "object" }),
    );
    close_issue.policy = ToolPolicy {
        on_interruption: Interruption::AtMostOnce,
        approval: true,
        execution_timeout: Some(Duration::from_secs(30)),
        heartbeat_timeout: Duration::from_secs(20),
        concurrency: 2,
    };
    let read_file = Tool::new("files.read", mcp("files"), json!({ "type": "object" }));
    let mut ask_user = Tool::new(
        "harness.ask_user",
        ToolRoute::Host {
            group: Some("harness".into()),
        },
        json!({ "type": "object", "properties": { "question": { "type": "string" } } }),
    );
    ask_user.policy.concurrency = 1;
    let notify = Tool::new("notify", ToolRoute::Host { group: None }, json!({}));
    let http = Tool::new(
        "http",
        ToolRoute::Builtin(Builtin::Http {
            allowed_hosts: vec!["api.example.com".into(), "hooks.example.com".into()],
        }),
        json!({}),
    );
    let webhook = Tool::new("webhook", ToolRoute::Builtin(Builtin::Webhook), json!({}));
    Toolset::new(
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
                tool: "github.list_issues".into(),
                args: json!({ "state": "open", "repo": "acme/api" }),
                outcome: ToolOutcome::Result(json!([{ "number": 4, "title": "Crash" }])),
            },
            RecordedCall {
                tool: "slack.post_message".into(),
                args: json!({ "channel": "#triage", "text": "1 stale issue" }),
                outcome: ToolOutcome::Failure(ToolFailure {
                    kind: ToolFailureKind::Unavailable,
                    message: "rate limited".into(),
                    data: Some(json!({ "retry_after": 30 })),
                }),
            },
            RecordedCall {
                tool: "github.close_issue".into(),
                args: json!({ "number": 4 }),
                outcome: ToolOutcome::Failure(ToolFailure {
                    kind: ToolFailureKind::OutcomeUnknown,
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
fn the_toolset_fixture_decodes_and_re_encodes_to_the_same_bytes() {
    let decoded = decode_toolset(TOOLSET).unwrap();
    assert_eq!(decoded, toolset());
    assert_eq!(encode_toolset(&decoded), TOOLSET);
}

/// The bytes of a pinned toolset document are what its digest is computed on: a store can
/// check a stored toolset against its key.
#[test]
fn a_toolset_document_hashes_to_the_toolset_digest() {
    assert_eq!(Digest::of(TOOLSET.as_bytes()), toolset().digest());
    for toolset in [
        Toolset::empty(),
        Toolset::from_host_tools(["a", "g.b"]).unwrap(),
        toolset(),
    ] {
        assert_eq!(
            Digest::of(encode_toolset(&toolset).as_bytes()),
            toolset.digest()
        );
        assert_eq!(decode_toolset(&encode_toolset(&toolset)).unwrap(), toolset);
    }
}

#[test]
fn a_toolset_document_may_list_tools_in_any_order() {
    let reordered = r#"{"v":1,"tools":[
        {"name":"b","route":{"type":"host"},"schemas":{"input":{}},"policy":{"on_interruption":"retry","approval":false,"heartbeat_timeout_ms":60000,"concurrency":16}},
        {"name":"a","route":{"type":"host"},"schemas":{"input":{}},"policy":{"on_interruption":"retry","approval":false,"heartbeat_timeout_ms":60000,"concurrency":16}}
    ],"servers":[]}"#;
    assert_eq!(
        decode_toolset(reordered).unwrap(),
        Toolset::from_host_tools(["a", "b"]).unwrap()
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
        { "tool": "github.get_issue", "args": { "number": 4 }, "outcome": { "result": { "title": "Crash" } } },
        { "tool": "slack.post", "args": {}, "outcome": { "error": { "kind": "error", "message": "no" } } }
      ],
      "signals": []
    }"#;
    let cassette = decode_cassette(written).unwrap();
    assert_eq!(cassette.matching, Matching::ByArgs);
    assert_eq!(cassette.calls.len(), 2);
    assert_eq!(
        cassette.calls[0].outcome,
        ToolOutcome::Result(json!({ "title": "Crash" }))
    );
}

#[test]
fn rejects_what_v1_does_not_define() {
    let policy = r#""policy":{"on_interruption":"retry","approval":false,"heartbeat_timeout_ms":60000,"concurrency":16}"#;
    let tool = |route: &str, extra: &str| {
        format!(
            r#"{{"v":1,"servers":[],"tools":[{{"name":"webhook","route":{route},"schemas":{{"input":{{}}}},{policy}{extra}}}]}}"#
        )
    };
    assert!(decode_toolset(&tool(r#"{"type":"builtin","builtin":"webhook"}"#, "")).is_ok());

    let invalid_toolsets = [
        tool(r#"{"type":"builtin","builtin":"webhook"}"#, r#","extra":1"#),
        tool(r#"{"type":"builtin","builtin":"ftp"}"#, ""),
        tool(r#"{"type":"teleport"}"#, ""),
        tool(r#"{"type":"builtin","builtin":"webhook","allowed_hosts":["a"]}"#, ""),
        tool(r#"{"type":"builtin","builtin":"http"}"#, ""),
        tool(r#"{"type":"host","group":"g"}"#, ""),
        tool(r#"{"type":"mcp","server":"github"}"#, ""),
        r#"{"v":1,"servers":[],"tools":[],"extra":[]}"#.to_owned(),
        r#"{"v":1,"servers":[]}"#.to_owned(),
        r#"{"v":1,"servers":[{"name":"s","transport":{"type":"ws","url":"x"},"concurrency":1}],"tools":[]}"#.to_owned(),
        r#"{"v":1,"servers":[{"name":"s","transport":{"type":"http","url":"x"},"concurrency":0}],"tools":[]}"#.to_owned(),
    ];
    for json in &invalid_toolsets {
        assert!(
            matches!(decode_toolset(json), Err(FormatError::InvalidToolset(_))),
            "{json} must be rejected, got {:?}",
            decode_toolset(json)
        );
    }
    assert_eq!(
        decode_toolset(r#"{"v":2,"servers":[],"tools":[]}"#),
        Err(FormatError::UnsupportedVersion(json!(2)))
    );
    assert_eq!(
        decode_toolset(r#"{"servers":[],"tools":[]}"#),
        Err(FormatError::MissingVersion)
    );

    let invalid_cassettes = [
        r#"{"v":1,"matching":"sequential","calls":[],"signals":[],"extra":1}"#,
        r#"{"v":1,"matching":"random","calls":[],"signals":[]}"#,
        r#"{"v":1,"calls":[],"signals":[]}"#,
        r#"{"v":1,"matching":"sequential","calls":[{"tool":"t","args":{},"outcome":{"result":1,"error":{"kind":"error","message":"m"}}}],"signals":[]}"#,
        r#"{"v":1,"matching":"sequential","calls":[{"tool":"t","args":{}}],"signals":[]}"#,
        r#"{"v":1,"matching":"sequential","calls":[{"tool":"t","args":{},"outcome":{"error":{"kind":"bad","message":"m"}}}],"signals":[]}"#,
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
    println!("{}", encode_toolset(&toolset()));
    print!("{}", encode_cassette(&cassette()));
}

const WORKFLOW: &str = include_str!("fixtures/workflow.v1.json");
const TOOLSET_WITH_WORKFLOWS: &str = include_str!("fixtures/toolset-workflows.v1.json");

/// Every field of a workflow version.
fn workflow() -> Workflow {
    Workflow::new(
        "triage",
        Some("Triage the stale issues of a repository, after approval.".into()),
        digest_of("script"),
        toolset().digest(),
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

/// A toolset pinning two workflows, one without a description or signals.
fn toolset_with_workflows() -> Toolset {
    let digest = Workflow::new(
        "digest",
        None,
        digest_of("digest"),
        Toolset::empty().digest(),
        WorkflowSchemas {
            input: json!({}),
            output: json!({ "type": "string" }),
            signals: BTreeMap::new(),
        },
    )
    .unwrap();
    Toolset::from_host_tools(["notify"])
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

#[test]
fn the_toolset_with_workflows_fixture_decodes_and_hashes_to_its_digest() {
    let decoded = decode_toolset(TOOLSET_WITH_WORKFLOWS).unwrap();
    assert_eq!(decoded, toolset_with_workflows());
    assert_eq!(encode_toolset(&decoded), TOOLSET_WITH_WORKFLOWS);
    assert_eq!(
        Digest::of(TOOLSET_WITH_WORKFLOWS.as_bytes()),
        toolset_with_workflows().digest()
    );
    assert!(decoded.contains("workflows.triage.signal"));
}

#[test]
fn rejects_workflows_v1_does_not_define() {
    let schemas = r#""schemas":{"input":{},"output":{},"signals":{}}"#;
    let digest = "0".repeat(64);
    let document = |fields: &str| format!(r#"{{"v":1,{fields}}}"#);
    assert!(
        decode_workflow(&document(&format!(
            r#""name":"triage","script":"{digest}","toolset":"{digest}",{schemas}"#
        )))
        .is_ok()
    );
    let invalid = [
        document(&format!(
            r#""name":"triage","script":"{digest}","toolset":"{digest}",{schemas},"x":1"#
        )),
        document(&format!(r#""name":"triage","script":"{digest}",{schemas}"#)),
        document(&format!(
            r#""name":"triage","script":"abc","toolset":"{digest}",{schemas}"#
        )),
        document(&format!(
            r#""name":"default","script":"{digest}","toolset":"{digest}",{schemas}"#
        )),
        document(&format!(
            r#""name":"triage","script":"{digest}","toolset":"{digest}","schemas":{{"input":{{}},"output":{{}},"signals":{{"rewind:x":{{}}}}}}"#
        )),
        document(&format!(
            r#""name":"triage","script":"{digest}","toolset":"{digest}","schemas":{{"input":{{}},"output":{{}}}}"#
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
        r#"{{"v":1,"servers":[],"tools":[],"workflows":[{w},{w}]}}"#,
        w = format_args!(r#"{{"name":"a","version":"{digest}",{schemas}}}"#)
    );
    assert!(matches!(
        decode_toolset(&pinned_twice),
        Err(FormatError::InvalidToolset(_))
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
    let mut tools = BTreeMap::new();
    tools.insert(
        "github.list_issues".to_owned(),
        vec![
            ToolRule {
                args: Some(json!({ "state": "open" })),
                answer: ToolOutcome::Result(json!([{ "number": 4 }])),
                times: Some(1),
            },
            ToolRule {
                args: None,
                answer: ToolOutcome::Result(json!(null)),
                times: None,
            },
        ],
    );
    tools.insert(
        "slack.post_message".to_owned(),
        vec![ToolRule {
            args: None,
            answer: ToolOutcome::Failure(ToolFailure {
                kind: ToolFailureKind::Unavailable,
                message: "rate limited".into(),
                data: Some(json!({ "retryAfter": 30 })),
            }),
            times: None,
        }],
    );
    tools.insert(
        "workflows.notify.start".to_owned(),
        vec![ToolRule::always(ToolOutcome::Failure(ToolFailure {
            kind: ToolFailureKind::Error,
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
            tools,
            mocks: Some("export default {};\n".into()),
            cassette: Some(Cassette {
                calls: vec![RecordedCall {
                    tool: "github.close_issue".into(),
                    args: json!({ "number": 4 }),
                    outcome: ToolOutcome::Result(json!({})),
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
                name: "ToolError".into(),
                message: Some("rate limited".into()),
            }),
            calls: vec![
                ExpectedCall {
                    tool: "slack.post_message".into(),
                    args: Some(json!({ "channel": "#triage" })),
                    times: Some(1),
                },
                ExpectedCall {
                    tool: "github.delete_repo".into(),
                    args: None,
                    times: Some(0),
                },
                ExpectedCall {
                    tool: "github.close_issue".into(),
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
    let given = r#"{"v":1,"name":"t","input":{},"world":{"tools":{"a.b":{"returns":1}}},
        "signals":[{"name":"go","when":"blocked"}],"expect":{"order":"any"}}"#;
    let decoded = decode_test(given).unwrap();
    assert_eq!(
        encode_test(&decoded),
        r#"{"name":"t","signals":[{"name":"go"}],"v":1,"world":{"tools":{"a.b":[{"returns":1}]}}}"#
    );
    // A result of `null` is a result.
    let null =
        decode_test(r#"{"v":1,"name":"t","world":{"tools":{"a.b":{"returns":null}}}}"#).unwrap();
    assert_eq!(
        null.world.tools["a.b"][0].answer,
        ToolOutcome::Result(json!(null))
    );
}

#[test]
fn rejects_tests_v1_does_not_define() {
    let invalid = [
        r#"{"name":"t"}"#,
        r#"{"v":1}"#,
        r#"{"v":1,"name":"t","x":1}"#,
        r#"{"v":1,"name":""}"#,
        r#"{"v":1,"name":"t","world":{"tools":{"a.b":{}}}}"#,
        r#"{"v":1,"name":"t","world":{"tools":{"a.b":{"returns":1,"error":{"message":"m"}}}}}"#,
        r#"{"v":1,"name":"t","world":{"tools":{"a.b":{"returns":1,"times":0}}}}"#,
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
        r#""name":"triage","script":"{digest}","toolset":"{digest}","schemas":{{"input":{{}},"output":{{}},"signals":{{}}}}"#
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
