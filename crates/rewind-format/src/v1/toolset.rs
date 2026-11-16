//! The pinned toolset document, v1 (`archi/02-journal.md`, "Stored format v1";
//! `archi/04-tools.md`, "Toolsets and code generation").
//!
//! ```json
//! {"servers":[{"concurrency":8,"name":"github","transport":{"type":"http","url":"https://…"}}],
//!  "tools":[{"name":"github.list_issues","route":{"type":"mcp","server":"github"},
//!            "description":"List the issues","schemas":{"input":{…},"output":{…}},
//!            "policy":{"approval":false,"concurrency":16,"execution_timeout_ms":300000,
//!                      "heartbeat_timeout_ms":60000,"on_interruption":"retry"}}],
//!  "v":1}
//! ```
//!
//! The document is written in its canonical form (RFC 8785, on one line), so its bytes hash to
//! the toolset's digest. Routes are `{"type":"mcp","server":…}`, `{"type":"host"}` (with a
//! `group` for a grouped host tool), `{"type":"builtin","builtin":"http","allowed_hosts":[…]}`
//! and `{"type":"builtin","builtin":"webhook"}`; transports `{"type":"http","url":…}` and
//! `{"type":"stdio","command":…,"args":[…],"env":{…}}`. Optional fields are omitted when
//! absent: `description`, `schemas.output`, `policy.execution_timeout_ms`, `route.group`.
//!
//! A toolset that pins workflows also has `"workflows":[{"name":"triage","version":"<sha-256>",
//! "description":…,"schemas":{"input":…,"output":…,"signals":{"approval":…}}}]`; one that pins
//! none has no `workflows`. The workflows' built-ins are derived from them, never written.
//!
//! The workflow version document (the `workflows` table) is
//! `{"description":…,"name":"triage","schemas":{…},"script":"<sha-256>","toolset":"<sha-256>",
//! "v":1}`, in its canonical form, so that its bytes hash to the version.

use std::collections::BTreeMap;
use std::time::Duration;

