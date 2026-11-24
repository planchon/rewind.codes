//! The secrets a run uses, and who may use them (`archi/04-tools.md`, "Secrets":
//! using).
//!
//! ```text
//! POST /v1/runs (code)        the request's    checked against the caller   SecretRights::Caller
//! POST /v1/runs/{id}/fork       the source's, or the request's: against the caller
//! POST /v1/runs (workflow),     the version's    only that its ns/ secrets     SecretRights::Version
//! workflows.<name>.start                         exist in the run's namespace
//! POST /v1/workflows            the request's    against the saver; ns/ by name only
//! ```
//!
//! A start that fails these checks is refused before any run exists
//! ([`StartError::SecretRequired`](crate::StartError::SecretRequired), 422 `secret-required`).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use rewind_core::{ActionAuth, ActionSet, SecretRef, SecretScope, ServerSecrets, Timestamp};

use crate::ports::{
    Day, NamespaceName, SecretError, SecretId, SecretNamespace, SecretStore, TenantId,
};

/// The most secrets a refusal lists as usable.
pub const MAX_USABLE: usize = 100;

/// What the caller may use (`archi/06-storage-and-scale.md`, "What the caller may do").
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum SecretRights {
    /// A namespace's secrets only, which whoever starts a run in that namespace uses: a
    /// caller without `secrets:use`.
    #[default]
    NamespaceOnly,
    /// Also the organization's secrets carrying all of these labels (none: every one): a caller
    /// with `secrets:use`, within its grant's or its token's labels.
    Organization { labels: BTreeMap<String, String> },
    /// The secrets of a saved workflow version: its saver's rights were checked when it was
    /// saved, and only the existence of its `ns/` secrets in the run's namespace is checked.
    Version,
}

/// Why the secrets of a start, a fork or a save refuse it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretProblem {
    /// The code imports this MCP server or API, whose `auth` takes a secret, and nothing is
    /// used for it.
    Missing { server: String },
    /// A secret names a server or an API the action set does not have, or one whose `auth`
    /// takes no secret.
    TakesNone { server: String },
    /// The secret does not exist: an `ns/` secret in the run's namespace, an `org/` secret in
    /// the organization.
    Unknown { server: String, secret: SecretRef },
    /// The caller may not use this secret: an organization's secret without `secrets:use`,
    /// or outside the caller's labels.
    Forbidden { server: String, secret: SecretRef },
}

impl fmt::Display for SecretProblem {
    /// Written for the model that writes the request.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SecretProblem::Missing { server } => write!(
                f,
                "the code imports {server}, which needs a secret: name one in \"secrets\" \
                 (\"{server}\": \"org/<name>\" or \"ns/<name>\")"
            ),
            SecretProblem::TakesNone { server } => write!(
                f,
                "{server} takes no secret: it is no server or API of the action set whose calls \
                 carry one"
            ),
            SecretProblem::Unknown { server, secret } => {
                write!(f, "the secret {secret} named for {server} does not exist")
            }
            SecretProblem::Forbidden { server, secret } => {
                write!(
                    f,
                    "the secret {secret} named for {server} is not yours to use"
                )
            }
        }
    }
}

/// A start, a fork or a save refused by its secrets: each one at fault, by server, and what
/// the caller may use instead (422 `secret-required`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefusedSecrets {
    /// Sorted by server.
    pub problems: Vec<SecretProblem>,
    /// The secrets the caller may use, sorted, at most [`MAX_USABLE`]: the organization's
    /// it may use, and those of the run's namespace. What each problem but
    /// [`TakesNone`](SecretProblem::TakesNone) may use instead.
    pub usable: Vec<SecretRef>,
}

impl RefusedSecrets {
    /// Whether a problem asks for a secret: anything but a secret for a server that takes
    /// none.
    pub fn asks_for_a_secret(&self) -> bool {
        self.problems
            .iter()
            .any(|problem| !matches!(problem, SecretProblem::TakesNone { .. }))
    }
}

impl fmt::Display for RefusedSecrets {
    /// Written for the model that writes the request: each problem, then the secrets it may
    /// use.
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
        if self.usable.is_empty() {
            return f.write_str(
                ". No secret exists that you may use: a person puts one first, the end \
                 user's in their namespace or the organization's",
            );
        }
        f.write_str(". The secrets you may use: ")?;
        for (index, secret) in self.usable.iter().enumerate() {
            if index > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{secret}")?;
        }
        Ok(())
    }
}

