//! The pinned action set document, v1 (`archi/02-journal.md`, "Stored format v1";
//! `archi/04-tools.md`, "Action sets and code generation").
//!
//! ```json
//! {"servers":[{"concurrency":8,"name":"github","transport":{"type":"http","url":"https://…"}}],
//!  "actions":[{"name":"github.list_issues","route":{"type":"mcp","server":"github"},
//!            "description":"List the issues","schemas":{"input":{…},"output":{…}},
//!            "policy":{"approval":false,"concurrency":16,"execution_timeout_ms":300000,
//!                      "idempotent":true,"reconnect_timeout_ms":60000}}],
//!  "v":1}
//! ```
//!
//! The document is written in its canonical form (RFC 8785, on one line), so its bytes hash to
//! the action set's digest. Routes are `{"type":"mcp","server":…}`, `{"type":"harness"}` (with a
//! `group` for a grouped harness action), `{"type":"builtin","builtin":"http","allowed_hosts":[…]}`
//! and `{"type":"builtin","builtin":"webhook"}`; transports `{"type":"http","url":…}` and
//! `{"type":"stdio","command":…,"args":[…],"env":{…}}`. Optional fields are omitted when
//! absent: `description`, `schemas.output`, `policy.execution_timeout_ms`, `route.group`.
//!
//! An action set that pins workflows also has `"workflows":[{"name":"triage","version":"<sha-256>",
//! "description":…,"schemas":{"input":…,"output":…,"signals":{"approval":…}}}]`; one that pins
//! none has no `workflows`. The workflows' built-ins are derived from them, never written.
//!
//! How calls authenticate (`archi/04-tools.md`, "Authenticating calls") is a server's or an
//! API's `auth`, left out when calls carry nothing: `{"type":"rewind"}`,
//! `{"type":"secret","header":"Authorization","format":"Bearer {secret}"}` (with
//! `"encoding":"base64"` for HTTP basic; `format` left out for the value alone) or
//! `{"type":"secret","env":"GITHUB_TOKEN"}`. Never a value.
//!
//! An action set with APIs has `"apis":[{"base_url":"https://api.example.com","concurrency":8,
//! "name":"dax","auth":…}]`; one without has no `apis`. An API action's route is
//! `{"type":"api","api":"dax"}`, and it has an `"operation":{"body":true,
//! "idempotency_key":false,"method":"post","parameters":[{"in":"path","name":"id"}],
//! "path":"/measures/{id}"}`; no other action has one.
//!
//! The workflow version document (the `workflows` table) is
//! `{"description":…,"labels":{"env":"dev"},"name":"triage","schemas":{…},"script":"<sha-256>",
//! "action_set":"<sha-256>","v":1}`, in its canonical form, so that its bytes hash to the version.
//! `labels`, the scope of a grant limited to labels it was saved under, is omitted when empty:
//! a version saved tenant-wide keeps the document it had before versions carried labels. So is
//! `secrets`, its attachments (`{"github":"ns/github","zendesk":"org/zendesk"}`): a version
//! that attaches none keeps the document it had before versions carried them.

use std::collections::BTreeMap;
use std::time::Duration;

