//! The secrets a run attaches, and who may attach them (`archi/04-tools.md`, "Secrets":
//! attaching).
//!
//! ```text
//! POST /v1/runs (script)        the request's    checked against the caller   AttachRights::Caller
//! POST /v1/runs/{id}/fork       the parent's, or the request's: against the caller
//! POST /v1/runs (workflow),     the version's    only that its ns/ secrets     AttachRights::Version
//! workflows.<name>.start                         exist in the run's namespace
//! POST /v1/workflows            the request's    against the saver; ns/ by name only
//! ```
//!
//! A start that fails these checks is refused before any run exists
//! ([`StartError::SecretRequired`](crate::StartError::SecretRequired), 422 `secret-required`).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use rewind_core::{ActionSet, Attachments, SecretRef, SecretScope, Timestamp, ToolAuth};

use crate::ports::{
    Day, NamespaceName, SecretError, SecretId, SecretNamespace, SecretStore, TenantId,
};

/// The most secrets a refusal lists as attachable.
pub const MAX_ATTACHABLE: usize = 100;

/// What the caller may attach (`archi/06-storage-and-scale.md`, "What the caller may do").
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum AttachRights {
    /// A namespace's secrets only, which whoever starts a run in that namespace attaches: a
    /// caller without `secrets:use`.
    #[default]
    NamespaceOnly,
    /// Also the organization's secrets carrying all of these labels (none: every one): a caller
    /// with `secrets:use`, within its grant's or its token's labels.
    Organization { labels: BTreeMap<String, String> },
    /// The attachments of a saved workflow version: its saver's rights were checked when it was
    /// saved, and only the existence of its `ns/` secrets in the run's namespace is checked.
    Version,
}

/// Why an attachment refuses a start, a fork or a save.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachmentProblem {
    /// The script imports this MCP server or API, whose `auth` takes a secret, and nothing is
    /// attached for it.
    Missing { server: String },
    /// An attachment names a server or an API the action set does not have, or one whose `auth`
    /// takes no secret.
    TakesNone { server: String },
    /// The secret does not exist: an `ns/` secret in the run's namespace, an `org/` secret in
    /// the organization.
    Unknown { server: String, secret: SecretRef },
    /// The caller may not attach this secret: an organization's secret without `secrets:use`,
    /// or outside the caller's labels.
    Forbidden { server: String, secret: SecretRef },
}

impl fmt::Display for AttachmentProblem {
    /// Written for the model that writes the request.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AttachmentProblem::Missing { server } => write!(
                f,
                "the script imports {server}, which needs a secret: attach one in \"secrets\" \
                 (\"{server}\": \"org/<name>\" or \"ns/<name>\")"
            ),
            AttachmentProblem::TakesNone { server } => write!(
                f,
                "{server} takes no secret: it is no server or API of the action set whose calls \
                 carry one"
            ),
            AttachmentProblem::Unknown { server, secret } => {
                write!(
                    f,
                    "the secret {secret} attached for {server} does not exist"
                )
            }
            AttachmentProblem::Forbidden { server, secret } => {
                write!(
                    f,
                    "the secret {secret} attached for {server} is not yours to attach"
                )
            }
        }
    }
}

/// A start, a fork or a save refused by its attachments: each one at fault, by server, and what
/// the caller may attach instead (422 `secret-required`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefusedAttachments {
    /// Sorted by server.
    pub problems: Vec<AttachmentProblem>,
    /// The secrets the caller may attach, sorted, at most [`MAX_ATTACHABLE`]: the organization's
    /// it may attach, and those of the run's namespace. What each problem but
    /// [`TakesNone`](AttachmentProblem::TakesNone) may attach instead.
    pub attachable: Vec<SecretRef>,
}

impl RefusedAttachments {
    /// Whether a problem asks for a secret: anything but an attachment for a server that takes
    /// none.
    pub fn asks_for_a_secret(&self) -> bool {
        self.problems
            .iter()
            .any(|problem| !matches!(problem, AttachmentProblem::TakesNone { .. }))
    }
}

impl fmt::Display for RefusedAttachments {
    /// Written for the model that writes the request: each problem, then the secrets it may
    /// attach.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, problem) in self.problems.iter().enumerate() {
            if index > 0 {
                f.write_str("; ")?;
            }
            write!(f, "{problem}")?;
        }
        if !self.asks_for_a_secret() {
            return Ok(());
        }
        if self.attachable.is_empty() {
            return f.write_str(
                ". No secret exists that you may attach: a person puts one first, the end \
                 user's in their namespace or the organization's",
            );
        }
        f.write_str(". The secrets you may attach: ")?;
        for (index, secret) in self.attachable.iter().enumerate() {
            if index > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{secret}")?;
        }
        Ok(())
    }
}

/// What the attachments of a start, a fork or a save are checked against.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AttachmentCheck<'a> {
    pub(crate) tenant: &'a TenantId,
    /// The run's namespace, where its `ns/` secrets are read; `None` for a version saved, which
    /// has none yet: its `ns/` secrets are checked by name only.
    pub(crate) namespace: Option<&'a NamespaceName>,
    pub(crate) action_set: &'a ActionSet,
    /// What the script imports from `rewind:tools`; `None`: every name.
    pub(crate) imports: Option<&'a BTreeSet<String>>,
    pub(crate) attachments: &'a Attachments,
    pub(crate) rights: &'a AttachRights,
}

