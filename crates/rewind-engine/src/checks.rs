//! The checks of the run plane that read the control plane: the bounds of a namespace, the
//! signals a run of a workflow takes, the secrets a workflow version attaches
//! (`archi/03-execution.md`, "Starting a run", "Workflows across runs"; `archi/04-tools.md`,
//! "Secrets"; `archi/06-storage-and-scale.md`, "Quotas"). The edge and the call executor share
//! them.

use std::time::Duration;

use rewind_core::{JsonValue, Workflow, is_reserved_signal};

use crate::attachments::{self, AttachRights, AttachmentCheck, RefusedAttachments};
use crate::engine::NamespaceBounds;
use crate::plane::NodeConfig;
use crate::ports::{
    CatalogStore, NamespaceName, RunHead, SecretError, SecretStore, Tenant, TenantId,
    limits as namespace_limits,
};
use crate::schema::Validator;
use crate::workflows::{SignalRefused, refused_message};

/// Why the bounds of a namespace could not be given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BoundsError {
    /// The namespace is being erased: it takes no run.
    Deleting,
    /// The catalog could not be read.
    Unavailable(String),
}

/// The bounds of a start or a fork in a namespace: the largest TTL (the deployment's, the
/// namespace's and the tenant's `maxTtl`) and the most lines of a script.
pub(crate) async fn bounds<C: CatalogStore>(
    catalog: &C,
    config: &NodeConfig,
    tenant: &TenantId,
    namespace: &NamespaceName,
) -> Result<NamespaceBounds, BoundsError> {
    let unavailable =
        |error: crate::ports::CatalogError| BoundsError::Unavailable(error.to_string());
    let record = catalog
        .get_tenant(tenant)
        .await
        .map_err(unavailable)?
        .unwrap_or_else(|| Tenant::new(tenant.clone()));
    let found = catalog
        .get_namespace(tenant, namespace)
        .await
        .map_err(unavailable)?;
    if found
        .as_ref()
        .is_some_and(|namespace| namespace.deleting_since.is_some())
    {
        return Err(BoundsError::Deleting);
    }
    let bounds = namespace_limits(&record, found.as_ref()).tightest(&record.quotas);
    let max_ttl = bounds
        .max_ttl
        .map_or(config.max_ttl, |max| max.min(config.max_ttl));
    Ok(NamespaceBounds {
        max_ttl,
        default_ttl: config.default_ttl.min(max_ttl),
        max_script_lines: bounds.max_script_lines.map(u64::from),
    })
}

/// The TTL a run asks for, within its bounds.
pub(crate) fn ttl(
    requested: Option<Duration>,
    bounds: &NamespaceBounds,
) -> Result<Duration, Duration> {
    let ttl = requested.unwrap_or(bounds.default_ttl);
    if ttl.is_zero() || ttl > bounds.max_ttl {
        return Err(ttl);
    }
    Ok(ttl)
}

/// Checks a signal sent to a run: Rewind's own names are refused, and a run of a workflow only
/// takes the signals its version declares, with a payload that matches their schema.
pub(crate) async fn signal<C: CatalogStore>(
    catalog: &C,
    head: &RunHead,
    name: &str,
    payload: &JsonValue,
) -> Result<(), SignalCheck> {
    if is_reserved_signal(name) {
        return Err(SignalCheck::Refused(
            SignalRefused::Reserved(name.to_owned()).to_string(),
        ));
    }
    let Some(of) = &head.metadata.workflow else {
        return Ok(());
    };
    let stored = catalog
        .get_workflow(&head.run.tenant, of.version)
        .await
        .map_err(|error| SignalCheck::Unavailable(error.to_string()))?
        .ok_or_else(|| {
            SignalCheck::Unavailable(format!(
                "the workflow version {} of run {} is not stored",
                of.version, head.run.id
            ))
        })?;
    declared(&stored.workflow, head, name, payload).map_err(SignalCheck::Refused)
}

/// Why a signal is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SignalCheck {
    Refused(String),
    Unavailable(String),
}

fn declared(
    workflow: &Workflow,
    head: &RunHead,
    name: &str,
    payload: &JsonValue,
) -> Result<(), String> {
    let Some(schema) = workflow.schemas().signals.get(name) else {
        let declared: Vec<String> = workflow
            .schemas()
            .signals
            .keys()
            .map(|name| format!("{name:?}"))
            .collect();
        return Err(SignalRefused::Undeclared(format!(
            "run {} is a run of {workflow}, which does not declare the signal {name:?} (it \
             declares {})",
            head.run.id,
            if declared.is_empty() {
                "none".to_owned()
            } else {
                declared.join(", ")
            }
        ))
        .to_string());
    };
    Validator::compile(schema)
        .check("payload", payload)
        .map_err(|refused| {
            SignalRefused::Payload(refused_message(
                &format!("the {name:?} signal of {workflow}"),
                refused,
            ))
            .to_string()
        })
}

/// Refuses a run of a version whose `ns/` secrets do not exist in the run's namespace. The
/// version's attachments are not checked against anyone's rights: they were when it was saved.
pub(crate) async fn version_attachments<S: SecretStore>(
    secrets: &S,
    now: rewind_core::Timestamp,
    tenant: &TenantId,
    namespace: &NamespaceName,
    workflow: &Workflow,
) -> Result<Result<(), RefusedAttachments>, SecretError> {
    if workflow.secrets().is_empty() {
        return Ok(Ok(()));
    }
    let check = AttachmentCheck {
        tenant,
        namespace: Some(namespace),
        toolset: &rewind_core::Toolset::empty(),
        imports: None,
        attachments: workflow.secrets(),
        rights: &AttachRights::Version,
    };
    attachments::check(secrets, now, check).await
}
