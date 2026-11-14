//! The OpenAPI 3.1 document of the API, generated from the DTOs with `utoipa`.
//!
//! It is committed at `api/openapi.json`; a test fails when the committed file is stale, and
//! `REWIND_OPENAPI_WRITE=1 cargo test -p rewind-api --test openapi` writes it again. The TypeScript
//! SDK and the dashboard generate their types from that file (`api/README.md`).
//!
//! Each operation below is a declaration: its function has no body, and serves only to carry
//! the operation's description to `utoipa`. The server implements the operations; its routes
//! are the paths of [`crate::routes`].

use utoipa::openapi::path::{HttpMethod, OperationBuilder, ParameterBuilder, ParameterIn};
use utoipa::openapi::request_body::RequestBodyBuilder;
use utoipa::openapi::response::{ResponseBuilder, ResponsesBuilder};
use utoipa::openapi::security::{ApiKey, ApiKeyValue, HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::openapi::{
    ContentBuilder, OpenApi as Document, PathItem, PathsBuilder, Ref, RefOr, Required,
};
use utoipa::{Modify, OpenApi};

use crate::auth::SESSION_COOKIE;
use crate::events::StreamEvent;
use crate::problem::{self, Problem};
use crate::webhooks::{
    WEBHOOK_ID, WEBHOOK_SIGNATURE, WEBHOOK_TIMESTAMP, WebhookEvent, WebhookEventType,
};

/// The API's document.
#[derive(Debug, OpenApi)]
#[openapi(
    info(
        title = "Rewind API",
        description = "The HTTP API /v1 of Rewind: toolsets, scripts, runs, host-tool tasks, \
            webhooks, reading, the organization, access and the server; and `/auth`, the \
            dashboard's sign-in. Every request to /v1 carries a Bearer token, an API key \
            (`rewind_sk_…`) or a member's access token from `/auth`, except over the embedded \
            server's unix socket and to the inbound webhook URL. What the caller may do is its \
            grant's: every operation requires one permission of the catalog \
            (`GET /v1/permissions`), and answers 403 without it. `/auth` is for a browser: it \
            reads and writes the cookie `rewind_session`, which /v1 never reads. Errors are \
            `application/problem+json`, with a `detail` written for a model. Fields are \
            camelCase, enumerated values snake_case, times RFC 3339, and durations a number and \
            a unit (`\"30s\"`, `\"1h30m\"`) or a number of milliseconds."
    ),
    servers((url = "/", description = "The server that serves this document.")),
    paths(
        operations::create_toolset,
        operations::list_toolsets,
        operations::get_toolset_declarations,
        operations::search_tools,
        operations::set_toolset_credentials,
        operations::check_script,
        operations::test_script,
        operations::list_scripts,
        operations::get_script,
        operations::start_run,
        operations::list_runs,
        operations::get_run_stats,
        operations::list_run_failures,
        operations::get_run,
        operations::delete_run,
        operations::get_run_trace,
        operations::get_run_entry,
        operations::stream_run_events,
        operations::signal_run,
        operations::cancel_run,
        operations::fork_run,
        operations::export_run_journal,
        operations::release_run,
        operations::give_up_run,
        operations::set_run_credentials,
        operations::claim_tasks,
        operations::heartbeat_task,
        operations::defer_task,
        operations::complete_task,
        operations::fail_task,
        operations::approve_task,
        operations::reject_task,
        operations::post_hook,
        operations::create_subscription,
        operations::list_subscriptions,
        operations::delete_subscription,
        operations::get_tool_stats,
        operations::get_payload,
        operations::whoami,
        operations::create_organization,
        operations::update_organization,
        operations::list_my_invitations,
        operations::accept_invitation,
        operations::list_members,
        operations::set_member_grant,
        operations::remove_member,
        operations::list_invitations,
        operations::create_invitation,
        operations::resend_invitation,
        operations::revoke_invitation,
        operations::list_permissions,
        operations::list_roles,
        operations::put_role,
        operations::delete_role,
        operations::list_api_keys,
        operations::create_api_key,
        operations::revoke_api_key,
        operations::get_server,
        operations::stop_server,
        operations::get_auth_config,
        operations::sign_in_with_github,
        operations::auth_callback,
        operations::send_magic_code,
        operations::verify_magic_code,
        operations::verify_email,
        operations::select_organization,
        operations::get_session,
        operations::join_waitlist,
        operations::sign_out,
    ),
    components(schemas(Problem, StreamEvent, WebhookEvent, WebhookEventType)),
    modifiers(&Conventions),
    security(("bearer" = [])),
    tags(
        (name = "toolsets", description = "Register and pin toolsets; their declarations and credentials."),
        (name = "scripts", description = "Type-check scripts, and run them as scenarios in mock mode."),
        (name = "runs", description = "Start, follow, steer, fork and erase runs."),
        (name = "tasks", description = "Host-tool tasks: long-polled by the harness's SDK worker, approved or rejected by the harness."),
        (name = "webhooks", description = "Inbound webhooks that become signals, and subscriptions to signed lifecycle events."),
        (name = "reading", description = "The scripts executed on the platform, tool statistics and payloads, for the dashboard."),
        (name = "organization", description = "Who calls; the organizations, their members and invitations."),
        (name = "access", description = "The catalog of permissions, the roles and the API keys."),
        (name = "server", description = "The server itself."),
        (name = "auth", description = "The dashboard's sign-in, for a browser: GitHub or a code by email, the organization picker, the waitlist."),
    )
)]
pub struct ApiDoc;

/// The document.
pub fn document() -> Document {
    ApiDoc::openapi()
}

/// The document as committed at `api/openapi.json`: pretty JSON, keys in a stable order, a
/// final newline.
pub fn to_json() -> String {
    let value = sorted(serde_json::to_value(document()).unwrap_or_default());
    let mut json = serde_json::to_string_pretty(&value).unwrap_or_default();
    json.push('\n');
    json
}

/// Object keys in sorted order, whatever the features of `serde_json` in the build: a
/// dependency that enables `preserve_order` must not change the committed file.
fn sorted(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(fields) => {
            let mut fields: Vec<_> = fields.into_iter().collect();
            fields.sort_by(|(a, _), (b, _)| a.cmp(b));
            serde_json::Value::Object(
                fields
                    .into_iter()
                    .map(|(key, field)| (key, sorted(field)))
                    .collect(),
            )
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(sorted).collect())
        }
        other => other,
    }
}

/// What every operation shares, added once rather than on each: the security scheme, the
/// error responses every operation can answer, and the outbound webhooks.
struct Conventions;

