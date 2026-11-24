//! Secrets and how calls prove who calls (`archi/04-tools.md`, "Authenticating calls").
//!
//! ```text
//! SecretRef     org/<name> | ns/<name>      what an attachment names: an organization's secret,
//!                                           or the secret <name> of the run's own namespace
//! Attachments   { <server or API>: SecretRef }   a run's, a fork's, a workflow version's
//! ActionAuth      none | rewind | secret(SecretPlacement)   how a server's or an API's calls
//!                                                         authenticate, pinned in the action set
//! SecretPlacement  header(name, format?, encoding) | env(variable)
//! ```
//!
//! The core knows names, scopes and places, never a value: values live in the `SecretStore` and
//! only the dispatcher reads them, for the runs that attached them.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

/// The largest value of a secret, in bytes of UTF-8 (`archi/04-tools.md`, "Secrets").
pub const MAX_SECRET_VALUE_BYTES: usize = 16 * 1024;

/// The placeholder a header's `format` replaces with the secret: `Bearer {secret}`.
pub const SECRET_PLACEHOLDER: &str = "{secret}";

/// The name of a secret: 1 to 64 lowercase ASCII letters, digits, `-` and `_`, starting with a
/// letter. The same name may be an organization's secret and a namespace's.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SecretName(String);

impl SecretName {
    pub const MAX_LEN: usize = 64;

    pub fn new(name: impl Into<String>) -> Result<Self, InvalidSecretName> {
        let name = name.into();
        let mut chars = name.chars();
        let valid = name.len() <= Self::MAX_LEN
            && chars.next().is_some_and(|c| c.is_ascii_lowercase())
            && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
        if valid {
            Ok(Self(name))
        } else {
            Err(InvalidSecretName(name))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SecretName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for SecretName {
    type Err = InvalidSecretName;

    fn from_str(name: &str) -> Result<Self, InvalidSecretName> {
        Self::new(name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "invalid secret name {0:?}: expected 1 to 64 lowercase letters, digits, - and _, starting \
     with a letter"
)]
pub struct InvalidSecretName(pub String);

/// Whose secret an attachment names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SecretScope {
    /// The tenant's, shared by every end user: `org/<name>`.
    Organization,
    /// The run's own namespace's, the end user's: `ns/<name>`. Read in the namespace of the run
    /// that attaches it, never in another.
    Namespace,
}

impl SecretScope {
    /// `org` or `ns`, as an attachment writes the scope.
    pub fn as_str(self) -> &'static str {
        match self {
            SecretScope::Organization => "org",
            SecretScope::Namespace => "ns",
        }
    }
}

/// What an attachment names: `org/<name>` or `ns/<name>` (`archi/04-tools.md`, "Secrets"). An
/// `ns/` reference is relative: the secret `<name>` of the namespace of the run that reads it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SecretRef {
    pub scope: SecretScope,
    pub name: SecretName,
}

impl SecretRef {
    pub fn organization(name: SecretName) -> Self {
        Self {
            scope: SecretScope::Organization,
            name,
        }
    }

    pub fn namespace(name: SecretName) -> Self {
        Self {
            scope: SecretScope::Namespace,
            name,
        }
    }

    /// Reads `org/<name>` or `ns/<name>`.
    pub fn parse(text: &str) -> Result<Self, InvalidSecretRef> {
        let invalid = || InvalidSecretRef(text.to_owned());
        let (scope, name) = text.split_once('/').ok_or_else(invalid)?;
        let scope = match scope {
            "org" => SecretScope::Organization,
            "ns" => SecretScope::Namespace,
            _ => return Err(invalid()),
        };
        let name = SecretName::new(name).map_err(|_| invalid())?;
        Ok(Self { scope, name })
    }
}

impl fmt::Display for SecretRef {
    /// `org/zendesk`, `ns/github`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.scope.as_str(), self.name)
    }
}

impl FromStr for SecretRef {
    type Err = InvalidSecretRef;

