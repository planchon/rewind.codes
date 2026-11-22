//! Secrets and signing keys (`archi/04-tools.md`, "Secrets" and "Signed calls";
//! `archi/07-code-architecture.md`, "HTTP API /v1").
//!
//! A secret is a named value of the organization (`org/<name>`) or of a namespace (`ns/<name>`),
//! written and never read back: every answer gives its metadata only. A run reads the secrets
//! it attached when it started, by server or API (`secrets` of `POST /v1/runs`, of a fork, of a
//! workflow version). The organization's signing key signs the calls of the servers whose `auth`
//! is `rewind`; its public keys are served at `/organizations/{tenant}/jwks.json`.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::common::{Labels, Timestamp};
use crate::namespaces::NamespaceName;

/// A run's, a fork's or a workflow version's attachments: for each MCP server or API whose
/// `auth` takes a secret, the secret it reads, `org/<name>` (the organization's) or `ns/<name>`
/// (the run's own namespace's): `{ "github": "ns/github", "zendesk": "org/zendesk" }`. Names
/// only, never a value.
pub type Attachments = BTreeMap<String, String>;

/// `PUT /v1/secrets/{name}` and `PUT /v1/namespaces/{name}/secrets/{secret}`: creates or
/// replaces a secret. Replacing it makes ready the calls a refusal or a missing secret parked in
/// the runs that attach it, sent again with the same idempotency key.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[schema(example = json!({ "value": "ghp_…", "labels": { "env": "prod" } }))]
pub struct PutSecret {
    /// The value: a string of at most 16 KiB. Written, never read back.
    pub value: String,
    /// An organization's secret's labels: a grant limited to labels sees, writes and attaches
    /// only the secrets carrying them, and the secrets it writes take them. A namespace's secret
    /// takes none (`invalid-request`).
    #[serde(default, skip_serializing_if = "Labels::is_empty")]
    pub labels: Labels,
}

/// Never printed by `Debug`: only the labels are.
impl fmt::Debug for PutSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PutSecret")
            .field("value", &"<redacted>")
            .field("labels", &self.labels)
            .finish()
    }
}

/// Whose secret.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SecretScope {
    /// The organization's, shared by every end user: attached as `org/<name>`.
    Org,
    /// A namespace's, the end user's: attached as `ns/<name>` by the runs of that namespace.
    Ns,
}

/// A secret, as listed: its metadata, never its value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Secret {
    /// 1 to 64 lowercase letters, digits, `-` and `_`, starting with a letter.
    pub name: String,
    pub scope: SecretScope,
    /// A namespace's secret's namespace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<NamespaceName>,
    /// How an attachment names it: `org/zendesk`, `ns/github`.
    pub reference: String,
    /// An organization's secret's labels; none for a namespace's.
    #[serde(default, skip_serializing_if = "Labels::is_empty")]
    pub labels: Labels,
    /// Who wrote it first: a principal (`user:…`, `key:…`, `connection:…`) or an end user's
    /// token (`token:<namespace>`).
    pub created_by: String,
    pub created_at: Timestamp,
    /// Who wrote it last.
    pub updated_by: String,
    pub updated_at: Timestamp,
    /// Its reads per day over the last 30 days, oldest first; a day without a read is left out.
    pub reads: Vec<DailyReads>,
    /// Its last read, if it was ever read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_read: Option<LastRead>,
}

/// The reads of a secret on one day (UTC).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DailyReads {
    /// `2026-11-20`.
    pub day: String,
    pub reads: u64,
}

/// The last read of a secret: by which run, of which namespace, when. A toolset's registration
/// reads an organization's secret to list the tools of the MCP server it is attached for, before
/// any run exists: such a read names neither run nor namespace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct LastRead {
    /// The run that read it; `null` for a toolset's registration.
    pub run: Option<String>,
    /// The namespace of that run; `null` for a toolset's registration.
    pub namespace: Option<NamespaceName>,
    pub at: Timestamp,
}

/// `GET /v1/secrets`, `GET /v1/namespaces/{name}/secrets`: the secrets the caller sees, sorted
/// by name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SecretList {
    pub secrets: Vec<Secret>,
}

