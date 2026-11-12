//! Toolsets pinned in the CLI process, for the local commands (`ptc check`, `ptc test`): from a
//! toolset file, or from the tools a scenario's mocks answer.
//!
//! A toolset file is the body of `POST /v1/toolsets` (`CreateToolset`), pinned as a server pins
//! it: the tools of each MCP server are discovered by connecting to it (`ptc-mcp`) with the
//! file's credentials, host tools and the `http` tool are taken as written, the `webhook`
//! built-in is always there.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use ptc_api::toolsets::{
    CreateToolset, OnInterruption, ServerCredentials as ApiServerCredentials,
    ToolPolicy as ApiPolicy,
};
use ptc_core::{
    Builtin, Interruption, McpServer, McpTransport, Tool, ToolPolicy, ToolRoute, Toolset,
};
use ptc_engine::ports::{Secret, ServerCredentials, TenantId};
use ptc_mcp::{McpConfig, McpDispatcher, McpEgress};
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
    let mut tools = Vec::new();
    if !create.mcp.is_empty() {
        // As the embedded server: the developer's own servers, at any address.
        let discovery = McpDispatcher::new(McpConfig {
            egress: McpEgress::open(),
            ..McpConfig::default()
        });
        let pinned = discover(&discovery, create, &mut servers, &mut tools).await;
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
        let mut tool = ptc_mcp::http_tool(http.allow.clone());
        apply(&mut tool.policy, http.policy.as_ref());
        tools.push(tool);
    }
    tools.push(webhook());
    Toolset::new(servers, tools).map_err(|error| error.to_string())
}

async fn discover(
    discovery: &McpDispatcher,
    create: &CreateToolset,
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
        let pinned = McpServer::new(server.name.clone(), transport);
        let credentials = create
            .credentials
            .as_ref()
            .and_then(|credentials| credentials.servers.get(&server.name))
            .map(secrets);
        let listed = discovery
            .list_tools(&TenantId::local(), &pinned, credentials.as_ref())
            .await
            .map_err(|error| format!("cannot list the tools of {:?}: {error}", server.name))?;
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

/// A server's credentials from a toolset file, as the transport takes them.
fn secrets(credentials: &ApiServerCredentials) -> ServerCredentials {
    let secret = |values: &BTreeMap<String, String>| {
        values
            .iter()
            .map(|(name, value)| (name.clone(), Secret::new(value.clone())))
            .collect()
    };
    ServerCredentials {
        headers: secret(&credentials.headers),
        env: secret(&credentials.env),
    }
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
        ptc_runtime::builtins::webhook_input_schema(),
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
    use ptc_api::toolsets::{HostTool, HttpTool};

    use super::*;

    #[tokio::test]
    async fn host_tools_and_http_are_pinned_as_written() {
        let create = CreateToolset {
            credentials: None,
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
