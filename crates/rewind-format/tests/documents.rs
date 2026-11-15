//! The cassette, pinned toolset and workflow version documents v1 are frozen like events: these
//! fixtures were written once and must decode forever, to the same domain values, and re-encode
//! to the same bytes.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::time::Duration;

use rewind_core::{
    Builtin, Cassette, Digest, Interruption, Matching, McpServer, McpTransport, RecordedCall,
    RecordedSignal, Tool, ToolFailure, ToolFailureKind, ToolOutcome, ToolPolicy, ToolRoute,
    Toolset, Workflow, WorkflowSchemas,
};
use rewind_format::{
    FormatError, decode_cassette, decode_toolset, decode_workflow, encode_cassette, encode_toolset,
    encode_workflow,
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