/// What the secrets of a start, a fork or a save are checked against.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SecretCheck<'a> {
    pub(crate) tenant: &'a TenantId,
    /// The run's namespace, where its `ns/` secrets are read; `None` for a version saved, which
    /// has none yet: its `ns/` secrets are checked by name only.
    pub(crate) namespace: Option<&'a NamespaceName>,
    pub(crate) action_set: &'a ActionSet,
    /// What the code imports from `rewind:actions`; `None`: every name.
    pub(crate) imports: Option<&'a BTreeSet<String>>,
    pub(crate) secrets: &'a ServerSecrets,
    pub(crate) rights: &'a SecretRights,
}

/// Checks secrets (`archi/04-tools.md`, "Secrets": using): each names a server or an API
/// of the action set whose `auth` takes a secret, and a secret that exists and the caller may
/// use; each server or API the code imports whose `auth` takes a secret has one. A
/// version's secrets ([`SecretRights::Version`]) were checked when it was saved: only its
/// `ns/` secrets' existence in the run's namespace is. `Ok(Err(..))` lists every problem and
/// what the caller may use instead.
pub(crate) async fn check<S: SecretStore>(
    secrets: &S,
    now: Timestamp,
    check: SecretCheck<'_>,
) -> Result<Result<(), RefusedSecrets>, SecretError> {
    let today = Day::of(now);
    let mut problems = Vec::new();
    let version = *check.rights == SecretRights::Version;
    for (server, secret) in check.secrets.iter() {
        let takes_secret = check
            .action_set
            .auth(server)
            .is_some_and(ActionAuth::takes_secret);
        if !takes_secret && !version {
            problems.push(SecretProblem::TakesNone {
                server: server.to_owned(),
            });
            continue;
        }
        let problem = |forbidden: bool| {
            let (server, secret) = (server.to_owned(), secret.clone());
            if forbidden {
                SecretProblem::Forbidden { server, secret }
            } else {
                SecretProblem::Unknown { server, secret }
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
                SecretRights::Version => {}
                SecretRights::NamespaceOnly => problems.push(problem(true)),
                SecretRights::Organization { labels } => {
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
            if imported && check.secrets.get(server).is_none() {
                problems.push(SecretProblem::Missing {
                    server: server.to_owned(),
                });
            }
        }
    }
    if problems.is_empty() {
        return Ok(Ok(()));
    }
    problems.sort_by(|a, b| a.server().cmp(b.server()));
    let mut refused = RefusedSecrets {
        problems,
        usable: Vec::new(),
    };
    if refused.asks_for_a_secret() {
        refused.usable = usable(secrets, check, today).await?;
    }
    Ok(Err(refused))
}

/// The secrets the caller may use: the organization's carrying its labels with
/// `secrets:use`, and those of the run's namespace.
async fn usable<S: SecretStore>(
    secrets: &S,
    check: SecretCheck<'_>,
    today: Day,
) -> Result<Vec<SecretRef>, SecretError> {
    let mut found = Vec::new();
    if let SecretRights::Organization { labels } = check.rights {
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
    found.truncate(MAX_USABLE);
    Ok(found)
}

/// Whether `labels` holds every one of `wanted`.
fn carries(labels: &BTreeMap<String, String>, wanted: &BTreeMap<String, String>) -> bool {
    wanted
        .iter()
        .all(|(key, value)| labels.get(key) == Some(value))
}

impl SecretProblem {
    /// The server or API at fault.
    pub fn server(&self) -> &str {
        match self {
            SecretProblem::Missing { server }
            | SecretProblem::TakesNone { server }
            | SecretProblem::Unknown { server, .. }
            | SecretProblem::Forbidden { server, .. } => server,
        }
    }

    /// The secret used, if the problem is about one.
    pub fn secret(&self) -> Option<&SecretRef> {
        match self {
            SecretProblem::Missing { .. } | SecretProblem::TakesNone { .. } => None,
            SecretProblem::Unknown { secret, .. } | SecretProblem::Forbidden { secret, .. } => {
                Some(secret)
            }
        }
    }
}