use rewind_core::{
    Builtin, Digest, Interruption, McpServer, McpTransport, PinnedWorkflow, RetiredTest, Suite,
    SuiteTest, Tool, ToolPolicy, ToolRoute, Toolset, UpdatedTest, Workflow, WorkflowSchemas,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolsetV1 {
    servers: Vec<ServerV1>,
    tools: Vec<ToolV1>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    workflows: Vec<PinnedWorkflowV1>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PinnedWorkflowV1 {
    name: String,
    version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    schemas: WorkflowSchemasV1,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkflowSchemasV1 {
    input: Value,
    output: Value,
    signals: BTreeMap<String, Value>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkflowV1 {
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    script: String,
    toolset: String,
    schemas: WorkflowSchemasV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tests: Vec<SuiteTestV1>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    retired: Vec<RetiredTestV1>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    updated: Vec<UpdatedTestV1>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SuiteTestV1 {
    name: String,
    test: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RetiredTestV1 {
    name: String,
    test: String,
    reason: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdatedTestV1 {
    name: String,
    from: String,
    to: String,
    reason: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ServerV1 {
    name: String,
    transport: TransportV1,
    concurrency: u32,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum TransportV1 {
    Http {
        url: String,
    },
    Stdio {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolV1 {
    name: String,
    route: RouteV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    schemas: SchemasV1,
    policy: PolicyV1,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum RouteV1 {
    Mcp {
        server: String,
    },
    Host {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        group: Option<String>,
    },
    Builtin {
        builtin: BuiltinV1,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        allowed_hosts: Option<Vec<String>>,
    },
}

#[derive(Serialize, Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum BuiltinV1 {
    Http,
    Webhook,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SchemasV1 {
    input: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    output: Option<Value>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyV1 {
    on_interruption: InterruptionV1,
    approval: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    execution_timeout_ms: Option<u64>,
    heartbeat_timeout_ms: u64,
    concurrency: u32,
}

#[derive(Serialize, Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum InterruptionV1 {
    Retry,
    AtMostOnce,
}

/// The document as a JSON value, `"v"` included. The caller writes it in canonical form.
pub(crate) fn toolset_document(toolset: &Toolset) -> Value {
    let dto = ToolsetV1 {
        servers: toolset.servers().iter().map(server_dto).collect(),
        tools: toolset.tools().iter().map(tool_dto).collect(),
        workflows: toolset.workflows().iter().map(pinned_dto).collect(),
    };
    let mut document = serde_json::to_value(dto).expect("toolset DTOs always serialize");
    if let Value::Object(fields) = &mut document {
        fields.insert("v".into(), Value::from(crate::VERSION));
    }
    document
}

/// Decodes the fields of a v1 document, its version already taken out.
pub(crate) fn decode_toolset(document: Value) -> Result<Toolset, String> {
    let dto: ToolsetV1 = serde_json::from_value(document).map_err(|err| err.to_string())?;
    let servers = dto.servers.into_iter().map(|server| McpServer {
        name: server.name,
        transport: match server.transport {
            TransportV1::Http { url } => McpTransport::Http { url },
            TransportV1::Stdio { command, args, env } => McpTransport::Stdio { command, args, env },
        },
        concurrency: server.concurrency,
    });
    let tools = dto
        .tools
        .into_iter()
        .map(tool_from_dto)
        .collect::<Result<Vec<Tool>, String>>()?;
    let workflows = dto
        .workflows
        .into_iter()
        .map(|workflow| {
            Ok(PinnedWorkflow {
                version: digest(&workflow.version, "the version of a workflow")?,
                name: workflow.name,
                description: workflow.description,
                schemas: schemas_from_dto(workflow.schemas),
            })
        })
        .collect::<Result<Vec<PinnedWorkflow>, String>>()?;
    Toolset::new(servers, tools)
        .and_then(|toolset| toolset.with_workflows(workflows))
        .map_err(|err| err.to_string())
}

/// The workflow version document as a JSON value, `"v"` included. The caller writes it in
/// canonical form.
pub(crate) fn workflow_document(workflow: &Workflow) -> Value {
    let suite = workflow.suite();
    let dto = WorkflowV1 {
        name: workflow.name().to_owned(),
        description: workflow.description().map(str::to_owned),
        script: workflow.script().to_hex(),
        toolset: workflow.toolset().to_hex(),
        schemas: schemas_dto(workflow.schemas()),
        parent: suite.parent.map(|parent| parent.to_hex()),
        tests: suite
            .tests
            .iter()
            .map(|test| SuiteTestV1 {
                name: test.name.clone(),
                test: test.test.to_hex(),
            })
            .collect(),
        retired: suite
            .retired
            .iter()
            .map(|retired| RetiredTestV1 {
                name: retired.name.clone(),
                test: retired.test.to_hex(),
                reason: retired.reason.clone(),
            })
            .collect(),
        updated: suite
            .updated
            .iter()
            .map(|updated| UpdatedTestV1 {
                name: updated.name.clone(),
                from: updated.from.to_hex(),
                to: updated.to.to_hex(),
                reason: updated.reason.clone(),
            })
            .collect(),
    };
    let mut document = serde_json::to_value(dto).expect("workflow DTOs always serialize");
    if let Value::Object(fields) = &mut document {
        fields.insert("v".into(), Value::from(crate::VERSION));
    }
    document
}

/// Decodes the fields of a v1 workflow version document, its version already taken out.
pub(crate) fn decode_workflow(document: Value) -> Result<Workflow, String> {
    let dto: WorkflowV1 = serde_json::from_value(document).map_err(|err| err.to_string())?;
    let suite = Suite {
        parent: dto
            .parent
            .map(|parent| digest(&parent, "\"parent\""))
            .transpose()?,
        tests: dto
            .tests
            .into_iter()
            .map(|test| {
                Ok(SuiteTest {
                    test: digest(&test.test, "a test's \"test\"")?,
                    name: test.name,
                })
            })
            .collect::<Result<_, String>>()?,
        retired: dto
            .retired
            .into_iter()
            .map(|retired| {
                Ok(RetiredTest {
                    test: digest(&retired.test, "a retired test's \"test\"")?,
                    name: retired.name,
                    reason: retired.reason,
                })
            })
            .collect::<Result<_, String>>()?,
        updated: dto
            .updated
            .into_iter()
            .map(|updated| {
                Ok(UpdatedTest {
                    from: digest(&updated.from, "an updated test's \"from\"")?,
                    to: digest(&updated.to, "an updated test's \"to\"")?,
                    name: updated.name,
                    reason: updated.reason,
                })
            })
            .collect::<Result<_, String>>()?,
    };
    Workflow::new(
        dto.name,
        dto.description,
        digest(&dto.script, "\"script\"")?,
        digest(&dto.toolset, "\"toolset\"")?,
        schemas_from_dto(dto.schemas),
    )
    .and_then(|workflow| workflow.with_suite(suite))
    .map_err(|err| err.to_string())
}

fn pinned_dto(workflow: &PinnedWorkflow) -> PinnedWorkflowV1 {
    PinnedWorkflowV1 {
        name: workflow.name.clone(),
        version: workflow.version.to_hex(),
        description: workflow.description.clone(),
        schemas: schemas_dto(&workflow.schemas),
    }
}

fn schemas_dto(schemas: &WorkflowSchemas) -> WorkflowSchemasV1 {
    WorkflowSchemasV1 {
        input: schemas.input.clone(),
        output: schemas.output.clone(),
        signals: schemas.signals.clone(),
    }
}

fn schemas_from_dto(dto: WorkflowSchemasV1) -> WorkflowSchemas {
    WorkflowSchemas {
        input: dto.input,
        output: dto.output,
        signals: dto.signals,
    }
}

fn digest(hex: &str, what: &str) -> Result<Digest, String> {
    Digest::from_hex(hex).map_err(|err| format!("{what}: {err}"))
}

fn server_dto(server: &McpServer) -> ServerV1 {
    ServerV1 {
        name: server.name.clone(),
        transport: match &server.transport {
            McpTransport::Http { url } => TransportV1::Http { url: url.clone() },
            McpTransport::Stdio { command, args, env } => TransportV1::Stdio {
                command: command.clone(),
                args: args.clone(),
                env: env.clone(),
            },
        },
        concurrency: server.concurrency,
    }
}

fn tool_dto(tool: &Tool) -> ToolV1 {
    let route = match &tool.route {
        ToolRoute::Mcp { server } => RouteV1::Mcp {
            server: server.clone(),
        },
        ToolRoute::Host { group } => RouteV1::Host {
            group: group.clone(),
        },
        ToolRoute::Builtin(Builtin::Http { allowed_hosts }) => RouteV1::Builtin {
            builtin: BuiltinV1::Http,
            allowed_hosts: Some(allowed_hosts.clone()),
        },
        ToolRoute::Builtin(Builtin::Webhook) => RouteV1::Builtin {
            builtin: BuiltinV1::Webhook,
            allowed_hosts: None,
        },
        ToolRoute::Builtin(Builtin::Workflow { .. }) => {
            unreachable!("a toolset declares no workflow built-in: they come from its workflows")
        }
    };
    let policy = &tool.policy;
    ToolV1 {
        name: tool.name.clone(),
        route,
        description: tool.description.clone(),
        schemas: SchemasV1 {
            input: tool.input_schema.clone(),
            output: tool.output_schema.clone(),
        },
        policy: PolicyV1 {
            on_interruption: match policy.on_interruption {
                Interruption::Retry => InterruptionV1::Retry,
                Interruption::AtMostOnce => InterruptionV1::AtMostOnce,
            },
            approval: policy.approval,
            execution_timeout_ms: policy.execution_timeout.map(millis),
            heartbeat_timeout_ms: millis(policy.heartbeat_timeout),
            concurrency: policy.concurrency,
        },
    }
}

fn tool_from_dto(dto: ToolV1) -> Result<Tool, String> {
    let route = match dto.route {
        RouteV1::Mcp { server } => ToolRoute::Mcp { server },
        RouteV1::Host { group } => ToolRoute::Host { group },
        RouteV1::Builtin {
            builtin: BuiltinV1::Http,
            allowed_hosts: Some(allowed_hosts),
        } => ToolRoute::Builtin(Builtin::Http { allowed_hosts }),
        RouteV1::Builtin {
            builtin: BuiltinV1::Http,
            allowed_hosts: None,
        } => {
            return Err(format!(
                "tool {:?}: the http built-in needs \"allowed_hosts\"",
                dto.name
            ));
        }
        RouteV1::Builtin {
            builtin: BuiltinV1::Webhook,
            allowed_hosts: None,
        } => ToolRoute::Builtin(Builtin::Webhook),
        RouteV1::Builtin {
            builtin: BuiltinV1::Webhook,
            allowed_hosts: Some(_),
        } => {
            return Err(format!(
                "tool {:?}: the webhook built-in takes no \"allowed_hosts\"",
                dto.name
            ));
        }
    };
    let policy = dto.policy;
    Ok(Tool {
        name: dto.name,
        route,
        description: dto.description,
        input_schema: dto.schemas.input,
        output_schema: dto.schemas.output,
        policy: ToolPolicy {
            on_interruption: match policy.on_interruption {
                InterruptionV1::Retry => Interruption::Retry,
                InterruptionV1::AtMostOnce => Interruption::AtMostOnce,
            },
            approval: policy.approval,
            execution_timeout: policy.execution_timeout_ms.map(Duration::from_millis),
            heartbeat_timeout: Duration::from_millis(policy.heartbeat_timeout_ms),
            concurrency: policy.concurrency,
        },
    })
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}
