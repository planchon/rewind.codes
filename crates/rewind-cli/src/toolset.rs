//! Toolsets pinned in the CLI process, for the local commands (`rewind check`, `rewind test`): from a
//! toolset file, or from the tools a scenario's mocks answer.
//!
//! A toolset file is the body of `POST /v1/toolsets` (`CreateToolset`), pinned as a server pins
//! it: the tools of each MCP server are discovered by connecting to it (`rewind-mcp`), each with
//! its `auth`; the operations of each API are read from its OpenAPI document (`rewind-openapi`);
//! host tools and the `http` tool are taken as written, the `webhook` built-in is always there.
//! As the embedded server, the CLI reaches any address, and APIs over `http` too. It holds no
//! signing key: a server whose `auth` is `rewind` is listed without a signature.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use rewind_api::toolsets::{
    CreateToolset, OnInterruption, OpenApiDocument, ToolPolicy as ApiPolicy,
};
use rewind_core::{
    Api, Builtin, Interruption, McpServer, McpTransport, Tool, ToolAuth, ToolPolicy, ToolRoute,
    Toolset,
};
use rewind_engine::ports::{Secret, TenantId};
use rewind_mcp::{ListingAuth, McpConfig, McpDispatcher};
use rewind_openapi::{ReadOptions, read};
use rewind_outbound::ToolEgress;
use serde_json::json;

use crate::error::CliError;
use crate::input::read_json;

/// Pins the toolset of a toolset file.
pub(crate) async fn pin_file(path: &Path) -> Result<Toolset, CliError> {
    let create: CreateToolset = read_json(path)?;
    pin(&create)
        .await
        .map_err(|error| CliError::usage(format!("{}: {error}", path.display())))
}

/// Pins a toolset as `POST /v1/toolsets` does.
pub(crate) async fn pin(create: &CreateToolset) -> Result<Toolset, String> {
    let mut servers = Vec::new();
    let (apis, mut tools) = read_apis(create).await?;
    if !create.mcp.is_empty() {
        let listing = if create.secrets.is_empty() {
            BTreeMap::new()
        } else {
            listing_secrets(create)
        };
        // As the embedded server: the developer's own servers, at any address.
        let discovery = McpDispatcher::new(McpConfig {
            egress: ToolEgress::open(),
            ..McpConfig::default()
        });
        let pinned = discover(&discovery, create, listing, &mut servers, &mut tools).await;
        discovery.close();
        pinned?;
    }
    for host in &create.host {
        let group = host.name.split_once('.').map(|(group, _)| group.to_owned());
        let mut tool = Tool::new(
            host.name.clone(),
            ToolRoute::Host { group },
            host.input_schema.clone(),
        );
        tool.description = Some(host.description.clone());
        tool.output_schema = host.output_schema.clone();
        apply(&mut tool.policy, host.policy.as_ref());
        tools.push(tool);
    }
    if let Some(http) = &create.http {
        let mut tool = rewind_mcp::http_tool(http.allow.clone());
        apply(&mut tool.policy, http.policy.as_ref());
        tools.push(tool);
    }
    tools.push(webhook());
    Toolset::from_parts(servers, apis, tools).map_err(|error| error.to_string())
}

/// The APIs of a toolset file, read from their OpenAPI documents as `POST /v1/toolsets` reads
/// them: given in the file, or read once from their URL, at any address.
async fn read_apis(create: &CreateToolset) -> Result<(Vec<Api>, Vec<Tool>), String> {
    let mut apis = Vec::new();
    let mut tools = Vec::new();
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
                let fetched = rewind_openapi::fetch(url, &ToolEgress::open())
                    .await
                    .map_err(|error| format!("the API {:?}: {error}", declared.name))?;
                options.document_url = Some(fetched.url);
                read(&fetched.document, &options)
            }
        }
        .map_err(|error| error.to_string())?;
        if let Some(unknown) = declared.policies.keys().find(|name| {
            !read_api
                .tools
                .iter()
                .any(|tool| tool.local_name() == name.as_str())
        }) {
            return Err(format!(
                "policies: the API {:?} has no operation {unknown:?}",
                declared.name
            ));
        }
        for mut tool in read_api.tools {
            let policy = declared.policies.get(tool.local_name());
            apply(&mut tool.policy, policy);
            tools.push(tool);
        }
        apis.push(read_api.api);
    }
    Ok((apis, tools))
}