/// Error responses every operation may answer, by status: those of a request that does not
/// even reach the operation's own logic.
const COMMON_ERRORS: [(&str, &str); 5] = [
    ("400", "The request is malformed (`invalid-request`)."),
    (
        "401",
        "No token, or an unknown, revoked or expired one (`unauthorized`).",
    ),
    (
        "403",
        "The caller's grant does not hold the permission this operation needs, which the \
         message names, or its allowed labels do not cover the request (`forbidden`).",
    ),
    ("500", "A fault of the platform (`internal`)."),
    (
        "503",
        "The server cannot serve the request now (`unavailable`).",
    ),
];

impl Modify for Conventions {
    fn modify(&self, openapi: &mut Document) {
        // utoipa fills the license from Cargo.toml, which has none.
        openapi.info.license = None;
        let components = openapi.components.get_or_insert_with(Default::default);
        components.add_security_scheme(
            "bearer",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .description(Some(
                        "An API key of the tenant (`rewind_sk_…`), or a member's access token \
                         from `/auth`. The embedded server's unix socket takes none.",
                    ))
                    .build(),
            ),
        );
        components.add_security_scheme(
            "session",
            SecurityScheme::ApiKey(ApiKey::Cookie(ApiKeyValue::with_description(
                SESSION_COOKIE,
                "The dashboard's session, sealed by the server: only `/auth` reads it.",
            ))),
        );

        for (path, item) in &mut openapi.paths.paths {
            let sign_in = path.starts_with("/auth/");
            for operation in operations_mut(item) {
                let unauthenticated = operation
                    .security
                    .as_ref()
                    .is_some_and(|requirements| requirements.iter().all(is_empty_requirement));
                for (status, description) in COMMON_ERRORS {
                    let description = match status {
                        "401" | "403" if unauthenticated => continue,
                        // `/auth` needs a session, never a permission.
                        "403" if sign_in => continue,
                        "401" if sign_in => "No session, or one that expired (`unauthorized`).",
                        _ => description,
                    };
                    operation
                        .responses
                        .responses
                        .entry(status.to_owned())
                        .or_insert_with(|| problem_response(description));
                }
            }
        }

        openapi.webhooks = Some(webhooks_paths().into());
    }
}

fn is_empty_requirement(requirement: &utoipa::openapi::security::SecurityRequirement) -> bool {
    serde_json::to_value(requirement)
        .ok()
        .and_then(|value| value.as_object().map(serde_json::Map::is_empty))
        .unwrap_or(false)
}

fn operations_mut(
    item: &mut PathItem,
) -> impl Iterator<Item = &mut utoipa::openapi::path::Operation> {
    [
        item.get.as_mut(),
        item.put.as_mut(),
        item.post.as_mut(),
        item.delete.as_mut(),
        item.patch.as_mut(),
    ]
    .into_iter()
    .flatten()
}

/// A problem+json response.
pub(crate) fn problem_response(description: &str) -> RefOr<utoipa::openapi::response::Response> {
    ResponseBuilder::new()
        .description(description)
        .content(
            problem::CONTENT_TYPE,
            ContentBuilder::new()
                .schema(Some(Ref::from_schema_name("Problem")))
                .build(),
        )
        .build()
        .into()
}

/// The outbound webhooks: one entry per event type, each a POST to the subscription's URL.
fn webhooks_paths() -> PathsBuilder {
    let mut paths = PathsBuilder::new();
    for kind in WebhookEventType::ALL {
        let header = |name: &str, description: &str| {
            ParameterBuilder::new()
                .name(name)
                .parameter_in(ParameterIn::Header)
                .required(Required::True)
                .description(Some(description))
                .schema(Some(
                    utoipa::openapi::ObjectBuilder::new()
                        .schema_type(utoipa::openapi::schema::Type::String),
                ))
                .build()
        };
        let operation = OperationBuilder::new()
            .operation_id(Some(format!("webhook_{}", kind.as_str().replace('.', "_"))))
            .summary(Some(format!("The lifecycle event `{kind}`")))
            .description(Some(format!(
                "Sent to every subscription to `{kind}` whose labels the run carries. Signed \
                 following Standard Webhooks: verify `{WEBHOOK_SIGNATURE}` with the \
                 subscription's secret before trusting the body, and deduplicate on \
                 `{WEBHOOK_ID}`. A delivery that does not get a 2xx answer is retried."
            )))
            .tag("webhooks")
            .parameter(header(
                WEBHOOK_ID,
                "The delivery's id; a retried delivery keeps it.",
            ))
            .parameter(header(
                WEBHOOK_TIMESTAMP,
                "When the delivery was signed, in seconds since the Unix epoch.",
            ))
            .parameter(header(
                WEBHOOK_SIGNATURE,
                "`v1,<base64 HMAC-SHA256 of {id}.{timestamp}.{body}>`, space-separated while a secret is rotated.",
            ))
            .request_body(Some(
                RequestBodyBuilder::new()
                    .content(
                        "application/json",
                        ContentBuilder::new()
                            .schema(Some(Ref::from_schema_name("WebhookEvent")))
                            .build(),
                    )
                    .required(Some(Required::True))
                    .build(),
            ))
            .responses(
                ResponsesBuilder::new()
                    .response("2XX", ResponseBuilder::new().description("Received."))
                    .build(),
            )
            .build();
        paths = paths.path(kind.as_str(), PathItem::new(HttpMethod::Post, operation));
    }
    paths
}

/// The operations, declared for `utoipa`. Their bodies are empty: the server implements them.
#[allow(dead_code)]
mod operations {
    use crate::access::{
        ApiKey, ApiKeyList, CreateApiKey, Grant, PermissionList, PutRole, Role, RoleList, SetGrant,
    };
    use crate::admin::ServerInfo;
    use crate::auth::{
        AuthConfig, AuthState, Callback, EmailVerification, GithubSignIn, MagicAuthRequest,
        MagicAuthVerification, OrganizationSelection, SessionRequest, WaitlistRequest,
    };
    use crate::events::{Entry, EventsQuery, StreamEvent};
    use crate::organization::{
        CreateInvitation, CreateOrganization, Identity, Invitation, InvitationList, MemberList,
        Organization, OrganizationSettings, UpdateOrganization,
    };
    use crate::problem::Problem;
    use crate::reading::{PayloadQuery, ToolStatsFilter, ToolStatsPage};
    use crate::runs::{
        CancelRun, FailureGroupPage, FailureQuery, ForkRun, GetRunQuery, JournalQuery, Run,
        RunFilter, RunPage, RunStats, RunStatsQuery, SignalAccepted, StartRun, Trace, TraceQuery,
    };
    use crate::scripts::{
        CheckRequest, CheckResult, Script, ScriptFilter, ScriptPage, TestRequest, TestResult,
    };
    use crate::tasks::{
        CompleteTask, DeferTask, FailTask, HeartbeatAck, RejectTask, TaskBatch, TaskQuery,
    };
    use crate::toolsets::{
        CreateToolset, Credentials, DeclarationsQuery, DeclarationsText, SearchQuery,
        ToolSearchResults, Toolset, ToolsetList,
    };
    use crate::webhooks::{CreateSubscription, Subscription, SubscriptionList};

