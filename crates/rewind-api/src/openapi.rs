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
use crate::calls::CallStreamMessage;
use crate::events::{ChangeEvent, StreamEvent};
use crate::namespaces::{QuotaKind, QuotaScope, SignedTokenClaims};
use crate::oauth::OAuthError;
use crate::problem::{self, Problem, QuotaReached};
use crate::webhooks::{
    WEBHOOK_ID, WEBHOOK_SIGNATURE, WEBHOOK_TIMESTAMP, WebhookEvent, WebhookEventType,
};

/// The API's document.
#[derive(Debug, OpenApi)]
#[openapi(
    info(
        title = "Rewind API",
        description = "The HTTP API /v1 of Rewind: action sets, workflows, runs, harness calls, \
            webhooks, reading, the organization, access, namespaces and the server; and `/auth`, \
            the dashboard's sign-in. Every request to /v1 carries a Bearer token, an API key \
            (`rewind_sk_…`), a member's access token from `/auth`, or an end user's token \
            (`rewind_nt_…`, or a JWT the customer signed with a key whose `kid` starts with \
            `rk_`), or a person's connection (`rewind_ct_…`), except over the embedded server's \
            unix socket and to the inbound signal URL. A connection names the organization a \
            request acts in with the header `Rewind-Organization`. What the caller may do is its \
            grant's, or its token's: every operation requires \
            one permission of the catalog (`GET /v1/permissions`), and answers 403 without it. \
            A key or a member names the namespace (the end user) a request acts for with the \
            header `Rewind-Namespace`: required to start a run, check or test a code, it \
            narrows a read to that namespace. An end user's token reaches its own namespace \
            only. `/auth` is for a browser: it \
            reads and writes the cookie `rewind_session`, which /v1 never reads. Errors are \
            `application/problem+json`, with a `detail` written for a model. Fields are \
            camelCase, enumerated values snake_case, times RFC 3339, and durations a number and \
            a unit (`\"30s\"`, `\"1h30m\"`) or a number of milliseconds. `/oauth` and \
            `/.well-known` are the OAuth authorization server of people's connections, in \
            OAuth's own formats."
    ),
    servers((url = "/", description = "The server that serves this document.")),
    paths(
        operations::create_action_set,
        operations::list_action_sets,
        operations::get_action_set_declarations,
        operations::search_actions,
        operations::create_workflow,
        operations::list_workflows,
        operations::get_workflow_versions,
        operations::record_tests,
        operations::get_test,
        operations::check_workflow,
        operations::test_workflow,
        operations::list_scripts,
        operations::get_script,
        operations::set_script_title,
        operations::start_run,
        operations::list_runs,
        operations::get_run_stats,
        operations::list_run_failures,
        operations::get_run,
        operations::delete_run,
        operations::get_run_transcript,
        operations::get_run_event,
        operations::stream_run_events,
        operations::stream_changes,
        operations::signal_run,
        operations::cancel_run,
        operations::fork_run,
        operations::export_run,
        operations::release_run,
        operations::give_up_run,
        operations::stream_calls,
        operations::defer_call,
        operations::complete_call,
        operations::fail_call,
        operations::approve_call,
        operations::reject_call,
        operations::post_hook,
        operations::create_subscription,
        operations::list_subscriptions,
        operations::delete_subscription,
        operations::get_action_stats,
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
        operations::list_namespaces,
        operations::get_namespace,
        operations::put_namespace,
        operations::patch_namespace,
        operations::delete_namespace,
        operations::get_namespace_defaults,
        operations::put_namespace_defaults,
        operations::list_namespace_names,
        operations::list_quota_alerts,
        operations::create_token,
        operations::revoke_namespace_tokens,
        operations::revoke_token,
        operations::list_token_keys,
        operations::create_token_key,
        operations::delete_token_key,
        operations::put_ceiling,
        operations::list_secrets,
        operations::put_secret,
        operations::delete_secret,
        operations::get_secret_usage,
        operations::list_namespace_secrets,
        operations::put_namespace_secret,
        operations::delete_namespace_secret,
        operations::get_namespace_secret_usage,
        operations::list_signing_keys,
        operations::rotate_signing_key,
        operations::get_jwks,
        operations::list_connections,
        operations::update_connection,
        operations::revoke_connection,
        operations::list_organization_connections,
        operations::remove_organization_connection,
        operations::get_consent,
        operations::answer_consent,
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
        operations::authorize,
        operations::register_client,
        operations::exchange_token,
        operations::revoke_oauth_token,
        operations::get_protected_resource,
        operations::get_protected_resource_mcp,
        operations::get_authorization_server,
    ),
    components(schemas(
        Problem,
        QuotaKind,
        QuotaReached,
        QuotaScope,
        StreamEvent,
        ChangeEvent,
        CallStreamMessage,
        WebhookEvent,
        WebhookEventType,
        SignedTokenClaims,
        OAuthError
    )),
    modifiers(&Conventions),
    security(("bearer" = [])),
    tags(
        (name = "action-sets", description = "Register and pin action sets: their MCP servers, their APIs from OpenAPI documents and how each authenticates its calls; their declarations."),
        (name = "workflows", description = "Workflows: named code with typed inputs, outputs and signals, which other workflows start and signal."),
        (name = "checks", description = "Type-check code, and run tests as test runs, in virtual time."),
        (name = "runs", description = "Start, follow, steer, fork and erase runs."),
        (name = "calls", description = "Harness calls: handed to the harness's SDK worker on its call stream, answered by their token, approved or rejected by the harness."),
        (name = "webhooks", description = "Signal URLs that become signals, and subscriptions to signed lifecycle events."),
        (name = "reading", description = "The workflows executed on the platform, action statistics and payloads, for the dashboard."),
        (name = "organization", description = "Who calls; the organizations, their members and invitations."),
        (name = "access", description = "The catalog of permissions, the roles and the API keys."),
        (name = "namespaces", description = "The end users of the organization: their limits, use and alerts, and their tokens."),
        (name = "secrets", description = "Named secrets of the organization and of each namespace, written and never read back, which runs attach when they start; the organization's signing keys and their public JWKS."),
        (name = "connections", description = "People's connections: the MCP clients, Claude above all, that act for a person in the organizations they opened; and the consent that makes one."),
        (name = "server", description = "The server itself."),
        (name = "auth", description = "The dashboard's sign-in, for a browser: GitHub or a code by email, the organization picker, the waitlist."),
        (name = "oauth", description = "The OAuth authorization server of people's connections, for MCP clients: discovery, authorization, registration, tokens and revocation, in OAuth's formats."),
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
                        "An API key of the tenant (`rewind_sk_…`), a member's access token from \
                         `/auth`, an end user's token: opaque (`rewind_nt_…`), or a JWT the \
                         customer signed with a key it registered (`kid` `rk_…`), or a person's \
                         connection (`rewind_ct_…`, with `Rewind-Organization`). The embedded \
                         server's unix socket takes none.",
                    ))
                    .build(),
            ),
        );
        components.add_security_scheme(
            "client",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Basic)
                    .description(Some(
                        "A confidential OAuth client's id and secret (`client_secret_basic`); \
                         a public client sends its `client_id` in the form instead.",
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
            // OAuth's endpoints declare their own answers, in OAuth's formats.
            let oauth = path.starts_with("/oauth/") || path.starts_with("/.well-known/");
            let namespaced = NAMESPACED.iter().any(|prefix| path.starts_with(prefix));
            let organized = path.starts_with("/v1/")
                && !UNORGANIZED.iter().any(|prefix| path.starts_with(prefix));
            for operation in operations_mut(item) {
                if namespaced {
                    add_namespace_header(operation);
                }
                if organized {
                    add_organization_header(operation);
                }
                if oauth {
                    continue;
                }
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

/// The paths whose operations act in a namespace: the header `Rewind-Namespace` names it for
/// a key or a member, and narrows what they read.
const NAMESPACED: [&str; 8] = [
    "/v1/runs",
    "/v1/changes",
    "/v1/check",
    "/v1/test",
    "/v1/scripts",
    "/v1/actions/stats",
    "/v1/payloads",
    "/v1/calls",
];

/// The paths under `/v1` that act in no tenant: a person's own connections and consents, and
/// the onboarding of a member without organization. Every other operation takes
/// `Rewind-Organization`.
const UNORGANIZED: [&str; 4] = [
    "/v1/connections",
    "/v1/oauth/",
    "/v1/invitations",
    "/v1/organizations",
];

/// Declares the optional header `Rewind-Organization`.
fn add_organization_header(operation: &mut utoipa::openapi::path::Operation) {
    operation.parameters.get_or_insert_with(Vec::new).push(
        ParameterBuilder::new()
            .name(crate::headers::ORGANIZATION)
            .parameter_in(ParameterIn::Header)
            .required(Required::False)
            .description(Some(
                "The organization (`org_…`) a connection's request acts in: required on \
                     the tenant's data with a connection's token (`rewind_ct_…`), which names \
                     no tenant. Another caller may repeat its own tenant; another is \
                     `forbidden`.",
            ))
            .schema(Some(utoipa::openapi::schema::Object::with_type(
                utoipa::openapi::schema::Type::String,
            )))
            .build()
            .into(),
    );
}

/// Declares the optional header `Rewind-Namespace` on an operation that does not declare it
/// itself (those that require it with a key say so in their own description).
fn add_namespace_header(operation: &mut utoipa::openapi::path::Operation) {
    let parameters = operation.parameters.get_or_insert_with(Vec::new);
    if parameters
        .iter()
        .any(|parameter| matches!(parameter, RefOr::T(parameter) if parameter.name == crate::headers::NAMESPACE))
    {
        return;
    }
    parameters.push(
        ParameterBuilder::new()
            .name(crate::headers::NAMESPACE)
            .parameter_in(ParameterIn::Header)
            .required(Required::False)
            .description(Some(
                "The namespace (the end user) a key's or a member's request reads: only its \
                 resources. With an end user's token, its own namespace or nothing; another \
                 is `forbidden`.",
            ))
            .schema(Some(utoipa::openapi::schema::Object::with_type(
                utoipa::openapi::schema::Type::String,
            )))
            .build()
            .into(),
    );
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

/// The outbound webhooks: one event per event type, each a POST to the subscription's URL.
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
    use crate::action_sets::{
        ActionSearchResults, ActionSet, ActionSetList, CreateActionSet, DeclarationsQuery,
        DeclarationsText, SearchQuery,
    };
    use crate::admin::ServerInfo;
    use crate::auth::{
        AuthConfig, AuthState, Callback, EmailVerification, GithubSignIn, MagicAuthRequest,
        MagicAuthVerification, OrganizationSelection, SessionRequest, WaitlistRequest,
    };
    use crate::calls::{
        CallStreamMessage, CallStreamQuery, CompleteCall, DeferCall, FailCall, RejectCall,
    };
    use crate::connections::{
        Connection, ConnectionList, Consent, ConsentAnswer, ConsentQuery, ConsentRedirect,
        UpdateConnection,
    };
    use crate::events::{ChangeEvent, EventsQuery, HistoryEvent, StreamEvent};
    use crate::namespaces::{
        CreateToken, CreateTokenKey, NamespaceDefaults, NamespaceDetail, NamespaceList,
        NamespaceListQuery, NamespaceNames, NamespacePatch, NamespaceQuery, PutCeiling,
        PutNamespace, PutNamespaceDefaults, QuotaAlertList, QuotaAlertQuery, RevokedTokens, Token,
        TokenKey, TokenKeyList,
    };
    use crate::oauth::{
        AuthorizationServerMetadata, AuthorizeQuery, ClientMetadata, OAuthError,
        ProtectedResourceMetadata, RegisteredClient, RevokeRequest, TokenRequest, TokenResponse,
    };
    use crate::organization::{
        CreateInvitation, CreateOrganization, Identity, Invitation, InvitationList, MemberList,
        Organization, OrganizationSettings, UpdateOrganization,
    };
    use crate::problem::Problem;
    use crate::reading::{ActionStatsFilter, ActionStatsPage, PayloadQuery};
    use crate::runs::{
        CancelRun, ExportQuery, FailureGroupPage, FailureQuery, ForkRun, GetRunQuery, Run,
        RunFilter, RunPage, RunStats, RunStatsQuery, SignalAccepted, StartRun, Transcript,
    };
    use crate::scripts::{
        CheckRequest, CheckResult, Script, ScriptFilter, ScriptPage, SetScriptTitle, TestRequest,
    };
    use crate::secrets::{
        Jwks, PutSecret, Secret, SecretList, SecretUsage, SigningKey, SigningKeyList,
    };
    use crate::testing::{Test, TestRun};
    use crate::webhooks::{CreateSubscription, Subscription, SubscriptionList};
    use crate::workflows::{
        CreateWorkflow, RecordTests, RecordedTests, Workflow, WorkflowPage, WorkflowQuery,
        WorkflowVersions,
    };

    // ---------------------------------------------------------------------------------------
    // Action sets

    /// Register and pin an action set
    ///
    /// Connects to the MCP servers and lists their actions, reads the APIs' OpenAPI documents
    /// (one action per operation), pins their schemas, policies and `auth`, and returns the
    /// action set with its declarations, for the model's prompt, and the API operations it left
    /// out. Content-addressed: registering the same action set again returns it (200). `secrets`
    /// attaches organization's secrets to list the actions of an MCP server that needs one
    /// (`secrets:use`). Requires `actions:write`.
    #[utoipa::path(
        post, path = "/v1/action-sets", tag = "action-sets", operation_id = "createActionSet",
        request_body = CreateActionSet,
        responses(
            (status = 201, description = "Registered.", body = ActionSet),
            (status = 200, description = "Already registered.", body = ActionSet),
            (status = 422, description = "Refused: an invalid schema, a duplicate name, a transport this server does not allow, an OpenAPI document or an `auth` that cannot be used (`invalid-action-set`); a secret attached for a listing that does not exist or is not the caller's (`secret-required`).", body = Problem, content_type = "application/problem+json"),
            (status = 502, description = "An MCP server or an OpenAPI document's URL cannot be reached (`action-set-unavailable`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn create_action_set() {}

    /// List the action sets
    ///
    /// The action sets the tenant registered, by id, with their servers and action names.
    #[utoipa::path(
        get, path = "/v1/action-sets", tag = "action-sets", operation_id = "listActionSets",
        responses((status = 200, description = "Every action set of the tenant.", body = ActionSetList))
    )]
    pub(super) fn list_action_sets() {}

    /// The action set's declarations
    #[utoipa::path(
        get, path = "/v1/action-sets/{id}/declarations", tag = "action-sets", operation_id = "getActionSetDeclarations",
        params(("id" = String, Path, description = "The action set's id."), DeclarationsQuery),
        responses(
            (status = 200, description = "The declarations in the requested format.", body = DeclarationsText),
            (status = 404, description = "No such action set.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn get_action_set_declarations() {}

    /// Search the action set's actions (`search_actions`)
    ///
    /// For action sets too large for the prompt: the declarations of the actions matching a query.
    #[utoipa::path(
        get, path = "/v1/action-sets/{id}/search", tag = "action-sets", operation_id = "searchActions",
        params(("id" = String, Path, description = "The action set's id."), SearchQuery),
        responses(
            (status = 200, description = "The matching actions, best first.", body = ActionSearchResults),
            (status = 404, description = "No such action set.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn search_actions() {}

    // ---------------------------------------------------------------------------------------
    // Workflows

    /// Save a workflow (`save_workflow`)
    ///
    /// Checks the workflow's code against its action set and its own schemas, then runs its
    /// suite: its base's tests, minus those it retires and with those it updates, each with a
    /// reason, plus its new ones. It is saved as a new version, the workflow's latest, only if
    /// every test passes. Saving the same workflow again returns the version saved first (200).
    /// A key limited to labels saves a version of its own, carrying its labels, even when the
    /// same workflow was saved under other labels or tenant-wide. Runs the code's tests:
    /// served by a server with the worker role (503 on the api role when there is a test to run).
    #[utoipa::path(
        post, path = "/v1/workflows", tag = "workflows", operation_id = "createWorkflow",
        request_body = CreateWorkflow,
        responses(
            (status = 201, description = "Saved as a new version.", body = Workflow),
            (status = 200, description = "Saved earlier.", body = Workflow),
            (status = 404, description = "No such action set, or base version.", body = Problem, content_type = "application/problem+json"),
            (status = 422, description = "The code was refused (`invalid-code`, with `diagnostics`); the name, a signal's name or a schema (`invalid-workflow`); a test or the suite (`invalid-test`); or tests did not pass (`tests-failed`, with `tests`).", body = Problem, content_type = "application/problem+json"),
            (status = 503, description = "A server without the worker role, with tests to run (`unavailable`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn create_workflow() {}

    /// List the workflows
    ///
    /// The latest version of each workflow the caller sees, by name; with `script`, every version
    /// whose code it is, newest first.
    #[utoipa::path(
        get, path = "/v1/workflows", tag = "workflows", operation_id = "listWorkflows",
        params(WorkflowQuery),
        responses((status = 200, description = "A page of workflows, by name.", body = WorkflowPage))
    )]
    pub(super) fn list_workflows() {}

    /// A workflow's versions
    #[utoipa::path(
        get, path = "/v1/workflows/{name}", tag = "workflows", operation_id = "getWorkflowVersions",
        params(("name" = String, Path, description = "The workflow's name.")),
        responses(
            (status = 200, description = "Its versions, newest first.", body = WorkflowVersions),
            (status = 404, description = "No such workflow.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn get_workflow_versions() {}

    /// Record tests from past runs (`record_tests`)
    ///
    /// Takes the workflow's completed and failed runs the caller sees, those `runs` names or
    /// those `query` selects (the 1,000 newest at most), groups them by the path they took, and
    /// records the newest run of each path as a test, up to `limit`. The tests are stored and
    /// join no suite: a save references them by digest.
    #[utoipa::path(
        post, path = "/v1/workflows/{name}/record", tag = "workflows", operation_id = "recordTests",
        params(("name" = String, Path, description = "The workflow's name.")),
        request_body = RecordTests,
        responses(
            (status = 200, description = "The tests recorded, and the runs left out.", body = RecordedTests),
            (status = 400, description = "The query cannot be read (`invalid-query`).", body = Problem, content_type = "application/problem+json"),
            (status = 404, description = "No such workflow, or run.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn record_tests() {}

    /// A stored test
    ///
    /// A test of a suite, or one recorded from a run. A recorded test holds its run's data and
    /// seed: reading it also needs `runs:read` in its namespace.
    #[utoipa::path(
        get, path = "/v1/tests/{digest}", tag = "workflows", operation_id = "getTest",
        params(("digest" = String, Path, description = "The test's digest.")),
        responses(
            (status = 200, description = "The test.", body = Test),
            (status = 404, description = "No such test.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn get_test() {}

    // ---------------------------------------------------------------------------------------
    // Workflows

    /// Type-check code (`check_workflow`)
    ///
    /// Against the sandbox lib and the action set's declarations, in strict mode, with the checks
    /// at start and the checker's own rules. Nothing runs.
    #[utoipa::path(
        post, path = "/v1/check", tag = "checks", operation_id = "checkWorkflow",
        params(("Rewind-Namespace" = Option<String>, Header, description = "The namespace (the end user) the request acts for. Required with a key or a member's token (400 without it); with an end user's token, its own namespace or nothing.")),
        request_body = CheckRequest,
        responses(
            (status = 200, description = "The diagnostics; `ok` when none is an error.", body = CheckResult),
            (status = 404, description = "No such action set.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn check_workflow() {}

    /// Run tests in a test run (`test_workflow`)
    ///
    /// In virtual time, each test against its world: rules, handlers and a cassette. No
    /// action is really called. With `workflow`, the suite of that version runs too, against the
    /// given code or action set. Served by a server with the worker role.
    #[utoipa::path(
        post, path = "/v1/test", tag = "checks", operation_id = "testWorkflow",
        params(("Rewind-Namespace" = Option<String>, Header, description = "The namespace (the end user) the request acts for. Required with a key or a member's token (400 without it); with an end user's token, its own namespace or nothing.")),
        request_body = TestRequest,
        responses(
            (status = 200, description = "Each test's outcome, and the expectations that do not hold.", body = TestRun),
            (status = 404, description = "No such action set, workflow or stored test.", body = Problem, content_type = "application/problem+json"),
            (status = 422, description = "The code was refused at start (`invalid-code`), or a test (`invalid-test`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn test_workflow() {}

    /// List the distinct workflows executed
    ///
    /// From the hourly counters, over a window of whole hours: each code the runs of a
    /// namespace or the tenant ran, with its runs by status and the actions it called. The scope as
    /// `GET /v1/runs/stats` takes it.
    #[utoipa::path(
        get, path = "/v1/scripts", tag = "reading", operation_id = "listScripts",
        params(ScriptFilter),
        responses(
            (status = 200, description = "A page of workflows, most recently run first.", body = ScriptPage),
            (status = 400, description = "`q` cannot be read, or holds another term than one `namespace:` of one value (`invalid-query`, with `query`); or the window is invalid.", body = Problem, content_type = "application/problem+json"),
            (status = 403, description = "The caller lacks `runs:read`, or its grant or token is limited to labels, which counters do not know (`forbidden`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn list_scripts() {}

    /// One code's source
    #[utoipa::path(
        get, path = "/v1/scripts/{digest}", tag = "reading", operation_id = "getScript",
        params(("digest" = String, Path, description = "SHA-256 of the source, lowercase hex.")),
        responses(
            (status = 200, description = "The code.", body = Script),
            (status = 404, description = "No such code.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn get_script() {}

    /// Give a code a title
    ///
    /// The title and description people see for the code, for the whole organization, over
    /// those of the comment at its top: the source does not change. `title: null` removes them,
    /// and the comment's show again. A new run of the code takes its origin from the title.
    /// Requires `scripts:write`.
    #[utoipa::path(
        patch, path = "/v1/scripts/{digest}", tag = "reading", operation_id = "setScriptTitle",
        params(("digest" = String, Path, description = "SHA-256 of the source, lowercase hex.")),
        request_body = SetScriptTitle,
        responses(
            (status = 200, description = "The code, with its title as it now is.", body = Script),
            (status = 404, description = "No such code, or not one the caller sees.", body = Problem, content_type = "application/problem+json"),
            (status = 422, description = "The title is empty or longer than 120 characters, or the description longer than 2,000 (`invalid-request`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn set_script_title() {}

    // ---------------------------------------------------------------------------------------
    // Runs

    /// Start a run (`run_workflow`)
    ///
    /// Compiles, checks and type-checks the code, then starts the run. With `wait`, the one
    /// request is answered when the run ends or is quarantined, or when `wait` passes, whichever
    /// comes first, however long `wait` is.
    #[utoipa::path(
        post, path = "/v1/runs", tag = "runs", operation_id = "startRun",
        params(
            ("Idempotency-Key" = Option<String>, Header, description = "A request repeated with the same key in the namespace returns the run the first one started (200)."),
            ("Rewind-Namespace" = Option<String>, Header, description = "The namespace (the end user) the request acts for. Required with a key or a member's token (400 without it); with an end user's token, its own namespace or nothing."),
        ),
        request_body = StartRun,
        responses(
            (status = 201, description = "Started.", body = Run),
            (status = 200, description = "Started earlier with the same `Idempotency-Key`.", body = Run),
            (status = 404, description = "No such action set.", body = Problem, content_type = "application/problem+json"),
            (status = 413, description = "The input is too large (`payload-too-large`).", body = Problem, content_type = "application/problem+json"),
            (status = 422, description = "The code was refused before any run exists (`invalid-code`), with `diagnostics`; or the input does not match the workflow (`invalid-input`).", body = Problem, content_type = "application/problem+json"),
            (status = 409, description = "The namespace is being erased (`namespace-deleting`).", body = Problem, content_type = "application/problem+json"),
            (status = 429, description = "The namespace or the tenant reached its `active_runs`, `runs_per_month` or `scripts` (`quota-exceeded`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn start_run() {}

    /// List runs (`list_runs`)
    ///
    /// From the run index of the partitions that hold the query's runs, each reading at most its
    /// scan budget for one page: a page may hold fewer runs than `limit`, or none, and still have
    /// a `nextCursor`. Only a page without `nextCursor` is the last.
    #[utoipa::path(
        get, path = "/v1/runs", tag = "runs", operation_id = "listRuns",
        params(RunFilter),
        responses(
            (status = 200, description = "A page of runs, in the order of `sort`; possibly short, with a cursor.", body = RunPage),
            (status = 400, description = "`q` cannot be read (`invalid-query`, with `query`), or another parameter is invalid.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn list_runs() {}

    /// Statistics of the runs of a namespace or the tenant
    ///
    /// From the hourly counters, over a window of whole hours: counts by status, a series over
    /// time, durations and facets (status, workflow, namespace) of the runs started in the
    /// window; the runs waiting now by what they wait for, and the quarantined ones. The scope is
    /// the tenant, or one namespace: a token's, the `Rewind-Namespace` header's, or a single
    /// `namespace:` term of `q`.
    #[utoipa::path(
        get, path = "/v1/runs/stats", tag = "runs", operation_id = "getRunStats",
        params(RunStatsQuery),
        responses(
            (status = 200, description = "The statistics.", body = RunStats),
            (status = 400, description = "`q` cannot be read, or holds another term than one `namespace:` of one value (`invalid-query`, with `query`); or the window is invalid.", body = Problem, content_type = "application/problem+json"),
            (status = 403, description = "The caller lacks `runs:read`, or its grant or token is limited to labels, which counters do not know (`forbidden`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn get_run_stats() {}

    /// Failures grouped by cause
    ///
    /// From the hourly counters, over a window of whole hours: the failed, cancelled and aborted
    /// runs of a namespace or the tenant, ended in the window, grouped by fingerprint: across
    /// workflows, the runs that ended with the same error. The scope as `GET /v1/runs/stats` takes
    /// it.
    #[utoipa::path(
        get, path = "/v1/runs/failures", tag = "runs", operation_id = "listRunFailures",
        params(FailureQuery),
        responses(
            (status = 200, description = "The groups, the largest first.", body = FailureGroupPage),
            (status = 400, description = "`q` cannot be read, or holds another term than one `namespace:` of one value (`invalid-query`, with `query`); or the window is invalid.", body = Problem, content_type = "application/problem+json"),
            (status = 403, description = "The caller lacks `runs:read`, or its grant or token is limited to labels, which counters do not know (`forbidden`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn list_run_failures() {}

    /// A run's state (`get_run`)
    ///
    /// With `wait`, the one request is answered when the run ends or is quarantined, or when
    /// `wait` passes, however long it is.
    #[utoipa::path(
        get, path = "/v1/runs/{id}", tag = "runs", operation_id = "getRun",
        params(("id" = String, Path, description = "The run's id, or its handle percent-encoded (`dana%2Ftriage%2F06bqx3k9f2mt`)."), GetRunQuery),
        responses(
            (status = 200, description = "The run.", body = Run),
            (status = 404, description = "No such run.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn get_run() {}

    /// Erase a run
    ///
    /// Its history, its archive, its read model rows and its payload references. An active run
    /// is cancelled first.
    #[utoipa::path(
        delete, path = "/v1/runs/{id}", tag = "runs", operation_id = "deleteRun",
        params(("id" = String, Path, description = "The run's id, or its handle percent-encoded (`dana%2Ftriage%2F06bqx3k9f2mt`).")),
        responses(
            (status = 204, description = "Erased."),
            (status = 404, description = "No such run.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn delete_run() {}

    /// The run's transcript, for a model
    #[utoipa::path(
        get, path = "/v1/runs/{id}/transcript", tag = "runs", operation_id = "getRunTranscript",
        params(("id" = String, Path, description = "The run's id, or its handle percent-encoded (`dana%2Ftriage%2F06bqx3k9f2mt`).")),
        responses(
            (status = 200, description = "The transcript.", body = Transcript),
            (status = 404, description = "No such run.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn get_run_transcript() {}

    /// One event in full
    ///
    /// Every payload of the event has its value, whatever its size.
    #[utoipa::path(
        get, path = "/v1/runs/{id}/events/{idx}", tag = "runs", operation_id = "getRunEvent",
        params(("id" = String, Path, description = "The run's id, or its handle percent-encoded (`dana%2Ftriage%2F06bqx3k9f2mt`)."), ("idx" = u64, Path, description = "The event's index, from 0.")),
        responses(
            (status = 200, description = "The event.", body = HistoryEvent),
            (status = 404, description = "No such run or event.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn get_run_event() {}

    /// Stream the run's history (server-sent events)
    ///
    /// Each event's `data` is a `StreamEvent`: the committed events from `from`, then, with
    /// `follow=1`, each new event as it is committed, and `end` when the run has ended.
    #[utoipa::path(
        get, path = "/v1/runs/{id}/events", tag = "runs", operation_id = "streamRunEvents",
        params(
            ("id" = String, Path, description = "The run's id, or its handle percent-encoded (`dana%2Ftriage%2F06bqx3k9f2mt`)."),
            EventsQuery,
            ("Last-Event-ID" = Option<String>, Header, description = "Resume after this event: the same as `from` set to it plus one."),
        ),
        responses(
            (status = 200, description = "A stream of server-sent events; each `data` is a `StreamEvent`.", body = StreamEvent, content_type = "text/event-stream"),
            (status = 404, description = "No such run.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn stream_run_events() {}

    /// Follow what changes in the organization (server-sent events)
    ///
    /// The runs of the organization that change from now on, and the changes of its control
    /// plane, as the caller may see them: an end user's token its namespace's, a key or a member
    /// with `Rewind-Namespace` that namespace's, a grant limited to labels the runs carrying them
    /// all. A run the caller cannot open never shows, not even its id. Each event's `event` field
    /// names it and its `data` is a `ChangeEvent`: `run`, with the run's summary as it is now, the
    /// same object as an item of `GET /v1/runs`, whenever its summary changes, pushed by the run's
    /// owner once its batch is durable (a deleted run is not announced); `script`, `workflow`,
    /// `secret`, `namespace` and `connection`, naming what changed in the control plane, whose
    /// views are read again; `resync`, with `{}`, when more changed at once than the server reads
    /// in one pass: read again what is shown. Nothing is read on a timer: after a drop, read again
    /// what is shown and open a new stream, as there is no resume. The server writes a comment
    /// line at least every 15 seconds while it sends nothing else. The stream closes at a member's or a connection's access token's
    /// expiry; once an end user's token or a connection no longer authenticates; once a key is
    /// revoked, or the grant of a key, a member or a connection is removed, changes its labels or
    /// no longer gives `runs:read`; and after a `resync` when the server cannot read the changes
    /// any more.
    #[utoipa::path(
        get, path = "/v1/changes", tag = "runs", operation_id = "streamChanges",
        responses(
            (status = 200, description = "A stream of server-sent events `run`, `script`, `workflow`, `secret`, `namespace`, `connection` and `resync`; each `data` is a `ChangeEvent`.", body = ChangeEvent, content_type = "text/event-stream"),
            (status = 401, description = "No token, or an unknown, revoked or expired one, an access token already expired included, though the identity provider's leeway would still take it (`unauthorized`): renew it.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn stream_changes() {}

    /// Send a signal (`signal_run`)
    ///
    /// The body, any JSON value, is the signal's payload (`null` when empty). It resolves the
    /// code's `signal(name)`, now or the next time the code waits for that name.
    #[utoipa::path(
        post, path = "/v1/runs/{id}/signals/{name}", tag = "runs", operation_id = "signalRun",
        params(
            ("id" = String, Path, description = "The run's id, or its handle percent-encoded (`dana%2Ftriage%2F06bqx3k9f2mt`)."),
            ("name" = String, Path, description = "The signal's name."),
            ("Idempotency-Key" = Option<String>, Header, description = "The same key sent twice to a run delivers one signal."),
        ),
        request_body(content = Option<Value>, description = "The payload: any JSON value. `null` when the body is empty.", content_type = "application/json"),
        responses(
            (status = 202, description = "In the run's mailbox.", body = SignalAccepted),
            (status = 404, description = "No such run.", body = Problem, content_type = "application/problem+json"),
            (status = 409, description = "The run has ended (`run-ended`).", body = Problem, content_type = "application/problem+json"),
            (status = 413, description = "The payload is too large (`payload-too-large`).", body = Problem, content_type = "application/problem+json"),
            (status = 422, description = "A name starting with `rewind:`; for a run of a workflow, a signal it does not declare or a payload that does not match (`invalid-signal`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn signal_run() {}

    /// Cancel a run (`cancel_run`)
    #[utoipa::path(
        post, path = "/v1/runs/{id}/cancel", tag = "runs", operation_id = "cancelRun",
        params(("id" = String, Path, description = "The run's id, or its handle percent-encoded (`dana%2Ftriage%2F06bqx3k9f2mt`).")),
        request_body(content = Option<CancelRun>, description = "Optional."),
        responses(
            (status = 202, description = "Cancellation requested; the run's state, after `wait` if given.", body = Run),
            (status = 404, description = "No such run.", body = Problem, content_type = "application/problem+json"),
            (status = 409, description = "The run has ended (`run-ended`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn cancel_run() {}

    /// Fork a run with a corrected code (`retry_run`)
    #[utoipa::path(
        post, path = "/v1/runs/{id}/fork", tag = "runs", operation_id = "forkRun",
        params(
            ("id" = String, Path, description = "The source run's id, or its handle percent-encoded."),
            ("Idempotency-Key" = Option<String>, Header, description = "A request repeated with the same key returns the fork the first one started (200)."),
        ),
        request_body = ForkRun,
        responses(
            (status = 201, description = "The fork, a new run.", body = Run),
            (status = 200, description = "Started earlier with the same `Idempotency-Key`.", body = Run),
            (status = 404, description = "No such run.", body = Problem, content_type = "application/problem+json"),
            (status = 422, description = "The corrected code was refused (`invalid-code`); or, for a run of a workflow, its version's suite did not pass against it (`tests-failed`, with `tests`).", body = Problem, content_type = "application/problem+json"),
            (status = 429, description = "Too many active runs (`quota-exceeded`).", body = Problem, content_type = "application/problem+json"),
            (status = 503, description = "A run of a workflow forked on a server without the worker role (`unavailable`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn fork_run() {}

    /// Export the history, or a cassette
    ///
    /// A document of the stored format v1 (`archi/02-history.md`).
    #[utoipa::path(
        get, path = "/v1/runs/{id}/export", tag = "runs", operation_id = "exportRun",
        params(("id" = String, Path, description = "The run's id, or its handle percent-encoded (`dana%2Ftriage%2F06bqx3k9f2mt`)."), ExportQuery),
        responses(
            (status = 200, description = "The run export or the cassette, stored format v1.", body = Object),
            (status = 404, description = "No such run.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn export_run() {}

    /// Take a run out of quarantine
    ///
    /// The run is activated again, typically after a fix of the platform.
    #[utoipa::path(
        post, path = "/v1/runs/{id}/release", tag = "runs", operation_id = "releaseRun",
        params(("id" = String, Path, description = "The run's id, or its handle percent-encoded (`dana%2Ftriage%2F06bqx3k9f2mt`).")),
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
        params(("id" = String, Path, description = "The run's id, or its handle percent-encoded (`dana%2Ftriage%2F06bqx3k9f2mt`).")),
        responses(
            (status = 200, description = "Given up; the run is aborted.", body = Run),
            (status = 404, description = "No such run.", body = Problem, content_type = "application/problem+json"),
            (status = 409, description = "The run is not quarantined (`not-quarantined`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn give_up_run() {}

    // ---------------------------------------------------------------------------------------
    // Harness calls

    /// Follow the harness calls handed to this worker (server-sent events)
    ///
    /// The calls of the harness actions it names (`actionSet`, `actions`), as they become ready,
    /// among the runs the caller sees: an end user's token those of its namespace, a key limited to
    /// labels those of the runs carrying them. At most `slots` are in flight on the stream at once.
    /// Each event's `event` field names it and its `data` is a `CallStreamMessage`: `call`, a call
    /// handed with its token, to answer with `/complete`, `/fail` or `/defer`; `gone`, a call
    /// handed on this stream that no longer waits for its answer. The open stream is the liveness
    /// of every call handed on it: no heartbeat. When it closes, its calls wait for the grace of
    /// their action (60 s by default) for a stream opened again with the same `worker`, which keeps
    /// them; then each is handed to another worker with its delivery + 1, or fails with
    /// `outcomeUnknown` when its action is not idempotent. A worker opens the stream again after it
    /// drops, with a backoff. The server writes a comment line at least every 15 seconds while it
    /// sends nothing else.
    #[utoipa::path(
        get, path = "/v1/calls/stream", tag = "calls", operation_id = "streamCalls",
        params(CallStreamQuery),
        responses(
            (status = 200, description = "A stream of server-sent events `call` and `gone`; each `data` is a `CallStreamMessage`.", body = CallStreamMessage, content_type = "text/event-stream"),
            (status = 404, description = "No such action set.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn stream_calls() {}

    /// Answer later
    ///
    /// The call waits for `/complete` or `/fail` with the same token, whether the stream it was
    /// handed on stays open or not, up to its timeout.
    #[utoipa::path(
        post, path = "/v1/calls/{token}/defer", tag = "calls", operation_id = "deferCall",
        params(("token" = String, Path, description = "The claim's token.")),
        request_body(content = Option<DeferCall>, description = "Optional."),
        responses(
            (status = 204, description = "Deferred."),
            (status = 410, description = "The call no longer exists (`call-gone`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn defer_call() {}

    /// Complete a call with its result
    #[utoipa::path(
        post, path = "/v1/calls/{token}/complete", tag = "calls", operation_id = "completeCall",
        params(("token" = String, Path, description = "The claim's token.")),
        request_body = CompleteCall,
        responses(
            (status = 204, description = "Delivered to the run."),
            (status = 410, description = "The call no longer exists; the result is dropped (`call-gone`).", body = Problem, content_type = "application/problem+json"),
            (status = 413, description = "The result is too large (`payload-too-large`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn complete_call() {}

    /// Fail a call
    #[utoipa::path(
        post, path = "/v1/calls/{token}/fail", tag = "calls", operation_id = "failCall",
        params(("token" = String, Path, description = "The claim's token.")),
        request_body = FailCall,
        responses(
            (status = 204, description = "Recorded: delivered as an `ActionError`, or delivered again."),
            (status = 410, description = "The call no longer exists (`call-gone`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn fail_call() {}

    /// Approve a call that waits for the harness
    ///
    /// A call under the `approval` policy is sent; a call parked by a schema change continues
    /// against the new schema.
    #[utoipa::path(
        post, path = "/v1/calls/{id}/approve", tag = "calls", operation_id = "approveCall",
        params(("id" = String, Path, description = "The call's id, from the run's state or the webhook.")),
        responses(
            (status = 204, description = "The call is ready."),
            (status = 404, description = "No such call.", body = Problem, content_type = "application/problem+json"),
            (status = 409, description = "The call does not wait for the harness (`call-not-waiting`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn approve_call() {}

    /// Reject a call that waits for the harness
    ///
    /// The call fails with an `ActionError`.
    #[utoipa::path(
        post, path = "/v1/calls/{id}/reject", tag = "calls", operation_id = "rejectCall",
        params(("id" = String, Path, description = "The call's id.")),
        request_body(content = Option<RejectCall>, description = "Optional."),
        responses(
            (status = 204, description = "The call failed."),
            (status = 404, description = "No such call.", body = Problem, content_type = "application/problem+json"),
            (status = 409, description = "The call does not wait for the harness (`call-not-waiting`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn reject_call() {}

    // ---------------------------------------------------------------------------------------
    // Webhooks

    /// Inbound webhook: becomes a signal
    ///
    /// At the URL a code got from `signalUrl(name)`. A JSON body is the signal's payload; any
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

    /// Latency, errors and redeliveries per action
    ///
    /// From the hourly counters, over a window of whole hours: per action, the calls of the runs of
    /// a namespace or the tenant, filtered by the start of an action's name. The scope as
    /// `GET /v1/runs/stats` takes it.
    #[utoipa::path(
        get, path = "/v1/actions/stats", tag = "reading", operation_id = "getActionStats",
        params(ActionStatsFilter),
        responses(
            (status = 200, description = "Statistics per action over the window.", body = ActionStatsPage),
            (status = 400, description = "`q` cannot be read, or holds another term than one `namespace:` of one value (`invalid-query`, with `query`); or the window is invalid.", body = Problem, content_type = "application/problem+json"),
            (status = 403, description = "The caller lacks `runs:read`, or its grant or token is limited to labels, which counters do not know (`forbidden`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn get_action_stats() {}

    /// A payload's value
    #[utoipa::path(
        get, path = "/v1/payloads/{digest}", tag = "reading", operation_id = "getPayload",
        params(("digest" = String, Path, description = "SHA-256 of the canonical JSON, lowercase hex."), PayloadQuery),
        responses(
            (status = 200, description = "The JSON value itself, in canonical form.", body = Value),
            (status = 400, description = "No `run` (`invalid-request`): a payload is read through its run.", body = Problem, content_type = "application/problem+json"),
            (status = 403, description = "The key's role or allowed labels do not cover the run (`forbidden`).", body = Problem, content_type = "application/problem+json"),
            (status = 404, description = "No such run, or no such payload in its history.", body = Problem, content_type = "application/problem+json"),
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
    // Namespaces

    /// List the namespaces
    ///
    /// The end users of the organization, each with the limits that apply, its exceptions and
    /// its use of a month, by CPU, runs, workflows or name. Requires `namespaces:read`.
    #[utoipa::path(
        get, path = "/v1/namespaces", tag = "namespaces", operation_id = "listNamespaces",
        params(NamespaceListQuery),
        responses((status = 200, description = "A page of namespaces.", body = NamespaceList))
    )]
    pub(super) fn list_namespaces() {}

    /// Read a namespace
    ///
    /// Its limits and exceptions, its use day by day over a month, its alerts and its tokens in
    /// use. Requires `namespaces:read`.
    #[utoipa::path(
        get, path = "/v1/namespaces/{name}", tag = "namespaces", operation_id = "getNamespace",
        params(("name" = String, Path, description = "The namespace."), NamespaceQuery),
        responses(
            (status = 200, description = "The namespace.", body = NamespaceDetail),
            (status = 404, description = "No such namespace.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn get_namespace() {}

    /// Create a namespace, or set its exceptions
    ///
    /// Replaces the namespace's exceptions to the default limits; a quota absent from them
    /// follows the default. Requires `namespaces:write`.
    #[utoipa::path(
        put, path = "/v1/namespaces/{name}", tag = "namespaces", operation_id = "putNamespace",
        params(("name" = String, Path, description = "The namespace: 1 to 128 letters, digits, -, _, ., : and @.")),
        request_body = PutNamespace,
        responses(
            (status = 200, description = "Created or changed.", body = NamespaceDetail),
            (status = 409, description = "The namespace is being erased (`namespace-deleting`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn put_namespace() {}

    /// Change a namespace
    ///
    /// A JSON merge patch of an existing namespace: `displayName` sets the label shown beside its
    /// name, which stays its key; `null` or `""` removes it; what is absent stays. Never creates
    /// the namespace. Requires `namespaces:write`.
    #[utoipa::path(
        patch, path = "/v1/namespaces/{name}", tag = "namespaces", operation_id = "patchNamespace",
        params(("name" = String, Path, description = "The namespace.")),
        request_body = NamespacePatch,
        responses(
            (status = 200, description = "The namespace as it now is.", body = NamespaceDetail),
            (status = 404, description = "No such namespace.", body = Problem, content_type = "application/problem+json"),
            (status = 409, description = "The namespace is being erased (`namespace-deleting`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn patch_namespace() {}

    /// Erase a namespace
    ///
    /// An end user who leaves: revokes the namespace's tokens, cancels its active runs, then
    /// erases in the background its runs, secrets and use. Requires `namespaces:write`.
    #[utoipa::path(
        delete, path = "/v1/namespaces/{name}", tag = "namespaces", operation_id = "deleteNamespace",
        params(("name" = String, Path, description = "The namespace.")),
        responses(
            (status = 202, description = "The erasure started."),
            (status = 404, description = "No such namespace.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn delete_namespace() {}

    /// Read the default limits
    ///
    /// The limits of every namespace without an exception, and the organization's ceiling
    /// with its use of the month. Requires `namespaces:read`.
    #[utoipa::path(
        get, path = "/v1/organization/namespace-defaults", tag = "namespaces", operation_id = "getNamespaceDefaults",
        params(NamespaceQuery),
        responses((status = 200, description = "The defaults and the ceiling.", body = NamespaceDefaults))
    )]
    pub(super) fn get_namespace_defaults() {}

    /// Set the default limits
    ///
    /// Requires `namespaces:write`.
    #[utoipa::path(
        put, path = "/v1/organization/namespace-defaults", tag = "namespaces", operation_id = "putNamespaceDefaults",
        request_body = PutNamespaceDefaults,
        responses((status = 200, description = "Changed.", body = NamespaceDefaults))
    )]
    pub(super) fn put_namespace_defaults() {}

    /// List the display names
    ///
    /// Every namespace of the organization that has a display name, with it, by name: what a
    /// client loads once to show the display names beside the names. Not paged. Requires
    /// `namespaces:read`.
    #[utoipa::path(
        get, path = "/v1/organization/namespace-names", tag = "namespaces", operation_id = "listNamespaceNames",
        responses((status = 200, description = "The display names.", body = NamespaceNames))
    )]
    pub(super) fn list_namespace_names() {}

    /// List the quota alerts
    ///
    /// The alerts of a month, of one namespace or of the whole organization, newest first.
    /// Requires `namespaces:read`.
    #[utoipa::path(
        get, path = "/v1/organization/quota-alerts", tag = "namespaces", operation_id = "listQuotaAlerts",
        params(QuotaAlertQuery),
        responses((status = 200, description = "The alerts.", body = QuotaAlertList))
    )]
    pub(super) fn list_quota_alerts() {}

    /// Mint an end user's token
    ///
    /// An opaque token of the namespace, with permissions among those a token may hold and the
    /// caller holds, optional labels and a life (1 hour by default, 30 days at most). Creates
    /// the namespace if needed. Requires `tokens:write`.
    #[utoipa::path(
        post, path = "/v1/namespaces/{name}/tokens", tag = "namespaces", operation_id = "createToken",
        params(("name" = String, Path, description = "The namespace.")),
        request_body = CreateToken,
        responses(
            (status = 201, description = "Minted; the answer holds the token, once.", body = Token),
            (status = 409, description = "The namespace is being erased (`namespace-deleting`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn create_token() {}

    /// Revoke the namespace's tokens
    ///
    /// Its opaque tokens, and the signed tokens issued before now. Requires `tokens:write`.
    #[utoipa::path(
        delete, path = "/v1/namespaces/{name}/tokens", tag = "namespaces", operation_id = "revokeNamespaceTokens",
        params(("name" = String, Path, description = "The namespace.")),
        responses(
            (status = 200, description = "Revoked.", body = RevokedTokens),
            (status = 404, description = "No such namespace.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn revoke_namespace_tokens() {}

    /// Revoke an end user's token
    ///
    /// Requires `tokens:write`.
    #[utoipa::path(
        delete, path = "/v1/tokens/{id}", tag = "namespaces", operation_id = "revokeToken",
        params(("id" = String, Path, description = "The token's id.")),
        responses(
            (status = 204, description = "Revoked."),
            (status = 404, description = "No such token.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn revoke_token() {}

    /// List the token keys
    ///
    /// The public keys the organization signs its end users' tokens with. Requires
    /// `tokens:write`.
    #[utoipa::path(
        get, path = "/v1/organization/token-keys", tag = "namespaces", operation_id = "listTokenKeys",
        responses((status = 200, description = "The keys.", body = TokenKeyList))
    )]
    pub(super) fn list_token_keys() {}

    /// Register a token key
    ///
    /// A public key (a JWK: Ed25519, P-256 or RSA) and the permissions its tokens may carry,
    /// among those a token may hold and the caller holds. Answers its `kid`. Requires
    /// `tokens:write`.
    #[utoipa::path(
        post, path = "/v1/organization/token-keys", tag = "namespaces", operation_id = "createTokenKey",
        request_body = CreateTokenKey,
        responses((status = 201, description = "Registered.", body = TokenKey))
    )]
    pub(super) fn create_token_key() {}

    /// Delete a token key
    ///
    /// Every token it signed is refused from the next request. Requires `tokens:write`.
    #[utoipa::path(
        delete, path = "/v1/organization/token-keys/{kid}", tag = "namespaces", operation_id = "deleteTokenKey",
        params(("kid" = String, Path, description = "The key's `kid`.")),
        responses(
            (status = 204, description = "Deleted."),
            (status = 404, description = "No such key.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn delete_token_key() {}

    /// Set an organization's ceiling
    ///
    /// The limits of its totals across namespaces: the customer's offer. The operator only.
    #[utoipa::path(
        put, path = "/v1/organizations/{tenant}/quotas", tag = "namespaces", operation_id = "putCeiling",
        params(("tenant" = String, Path, description = "The tenant (the organization's id).")),
        request_body = PutCeiling,
        responses(
            (status = 204, description = "Changed."),
            (status = 404, description = "No such tenant.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn put_ceiling() {}

    // ---------------------------------------------------------------------------------------
    // Secrets

    /// List the organization's secrets
    ///
    /// The organization's secrets the caller sees (those carrying its labels): each one's name,
    /// labels, who wrote it and when, its reads per day over 30 days and its last read; never a
    /// value. Requires `secrets:read`.
    #[utoipa::path(
        get, path = "/v1/secrets", tag = "secrets", operation_id = "listSecrets",
        responses((status = 200, description = "The secrets, sorted by name.", body = SecretList))
    )]
    pub(super) fn list_secrets() {}

    /// Put an organization's secret
    ///
    /// Creates or replaces it, with its labels (a grant limited to labels gives it its own).
    /// The calls parked by a refusal or a missing secret in the runs that attach it are ready
    /// again, sent with the same idempotency key. Refused to an end user's token. Requires
    /// `secrets:write`.
    #[utoipa::path(
        put, path = "/v1/secrets/{name}", tag = "secrets", operation_id = "putSecret",
        params(("name" = String, Path, description = "The secret's name: 1 to 64 lowercase letters, digits, `-` and `_`, starting with a letter.")),
        request_body = PutSecret,
        responses(
            (status = 201, description = "Created.", body = Secret),
            (status = 200, description = "Replaced.", body = Secret),
            (status = 400, description = "An invalid name, or a value above 16 KiB (`invalid-request`).", body = Problem, content_type = "application/problem+json"),
            (status = 403, description = "The caller may not write it: an end user's token, or labels outside its grant's.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn put_secret() {}

    /// Delete an organization's secret
    ///
    /// The next call of a run that attaches it is parked (`credentials.expired`, reason
    /// `missing`). `GET /v1/secrets/{name}/usage` says first what it would stop. Requires
    /// `secrets:write`.
    #[utoipa::path(
        delete, path = "/v1/secrets/{name}", tag = "secrets", operation_id = "deleteSecret",
        params(("name" = String, Path, description = "The secret's name.")),
        responses(
            (status = 204, description = "Deleted."),
            (status = 404, description = "No such secret.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn delete_secret() {}

    /// What reads an organization's secret
    ///
    /// The active runs that attach it, those of them waiting on a call it serves, the
    /// workflows whose latest version attaches it, its reads per day and its last read.
    /// Requires `secrets:read`.
    #[utoipa::path(
        get, path = "/v1/secrets/{name}/usage", tag = "secrets", operation_id = "getSecretUsage",
        params(("name" = String, Path, description = "The secret's name.")),
        responses(
            (status = 200, description = "What reads it.", body = SecretUsage),
            (status = 404, description = "No such secret.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn get_secret_usage() {}

    /// List an end user's secrets
    ///
    /// The secrets of the namespace, as `GET /v1/secrets` lists the organization's. Requires
    /// `secrets:read`.
    #[utoipa::path(
        get, path = "/v1/namespaces/{name}/secrets", tag = "secrets", operation_id = "listNamespaceSecrets",
        params(("name" = String, Path, description = "The namespace.")),
        responses((status = 200, description = "The secrets, sorted by name.", body = SecretList))
    )]
    pub(super) fn list_namespace_secrets() {}

    /// Put an end user's secret
    ///
    /// Creates or replaces the secret `secret` of the namespace, read by its runs that attach
    /// `ns/<secret>`; a token puts its own namespace's. It takes no labels. The calls parked by a
    /// refusal or a missing secret in the namespace's runs that attach it are ready again.
    /// Refused to a grant or a token limited to labels. Requires `secrets:write`.
    #[utoipa::path(
        put, path = "/v1/namespaces/{name}/secrets/{secret}", tag = "secrets", operation_id = "putNamespaceSecret",
        params(
            ("name" = String, Path, description = "The namespace."),
            ("secret" = String, Path, description = "The secret's name."),
        ),
        request_body = PutSecret,
        responses(
            (status = 201, description = "Created.", body = Secret),
            (status = 200, description = "Replaced.", body = Secret),
            (status = 400, description = "An invalid name, labels, or a value above 16 KiB (`invalid-request`).", body = Problem, content_type = "application/problem+json"),
            (status = 403, description = "The caller may not write it: a grant or a token limited to labels.", body = Problem, content_type = "application/problem+json"),
            (status = 409, description = "The namespace is being erased (`namespace-deleting`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn put_namespace_secret() {}

    /// Delete an end user's secret
    ///
    /// As `DELETE /v1/secrets/{name}`, for the namespace's secret. Requires `secrets:write`.
    #[utoipa::path(
        delete, path = "/v1/namespaces/{name}/secrets/{secret}", tag = "secrets", operation_id = "deleteNamespaceSecret",
        params(
            ("name" = String, Path, description = "The namespace."),
            ("secret" = String, Path, description = "The secret's name."),
        ),
        responses(
            (status = 204, description = "Deleted."),
            (status = 404, description = "No such secret.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn delete_namespace_secret() {}

    /// What reads an end user's secret
    ///
    /// As `GET /v1/secrets/{name}/usage`, for the namespace's secret: its runs, and the
    /// workflows whose latest version attaches `ns/<secret>`. Requires `secrets:read`.
    #[utoipa::path(
        get, path = "/v1/namespaces/{name}/secrets/{secret}/usage", tag = "secrets", operation_id = "getNamespaceSecretUsage",
        params(
            ("name" = String, Path, description = "The namespace."),
            ("secret" = String, Path, description = "The secret's name."),
        ),
        responses(
            (status = 200, description = "What reads it.", body = SecretUsage),
            (status = 404, description = "No such secret.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn get_namespace_secret_usage() {}

    /// List the signing keys
    ///
    /// The organization's Ed25519 keys that sign the calls of servers whose `auth` is `rewind`:
    /// the current one and those retired less than 24 hours ago, with the JWT's issuer and the
    /// URL of the public keys. Requires `secrets:read`.
    #[utoipa::path(
        get, path = "/v1/organization/signing-keys", tag = "secrets", operation_id = "listSigningKeys",
        responses((status = 200, description = "The keys, newest first.", body = SigningKeyList))
    )]
    pub(super) fn list_signing_keys() {}

    /// Replace the signing key
    ///
    /// A new key signs from now; the old one stops signing and stays published 24 hours.
    /// Requires `organization:write`.
    #[utoipa::path(
        post, path = "/v1/organization/signing-keys", tag = "secrets", operation_id = "rotateSigningKey",
        responses((status = 201, description = "The new key.", body = SigningKey))
    )]
    pub(super) fn rotate_signing_key() {}

    /// The organization's public keys
    ///
    /// A JSON Web Key Set: the current signing key and those retired less than 24 hours ago,
    /// as the servers that receive signed calls check them (`verifyRewindCall`). Without
    /// authentication; `Cache-Control: max-age=300`.
    #[utoipa::path(
        get, path = "/organizations/{tenant}/jwks.json", tag = "secrets", operation_id = "getJwks",
        params(("tenant" = String, Path, description = "The tenant (the organization's id).")),
        security(()),
        responses(
            (status = 200, description = "The public keys.", body = Jwks),
            (status = 404, description = "No such organization.", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn get_jwks() {}

    // ---------------------------------------------------------------------------------------
    // Connections

    /// The caller's connections
    ///
    /// The person's connections, in every organization: the client, the organizations each
    /// opens with its access and labels, when it was made and last used. For a member, even
    /// without organization.
    #[utoipa::path(
        get, path = "/v1/connections", tag = "connections", operation_id = "listConnections",
        responses(
            (status = 200, description = "The caller's connections.", body = ConnectionList),
            (status = 404, description = "This server has no sign-in (`not-found`).", body = Problem, content_type = "application/problem+json"),
            (status = 502, description = "The identity provider is unavailable (`identity-unavailable`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn list_connections() {}

    /// Change a connection
    ///
    /// The organizations it opens from now on, their access and labels: each one where the
    /// person holds a grant giving that access. Its person only, without reconnecting the
    /// client.
    #[utoipa::path(
        patch, path = "/v1/connections/{id}", tag = "connections", operation_id = "updateConnection",
        params(("id" = String, Path, description = "The connection's id, `cn_…`.")),
        request_body = UpdateConnection,
        responses(
            (status = 200, description = "The connection as it now is.", body = Connection),
            (status = 404, description = "No connection of the caller with this id (`not-found`).", body = Problem, content_type = "application/problem+json"),
            (status = 502, description = "The identity provider is unavailable (`identity-unavailable`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn update_connection() {}

    /// Revoke a connection
    ///
    /// Its tokens and its grants, at once: the client must connect again. Its person only.
    #[utoipa::path(
        delete, path = "/v1/connections/{id}", tag = "connections", operation_id = "revokeConnection",
        params(("id" = String, Path, description = "The connection's id, `cn_…`.")),
        responses(
            (status = 204, description = "Revoked."),
            (status = 404, description = "No connection of the caller with this id (`not-found`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn revoke_connection() {}

    /// The connections that open the organization
    ///
    /// Each with its person, its client, its access and labels here (`organizations` holds
    /// this organization only), and its last use. Requires `members:read`.
    #[utoipa::path(
        get, path = "/v1/organization/connections", tag = "connections", operation_id = "listOrganizationConnections",
        responses(
            (status = 200, description = "The connections that open the organization.", body = ConnectionList),
            (status = 502, description = "The identity provider is unavailable (`identity-unavailable`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn list_organization_connections() {}

    /// Remove the organization from a connection
    ///
    /// The connection no longer opens the organization; it keeps the others. Requires
    /// `members:manage`, and the permissions of the connection's access here.
    #[utoipa::path(
        delete, path = "/v1/organization/connections/{id}", tag = "connections", operation_id = "removeOrganizationConnection",
        params(("id" = String, Path, description = "The connection's id, `cn_…`.")),
        responses(
            (status = 204, description = "Removed."),
            (status = 404, description = "No connection opens the organization with this id (`not-found`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn remove_organization_connection() {}

    /// What an authorization request asks
    ///
    /// For the consent page: the client, the host it returns to, and the person's
    /// organizations with the accesses they may give in each. For a member, even without
    /// organization.
    #[utoipa::path(
        get, path = "/v1/oauth/consent", tag = "connections", operation_id = "getConsent",
        params(ConsentQuery),
        responses(
            (status = 200, description = "The request.", body = Consent),
            (status = 400, description = "The request cannot be read, or it lapsed: the client starts again (`invalid-request`).", body = Problem, content_type = "application/problem+json"),
            (status = 404, description = "This server has no OAuth (`not-found`).", body = Problem, content_type = "application/problem+json"),
            (status = 502, description = "The identity provider is unavailable (`identity-unavailable`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn get_consent() {}

    /// Allow or deny an authorization request
    ///
    /// Allowing it needs at least one organization, each where the person's grant gives the
    /// access chosen: no one gives more than they hold. Answers where to send the browser: the
    /// client's redirect URI with a code (5 minutes) and the state, or with
    /// `error=access_denied`. The connection is made when the client exchanges the code. For a
    /// member, even without organization.
    #[utoipa::path(
        post, path = "/v1/oauth/consent", tag = "connections", operation_id = "answerConsent",
        request_body = ConsentAnswer,
        responses(
            (status = 200, description = "Where to send the browser.", body = ConsentRedirect),
            (status = 400, description = "The request cannot be read or lapsed, or no organization was chosen (`invalid-request`).", body = Problem, content_type = "application/problem+json"),
            (status = 403, description = "The person's grant in an organization does not give the access chosen (`forbidden`).", body = Problem, content_type = "application/problem+json"),
            (status = 404, description = "This server has no OAuth (`not-found`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn answer_consent() {}

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
            (status = 302, description = "To the dashboard: `/`, or `/?auth_error=<code>` when the sign-in did not go through.", headers(
                ("Location" = String, description = "`/`, or `/?auth_error=<code>`."),
                ("Set-Cookie" = String, description = "`rewind_session` when signed in or at a step; `rewind_oauth` cleared."),
            )),
        )
    )]
    pub(super) fn auth_callback() {}

    /// Send a code by email
    ///
    /// A 6-digit code, to give to `POST /auth/magic-auth/verify`. While sign-ups are closed, an
    /// email without an account gets no code: the answer is `sign_up_closed`, and the person
    /// may join the waitlist.
    #[utoipa::path(
        post, path = "/auth/magic-auth", tag = "auth", operation_id = "sendMagicCode",
        security(()),
        request_body = MagicAuthRequest,
        responses(
            (status = 202, description = "Sent."),
            (status = 200, description = "No code sent: sign-ups are closed and the email has no account (`sign_up_closed`).", body = AuthState, headers(
                ("Set-Cookie" = String, description = "`rewind_session`, holding the step."),
            )),
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

    // ---------------------------------------------------------------------------------------
    // OAuth

    /// Authorize a client
    ///
    /// Checks the authorization request, then sends the browser to the dashboard's consent
    /// page with the request sealed for 10 minutes. An unknown client, or a redirect URI it
    /// does not list, is answered here and never redirected; any other fault goes back to the
    /// client's redirect URI with `error` and `state` (`invalid_request`,
    /// `unsupported_response_type`, `invalid_target`). PKCE with `S256` is required.
    #[utoipa::path(
        get, path = "/oauth/authorize", tag = "oauth", operation_id = "authorize",
        security(()),
        params(AuthorizeQuery),
        responses(
            (status = 302, description = "To the consent page, or back to the client with an error.", headers(
                ("Location" = String, description = "`{dashboard}/connect/authorize?request=…`, or the client's redirect URI with `error` and `state`."),
            )),
            (status = 400, description = "An unknown client, a redirect URI it does not list, or no client or redirect URI at all (`invalid-request`).", body = Problem, content_type = "application/problem+json"),
            (status = 404, description = "This server has no OAuth (`not-found`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn authorize() {}

    /// Register a client
    ///
    /// RFC 7591, for the MCP clients without a metadata document. A client that no connection
    /// uses is deleted after 24 hours. The secret of a confidential client is only in this
    /// answer.
    #[utoipa::path(
        post, path = "/oauth/register", tag = "oauth", operation_id = "registerClient",
        security(()),
        request_body = ClientMetadata,
        responses(
            (status = 201, description = "Registered.", body = RegisteredClient),
            (status = 400, description = "`invalid_redirect_uri` or `invalid_client_metadata`.", body = OAuthError),
            (status = 404, description = "This server has no OAuth (`not-found`).", body = Problem, content_type = "application/problem+json"),
            (status = 503, description = "Too many clients are registered (`temporarily_unavailable`).", body = OAuthError),
        )
    )]
    pub(super) fn register_client() {}

    /// Exchange a code or a refresh token for tokens
    ///
    /// `authorization_code` (with its `code_verifier`) creates the connection, its grants and
    /// its tokens; `refresh_token` replaces both tokens, and removes the organizations the
    /// person left. A code or refresh token that is unknown, expired, used or another client's
    /// is `invalid_grant`; one used twice also revokes its connection.
    #[utoipa::path(
        post, path = "/oauth/token", tag = "oauth", operation_id = "exchangeToken",
        security((), ("client" = [])),
        request_body(content = TokenRequest, content_type = "application/x-www-form-urlencoded"),
        responses(
            (status = 200, description = "The tokens, never cached.", body = TokenResponse),
            (status = 400, description = "`invalid_request`, `invalid_grant`, `unsupported_grant_type` or `invalid_target`.", body = OAuthError),
            (status = 401, description = "The client is unknown or its secret is wrong (`invalid_client`).", body = OAuthError),
            (status = 404, description = "This server has no OAuth (`not-found`).", body = Problem, content_type = "application/problem+json"),
            (status = 503, description = "The identity provider cannot check the person's memberships now (`temporarily_unavailable`).", body = OAuthError),
        )
    )]
    pub(super) fn exchange_token() {}

    /// Revoke a token's connection
    ///
    /// RFC 7009: an access token or a refresh token revokes its whole connection. An unknown
    /// token is answered as revoked.
    #[utoipa::path(
        post, path = "/oauth/revoke", tag = "oauth", operation_id = "revokeOAuthToken",
        security((), ("client" = [])),
        request_body(content = RevokeRequest, content_type = "application/x-www-form-urlencoded"),
        responses(
            (status = 200, description = "Revoked, or unknown."),
            (status = 400, description = "`invalid_request`, or the token is another client's (`unauthorized_client`).", body = OAuthError),
            (status = 401, description = "The client is unknown or its secret is wrong (`invalid_client`).", body = OAuthError),
            (status = 404, description = "This server has no OAuth (`not-found`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn revoke_oauth_token() {}

    /// The protected resource's metadata
    ///
    /// RFC 9728, the same as at `/.well-known/oauth-protected-resource/mcp`.
    #[utoipa::path(
        get, path = "/.well-known/oauth-protected-resource", tag = "oauth", operation_id = "getProtectedResource",
        security(()),
        responses(
            (status = 200, description = "Where `/mcp` takes its tokens from.", body = ProtectedResourceMetadata),
            (status = 404, description = "This server has no OAuth (`not-found`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn get_protected_resource() {}

    /// The metadata of `/mcp`
    ///
    /// RFC 9728: `/mcp` is the resource, this server its authorization server.
    #[utoipa::path(
        get, path = "/.well-known/oauth-protected-resource/mcp", tag = "oauth", operation_id = "getProtectedResourceMcp",
        security(()),
        responses(
            (status = 200, description = "Where `/mcp` takes its tokens from.", body = ProtectedResourceMetadata),
            (status = 404, description = "This server has no OAuth (`not-found`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn get_protected_resource_mcp() {}

    /// The authorization server's metadata
    ///
    /// RFC 8414: its endpoints, PKCE with `S256`, client metadata documents, registration.
    #[utoipa::path(
        get, path = "/.well-known/oauth-authorization-server", tag = "oauth", operation_id = "getAuthorizationServer",
        security(()),
        responses(
            (status = 200, description = "The authorization server.", body = AuthorizationServerMetadata),
            (status = 404, description = "This server has no OAuth (`not-found`).", body = Problem, content_type = "application/problem+json"),
        )
    )]
    pub(super) fn get_authorization_server() {}
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
                // OAuth's own errors are its JSON, as its RFCs say.
                let oauth = matches!(
                    path.as_str(),
                    "/oauth/register" | "/oauth/token" | "/oauth/revoke"
                );
                for (status, response) in operation["responses"].as_object().unwrap() {
                    if status.starts_with('4') || status.starts_with('5') {
                        let content = response["content"].as_object();
                        let problem = content.is_some_and(|content| {
                            content.contains_key("application/problem+json")
                        });
                        let oauth_error = oauth
                            && status != "404"
                            && response["content"]["application/json"]["schema"]["$ref"]
                                == "#/components/schemas/OAuthError";
                        assert!(problem || oauth_error, "{method} {path} {status}");
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