    fn from_str(text: &str) -> Result<Self, InvalidSecretRef> {
        Self::parse(text)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "invalid secret reference {0:?}: expected org/<name> for an organization's secret or \
     ns/<name> for the run's namespace's"
)]
pub struct InvalidSecretRef(pub String);

/// The secrets a run reads, one per MCP server or API whose `auth` takes a secret, by the
/// server's or the API's name: `{ "github": "ns/github", "zendesk": "org/zendesk" }`
/// (`archi/04-tools.md`, "Secrets"). Given in the request that gives the code, never in the
/// code; a workflow version carries its own in its document.
///
/// The keys are not checked against an action set here: whether each names a server that takes a
/// secret, and whether every such server the code imports has one, is checked when a run
/// starts or a version is saved.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Attachments(BTreeMap<String, SecretRef>);

impl Attachments {
    pub fn new() -> Self {
        Self::default()
    }

    /// The secret attached for this server or API.
    pub fn get(&self, server: &str) -> Option<&SecretRef> {
        self.0.get(server)
    }

    /// Attaches `secret` for `server`, replacing what was attached for it.
    pub fn insert(&mut self, server: impl Into<String>, secret: SecretRef) -> Option<SecretRef> {
        self.0.insert(server.into(), secret)
    }

    /// By server or API name, sorted.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &SecretRef)> {
        self.0
            .iter()
            .map(|(server, secret)| (server.as_str(), secret))
    }

    /// The servers and APIs that read `secret`.
    pub fn servers_reading<'a>(&'a self, secret: &'a SecretRef) -> impl Iterator<Item = &'a str> {
        self.iter()
            .filter(move |(_, attached)| *attached == secret)
            .map(|(server, _)| server)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }
}

impl FromIterator<(String, SecretRef)> for Attachments {
    fn from_iter<I: IntoIterator<Item = (String, SecretRef)>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

impl From<BTreeMap<String, SecretRef>> for Attachments {
    fn from(map: BTreeMap<String, SecretRef>) -> Self {
        Self(map)
    }
}

/// How the calls to an MCP server or an API prove who calls (`archi/04-tools.md`,
/// "Authenticating calls"). Part of the pinned action set, and so of its digest; never a value.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum ActionAuth {
    /// Nothing: a public server. Left out of the pinned document.
    #[default]
    None,
    /// A JWT signed with the tenant's Ed25519 key, in `Authorization: Bearer`
    /// (`archi/04-tools.md`, "Signed calls"). HTTP servers and APIs only.
    Rewind,
    /// The secret the run attached for the server, put where `SecretPlacement` says.
    Secret(SecretPlacement),
}

impl ActionAuth {
    /// Whether a run that imports the server must attach a secret for it.
    pub fn takes_secret(&self) -> bool {
        matches!(self, ActionAuth::Secret(_))
    }
}

/// Where the attached secret goes in a request. A secret never goes in a URL.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum SecretPlacement {
    /// In this HTTP header: `format` with [`{secret}`](SECRET_PLACEHOLDER) replaced by the value
    /// (encoded as `encoding` says), or the value alone without `format`. HTTP servers and APIs.
    Header {
        name: String,
        format: Option<String>,
        encoding: SecretEncoding,
    },
    /// In this environment variable of the process: a stdio MCP server.
    Env { variable: String },
}

impl SecretPlacement {
    /// The value alone, in `name`.
    pub fn header(name: impl Into<String>) -> Self {
        SecretPlacement::Header {
            name: name.into(),
            format: None,
            encoding: SecretEncoding::Plain,
        }
    }

    /// `Authorization: Bearer {secret}`: what `http` `bearer`, `oauth2` and `openIdConnect`
    /// security schemes of an OpenAPI document mean.
    pub fn bearer() -> Self {
        SecretPlacement::Header {
            name: "Authorization".into(),
            format: Some(format!("Bearer {SECRET_PLACEHOLDER}")),
            encoding: SecretEncoding::Plain,
        }
    }

