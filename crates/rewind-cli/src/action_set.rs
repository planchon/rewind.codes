//! Action sets pinned in the CLI process, for the local commands (`rewind check`, `rewind test`):
//! from an action set file, or from the actions a scenario's handlers answer.
//!
//! An action set file is the body of `POST /v1/action-sets` (`CreateActionSet`), pinned as a server
//! pins it: the actions of each MCP server are discovered by connecting to it (`rewind-mcp`), each
//! with its `auth`; the operations of each API are read from its OpenAPI document
//! (`rewind-openapi`); harness actions and the `http` action are taken as written, the `webhook`
//! built-in is always there. As the embedded server, the CLI reaches any address, and APIs over
//! `http` too. It holds no signing key: a server whose `auth` is `rewind` is listed without a
//! signature.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use rewind_api::action_sets::{ActionPolicy as ApiPolicy, CreateActionSet, OpenApiDocument};
use rewind_core::{
    Action, ActionAuth, ActionPolicy, ActionRoute, ActionSet, Api, Builtin, McpServer, McpTransport,
};
use rewind_engine::ports::{Secret, TenantId};
use rewind_mcp::{ListingAuth, McpConfig, McpDispatcher};
use rewind_openapi::{ReadOptions, read};
use rewind_outbound::ActionEgress;
use serde_json::json;

use crate::error::CliError;
use crate::input::read_json;

/// Pins the action set of an action set file.
pub(crate) async fn pin_file(path: &Path) -> Result<ActionSet, CliError> {
    let create: CreateActionSet = read_json(path)?;
    pin(&create)
        .await
        .map_err(|error| CliError::usage(format!("{}: {error}", path.display())))
}

/// Pins an action set as `POST /v1/action-sets` does.
pub(crate) async fn pin(create: &CreateActionSet) -> Result<ActionSet, String> {
    let mut servers = Vec::new();
    if !create.secrets.is_empty() {
        return Err(LISTING_SECRETS.to_owned());
    }
    let (apis, mut actions) = read_apis(create).await?;
    if !create.mcp.is_empty() {
        // A secret attached for a listing is refused above: only the server reads secrets.
        let listing = BTreeMap::new();
        // As the embedded server: the developer's own servers, at any address.
        let discovery = McpDispatcher::new(McpConfig {
            egress: ActionEgress::open(),
            ..McpConfig::default()
        });
        let pinned = discover(&discovery, create, listing, &mut servers, &mut actions).await;
        discovery.close();
        pinned?;
    }
    for harness in &create.harness {
        let group = harness
            .name
            .split_once('.')
            .map(|(group, _)| group.to_owned());
        let mut action = Action::new(
            harness.name.clone(),
            ActionRoute::Harness { group },
            harness.input_schema.clone(),
        );
        action.description = Some(harness.description.clone());
        action.output_schema = harness.output_schema.clone();
        apply(&mut action.policy, harness.policy.as_ref());
        actions.push(action);
    }
    if let Some(http) = &create.http {
        let mut action = rewind_mcp::http_action(http.allow.clone());
        apply(&mut action.policy, http.policy.as_ref());
        actions.push(action);
    }
    actions.push(signal_url());
    ActionSet::from_parts(servers, apis, actions).map_err(|error| error.to_string())
}

/// The APIs of an action set file, read from their OpenAPI documents as `POST /v1/action-sets`
/// reads them: given in the file, or read once from their URL, at any address.
async fn read_apis(create: &CreateActionSet) -> Result<(Vec<Api>, Vec<Action>), String> {
    let mut apis = Vec::new();
    let mut actions = Vec::new();
    for declared in &create.apis {
        let mut options = ReadOptions::new(declared.name.clone());
        options.base_url = declared.base_url.clone();
        options.auth = declared
            .auth
            .as_ref()
            .map(|auth| {
                auth.to_core()
                    .map_err(|error| format!("the API {:?}: {error}", declared.name))
            })
            .transpose()?;
        options.operations = declared.operations.clone();
        options.concurrency = declared.concurrency;
        options.allow_http = true;
        let read_api = match &declared.openapi {
            OpenApiDocument::Document(document) => read(document, &options),
            OpenApiDocument::Url(url) => {
                let fetched = rewind_openapi::fetch(url, &ActionEgress::open())
                    .await
                    .map_err(|error| format!("the API {:?}: {error}", declared.name))?;
                options.document_url = Some(fetched.url);
                read(&fetched.document, &options)
            }
        }
        .map_err(|error| error.to_string())?;
        if let Some(unknown) = declared.policies.keys().find(|name| {
            !read_api
                .actions
                .iter()
                .any(|action| action.local_name() == name.as_str())
        }) {
            return Err(format!(
                "policies: the API {:?} has no operation {unknown:?}",
                declared.name
            ));
        }
        for mut action in read_api.actions {
            let policy = declared.policies.get(action.local_name());
            apply(&mut action.policy, policy);
            actions.push(action);
        }
        apis.push(read_api.api);
    }
    Ok((apis, actions))
}