/// Checks attachments (`archi/04-tools.md`, "Secrets": attaching): each names a server or an API
/// of the action set whose `auth` takes a secret, and a secret that exists and the caller may
/// attach; each server or API the script imports whose `auth` takes a secret has one. A
/// version's attachments ([`AttachRights::Version`]) were checked when it was saved: only its
/// `ns/` secrets' existence in the run's namespace is. `Ok(Err(..))` lists every problem and
/// what the caller may attach instead.
pub(crate) async fn check<S: SecretStore>(
    secrets: &S,
    now: Timestamp,
    check: AttachmentCheck<'_>,
) -> Result<Result<(), RefusedAttachments>, SecretError> {
    let today = Day::of(now);
    let mut problems = Vec::new();
    let version = *check.rights == AttachRights::Version;
    for (server, secret) in check.attachments.iter() {
        let takes_secret = check
            .action_set
            .auth(server)
            .is_some_and(ToolAuth::takes_secret);
        if !takes_secret && !version {
            problems.push(AttachmentProblem::TakesNone {
                server: server.to_owned(),
            });
            continue;
        }
        let problem = |forbidden: bool| {
            let (server, secret) = (server.to_owned(), secret.clone());
            if forbidden {
                AttachmentProblem::Forbidden { server, secret }
            } else {
                AttachmentProblem::Unknown { server, secret }
            }
        };
        match secret.scope {
            SecretScope::Namespace => {
                // A version saved has no namespace yet: the name was checked when it was read.
                let Some(namespace) = check.namespace else {
                    continue;
                };
                let id = SecretId::resolve(secret, namespace);
                if secrets.meta(check.tenant, &id, today).await?.is_none() {
                    problems.push(problem(false));
                }
            }
            SecretScope::Organization => match check.rights {
                AttachRights::Version => {}
                AttachRights::NamespaceOnly => problems.push(problem(true)),
                AttachRights::Organization { labels } => {
                    let id = SecretId::organization(secret.name.clone());
                    let meta = secrets.meta(check.tenant, &id, today).await?;
                    // A secret outside the caller's labels is one it does not see.
                    if !meta.is_some_and(|meta| carries(&meta.labels, labels)) {
                        problems.push(problem(false));
                    }
                }
            },
        }
    }
    if !version {
        for server in check.action_set.taking_secrets() {
            let imported = check.imports.is_none_or(|imports| imports.contains(server));
            if imported && check.attachments.get(server).is_none() {
                problems.push(AttachmentProblem::Missing {
                    server: server.to_owned(),
                });
            }
        }
    }
    if problems.is_empty() {
        return Ok(Ok(()));
    }
    problems.sort_by(|a, b| a.server().cmp(b.server()));
    let mut refused = RefusedAttachments {
        problems,
        attachable: Vec::new(),
    };
    if refused.asks_for_a_secret() {
        refused.attachable = attachable(secrets, check, today).await?;
    }
    Ok(Err(refused))
}

/// The secrets the caller may attach: the organization's carrying its labels with
/// `secrets:use`, and those of the run's namespace.
async fn attachable<S: SecretStore>(
    secrets: &S,
    check: AttachmentCheck<'_>,
    today: Day,
) -> Result<Vec<SecretRef>, SecretError> {
    let mut found = Vec::new();
    if let AttachRights::Organization { labels } = check.rights {
        let listed = secrets
            .list(check.tenant, &SecretNamespace::Organization, labels, today)
            .await?;
        found.extend(listed.iter().map(|meta| meta.id.to_ref()));
    }
    if let Some(namespace) = check.namespace {
        let listed = secrets
            .list(
                check.tenant,
                &SecretNamespace::Namespace(namespace.clone()),
                &BTreeMap::new(),
                today,
            )
            .await?;
        found.extend(listed.iter().map(|meta| meta.id.to_ref()));
    }
    found.sort();
    found.truncate(MAX_ATTACHABLE);
    Ok(found)
}

/// Whether `labels` holds every one of `wanted`.
fn carries(labels: &BTreeMap<String, String>, wanted: &BTreeMap<String, String>) -> bool {
    wanted
        .iter()
        .all(|(key, value)| labels.get(key) == Some(value))
}

impl AttachmentProblem {
    /// The server or API at fault.
    pub fn server(&self) -> &str {
        match self {
            AttachmentProblem::Missing { server }
            | AttachmentProblem::TakesNone { server }
            | AttachmentProblem::Unknown { server, .. }
            | AttachmentProblem::Forbidden { server, .. } => server,
        }
    }

    /// The secret attached, if the problem is about one.
    pub fn secret(&self) -> Option<&SecretRef> {
        match self {
            AttachmentProblem::Missing { .. } | AttachmentProblem::TakesNone { .. } => None,
            AttachmentProblem::Unknown { secret, .. }
            | AttachmentProblem::Forbidden { secret, .. } => Some(secret),
        }
    }
}