/// `GET /v1/secrets/{name}/usage`, `GET /v1/namespaces/{name}/secrets/{secret}/usage`: what
/// reads a secret, what deleting it would stop.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SecretUsage {
    /// The secret, with its reads and its last read.
    pub secret: Secret,
    /// The active runs that attach it, oldest first, at most 100.
    pub active_runs: Vec<String>,
    /// How many active runs attach it.
    pub active_total: u64,
    /// Those of them with a call to a server it serves that is parked or not yet sent, at most
    /// 100.
    pub waiting_runs: Vec<String>,
    pub waiting_total: u64,
    /// The workflows whose latest version attaches it. A version attaching `ns/<name>` counts
    /// for the secret `<name>` of every namespace.
    pub workflows: Vec<WorkflowAttaching>,
}

/// The latest version of a workflow, which attaches a secret.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowAttaching {
    pub name: String,
    /// The version (SHA-256, lowercase hex).
    pub version: String,
    /// The servers and APIs it attaches the secret for.
    pub servers: Vec<String>,
}

/// One of the organization's signing keys (`archi/04-tools.md`, "Signed calls"): Ed25519.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SigningKey {
    /// The key's id, in the `kid` of the JWTs it signs: the first 8 bytes of the SHA-256 of
    /// the public key, in hex.
    pub kid: String,
    pub created_at: Timestamp,
    /// When it stopped signing: it stays published 24 hours longer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retired_at: Option<Timestamp>,
}

/// `GET /v1/organization/signing-keys`: the current key and those retired less than 24 hours
/// ago, newest first, and where their public keys are.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SigningKeyList {
    pub keys: Vec<SigningKey>,
    /// The JWT's issuer, `{public-url}/organizations/{tenant}`: what `verifyRewindCall` is
    /// given as `issuer`.
    pub issuer: String,
    /// `{issuer}/jwks.json`.
    pub jwks_url: String,
}

/// `GET /organizations/{tenant}/jwks.json`: the organization's public keys, as a JSON Web Key
/// Set (RFC 7517), served without authentication.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Jwks {
    pub keys: Vec<Jwk>,
}

/// An Ed25519 public key (RFC 8037).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Jwk {
    /// `OKP`.
    pub kty: String,
    /// `Ed25519`.
    pub crv: String,
    /// The public key, in base64url without padding.
    pub x: String,
    pub kid: String,
    /// `EdDSA`.
    pub alg: String,
    /// `sig`.
    #[serde(rename = "use")]
    pub r#use: String,
}

impl Jwk {
    /// The JWK of an Ed25519 public key, `x` already in base64url without padding.
    pub fn ed25519(kid: impl Into<String>, x: impl Into<String>) -> Self {
        Self {
            kty: "OKP".into(),
            crv: "Ed25519".into(),
            x: x.into(),
            kid: kid.into(),
            alg: "EdDSA".into(),
            r#use: "sig".into(),
        }
    }
}

/// Why an attachment refused a start, a fork or a save (`secret-required`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentProblemKind {
    /// The script imports the server, whose `auth` takes a secret, and nothing is attached.
    Missing,
    /// The server or API takes no secret, or the toolset has none of that name.
    TakesNone,
    /// The secret does not exist (an `ns/` secret in the run's namespace).
    Unknown,
    /// The caller may not attach it.
    Forbidden,
}

/// One attachment that refused a start, a fork or a save, with what the caller may attach for
/// that server instead.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentProblem {
    /// The MCP server or API.
    pub server: String,
    pub problem: AttachmentProblemKind,
    /// The secret attached, as the request named it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
    /// The secrets the caller may attach for that server, `org/<name>` and `ns/<name>`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachable: Vec<String>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_value_is_never_printed_and_unknown_fields_are_refused() {
        let put: PutSecret = serde_json::from_value(json!({ "value": "ghp_secret" })).unwrap();
        assert!(!format!("{put:?}").contains("ghp_secret"));
        assert!(
            serde_json::from_value::<PutSecret>(json!({ "value": "x", "scope": "org" })).is_err()
        );
    }

    #[test]
    fn a_jwk_is_an_ed25519_key() {
        assert_eq!(
            serde_json::to_value(Jwk::ed25519(
                "9f2c4e1a07b3d5f8",
                "11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo"
            ))
            .unwrap(),
            json!({
                "kty": "OKP",
                "crv": "Ed25519",
                "x": "11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo",
                "kid": "9f2c4e1a07b3d5f8",
                "alg": "EdDSA",
                "use": "sig",
            })
        );
    }
}