async fn discover(
    discovery: &McpDispatcher,
    create: &CreateActionSet,
    mut listing: BTreeMap<String, Secret>,
    servers: &mut Vec<McpServer>,
    actions: &mut Vec<Action>,
) -> Result<(), String> {
    for server in &create.mcp {
        let transport = match (&server.url, &server.command) {
            (Some(url), None) => McpTransport::Http { url: url.clone() },
            (None, Some(command)) => McpTransport::Stdio {
                command: command.clone(),
                args: server.args.clone(),
                env: server.env.clone(),
            },
            _ => {
                return Err(format!(
                    "the MCP server {:?} needs exactly one of url and command",
                    server.name
                ));
            }
        };
        let mut pinned = McpServer::new(server.name.clone(), transport);
        if let Some(auth) = &server.auth {
            pinned.auth = auth
                .to_core()
                .map_err(|error| format!("the MCP server {:?}: {error}", server.name))?;
        }
        // The CLI holds no signing key: a server whose auth is `rewind` is listed unsigned.
        let auth = match (&pinned.auth, listing.remove(&server.name)) {
            (ActionAuth::Secret(_), Some(secret)) => ListingAuth::Secret(secret),
            _ => ListingAuth::None,
        };
        let listed = discovery
            .list_tools(&TenantId::local(), &pinned, auth)
            .await
            .map_err(|error| {
                let unsigned = if pinned.auth == ActionAuth::Rewind {
                    " (the CLI does not sign: register the action set on a server, which signs its \
                     listing)"
                } else {
                    ""
                };
                format!(
                    "cannot list the actions of {:?}: {error}{unsigned}",
                    server.name
                )
            })?;
        if let Some(unknown) = server.policies.keys().find(|name| {
            !listed
                .iter()
                .any(|action| short_name(&server.name, &action.name) == name.as_str())
        }) {
            return Err(format!(
                "policies: the MCP server {:?} has no action {unknown:?}",
                server.name
            ));
        }
        for mut action in listed {
            let policy = server.policies.get(short_name(&server.name, &action.name));
            apply(&mut action.policy, policy);
            actions.push(action);
        }
        servers.push(pinned);
    }
    Ok(())
}

/// Why this process pins no action set file that attaches secrets to list its servers' actions: a
/// secret is written and never read back, and only the server's dispatcher reads it
/// (`archi/04-tools.md`, "Secrets").
const LISTING_SECRETS: &str = "secrets: this process cannot read a secret, which only the \
    server reads: register the action set on the server (a run's --action-set FILE does), then give \
    its id to --action-set";

/// `list_issues` for `github.list_issues`.
fn short_name<'a>(server: &str, name: &'a str) -> &'a str {
    name.strip_prefix(server)
        .and_then(|rest| rest.strip_prefix('.'))
        .unwrap_or(name)
}

/// The fields of a policy that an action set file gives, over the route's defaults.
fn apply(policy: &mut ActionPolicy, given: Option<&ApiPolicy>) {
    let Some(given) = given else {
        return;
    };
    if let Some(idempotent) = given.idempotent {
        policy.idempotent = idempotent;
    }
    if let Some(approval) = given.approval {
        policy.approval = approval;
    }
    if let Some(timeout) = given.timeout {
        policy.execution_timeout = Some(timeout.get());
    }
    if let Some(reconnect) = given.reconnect_timeout {
        policy.reconnect_timeout = reconnect.get();
    }
    if let Some(concurrency) = given.concurrency {
        policy.concurrency = concurrency;
    }
}

/// The `webhook` built-in, which every action set pins.
fn signal_url() -> Action {
    Action::new(
        "signalUrl",
        ActionRoute::Builtin(Builtin::SignalUrl),
        rewind_runtime::builtins::signal_url_input_schema(),
    )
}

/// The action set of a scenario that names none: a harness action for each action its handlers answer
/// (a name `group.action` puts it in a group), and `webhook`. In a test run every call goes to the
/// handlers, whatever its route. The `http` action needs its allow-list: a scenario whose handlers
/// answer it names an action set file.
pub(crate) fn of_handlers(names: &BTreeSet<String>) -> Result<ActionSet, CliError> {
    if names.contains("http") {
        return Err(CliError::usage(
            "the handlers answer the http action: name an action set file with its allow-list \
             (--action-set, with \"http\": {\"allow\": [hosts]} in the file)",
        ));
    }
    let mut actions: Vec<Action> = names
        .iter()
        .filter(|name| name.as_str() != "signalUrl")
        .map(|name| {
            let group = name.split_once('.').map(|(group, _)| group.to_owned());
            Action::new(name.clone(), ActionRoute::Harness { group }, json!({}))
        })
        .collect();
    actions.push(signal_url());
    ActionSet::new([], actions)
        .map_err(|error| CliError::usage(format!("the answered actions: {error}")))
}