async fn discover(
    discovery: &McpDispatcher,
    create: &CreateToolset,
    mut listing: BTreeMap<String, Secret>,
    servers: &mut Vec<McpServer>,
    tools: &mut Vec<Tool>,
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
            (ToolAuth::Secret(_), Some(secret)) => ListingAuth::Secret(secret),
            _ => ListingAuth::None,
        };
        let listed = discovery
            .list_tools(&TenantId::local(), &pinned, auth)
            .await
            .map_err(|error| {
                let unsigned = if pinned.auth == ToolAuth::Rewind {
                    " (the CLI does not sign: register the toolset on a server, which signs its \
                     listing)"
                } else {
                    ""
                };
                format!(
                    "cannot list the tools of {:?}: {error}{unsigned}",
                    server.name
                )
            })?;
        if let Some(unknown) = server.policies.keys().find(|name| {
            !listed
                .iter()
                .any(|tool| short_name(&server.name, &tool.name) == name.as_str())
        }) {
            return Err(format!(
                "policies: the MCP server {:?} has no tool {unknown:?}",
                server.name
            ));
        }
        for mut tool in listed {
            let policy = server.policies.get(short_name(&server.name, &tool.name));
            apply(&mut tool.policy, policy);
            tools.push(tool);
        }
        servers.push(pinned);
    }
    Ok(())
}

/// The secrets a toolset file attaches to list its servers' tools (`secrets`), by server.
/// Called only when `secrets` is not empty.
#[expect(
    clippy::todo,
    reason = "the engine-server workstream gives the CLI its secrets for the listings"
)]
fn listing_secrets(create: &CreateToolset) -> BTreeMap<String, Secret> {
    let _ = create;
    todo!("engine-server: the CLI's secrets for the listings")
}

/// `list_issues` for `github.list_issues`.
fn short_name<'a>(server: &str, name: &'a str) -> &'a str {
    name.strip_prefix(server)
        .and_then(|rest| rest.strip_prefix('.'))
        .unwrap_or(name)
}

/// The fields of a policy that a toolset file gives, over the route's defaults.
fn apply(policy: &mut ToolPolicy, given: Option<&ApiPolicy>) {
    let Some(given) = given else {
        return;
    };
    if let Some(on_interruption) = given.on_interruption {
        policy.on_interruption = match on_interruption {
            OnInterruption::Retry => Interruption::Retry,
            OnInterruption::AtMostOnce => Interruption::AtMostOnce,
        };
    }
    if let Some(approval) = given.approval {
        policy.approval = approval;
    }
    if let Some(timeout) = given.timeout {
        policy.execution_timeout = Some(timeout.get());
    }
    if let Some(heartbeat) = given.heartbeat_timeout {
        policy.heartbeat_timeout = heartbeat.get();
    }
    if let Some(concurrency) = given.concurrency {
        policy.concurrency = concurrency;
    }
}

/// The `webhook` built-in, which every toolset pins.
fn webhook() -> Tool {
    Tool::new(
        "webhook",
        ToolRoute::Builtin(Builtin::Webhook),
        rewind_runtime::builtins::webhook_input_schema(),
    )
}

/// The toolset of a scenario that names none: a host tool for each tool its mocks answer (a
/// name `group.tool` puts it in a group), and `webhook`. In mock mode every call goes to the
/// mocks, whatever its route. The `http` tool needs its allow-list: a scenario whose mocks
/// answer it names a toolset file.
pub(crate) fn of_mocks(names: &BTreeSet<String>) -> Result<Toolset, CliError> {
    if names.contains("http") {
        return Err(CliError::usage(
            "the mocks answer the http tool: name a toolset file with its allow-list \
             (\"toolset\" in the scenario, \"http\": {\"allow\": [hosts]} in the file)",
        ));
    }
    let mut tools: Vec<Tool> = names
        .iter()
        .filter(|name| name.as_str() != "webhook")
        .map(|name| {
            let group = name.split_once('.').map(|(group, _)| group.to_owned());
            Tool::new(name.clone(), ToolRoute::Host { group }, json!({}))
        })
        .collect();
    tools.push(webhook());
    Toolset::new([], tools).map_err(|error| CliError::usage(format!("the mocked tools: {error}")))
}

