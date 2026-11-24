//! The agent tools of the MCP set (`archi/07-code-architecture.md`, "MCP and OAuth"): the
//! SDK's handlers, with the namespace and the action set taken from each call's arguments rather
//! than from options, no labels of their own (the caller's grant carries them), the MCP bounds
//! (`wait` at most 200 s, 30 s by default; a result at most 80 000 characters), and the tools
//! that find what the caller reaches.
//!
//! The organization (`org`) is not read here: the MCP server resolves it among the caller's
//! organizations, checks the tool's permission there, and hands [`McpTools::handle`] an API
//! bound to that organization and to the call's namespace.

use std::collections::BTreeMap;
use std::time::Duration;

use rewind_api::access::Permission;
use rewind_api::namespaces::NamespaceListQuery;
use serde_json::Value;

use crate::api::McpApi;
use crate::definitions::MCP_TOOLS;
use crate::input::{InputError, Values, optional_string, parse};
use crate::render::{
    Limits, MCP_RESULT_LIMIT, OrganizationView, describe_action_sets, describe_namespaces,
    describe_orgs, truncate_middle,
};
use crate::tools::{AgentError, AgentToolResult, Failure, HandleOptions, Session, finish, result};

/// The longest wait of the MCP set: Claude gives a call 240 s.
pub const MCP_MAX_WAIT: Duration = Duration::from_secs(200);

/// The wait of the MCP set when the model gives none.
pub const MCP_DEFAULT_WAIT: Duration = Duration::from_secs(30);

/// An organization the caller reaches, as its host knows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Organization {
    /// The tenant's id.
    pub id: String,
    /// Its name, when it has one.
    pub name: Option<String>,
    /// The permissions the caller may use there, each once.
    pub permissions: Vec<Permission>,
}

impl Organization {
    /// Whether the caller may use `permission` there.
    pub fn allows(&self, permission: Permission) -> bool {
        self.permissions.contains(&permission)
    }
}

/// No labels of their own: the caller's grant scopes what they see and what they start.
static NO_LABELS: BTreeMap<String, String> = BTreeMap::new();

/// The handlers of the MCP set.
#[derive(Debug, Clone)]
pub struct McpTools {
    limits: Limits,
    default_wait: Duration,
    max_wait: Duration,
}

impl Default for McpTools {
    fn default() -> Self {
        Self {
            limits: Limits::for_total(MCP_RESULT_LIMIT),
            default_wait: MCP_DEFAULT_WAIT,
            max_wait: MCP_MAX_WAIT,
        }
    }
}

impl McpTools {
    /// The bounds of the MCP set.
    pub fn new() -> Self {
        Self::default()
    }

    /// The bounds of each result.
    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// `list_orgs`: the organizations the caller reaches, with what the tools may do in each.
    pub fn list_orgs(&self, organizations: &[Organization]) -> AgentToolResult {
        let views: Vec<OrganizationView<'_>> = organizations
            .iter()
            .map(|organization| OrganizationView {
                id: &organization.id,
                name: organization.name.as_deref(),
                permissions: &organization.permissions,
            })
            .collect();
        result(
            truncate_middle(&describe_orgs(&views), self.limits.total),
            false,
        )
    }

    /// Executes a call of a tool of the MCP set but `list_orgs`, with `api` bound to the
    /// organization of the call and to its namespace. `input` is the call's arguments, `org`
    /// included, which is left alone. Mistakes the model can fix come back as a result with
    /// `is_error`; a fault of the platform is an error.
    pub async fn handle<A: McpApi>(
        &self,
        api: &A,
        name: &str,
        input: &Value,
    ) -> Result<AgentToolResult, AgentError> {
        if name == "list_orgs" || !MCP_TOOLS.contains(&name) {
            return Err(AgentError::UnknownTool(name.to_owned()));
        }
        let call = HandleOptions::default();
        let handled = match parse(name, input) {
            Ok(values) => self.dispatch(api, name, &values, &call).await,
            Err(error) => Err(error.into()),
        };
        finish(name, handled, &self.limits)
    }

    async fn dispatch<A: McpApi>(
        &self,
        api: &A,
        name: &str,
        values: &Values,
        call: &HandleOptions,
    ) -> Result<AgentToolResult, Failure> {
        match name {
            "list_namespaces" => {
                let prefix = optional_string(values, "prefix")?;
                let query = NamespaceListQuery {
                    prefix: prefix.clone(),
                    ..NamespaceListQuery::default()
                };
                let list = api.list_namespaces(&query).await?;
                return Ok(result(describe_namespaces(&list, prefix.as_deref()), false));
            }
            "list_action_sets" => {
                let list = api.list_action_sets().await?;
                return Ok(result(describe_action_sets(&list), false));
            }
            "check_script" | "test_script" | "run_script" => {
                require_namespace(values)?;
            }
            _ => {}
        }
        let action_set = match name {
            "search_actions" => Some(require_action_set(values)?),
            "check_script" | "test_script" | "run_script" => optional_string(values, "action_set")?,
            _ => None,
        };
        let session = Session {
            api,
            action_set,
            labels: &NO_LABELS,
            limits: self.limits,
            default_wait: self.default_wait,
            max_wait: self.max_wait,
            ttl: None,
            call,
        };
        match name {
            "list_secrets" => {
                let namespace = optional_string(values, "namespace")?;
                session.list_secrets(namespace.as_deref()).await
            }
            "search_actions" => session.search_actions(values).await,
            "check_script" => session.check_script(values).await,
            "test_script" => session.test_script(values).await,
            "run_script" => session.run_script(values).await,
            "get_run" => session.get_run(values).await,
            "list_runs" => session.list_runs(values).await,
            "signal_run" => session.signal_run(values).await,
            "cancel_run" => session.cancel_run(values).await,
            "retry_run" => session.retry_run(values).await,
            "get_workflow" => session.get_workflow(values).await,
            "record_tests" => session.record_tests(values).await,
            other => Err(Failure::Agent(AgentError::UnknownTool(other.to_owned()))),
        }
    }
}

/// What checks, tests and runs a script names its namespace: the end user it acts for.
fn require_namespace(values: &Values) -> Result<(), InputError> {
    match values.get("namespace") {
        Some(Value::String(name)) if !name.is_empty() => Ok(()),
        _ => Err(InputError::new(
            "`namespace` must be a non-empty string: the namespace (the end user) the script acts for, a name list_namespaces gives or a new one.",
        )),
    }
}

fn require_action_set(values: &Values) -> Result<String, InputError> {
    match values.get("action_set") {
        Some(Value::String(id)) if !id.is_empty() => Ok(id.clone()),
        _ => Err(InputError::new(
            "`action_set` must be a non-empty string: an action set's id, as list_action_sets gives it.",
        )),
    }
}
