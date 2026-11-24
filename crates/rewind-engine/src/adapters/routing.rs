use rewind_core::{ActionFailure, ActionFailureKind, ActionRoute};

use crate::ports::{ActionDispatcher, Call, CallAuth, Dispatched};

/// Sends each call to the dispatcher of its route, so that a composition root plugs a real
/// adapter or a handler per route (`archi/04-tools.md`, "Routes"):
///
/// | Route | Dispatcher |
/// | --- | --- |
/// | MCP actions | `mcp`: `rewind-mcp`, or handlers |
/// | API actions | `api`: `rewind-openapi`'s `ApiDispatcher`, or handlers |
/// | Built-ins (`http`, and `webhook` in a test run: the engine answers it for live runs) | `builtins`: `rewind-mcp`'s `HttpAction`, or handlers |
/// | Harness actions | `harness`: handlers, in a test run only. Outside it, harness actions are handed to the SDK workers' streams and never dispatched |
///
/// [`Unrouted`] stands for a route without a dispatcher. A call's [`CallAuth`] goes only to the
/// dispatcher of its route.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoutingDispatcher<M, A, B, H> {
    pub mcp: M,
    pub api: A,
    pub builtins: B,
    pub harness: H,
}

impl<M, A, B, H> RoutingDispatcher<M, A, B, H> {
    pub fn new(mcp: M, api: A, builtins: B, harness: H) -> Self {
        Self {
            mcp,
            api,
            builtins,
            harness,
        }
    }
}

impl<M, A, B, H> ActionDispatcher for RoutingDispatcher<M, A, B, H>
where
    M: ActionDispatcher,
    A: ActionDispatcher,
    B: ActionDispatcher,
    H: ActionDispatcher,
{
    async fn dispatch(&self, call: Call, auth: CallAuth) -> Dispatched {
        match call.action.route {
            ActionRoute::Mcp { .. } => self.mcp.dispatch(call, auth).await,
            ActionRoute::Api { .. } => self.api.dispatch(call, auth).await,
            ActionRoute::Builtin(_) => self.builtins.dispatch(call, auth).await,
            ActionRoute::Harness { .. } => self.harness.dispatch(call, auth).await,
        }
    }
}

/// The dispatcher of a route this deployment does not serve: every call fails, for good, with
/// `unavailable`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Unrouted;

impl ActionDispatcher for Unrouted {
    async fn dispatch(&self, call: Call, _auth: CallAuth) -> Dispatched {
        let route = match call.action.route {
            ActionRoute::Mcp { .. } => "MCP actions",
            ActionRoute::Api { .. } => "API actions",
            ActionRoute::Builtin(_) => "built-in actions",
            ActionRoute::Harness { .. } => "harness actions",
        };
        Dispatched::Failed(ActionFailure {
            kind: ActionFailureKind::Unavailable,
            message: format!(
                "{} cannot be called: this server dispatches no {route}",
                call.action.name
            ),
            data: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use rewind_core::{Action, Builtin, McpServer, McpTransport, RunId, Seq};
    use serde_json::json;

    use super::*;
    use crate::ports::{NamespaceName, RunRef, Secret, TaskToken, TenantId};

    /// Answers with its name, and records the authentication it received.
    #[derive(Debug, Clone, Default)]
    struct Named {
        name: &'static str,
        auth: Arc<Mutex<Vec<CallAuth>>>,
    }

    impl ActionDispatcher for Named {
        async fn dispatch(&self, _call: Call, auth: CallAuth) -> Dispatched {
            if let Ok(mut seen) = self.auth.lock() {
                seen.push(auth);
            }
            Dispatched::Result(json!(self.name))
        }
    }

    fn call(route: ActionRoute, name: &str) -> Call {
        let run = RunRef::new(TenantId::local(), RunId::from_u128(1));
        Call {
            idempotency_key: format!("{}/1", run.id),
            run,
            namespace: NamespaceName::new("alice").unwrap(),
            labels: Default::default(),
            seq: Seq(1),
            action: Action::new(name, route, json!({})),
            server: None,
            api: None,
            args: json!({}),
            delivery: 1,
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
                name: "api",
                ..Named::default()
            },
            Named {
                name: "builtins",
                ..Named::default()
            },
            Named {
                name: "harness",
                ..Named::default()
            },
        );
        let server = McpServer::new(
            "github",
            McpTransport::Http {
                url: "https://mcp.example.com".into(),
            },
        );
        let mcp_route = ActionRoute::Mcp {
            server: server.name.clone(),
        };
        let auth = CallAuth::Secret {
            value: Secret::new("ghp_123"),
            placement: rewind_core::SecretPlacement::bearer(),
        };
        let answered = [
            (call(mcp_route, "github.list_issues"), "mcp"),
            (
                call(ActionRoute::Api { api: "dax".into() }, "dax.createMeasure"),
                "api",
            ),
            (
                call(
                    ActionRoute::Builtin(Builtin::Http {
                        allowed_hosts: vec!["example.com".into()],
                    }),
                    "http",
                ),
                "builtins",
            ),
            (
                call(ActionRoute::Harness { group: None }, "ask_user"),
                "harness",
            ),
        ];
        for (call, expected) in answered {
            let auth = if expected == "mcp" {
                auth.clone()
            } else {
                CallAuth::None
            };
            assert_eq!(
                router.dispatch(call, auth).await,
                Dispatched::Result(json!(expected))
            );
        }
        assert_eq!(
            *mcp.auth.lock().unwrap(),
            [auth],
            "the authentication reaches the route's dispatcher"
        );
    }

    #[tokio::test]
    async fn a_route_without_a_dispatcher_fails_its_calls() {
        let router = RoutingDispatcher::new(Unrouted, Unrouted, Unrouted, Unrouted);
        let Dispatched::Failed(failure) = router
            .dispatch(
                call(ActionRoute::Harness { group: None }, "ask_user"),
                CallAuth::None,
            )
            .await
        else {
            panic!("an unrouted call fails");
        };
        assert_eq!(failure.kind, ActionFailureKind::Unavailable);
        assert_eq!(
            failure.message,
            "ask_user cannot be called: this server dispatches no harness actions"
        );
    }
}