#[cfg(test)]
mod tests {
    use rewind_api::toolsets::{Api as ApiDeclared, HostTool, HttpTool};

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
        let create = CreateToolset {
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
            ..CreateToolset::default()
        };
        let toolset = pin(&create).await.unwrap();
        assert_eq!(toolset.apis()[0].base_url, "http://127.0.0.1:8080/v1");
        let create_measure = toolset.tool("dax.createMeasure").unwrap();
        // A POST without Idempotency-Key is called at most once.
        assert_eq!(
            create_measure.policy.on_interruption,
            Interruption::AtMostOnce
        );
        assert!(toolset.tool("dax.listMeasures").unwrap().policy.approval);
    }

    #[tokio::test]
    async fn host_tools_and_http_are_pinned_as_written() {
        let create = CreateToolset {
            secrets: std::collections::BTreeMap::new(),
            apis: Vec::new(),
            workflows: Vec::new(),
            base: None,
            mcp: Vec::new(),
            host: vec![HostTool {
                name: "crm.ask_user".into(),
                description: "Ask the user.".into(),
                input_schema: json!({"type": "object", "properties": {"q": {"type": "string"}}}),
                output_schema: Some(json!({"type": "string"})),
                policy: Some(ApiPolicy {
                    on_interruption: Some(OnInterruption::AtMostOnce),
                    approval: Some(true),
                    concurrency: Some(1),
                    ..ApiPolicy::default()
                }),
            }],
            http: Some(HttpTool {
                allow: vec!["api.example.com".into()],
                policy: None,
            }),
        };
        let toolset = pin(&create).await.unwrap();
        let names: Vec<&str> = toolset
            .tools()
            .iter()
            .map(|tool| tool.name.as_str())
            .collect();
        assert_eq!(names, ["crm.ask_user", "http", "webhook"]);
        let ask = &toolset.tools()[0];
        assert_eq!(
            ask.route,
            ToolRoute::Host {
                group: Some("crm".into())
            }
        );
        assert_eq!(ask.policy.on_interruption, Interruption::AtMostOnce);
        assert!(ask.policy.approval);
        assert_eq!(ask.policy.concurrency, 1);
        assert_eq!(ask.output_schema, Some(json!({"type": "string"})));
    }

    #[tokio::test]
    async fn an_mcp_server_needs_one_transport() {
        let create: CreateToolset =
            serde_json::from_value(json!({ "mcp": [{ "name": "github" }] })).unwrap();
        let error = pin(&create).await.unwrap_err();
        assert!(error.contains("exactly one of url and command"), "{error}");
    }

    #[test]
    fn the_mocked_tools_are_host_tools() {
        let mut names: BTreeSet<String> = ["github.list_issues", "ask_user", "webhook"]
            .into_iter()
            .map(String::from)
            .collect();
        let toolset = of_mocks(&names).unwrap();
        let routes: Vec<(&str, &ToolRoute)> = toolset
            .tools()
            .iter()
            .map(|tool| (tool.name.as_str(), &tool.route))
            .collect();
        assert_eq!(routes.len(), 3);
        assert!(matches!(
            routes[0],
            ("ask_user", ToolRoute::Host { group: None })
        ));
        assert!(matches!(
            routes[1],
            ("github.list_issues", ToolRoute::Host { group: Some(_) })
        ));
        assert!(matches!(routes[2].1, ToolRoute::Builtin(Builtin::Webhook)));
        names.insert("http".into());
        let error = of_mocks(&names).unwrap_err();
        assert!(error.to_string().contains("allow-list"), "{error}");
        assert_eq!(short_name("github", "github.list_issues"), "list_issues");
    }
}