    // ---------------------------------------------------------------------------------------
    // Toolsets

    /// Register and pin a toolset
    ///
    /// Connects to the MCP servers, lists their tools, pins their schemas and policies, and
    /// returns the toolset with its declarations, for the model's prompt. Content-addressed:
    /// registering the same toolset again returns it (200).
    #[utoipa::path(
        post, path = "/v1/toolsets", tag = "toolsets", operation_id = "createToolset",
        request_body = CreateToolset,
        responses(
            (status = 201, description = "Registered.", body = Toolset),
            (status = 200, description = "Already registered.", body = Toolset),
            (status = 422, description = "Refused: an invalid schema, a duplicate name, a transport this server does not allow (`invalid-toolset`).", body = Problem, content_type = "application/problem+json"),
            (status = 502, description = "An MCP server cannot be reached (`toolset-unavailable`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn create_toolset() {}

    /// List the toolsets
    ///
    /// The toolsets the tenant registered, by id, with their servers and tool names.
    #[utoipa::path(
        get, path = "/v1/toolsets", tag = "toolsets", operation_id = "listToolsets",
        responses((status = 200, description = "Every toolset of the tenant.", body = ToolsetList))
    )]
    pub(super) fn list_toolsets() {}

    /// The toolset's declarations
    #[utoipa::path(
        get, path = "/v1/toolsets/{id}/declarations", tag = "toolsets", operation_id = "getToolsetDeclarations",
        params(("id" = String, Path, description = "The toolset's id."), DeclarationsQuery),
        responses(
            (status = 200, description = "The declarations in the requested format.", body = DeclarationsText),
            (status = 404, description = "No such toolset.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn get_toolset_declarations() {}

    /// Search the toolset's tools (`search_tools`)
    ///
    /// For toolsets too large for the prompt: the declarations of the tools matching a query.
    #[utoipa::path(
        get, path = "/v1/toolsets/{id}/search", tag = "toolsets", operation_id = "searchTools",
        params(("id" = String, Path, description = "The toolset's id."), SearchQuery),
        responses(
            (status = 200, description = "The matching tools, best first.", body = ToolSearchResults),
            (status = 404, description = "No such toolset.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn search_tools() {}

    /// Set or renew the toolset's credentials
    #[utoipa::path(
        put, path = "/v1/toolsets/{id}/credentials", tag = "toolsets", operation_id = "setToolsetCredentials",
        params(("id" = String, Path, description = "The toolset's id.")),
        request_body = Credentials,
        responses(
            (status = 204, description = "Stored. Calls parked by `credentials.expired` are ready again."),
            (status = 404, description = "No such toolset.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn set_toolset_credentials() {}

    // ---------------------------------------------------------------------------------------
    // Scripts

    /// Type-check a script (`check_script`)
    ///
    /// Against the sandbox lib and the toolset's declarations, in strict mode, with the checks
    /// at start and the checker's own rules. Nothing runs.
    #[utoipa::path(
        post, path = "/v1/check", tag = "scripts", operation_id = "checkScript",
        request_body = CheckRequest,
        responses(
            (status = 200, description = "The diagnostics; `ok` when none is an error.", body = CheckResult),
            (status = 404, description = "No such toolset.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn check_script() {}

    /// Run a scenario in mock mode (`test_script`)
    ///
    /// In virtual time, against a cassette, mock handlers and static responses. No tool is
    /// really called.
    #[utoipa::path(
        post, path = "/v1/test", tag = "scripts", operation_id = "testScript",
        request_body = TestRequest,
        responses(
            (status = 200, description = "The outcome and the expectations that do not hold.", body = TestResult),
            (status = 404, description = "No such toolset.", body = Problem, content_type = "application/problem+json"),
            (status = 422, description = "The script was refused at start (`invalid-script`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn test_script() {}

    /// List the distinct scripts executed
    #[utoipa::path(
        get, path = "/v1/scripts", tag = "reading", operation_id = "listScripts",
        params(ScriptFilter),
        responses((status = 200, description = "A page of scripts, most recently run first.", body = ScriptPage))
    )]
    pub(super) fn list_scripts() {}

    /// One script's source
    #[utoipa::path(
        get, path = "/v1/scripts/{digest}", tag = "reading", operation_id = "getScript",
        params(("digest" = String, Path, description = "SHA-256 of the source, lowercase hex.")),
        responses(
            (status = 200, description = "The script.", body = Script),
            (status = 404, description = "No such script.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn get_script() {}

    // ---------------------------------------------------------------------------------------
    // Runs

    /// Start a run (`run_script`)
    ///
    /// Compiles, checks and type-checks the script, then starts the run. With `wait`, answers
    /// when the run ends or `wait` passes, whichever comes first.
    #[utoipa::path(
        post, path = "/v1/runs", tag = "runs", operation_id = "startRun",
        params(("Idempotency-Key" = Option<String>, Header, description = "A request repeated with the same key returns the run the first one started (200).")),
        request_body = StartRun,
        responses(
            (status = 201, description = "Started.", body = Run),
            (status = 200, description = "Started earlier with the same `Idempotency-Key`.", body = Run),
            (status = 404, description = "No such toolset.", body = Problem, content_type = "application/problem+json"),
            (status = 413, description = "The input is too large (`payload-too-large`).", body = Problem, content_type = "application/problem+json"),
            (status = 422, description = "The script was refused before any run exists (`invalid-script`), with `diagnostics`.", body = Problem, content_type = "application/problem+json"),
            (status = 429, description = "The tenant already has its maximum of active runs (`quota-exceeded`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn start_run() {}

    /// List runs (`list_runs`)
    #[utoipa::path(
        get, path = "/v1/runs", tag = "runs", operation_id = "listRuns",
        params(RunFilter),
        responses(
            (status = 200, description = "A page of runs, in the order of `sort`.", body = RunPage),
            (status = 400, description = "`q` cannot be read (`invalid-query`, with `query`), or another parameter is invalid.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn list_runs() {}

    /// Statistics of the runs a query matches
    ///
    /// Counts by status, a series over time, durations and facets of the runs started in the
    /// window; the runs waiting now by what they wait for, and the quarantined ones.
    #[utoipa::path(
        get, path = "/v1/runs/stats", tag = "runs", operation_id = "getRunStats",
        params(RunStatsQuery),
        responses(
            (status = 200, description = "The statistics.", body = RunStats),
            (status = 400, description = "`q` cannot be read (`invalid-query`, with `query`), or the window is invalid.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn get_run_stats() {}

    /// Failures grouped by cause
    ///
    /// The failed, cancelled and aborted runs a query matches, ended in the window, grouped
    /// by fingerprint: across scripts, the runs that ended with the same error.
    #[utoipa::path(
        get, path = "/v1/runs/failures", tag = "runs", operation_id = "listRunFailures",
        params(FailureQuery),
        responses(
            (status = 200, description = "The groups, the largest first.", body = FailureGroupPage),
            (status = 400, description = "`q` cannot be read (`invalid-query`, with `query`), or the window is invalid.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn list_run_failures() {}

    /// A run's state (`get_run`)
    #[utoipa::path(
        get, path = "/v1/runs/{id}", tag = "runs", operation_id = "getRun",
        params(("id" = String, Path, description = "The run id."), GetRunQuery),
        responses(
            (status = 200, description = "The run.", body = Run),
            (status = 404, description = "No such run.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn get_run() {}

    /// Erase a run
    ///
    /// Its journal, its archive, its read model rows and its payload references. An active run
    /// is cancelled first.
    #[utoipa::path(
        delete, path = "/v1/runs/{id}", tag = "runs", operation_id = "deleteRun",
        params(("id" = String, Path, description = "The run id.")),
        responses(
            (status = 204, description = "Erased."),
            (status = 404, description = "No such run.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn delete_run() {}

    /// The run's trace, for a model
    #[utoipa::path(
        get, path = "/v1/runs/{id}/trace", tag = "runs", operation_id = "getRunTrace",
        params(("id" = String, Path, description = "The run id."), TraceQuery),
        responses(
            (status = 200, description = "The trace.", body = Trace),
            (status = 404, description = "No such run.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn get_run_trace() {}

    /// One journal entry in full
    ///
    /// Every payload of the entry has its value, whatever its size.
    #[utoipa::path(
        get, path = "/v1/runs/{id}/entries/{idx}", tag = "runs", operation_id = "getRunEntry",
        params(("id" = String, Path, description = "The run id."), ("idx" = u64, Path, description = "The entry's index, from 0.")),
        responses(
            (status = 200, description = "The entry.", body = Entry),
            (status = 404, description = "No such run or entry.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn get_run_entry() {}

    /// Stream the run's journal (server-sent events)
    ///
    /// Each event's `data` is a `StreamEvent`: the committed entries from `from`, then, with
    /// `follow=1`, each new entry as it is committed, and `end` when the run has ended.
    #[utoipa::path(
        get, path = "/v1/runs/{id}/events", tag = "runs", operation_id = "streamRunEvents",
        params(
            ("id" = String, Path, description = "The run id."),
            EventsQuery,
            ("Last-Event-ID" = Option<String>, Header, description = "Resume after this entry: the same as `from` set to it plus one."),
        ),
        responses(
            (status = 200, description = "A stream of server-sent events; each `data` is a `StreamEvent`.", body = StreamEvent, content_type = "text/event-stream"),
            (status = 404, description = "No such run.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn stream_run_events() {}

    /// Send a signal (`signal_run`)
    ///
    /// The body, any JSON value, is the signal's payload (`null` when empty). It resolves the
    /// script's `signal(name)`, now or the next time the script waits for that name.
    #[utoipa::path(
        post, path = "/v1/runs/{id}/signals/{name}", tag = "runs", operation_id = "signalRun",
        params(
            ("id" = String, Path, description = "The run id."),
            ("name" = String, Path, description = "The signal's name."),
            ("Idempotency-Key" = Option<String>, Header, description = "The same key sent twice to a run delivers one signal."),
        ),
        request_body(content = Option<Value>, description = "The payload: any JSON value. `null` when the body is empty.", content_type = "application/json"),
        responses(
            (status = 202, description = "In the run's mailbox.", body = SignalAccepted),
            (status = 404, description = "No such run.", body = Problem, content_type = "application/problem+json"),
            (status = 409, description = "The run has ended (`run-ended`).", body = Problem, content_type = "application/problem+json"),
            (status = 413, description = "The payload is too large (`payload-too-large`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn signal_run() {}

    /// Cancel a run (`cancel_run`)
    #[utoipa::path(
        post, path = "/v1/runs/{id}/cancel", tag = "runs", operation_id = "cancelRun",
        params(("id" = String, Path, description = "The run id.")),
        request_body(content = Option<CancelRun>, description = "Optional."),
        responses(
            (status = 202, description = "Cancellation requested; the run's state, after `wait` if given.", body = Run),
            (status = 404, description = "No such run.", body = Problem, content_type = "application/problem+json"),
            (status = 409, description = "The run has ended (`run-ended`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn cancel_run() {}

    /// Fork a run with a corrected script (`retry_run`)
    #[utoipa::path(
        post, path = "/v1/runs/{id}/fork", tag = "runs", operation_id = "forkRun",
        params(
            ("id" = String, Path, description = "The parent run's id."),
            ("Idempotency-Key" = Option<String>, Header, description = "A request repeated with the same key returns the fork the first one started (200)."),
        ),
        request_body = ForkRun,
        responses(
            (status = 201, description = "The fork, a new run.", body = Run),
            (status = 200, description = "Started earlier with the same `Idempotency-Key`.", body = Run),
            (status = 404, description = "No such run.", body = Problem, content_type = "application/problem+json"),
            (status = 422, description = "The corrected script was refused (`invalid-script`).", body = Problem, content_type = "application/problem+json"),
            (status = 429, description = "Too many active runs (`quota-exceeded`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn fork_run() {}

    /// Export the journal, or a cassette
    ///
    /// A document of the stored format v1 (`archi/02-journal.md`).
    #[utoipa::path(
        get, path = "/v1/runs/{id}/journal", tag = "runs", operation_id = "exportRunJournal",
        params(("id" = String, Path, description = "The run id."), JournalQuery),
        responses(
            (status = 200, description = "The journal document or the cassette, stored format v1.", body = Object),
            (status = 404, description = "No such run.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn export_run_journal() {}

    /// Take a run out of quarantine
    ///
    /// The run is activated again, typically after a fix of the platform.
    #[utoipa::path(
        post, path = "/v1/runs/{id}/release", tag = "runs", operation_id = "releaseRun",
        params(("id" = String, Path, description = "The run id.")),
        responses(
            (status = 200, description = "Released.", body = Run),
            (status = 404, description = "No such run.", body = Problem, content_type = "application/problem+json"),
            (status = 409, description = "The run is not quarantined (`not-quarantined`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn release_run() {}

    /// Give up a quarantined run
    ///
    /// The host ends it: `run_aborted` with `non_determinism`, or `internal` after repeated
    /// failures.
    #[utoipa::path(
        post, path = "/v1/runs/{id}/give-up", tag = "runs", operation_id = "giveUpRun",
        params(("id" = String, Path, description = "The run id.")),
        responses(
            (status = 200, description = "Given up; the run is aborted.", body = Run),
            (status = 404, description = "No such run.", body = Problem, content_type = "application/problem+json"),
            (status = 409, description = "The run is not quarantined (`not-quarantined`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn give_up_run() {}

    /// Set or renew the run's credentials
    #[utoipa::path(
        put, path = "/v1/runs/{id}/credentials", tag = "runs", operation_id = "setRunCredentials",
        params(("id" = String, Path, description = "The run id.")),
        request_body = Credentials,
        responses(
            (status = 204, description = "Stored. The run's calls parked by `credentials.expired` are ready again."),
            (status = 404, description = "No such run.", body = Problem, content_type = "application/problem+json"),
            (status = 409, description = "The run has ended (`run-ended`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn set_run_credentials() {}

    // ---------------------------------------------------------------------------------------
    // Host-tool tasks

    /// Claim host-tool tasks (long-poll)
    #[utoipa::path(
        get, path = "/v1/tasks", tag = "tasks", operation_id = "claimTasks",
        params(TaskQuery),
        responses((status = 200, description = "The tasks claimed; none when `wait` passed.", body = TaskBatch))
    )]
    pub(super) fn claim_tasks() {}

    /// Keep a claim alive
    #[utoipa::path(
        post, path = "/v1/tasks/{token}/heartbeat", tag = "tasks", operation_id = "heartbeatTask",
        params(("token" = String, Path, description = "The claim's token.")),
        responses(
            (status = 200, description = "Extended.", body = HeartbeatAck),
            (status = 410, description = "The task no longer exists: stop working on it (`task-gone`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn heartbeat_task() {}

    /// Answer later
    ///
    /// The task waits for `/complete` or `/fail` with the same token, without heartbeats, up to
    /// its timeout.
    #[utoipa::path(
        post, path = "/v1/tasks/{token}/defer", tag = "tasks", operation_id = "deferTask",
        params(("token" = String, Path, description = "The claim's token.")),
        request_body(content = Option<DeferTask>, description = "Optional."),
        responses(
            (status = 204, description = "Deferred."),
            (status = 410, description = "The task no longer exists (`task-gone`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn defer_task() {}

    /// Complete a task with its result
    #[utoipa::path(
        post, path = "/v1/tasks/{token}/complete", tag = "tasks", operation_id = "completeTask",
        params(("token" = String, Path, description = "The claim's token.")),
        request_body = CompleteTask,
        responses(
            (status = 204, description = "Delivered to the run."),
            (status = 410, description = "The task no longer exists; the result is dropped (`task-gone`).", body = Problem, content_type = "application/problem+json"),
            (status = 413, description = "The result is too large (`payload-too-large`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn complete_task() {}

    /// Fail a task
    #[utoipa::path(
        post, path = "/v1/tasks/{token}/fail", tag = "tasks", operation_id = "failTask",
        params(("token" = String, Path, description = "The claim's token.")),
        request_body = FailTask,
        responses(
            (status = 204, description = "Recorded: delivered as a `ToolError`, or retried."),
            (status = 410, description = "The task no longer exists (`task-gone`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn fail_task() {}

    /// Approve a task that waits for the harness
    ///
    /// A task under the `approval` policy is sent; a task parked by a schema change continues
    /// against the new schema.
    #[utoipa::path(
        post, path = "/v1/tasks/{id}/approve", tag = "tasks", operation_id = "approveTask",
        params(("id" = String, Path, description = "The task's id, from the run's state or the webhook.")),
        responses(
            (status = 204, description = "The task is ready."),
            (status = 404, description = "No such task.", body = Problem, content_type = "application/problem+json"),
            (status = 409, description = "The task does not wait for the harness (`task-not-waiting`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn approve_task() {}

    /// Reject a task that waits for the harness
    ///
    /// The call fails with a `ToolError`.
    #[utoipa::path(
        post, path = "/v1/tasks/{id}/reject", tag = "tasks", operation_id = "rejectTask",
        params(("id" = String, Path, description = "The task's id.")),
        request_body(content = Option<RejectTask>, description = "Optional."),
        responses(
            (status = 204, description = "The call failed."),
            (status = 404, description = "No such task.", body = Problem, content_type = "application/problem+json"),
            (status = 409, description = "The task does not wait for the harness (`task-not-waiting`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn reject_task() {}

    // ---------------------------------------------------------------------------------------
    // Webhooks

    /// Inbound webhook: becomes a signal
    ///
    /// At the URL a script got from `webhook(name)`. A JSON body is the signal's payload; any
    /// other body becomes the payload as a string; an empty body is `null`. The first of
    /// `Idempotency-Key`, `webhook-id` and `X-GitHub-Delivery` present is the signal's
    /// idempotency key.
    #[utoipa::path(
        post, path = "/hooks/{token}", tag = "webhooks", operation_id = "postHook",
        params(("token" = String, Path, description = "The unguessable token of the URL.")),
        request_body(content = Option<Value>, description = "Anything: the signal's payload.", content_type = "application/json"),
        security(()),
        responses(
            (status = 202, description = "Delivered to the run as a signal."),
            (status = 404, description = "No such webhook.", body = Problem, content_type = "application/problem+json"),
            (status = 409, description = "The run has ended (`run-ended`).", body = Problem, content_type = "application/problem+json"),
            (status = 413, description = "The body is too large (`payload-too-large`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn post_hook() {}

    /// Subscribe to lifecycle events
    ///
    /// The answer holds the signing secret, once.
    #[utoipa::path(
        post, path = "/v1/subscriptions", tag = "webhooks", operation_id = "createSubscription",
        request_body = CreateSubscription,
        responses((status = 201, description = "Subscribed.", body = Subscription))
    )]
    pub(super) fn create_subscription() {}

    /// List subscriptions
    #[utoipa::path(
        get, path = "/v1/subscriptions", tag = "webhooks", operation_id = "listSubscriptions",
        responses((status = 200, description = "Every subscription of the tenant, without secrets.", body = SubscriptionList))
    )]
    pub(super) fn list_subscriptions() {}

    /// Delete a subscription
    #[utoipa::path(
        delete, path = "/v1/subscriptions/{id}", tag = "webhooks", operation_id = "deleteSubscription",
        params(("id" = String, Path, description = "The subscription's id.")),
        responses(
            (status = 204, description = "Deleted."),
            (status = 404, description = "No such subscription.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn delete_subscription() {}

    // ---------------------------------------------------------------------------------------
    // Reading

    /// Latency, errors and retries per tool
    #[utoipa::path(
        get, path = "/v1/tools/stats", tag = "reading", operation_id = "getToolStats",
        params(ToolStatsFilter),
        responses((status = 200, description = "Statistics per tool over the window.", body = ToolStatsPage))
    )]
    pub(super) fn get_tool_stats() {}

    /// A payload's value
    #[utoipa::path(
        get, path = "/v1/payloads/{digest}", tag = "reading", operation_id = "getPayload",
        params(("digest" = String, Path, description = "SHA-256 of the canonical JSON, lowercase hex."), PayloadQuery),
        responses(
            (status = 200, description = "The JSON value itself, in canonical form.", body = Value),
            (status = 403, description = "The key's role or allowed labels do not cover this request (`forbidden`): a key limited to labels must name a run.", body = Problem, content_type = "application/problem+json"),
            (status = 404, description = "No such payload for this tenant, or not in the journal of the run given.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn get_payload() {}

    // ---------------------------------------------------------------------------------------
    // Organization

    /// Who calls
    ///
    /// The tenant, the principal (an API key, or a member with their email, name and picture),
    /// the grant, and the permissions it holds; for a member, the organization of the session
    /// and every organization of the user. Over the embedded server's socket: the operator of
    /// the tenant `local`. Any caller, even a member without organization.
    #[utoipa::path(
        get, path = "/v1/whoami", tag = "organization", operation_id = "whoami",
        responses(
            (status = 200, description = "Who calls.", body = Identity),
            (status = 502, description = "The identity provider is unavailable (`identity-unavailable`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn whoami() {}

    /// Create an organization
    ///
    /// For a member, even without organization: the organization in the identity provider with
    /// the caller as a member, then its tenant and the caller's `owner` grant. Switch to it with
    /// `POST /auth/session`.
    #[utoipa::path(
        post, path = "/v1/organizations", tag = "organization", operation_id = "createOrganization",
        request_body = CreateOrganization,
        responses(
            (status = 201, description = "Created; the caller is its owner.", body = Organization),
            (status = 502, description = "The identity provider is unavailable (`identity-unavailable`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn create_organization() {}

    /// Change the organization
    ///
    /// Its name and its default role. Requires `organization:write`.
    #[utoipa::path(
        patch, path = "/v1/organization", tag = "organization", operation_id = "updateOrganization",
        request_body = UpdateOrganization,
        responses(
            (status = 200, description = "The organization as it now is.", body = OrganizationSettings),
            (status = 502, description = "The identity provider is unavailable (`identity-unavailable`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn update_organization() {}

    /// The invitations sent to the caller
    ///
    /// The pending invitations to the caller's email, with their organizations. For a member,
    /// even without organization.
    #[utoipa::path(
        get, path = "/v1/invitations", tag = "organization", operation_id = "listMyInvitations",
        responses(
            (status = 200, description = "The pending invitations.", body = InvitationList),
            (status = 502, description = "The identity provider is unavailable (`identity-unavailable`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn list_my_invitations() {}

    /// Accept an invitation
    ///
    /// Joins the invitation's organization; its pending grant becomes the caller's. Switch to
    /// the organization with `POST /auth/session`. For a member, even without organization.
    #[utoipa::path(
        post, path = "/v1/invitations/{id}/accept", tag = "organization", operation_id = "acceptInvitation",
        params(("id" = String, Path, description = "The invitation's id.")),
        responses(
            (status = 200, description = "Accepted.", body = Invitation),
            (status = 404, description = "No pending invitation with this id for the caller.", body = Problem, content_type = "application/problem+json"),
            (status = 502, description = "The identity provider is unavailable (`identity-unavailable`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn accept_invitation() {}

    /// List the members
    ///
    /// The organization's memberships, each with its grant. Requires `members:read`.
    #[utoipa::path(
        get, path = "/v1/organization/members", tag = "organization", operation_id = "listMembers",
        responses(
            (status = 200, description = "Every member.", body = MemberList),
            (status = 502, description = "The identity provider is unavailable (`identity-unavailable`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn list_members() {}

    /// Set a member's grant
    ///
    /// Their role and allowed labels. Requires `members:manage`, and every permission of the
    /// role given and of the member's current role.
    #[utoipa::path(
        put, path = "/v1/organization/members/{user}/grant", tag = "organization", operation_id = "setMemberGrant",
        params(("user" = String, Path, description = "The member's user id.")),
        request_body = SetGrant,
        responses(
            (status = 200, description = "The member's grant.", body = Grant),
            (status = 404, description = "No such member, or no such role.", body = Problem, content_type = "application/problem+json"),
            (status = 409, description = "The organization would have no owner (`last-owner`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn set_member_grant() {}

    /// Remove a member
    ///
    /// Deletes the member's grant, then their membership. Requires `members:manage`, and every
    /// permission of the member's role.
    #[utoipa::path(
        delete, path = "/v1/organization/members/{user}", tag = "organization", operation_id = "removeMember",
        params(("user" = String, Path, description = "The member's user id.")),
        responses(
            (status = 204, description = "Removed."),
            (status = 404, description = "No such member.", body = Problem, content_type = "application/problem+json"),
            (status = 409, description = "The organization would have no owner (`last-owner`).", body = Problem, content_type = "application/problem+json"),
            (status = 502, description = "The identity provider is unavailable (`identity-unavailable`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn remove_member() {}

    /// List the organization's invitations
    ///
    /// Each with its pending grant. Requires `members:read`.
    #[utoipa::path(
        get, path = "/v1/organization/invitations", tag = "organization", operation_id = "listInvitations",
        responses(
            (status = 200, description = "Every invitation of the organization.", body = InvitationList),
            (status = 502, description = "The identity provider is unavailable (`identity-unavailable`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn list_invitations() {}

    /// Invite someone
    ///
    /// The invitation by email, and its pending grant, which the invitee gets on accepting it.
    /// Requires `members:invite`, and every permission of the role given.
    #[utoipa::path(
        post, path = "/v1/organization/invitations", tag = "organization", operation_id = "createInvitation",
        request_body = CreateInvitation,
        responses(
            (status = 201, description = "Sent.", body = Invitation),
            (status = 404, description = "No such role.", body = Problem, content_type = "application/problem+json"),
            (status = 502, description = "The identity provider is unavailable (`identity-unavailable`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn create_invitation() {}

    /// Send an invitation again
    ///
    /// Requires `members:invite`.
    #[utoipa::path(
        post, path = "/v1/organization/invitations/{id}/resend", tag = "organization", operation_id = "resendInvitation",
        params(("id" = String, Path, description = "The invitation's id.")),
        responses(
            (status = 200, description = "Sent again, with a new expiry.", body = Invitation),
            (status = 404, description = "No such invitation, or one already accepted or revoked.", body = Problem, content_type = "application/problem+json"),
            (status = 502, description = "The identity provider is unavailable (`identity-unavailable`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn resend_invitation() {}

    /// Revoke an invitation
    ///
    /// And delete its pending grant. Requires `members:invite`.
    #[utoipa::path(
        delete, path = "/v1/organization/invitations/{id}", tag = "organization", operation_id = "revokeInvitation",
        params(("id" = String, Path, description = "The invitation's id.")),
        responses(
            (status = 204, description = "Revoked."),
            (status = 404, description = "No such pending invitation.", body = Problem, content_type = "application/problem+json"),
            (status = 502, description = "The identity provider is unavailable (`identity-unavailable`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn revoke_invitation() {}

    // ---------------------------------------------------------------------------------------
    // Access

    /// The catalog of permissions
    ///
    /// Every permission, with what it lets the caller do. Any caller.
    #[utoipa::path(
        get, path = "/v1/permissions", tag = "access", operation_id = "listPermissions",
        responses((status = 200, description = "The catalog.", body = PermissionList))
    )]
    pub(super) fn list_permissions() {}

    /// List the roles
    ///
    /// The built-in roles, then the organization's own, with their permissions. Requires
    /// `roles:read`.
    #[utoipa::path(
        get, path = "/v1/roles", tag = "access", operation_id = "listRoles",
        responses((status = 200, description = "Every role of the organization.", body = RoleList))
    )]
    pub(super) fn list_roles() {}

    /// Create or change a role
    ///
    /// A role of the organization: its name is 1 to 64 lowercase letters, digits, `-` and `_`,
    /// and never a built-in name or `operator`. A change applies at once to every principal
    /// holding the role. Requires `roles:write`, and every permission the role gives.
    #[utoipa::path(
        put, path = "/v1/roles/{name}", tag = "access", operation_id = "putRole",
        params(("name" = String, Path, description = "The role's name.")),
        request_body = PutRole,
        responses(
            (status = 201, description = "Created.", body = Role),
            (status = 200, description = "Changed.", body = Role),
        )
    )]
    pub(super) fn put_role() {}

    /// Delete a role
    ///
    /// Only a role no grant points to. Requires `roles:write`.
    #[utoipa::path(
        delete, path = "/v1/roles/{name}", tag = "access", operation_id = "deleteRole",
        params(("name" = String, Path, description = "The role's name.")),
        responses(
            (status = 204, description = "Deleted."),
            (status = 404, description = "No such role of the organization.", body = Problem, content_type = "application/problem+json"),
            (status = 409, description = "A grant points to the role (`role-in-use`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn delete_role() {}

    /// List the API keys
    ///
    /// The tenant's keys, without their secret, each with its role and allowed labels.
    /// Requires `api_keys:read`.
    #[utoipa::path(
        get, path = "/v1/api-keys", tag = "access", operation_id = "listApiKeys",
        responses((status = 200, description = "Every key of the tenant, revoked ones included.", body = ApiKeyList))
    )]
    pub(super) fn list_api_keys() {}

    /// Create an API key
    ///
    /// With a name, a role and allowed labels. The operator may name another tenant. Requires
    /// `api_keys:write`, and every permission of the role given.
    #[utoipa::path(
        post, path = "/v1/api-keys", tag = "access", operation_id = "createApiKey",
        request_body = CreateApiKey,
        responses((status = 201, description = "Created; the answer holds the key, once.", body = ApiKey))
    )]
    pub(super) fn create_api_key() {}

    /// Revoke an API key
    ///
    /// Requires `api_keys:write`.
    #[utoipa::path(
        delete, path = "/v1/api-keys/{id}", tag = "access", operation_id = "revokeApiKey",
        params(("id" = String, Path, description = "The key's id.")),
        responses(
            (status = 204, description = "Revoked."),
            (status = 404, description = "No such key.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn revoke_api_key() {}

    // ---------------------------------------------------------------------------------------
    // Server

    /// What this server is
    ///
    /// Any caller.
    #[utoipa::path(
        get, path = "/v1/server", tag = "server", operation_id = "getServer",
        responses((status = 200, description = "The server's version, epochs and roles.", body = ServerInfo))
    )]
    pub(super) fn get_server() {}

    /// Stop the embedded server
    ///
    /// Only the embedded server stops this way, once its calls in flight are committed: the
    /// CLI uses it to replace a server of another version. Other servers answer 403. Only the
    /// operator.
    #[utoipa::path(
        post, path = "/v1/server/stop", tag = "server", operation_id = "stopServer",
        responses((status = 202, description = "Stopping."))
    )]
    pub(super) fn stop_server() {}

    // ---------------------------------------------------------------------------------------
    // Sign-in

    /// How to sign in
    ///
    /// The sign-in methods, and whether sign-ups go through the waitlist.
    #[utoipa::path(
        get, path = "/auth/config", tag = "auth", operation_id = "getAuthConfig",
        security(()),
        responses(
            (status = 200, description = "The sign-in methods.", body = AuthConfig),
            (status = 404, description = "This server has no sign-in: use an API key.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn get_auth_config() {}

    /// Sign in with GitHub
    ///
    /// Redirects to the identity provider, with a random `state` and the page's invitation
    /// token kept in the cookie `rewind_oauth` for 5 minutes.
    #[utoipa::path(
        get, path = "/auth/github", tag = "auth", operation_id = "signInWithGithub",
        security(()),
        params(GithubSignIn),
        responses(
            (status = 302, description = "To the identity provider.", headers(
                ("Location" = String, description = "The provider's authorization URL."),
                ("Set-Cookie" = String, description = "`rewind_oauth`."),
            )),
        )
    )]
    pub(super) fn sign_in_with_github() {}

    /// The identity provider's redirect
    ///
    /// Checks `state`, exchanges the code, keeps the outcome in the session cookie, and
    /// redirects to the dashboard, which asks `POST /auth/session` where the sign-in stands.
    /// When the sign-in did not go through, the redirect names why with a code
    /// (`state_mismatch`, `provider_error`, `sign_in_failed`), and the session cookie is left
    /// as it was.
    #[utoipa::path(
        get, path = "/auth/callback", tag = "auth", operation_id = "authCallback",
        security(()),
        params(Callback),
        responses(
            (status = 302, description = "To the dashboard: `/#/`, or `/?auth_error=<code>#/` when the sign-in did not go through.", headers(
                ("Location" = String, description = "`/#/`, or `/?auth_error=<code>#/`."),
                ("Set-Cookie" = String, description = "`rewind_session` when signed in or at a step; `rewind_oauth` cleared."),
            )),
        )
    )]
    pub(super) fn auth_callback() {}

    /// Send a code by email
    ///
    /// A 6-digit code, to give to `POST /auth/magic-auth/verify`. The same answer whether the
    /// email has an account or not.
    #[utoipa::path(
        post, path = "/auth/magic-auth", tag = "auth", operation_id = "sendMagicCode",
        security(()),
        request_body = MagicAuthRequest,
        responses(
            (status = 202, description = "Sent."),
            (status = 502, description = "The identity provider is unavailable (`identity-unavailable`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn send_magic_code() {}

    /// Sign in with the code
    #[utoipa::path(
        post, path = "/auth/magic-auth/verify", tag = "auth", operation_id = "verifyMagicCode",
        security(()),
        request_body = MagicAuthVerification,
        responses(
            (status = 200, description = "Where the sign-in stands; the session is in the cookie.", body = AuthState, headers(
                ("Set-Cookie" = String, description = "`rewind_session`."),
            )),
            (status = 400, description = "The code is wrong or expired, or the invitation unknown (`invalid-request`).", body = Problem, content_type = "application/problem+json"),
            (status = 502, description = "The identity provider is unavailable (`identity-unavailable`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn verify_magic_code() {}

    /// Verify the email
    ///
    /// The code sent after `email_verification_required`.
    #[utoipa::path(
        post, path = "/auth/email-verification", tag = "auth", operation_id = "verifyEmail",
        security(("session" = [])),
        request_body = EmailVerification,
        responses(
            (status = 200, description = "Where the sign-in stands.", body = AuthState),
            (status = 400, description = "The code is wrong or expired (`invalid-request`).", body = Problem, content_type = "application/problem+json"),
            (status = 502, description = "The identity provider is unavailable (`identity-unavailable`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn verify_email() {}

    /// Choose an organization
    ///
    /// After `organization_selection_required`.
    #[utoipa::path(
        post, path = "/auth/organization", tag = "auth", operation_id = "selectOrganization",
        security(("session" = [])),
        request_body = OrganizationSelection,
        responses(
            (status = 200, description = "Where the sign-in stands.", body = AuthState),
            (status = 403, description = "The user is not a member of this organization (`forbidden`).", body = Problem, content_type = "application/problem+json"),
            (status = 502, description = "The identity provider is unavailable (`identity-unavailable`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn select_organization() {}

    /// The session
    ///
    /// A valid access token, refreshed when less than a minute is left; with `organizationId`,
    /// for that organization. A member's first session in an organization writes their grant:
    /// the pending grant of the invitation they accepted, else the organization's default role.
    #[utoipa::path(
        post, path = "/auth/session", tag = "auth", operation_id = "getSession",
        security(("session" = [])),
        request_body = SessionRequest,
        responses(
            (status = 200, description = "Where the sign-in stands.", body = AuthState),
            (status = 403, description = "The user is not a member of this organization (`forbidden`).", body = Problem, content_type = "application/problem+json"),
            (status = 502, description = "The identity provider is unavailable (`identity-unavailable`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn get_session() {}

    /// Join the waitlist
    ///
    /// Sign-ups are closed: the operator's approval sends an invitation.
    #[utoipa::path(
        post, path = "/auth/waitlist", tag = "auth", operation_id = "joinWaitlist",
        security(()),
        request_body = WaitlistRequest,
        responses(
            (status = 202, description = "On the waitlist."),
            (status = 502, description = "The identity provider is unavailable (`identity-unavailable`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn join_waitlist() {}

    /// Sign out
    ///
    /// Revokes the session in the identity provider and clears the cookie.
    #[utoipa::path(
        post, path = "/auth/logout", tag = "auth", operation_id = "signOut",
        security(("session" = [])),
        responses(
            (status = 204, description = "Signed out.", headers(
                ("Set-Cookie" = String, description = "`rewind_session`, cleared."),
            )),
            (status = 502, description = "The identity provider could not revoke the session (`identity-unavailable`): the cookie is cleared anyway, and the session expires at the provider on its own.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn sign_out() {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::OPERATIONS;

    /// The document and the routes name the same operations.
    #[test]
    fn every_route_is_documented_and_nothing_else() {
        let document = serde_json::to_value(document()).unwrap();
        let mut documented: Vec<(String, String)> = Vec::new();
        for (path, item) in document["paths"].as_object().unwrap() {
            for method in item.as_object().unwrap().keys() {
                if ["get", "put", "post", "delete", "patch"].contains(&method.as_str()) {
                    documented.push((method.to_uppercase(), path.clone()));
                }
            }
        }
        documented.sort();
        let mut routes: Vec<(String, String)> = OPERATIONS
            .iter()
            .map(|(method, path)| ((*method).to_owned(), (*path).to_owned()))
            .collect();
        routes.sort();
        assert_eq!(documented, routes);
    }

    #[test]
    fn every_error_answer_is_problem_json_and_every_operation_has_an_id() {
        let document = serde_json::to_value(document()).unwrap();
        assert_eq!(document["openapi"], "3.1.0");
        let mut ids = Vec::new();
        for (path, item) in document["paths"].as_object().unwrap() {
            for (method, operation) in item.as_object().unwrap() {
                let id = operation["operationId"].as_str().unwrap();
                ids.push(id.to_owned());
                for (status, response) in operation["responses"].as_object().unwrap() {
                    if status.starts_with('4') || status.starts_with('5') {
                        let content = response["content"].as_object();
                        assert!(
                            content.is_some_and(
                                |content| content.contains_key("application/problem+json")
                            ),
                            "{method} {path} {status}"
                        );
                    }
                }
            }
        }
        let count = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), count, "operation ids are unique");
        let webhooks = document["webhooks"].as_object().unwrap();
        assert_eq!(webhooks.len(), WebhookEventType::ALL.len());
    }

    /// Generators refuse a document with a dangling reference.
    #[test]
    fn every_reference_resolves() {
        let document = serde_json::to_value(document()).unwrap();
        let schemas = document["components"]["schemas"].as_object().unwrap();
        let mut references = Vec::new();
        collect_references(&document, &mut references);
        assert!(!references.is_empty());
        for reference in references {
            let name = reference
                .strip_prefix("#/components/schemas/")
                .unwrap_or_else(|| panic!("unexpected reference {reference}"));
            assert!(schemas.contains_key(name), "{reference} does not resolve");
        }
    }

    fn collect_references(value: &serde_json::Value, references: &mut Vec<String>) {
        match value {
            serde_json::Value::Object(fields) => {
                for (key, field) in fields {
                    match (key.as_str(), field) {
                        ("$ref", serde_json::Value::String(reference)) => {
                            references.push(reference.clone());
                        }
                        _ => collect_references(field, references),
                    }
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    collect_references(item, references);
                }
            }
            _ => {}
        }
    }
}
