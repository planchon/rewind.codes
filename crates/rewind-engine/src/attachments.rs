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

use std::collections::BTreeMap;
use std::fmt;

use rewind_core::SecretRef;

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
    /// An attachment names a server or an API the toolset does not have, or one whose `auth`
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
                "{server} takes no secret: it is no server or API of the toolset whose calls \
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