#[cfg(test)]
mod tests {
    use rewind_api::action_sets::{Api as ApiDeclared, HarnessAction, HttpAction};

    use super::*;

    #[tokio::test]
    async fn apis_are_read_from_their_documents_as_a_server_reads_them() {
        let document = json!({
            "openapi": "3.1.0",
            "info": { "title": "t", "version": "1" },
            "servers": [{ "url": "http://127.0.0.1:8080/v1" }],
            "paths": {
                "/measures": {
                    "post": { "operationId": "createMeasure", "responses": { "201": { "description": "ok" } } },
                    "get": { "operationId": "listMeasures", "responses": { "200": { "description": "ok" } } }
                }
            }
        });
        let create = CreateActionSet {
            apis: vec![ApiDeclared {
                name: "dax".into(),
                openapi: OpenApiDocument::Document(document),
                base_url: None,
                auth: None,
                operations: None,
                concurrency: None,
                policies: [(
                    "listMeasures".to_owned(),
                    ApiPolicy {
                        approval: Some(true),
                        ..ApiPolicy::default()
                    },
                )]
                .into(),
            }],
            ..CreateActionSet::default()
        };
        let action_set = pin(&create).await.unwrap();
        assert_eq!(action_set.apis()[0].base_url, "http://127.0.0.1:8080/v1");
        let create_measure = action_set.action("dax.createMeasure").unwrap();
        // A POST without Idempotency-Key is called at most once.
        assert!(!create_measure.policy.idempotent);
        assert!(
            action_set
                .action("dax.listMeasures")
                .unwrap()
                .policy
                .approval
        );
    }

    #[tokio::test]
    async fn harness_actions_and_http_are_pinned_as_written() {
        let create = CreateActionSet {
            secrets: std::collections::BTreeMap::new(),
            apis: Vec::new(),
            workflows: Vec::new(),
            base: None,
            mcp: Vec::new(),
            harness: vec![HarnessAction {
                name: "crm.ask_user".into(),
                description: "Ask the user.".into(),
                input_schema: json!({"type": "object", "properties": {"q": {"type": "string"}}}),
                output_schema: Some(json!({"type": "string"})),
                policy: Some(ApiPolicy {
                    idempotent: Some(false),
                    approval: Some(true),
                    concurrency: Some(1),
                    ..ApiPolicy::default()
                }),
            }],
            http: Some(HttpAction {
                allow: vec!["api.example.com".into()],
                policy: None,
            }),
        };
        let action_set = pin(&create).await.unwrap();
        let names: Vec<&str> = action_set
            .actions()
            .iter()
            .map(|action| action.name.as_str())
            .collect();
        assert_eq!(names, ["crm.ask_user", "http", "signalUrl"]);
        let ask = &action_set.actions()[0];
        assert_eq!(
            ask.route,
            ActionRoute::Harness {
                group: Some("crm".into())
            }
        );
        assert!(!ask.policy.idempotent);
        assert!(ask.policy.approval);
        assert_eq!(ask.policy.concurrency, 1);
        assert_eq!(ask.output_schema, Some(json!({"type": "string"})));
    }

    #[tokio::test]
    async fn an_mcp_server_needs_one_transport() {
        let create: CreateActionSet =
            serde_json::from_value(json!({ "mcp": [{ "name": "github" }] })).unwrap();
        let error = pin(&create).await.unwrap_err();
        assert!(error.contains("exactly one of url and command"), "{error}");
    }

    #[test]
    fn the_answered_actions_are_harness_actions() {
        let mut names: BTreeSet<String> = ["github.list_issues", "ask_user", "signalUrl"]
            .into_iter()
            .map(String::from)
            .collect();
        let action_set = of_handlers(&names).unwrap();
        let routes: Vec<(&str, &ActionRoute)> = action_set
            .actions()
            .iter()
            .map(|action| (action.name.as_str(), &action.route))
            .collect();
        assert_eq!(routes.len(), 3);
        assert!(matches!(
            routes[0],
            ("ask_user", ActionRoute::Harness { group: None })
        ));
        assert!(matches!(
            routes[1],
            (
                "github.list_issues",
                ActionRoute::Harness { group: Some(_) }
            )
        ));
        assert!(matches!(
            routes[2].1,
            ActionRoute::Builtin(Builtin::SignalUrl)
        ));
        names.insert("http".into());
        let error = of_handlers(&names).unwrap_err();
        assert!(error.to_string().contains("allow-list"), "{error}");
        assert_eq!(short_name("github", "github.list_issues"), "list_issues");
    }
}