use rewind_core::{
    Action, ActionAuth, ActionPolicy, ActionRoute, ActionSet, Api, ApiOperation, ApiParameter,
    Attachments, Builtin, Digest, HttpMethod, McpServer, McpTransport, ParameterLocation,
    PinnedWorkflow, RetiredTest, SecretEncoding, SecretPlacement, SecretRef, Suite, SuiteTest,
    UpdatedTest, Workflow, WorkflowSchemas,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ActionSetV1 {
    servers: Vec<ServerV1>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    apis: Vec<ApiV1>,
    actions: Vec<ActionV1>,
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
    action_set: String,
    schemas: WorkflowSchemasV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parent: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tests: Vec<SuiteTestV1>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    retired: Vec<RetiredTestV1>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    updated: Vec<UpdatedTestV1>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    labels: BTreeMap<String, String>,
    /// `org/<name>` or `ns/<name>`, by server or API.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    secrets: BTreeMap<String, String>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    auth: Option<AuthV1>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ApiV1 {
    name: String,
    base_url: String,
    concurrency: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    auth: Option<AuthV1>,
}

/// No auth is written as no `auth` field.
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum AuthV1 {
    /// A struct variant, so that `deny_unknown_fields` applies to it.
    Rewind {},
    /// Exactly one of `header` and `env`.
    Secret {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        header: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        format: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        encoding: Option<EncodingV1>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        env: Option<String>,
    },
}

/// `plain` is written as no `encoding` field.
#[derive(Serialize, Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum EncodingV1 {
    Base64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OperationV1 {
    method: MethodV1,
    path: String,
    parameters: Vec<ParameterV1>,
    body: bool,
    idempotency_key: bool,
}

#[derive(Serialize, Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum MethodV1 {
    Get,
    Put,
    Post,
    Delete,
    Options,
    Head,
    Patch,
    Trace,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ParameterV1 {
    name: String,
    #[serde(rename = "in")]
    location: LocationV1,
}

#[derive(Serialize, Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum LocationV1 {
    Path,
    Query,
    Header,
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
struct ActionV1 {
    name: String,
    route: RouteV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    operation: Option<OperationV1>,
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
    Api {
        api: String,
    },
    Harness {
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
pub(crate) struct PolicyV1 {
    idempotent: bool,
    approval: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    execution_timeout_ms: Option<u64>,
    reconnect_timeout_ms: u64,
    concurrency: u32,
}

/// The document as a JSON value, `"v"` included. The caller writes it in canonical form.
pub(crate) fn action_set_document(action_set: &ActionSet) -> Value {
    let dto = ActionSetV1 {
        servers: action_set.servers().iter().map(server_dto).collect(),
        apis: action_set.apis().iter().map(api_dto).collect(),
        actions: action_set.actions().iter().map(action_dto).collect(),
        workflows: action_set.workflows().iter().map(pinned_dto).collect(),
    };
    let mut document = serde_json::to_value(dto).expect("action set DTOs always serialize");
    if let Value::Object(fields) = &mut document {
        fields.insert("v".into(), Value::from(crate::VERSION));
    }
    document
}

/// Decodes the fields of a v1 document, its version already taken out.
pub(crate) fn decode_action_set(document: Value) -> Result<ActionSet, String> {
    let dto: ActionSetV1 = serde_json::from_value(document).map_err(|err| err.to_string())?;
    let servers = dto
        .servers
        .into_iter()
        .map(|server| {
            Ok(McpServer {
                auth: auth_from_dto(server.auth)
                    .map_err(|err| format!("server {:?}: {err}", server.name))?,
                name: server.name,
                transport: match server.transport {
                    TransportV1::Http { url } => McpTransport::Http { url },
                    TransportV1::Stdio { command, args, env } => {
                        McpTransport::Stdio { command, args, env }
                    }
                },
                concurrency: server.concurrency,
            })
        })
        .collect::<Result<Vec<McpServer>, String>>()?;
    let apis = dto
        .apis
        .into_iter()
        .map(|api| {
            Ok(Api {
                auth: auth_from_dto(api.auth)
                    .map_err(|err| format!("API {:?}: {err}", api.name))?,
                name: api.name,
                base_url: api.base_url,
                concurrency: api.concurrency,
            })
        })
        .collect::<Result<Vec<Api>, String>>()?;
    let actions = dto
        .actions
        .into_iter()
        .map(action_from_dto)
        .collect::<Result<Vec<Action>, String>>()?;
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
    ActionSet::from_parts(servers, apis, actions)
        .and_then(|action_set| action_set.with_workflows(workflows))
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
        action_set: workflow.action_set().to_hex(),
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
        labels: workflow.labels().clone(),
        secrets: workflow
            .secrets()
            .iter()
            .map(|(server, secret)| (server.to_owned(), secret.to_string()))
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
    let secrets = dto
        .secrets
        .into_iter()
        .map(|(server, secret)| {
            let secret = SecretRef::parse(&secret)
                .map_err(|err| format!("the secret attached for {server:?}: {err}"))?;
            Ok((server, secret))
        })
        .collect::<Result<Attachments, String>>()?;
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
        digest(&dto.action_set, "\"action_set\"")?,
        schemas_from_dto(dto.schemas),
    )
    .and_then(|workflow| workflow.with_suite(suite))
    .map(|workflow| workflow.with_labels(dto.labels).with_secrets(secrets))
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
        auth: auth_dto(&server.auth),
    }
}

fn api_dto(api: &Api) -> ApiV1 {
    ApiV1 {
        name: api.name.clone(),
        base_url: api.base_url.clone(),
        concurrency: api.concurrency,
        auth: auth_dto(&api.auth),
    }
}

fn auth_dto(auth: &ActionAuth) -> Option<AuthV1> {
    Some(match auth {
        ActionAuth::None => return None,
        ActionAuth::Rewind => AuthV1::Rewind {},
        ActionAuth::Secret(SecretPlacement::Header {
            name,
            format,
            encoding,
        }) => AuthV1::Secret {
            header: Some(name.clone()),
            format: format.clone(),
            encoding: match encoding {
                SecretEncoding::Plain => None,
                SecretEncoding::Base64 => Some(EncodingV1::Base64),
            },
            env: None,
        },
        ActionAuth::Secret(SecretPlacement::Env { variable }) => AuthV1::Secret {
            header: None,
            format: None,
            encoding: None,
            env: Some(variable.clone()),
        },
    })
}

fn auth_from_dto(dto: Option<AuthV1>) -> Result<ActionAuth, String> {
    Ok(match dto {
        None => ActionAuth::None,
        Some(AuthV1::Rewind {}) => ActionAuth::Rewind,
        Some(AuthV1::Secret {
            header: Some(name),
            format,
            encoding,
            env: None,
        }) => ActionAuth::Secret(SecretPlacement::Header {
            name,
            format,
            encoding: match encoding {
                None => SecretEncoding::Plain,
                Some(EncodingV1::Base64) => SecretEncoding::Base64,
            },
        }),
        Some(AuthV1::Secret {
            header: None,
            format: None,
            encoding: None,
            env: Some(variable),
        }) => ActionAuth::Secret(SecretPlacement::Env { variable }),
        Some(AuthV1::Secret { .. }) => {
            return Err(
                "a secret's auth has either a \"header\" (with an optional \"format\" and \
                 \"encoding\") or an \"env\""
                    .into(),
            );
        }
    })
}

fn operation_dto(operation: &ApiOperation) -> OperationV1 {
    OperationV1 {
        method: match operation.method {
            HttpMethod::Get => MethodV1::Get,
            HttpMethod::Put => MethodV1::Put,
            HttpMethod::Post => MethodV1::Post,
            HttpMethod::Delete => MethodV1::Delete,
            HttpMethod::Options => MethodV1::Options,
            HttpMethod::Head => MethodV1::Head,
            HttpMethod::Patch => MethodV1::Patch,
            HttpMethod::Trace => MethodV1::Trace,
        },
        path: operation.path.clone(),
        parameters: operation
            .parameters
            .iter()
            .map(|parameter| ParameterV1 {
                name: parameter.name.clone(),
                location: match parameter.location {
                    ParameterLocation::Path => LocationV1::Path,
                    ParameterLocation::Query => LocationV1::Query,
                    ParameterLocation::Header => LocationV1::Header,
                },
            })
            .collect(),
        body: operation.body,
        idempotency_key: operation.idempotency_key,
    }
}

fn operation_from_dto(dto: OperationV1) -> ApiOperation {
    ApiOperation {
        method: match dto.method {
            MethodV1::Get => HttpMethod::Get,
            MethodV1::Put => HttpMethod::Put,
            MethodV1::Post => HttpMethod::Post,
            MethodV1::Delete => HttpMethod::Delete,
            MethodV1::Options => HttpMethod::Options,
            MethodV1::Head => HttpMethod::Head,
            MethodV1::Patch => HttpMethod::Patch,
            MethodV1::Trace => HttpMethod::Trace,
        },
        path: dto.path,
        parameters: dto
            .parameters
            .into_iter()
            .map(|parameter| ApiParameter {
                name: parameter.name,
                location: match parameter.location {
                    LocationV1::Path => ParameterLocation::Path,
                    LocationV1::Query => ParameterLocation::Query,
                    LocationV1::Header => ParameterLocation::Header,
                },
            })
            .collect(),
        body: dto.body,
        idempotency_key: dto.idempotency_key,
    }
}

fn action_dto(action: &Action) -> ActionV1 {
    let route = match &action.route {
        ActionRoute::Mcp { server } => RouteV1::Mcp {
            server: server.clone(),
        },
        ActionRoute::Api { api } => RouteV1::Api { api: api.clone() },
        ActionRoute::Harness { group } => RouteV1::Harness {
            group: group.clone(),
        },
        ActionRoute::Builtin(Builtin::Http { allowed_hosts }) => RouteV1::Builtin {
            builtin: BuiltinV1::Http,
            allowed_hosts: Some(allowed_hosts.clone()),
        },
        ActionRoute::Builtin(Builtin::Webhook) => RouteV1::Builtin {
            builtin: BuiltinV1::Webhook,
            allowed_hosts: None,
        },
        ActionRoute::Builtin(Builtin::Workflow { .. }) => {
            unreachable!(
                "an action set declares no workflow built-in: they come from its workflows"
            )
        }
    };
    let policy = &action.policy;
    ActionV1 {
        name: action.name.clone(),
        route,
        operation: action.operation.as_ref().map(operation_dto),
        description: action.description.clone(),
        schemas: SchemasV1 {
            input: action.input_schema.clone(),
            output: action.output_schema.clone(),
        },
        policy: policy_dto(policy),
    }
}

/// An action's policy as the action set document and a call's task write it.
pub(crate) fn policy_dto(policy: &ActionPolicy) -> PolicyV1 {
    PolicyV1 {
        idempotent: policy.idempotent,
        approval: policy.approval,
        execution_timeout_ms: policy.execution_timeout.map(millis),
        reconnect_timeout_ms: millis(policy.reconnect_timeout),
        concurrency: policy.concurrency,
    }
}

pub(crate) fn policy_from_dto(policy: PolicyV1) -> ActionPolicy {
    ActionPolicy {
        idempotent: policy.idempotent,
        approval: policy.approval,
        execution_timeout: policy.execution_timeout_ms.map(Duration::from_millis),
        reconnect_timeout: Duration::from_millis(policy.reconnect_timeout_ms),
        concurrency: policy.concurrency,
    }
}

fn action_from_dto(dto: ActionV1) -> Result<Action, String> {
    let route = match dto.route {
        RouteV1::Mcp { server } => ActionRoute::Mcp { server },
        RouteV1::Api { api } => ActionRoute::Api { api },
        RouteV1::Harness { group } => ActionRoute::Harness { group },
        RouteV1::Builtin {
            builtin: BuiltinV1::Http,
            allowed_hosts: Some(allowed_hosts),
        } => ActionRoute::Builtin(Builtin::Http { allowed_hosts }),
        RouteV1::Builtin {
            builtin: BuiltinV1::Http,
            allowed_hosts: None,
        } => {
            return Err(format!(
                "action {:?}: the http built-in needs \"allowed_hosts\"",
                dto.name
            ));
        }
        RouteV1::Builtin {
            builtin: BuiltinV1::Webhook,
            allowed_hosts: None,
        } => ActionRoute::Builtin(Builtin::Webhook),
        RouteV1::Builtin {
            builtin: BuiltinV1::Webhook,
            allowed_hosts: Some(_),
        } => {
            return Err(format!(
                "action {:?}: the webhook built-in takes no \"allowed_hosts\"",
                dto.name
            ));
        }
    };
    Ok(Action {
        name: dto.name,
        route,
        operation: dto.operation.map(operation_from_dto),
        description: dto.description,
        input_schema: dto.schemas.input,
        output_schema: dto.schemas.output,
        policy: policy_from_dto(dto.policy),
    })
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}