    /// `Authorization: Basic {secret}`, the secret (`user:password`) in base64: the `http`
    /// `basic` security scheme of an OpenAPI document.
    pub fn basic() -> Self {
        SecretPlacement::Header {
            name: "Authorization".into(),
            format: Some(format!("Basic {SECRET_PLACEHOLDER}")),
            encoding: SecretEncoding::Base64,
        }
    }
}

/// How the secret is written into a header's `format`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum SecretEncoding {
    /// As it is.
    #[default]
    Plain,
    /// In standard base64, with padding: HTTP basic authentication.
    Base64,
}

impl SecretEncoding {
    pub fn as_str(self) -> &'static str {
        match self {
            SecretEncoding::Plain => "plain",
            SecretEncoding::Base64 => "base64",
        }
    }
}

/// Headers Rewind sets itself on a call, which no `auth` may take (`archi/04-tools.md`,
/// "Routes" and "API actions"), compared without case.
pub const RESERVED_HEADERS: &[&str] = &[
    "content-length",
    "content-type",
    "host",
    "idempotency-key",
    "traceparent",
    "tracestate",
];

/// A header name as RFC 9110 defines a token.
pub(crate) fn is_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

/// An environment variable's name: ASCII letters, digits and `_`, not starting with a digit.
pub(crate) fn is_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_short_lowercase_and_start_with_a_letter() {
        for valid in ["github", "a", "zendesk-prod_2", &"x".repeat(64)] {
            assert_eq!(SecretName::new(valid).unwrap().as_str(), valid);
        }
        for invalid in [
            "",
            "GitHub",
            "2fa",
            "-x",
            "_x",
            "a b",
            "a/b",
            "é",
            &"x".repeat(65),
        ] {
            assert_eq!(
                SecretName::new(invalid),
                Err(InvalidSecretName(invalid.to_owned()))
            );
        }
    }

    #[test]
    fn references_name_a_scope_and_a_name() {
        let github: SecretRef = "ns/github".parse().unwrap();
        assert_eq!(github.scope, SecretScope::Namespace);
        assert_eq!(github.name.as_str(), "github");
        assert_eq!(github.to_string(), "ns/github");
        let zendesk = SecretRef::parse("org/zendesk").unwrap();
        assert_eq!(
            zendesk,
            SecretRef::organization(SecretName::new("zendesk").unwrap())
        );
        assert_eq!(zendesk.to_string(), "org/zendesk");
        for invalid in [
            "github",
            "org/",
            "user/github",
            "org/GitHub",
            "ns/a/b",
            "/github",
        ] {
            assert!(SecretRef::parse(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn attachments_say_which_servers_read_a_secret() {
        let github = SecretRef::parse("ns/github").unwrap();
        let attachments: Attachments = [
            ("github".to_owned(), github.clone()),
            ("issues".to_owned(), github.clone()),
            (
                "zendesk".to_owned(),
                SecretRef::parse("org/zendesk").unwrap(),
            ),
        ]
        .into_iter()
        .collect();
        assert_eq!(attachments.get("github"), Some(&github));
        assert_eq!(attachments.get("slack"), None);
        assert_eq!(
            attachments.servers_reading(&github).collect::<Vec<_>>(),
            ["github", "issues"]
        );
        assert!(!attachments.is_empty());
        assert!(Attachments::new().is_empty());
    }

    #[test]
    fn header_and_variable_names_are_checked() {
        for valid in ["Authorization", "X-Api-Key", "x_token"] {
            assert!(is_header_name(valid), "{valid}");
        }
        for invalid in ["", "X Api", "X:Api", "é"] {
            assert!(!is_header_name(invalid), "{invalid}");
        }
        for valid in ["GITHUB_TOKEN", "_x", "a1"] {
            assert!(is_env_name(valid), "{valid}");
        }
        for invalid in ["", "1A", "A-B", "A B"] {
            assert!(!is_env_name(invalid), "{invalid}");
        }
        assert!(ActionAuth::Secret(SecretPlacement::bearer()).takes_secret());
        assert!(!ActionAuth::Rewind.takes_secret());
        assert!(!ActionAuth::None.takes_secret());
    }
}
