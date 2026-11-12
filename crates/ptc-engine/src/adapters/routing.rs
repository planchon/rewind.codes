use ptc_core::{ToolFailure, ToolFailureKind, ToolRoute};

use crate::ports::{Credentials, Dispatched, ToolCall, ToolDispatcher};

/// Sends each call to the dispatcher of its route, so that a composition root plugs a real
/// adapter or a mock per route (`archi/04-tools.md`, "Routes"):
///
/// | Route | Dispatcher |
/// | --- | --- |
/// | MCP tools | `mcp`: `ptc-mcp`, or mocks |
/// | Built-ins (`http`, and `webhook` in mock mode: the engine answers it for live runs) | `builtins`: `ptc-mcp`'s `HttpTool`, or mocks |
/// | Host tools | `host`: mocks, in mock mode only. Outside it, host tools are claimed by the harness's worker and never dispatched |
///
/// [`Unrouted`] stands for a route without a dispatcher. Credentials go only to the dispatcher
/// of the call's route.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoutingDispatcher<M, B, H> {
    pub mcp: M,
    pub builtins: B,
    pub host: H,
}

impl<M, B, H> RoutingDispatcher<M, B, H> {
    pub fn new(mcp: M, builtins: B, host: H) -> Self {
        Self {
            mcp,
            builtins,
            host,
        }
    }
}

impl<M, B, H> ToolDispatcher for RoutingDispatcher<M, B, H>
where
    M: ToolDispatcher,
    B: ToolDispatcher,
    H: ToolDispatcher,
{
    async fn dispatch(&self, call: ToolCall, credentials: Option<Credentials>) -> Dispatched {
        match call.tool.route {
            ToolRoute::Mcp { .. } => self.mcp.dispatch(call, credentials).await,
            ToolRoute::Builtin(_) => self.builtins.dispatch(call, credentials).await,
            ToolRoute::Host { .. } => self.host.dispatch(call, credentials).await,
        }
    }
}

/// The dispatcher of a route this deployment does not serve: every call fails, for good, with
/// `unavailable`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Unrouted;

impl ToolDispatcher for Unrouted {
    async fn dispatch(&self, call: ToolCall, _credentials: Option<Credentials>) -> Dispatched {
        let route = match call.tool.route {
            ToolRoute::Mcp { .. } => "MCP tools",
            ToolRoute::Builtin(_) => "built-in tools",
            ToolRoute::Host { .. } => "host tools",
        };
        Dispatched::Failed(ToolFailure {
            kind: ToolFailureKind::Unavailable,
            message: format!(
                "{} cannot be called: this server dispatches no {route}",
                call.tool.name
            ),
            data: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use ptc_core::{Builtin, McpServer, McpTransport, RunId, Seq, Tool};
    use serde_json::json;

    use super::*;
    use crate::ports::{RunRef, TaskToken, TenantId};

    /// Answers with its name, and records the credentials it received.
    #[derive(Debug, Clone, Default)]
    struct Named {
        name: &'static str,
        credentials: Arc<Mutex<Vec<Option<Credentials>>>>,
    }

    impl ToolDispatcher for Named {
        async fn dispatch(&self, _call: ToolCall, credentials: Option<Credentials>) -> Dispatched {
            if let Ok(mut seen) = self.credentials.lock() {
                seen.push(credentials);
            }
            Dispatched::Result(json!(self.name))
        }
    }

    fn call(route: ToolRoute, name: &str) -> ToolCall {
        let run = RunRef::new(TenantId::local(), RunId::from_u128(1));
        ToolCall {
            idempotency_key: format!("{}/1", run.id),
            run,
            seq: Seq(1),
            tool: Tool::new(name, route, json!({})),
            server: None,
            args: json!({}),
            attempt: 1,
            schema_change_approved: false,
            token: TaskToken("t".into()),
            trace_context: None,
        }
    }

    #[tokio::test]
    async fn each_route_goes_to_its_dispatcher() {
        let mcp = Named {
            name: "mcp",
            ..Named::default()
        };
        let router = RoutingDispatcher::new(
            mcp.clone(),
            Named {
                name: "builtins",
                ..Named::default()
            },
            Named {
                name: "host",
                ..Named::default()
            },
        );
        let server = McpServer::new(
            "github",
            McpTransport::Http {
                url: "https://mcp.example.com".into(),
            },
        );
        let mcp_route = ToolRoute::Mcp {
            server: server.name.clone(),
        };
        let credentials = Credentials::default();
        let answered = [
            (call(mcp_route, "github.list_issues"), "mcp"),
            (
                call(
                    ToolRoute::Builtin(Builtin::Http {
                        allowed_hosts: vec!["example.com".into()],
                    }),
                    "http",
                ),
                "builtins",
            ),
            (call(ToolRoute::Host { group: None }, "ask_user"), "host"),
        ];
        for (call, expected) in answered {
            assert_eq!(
                router.dispatch(call, Some(credentials.clone())).await,
                Dispatched::Result(json!(expected))
            );
        }
        assert_eq!(
            *mcp.credentials.lock().unwrap(),
            [Some(credentials)],
            "the credentials reach the route's dispatcher"
        );
    }

    #[tokio::test]
    async fn a_route_without_a_dispatcher_fails_its_calls() {
        let router = RoutingDispatcher::new(Unrouted, Unrouted, Unrouted);
        let Dispatched::Failed(failure) = router
            .dispatch(call(ToolRoute::Host { group: None }, "ask_user"), None)
            .await
        else {
            panic!("an unrouted call fails");
        };
        assert_eq!(failure.kind, ToolFailureKind::Unavailable);
        assert_eq!(
            failure.message,
            "ask_user cannot be called: this server dispatches no host tools"
        );
    }
}
