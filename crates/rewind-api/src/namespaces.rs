//! Namespaces: the end users of a customer, their limits, their use and their alerts; end users'
//! tokens and the keys that sign them (`archi/06-storage-and-scale.md`, "Namespaces", "Quotas",
//! "Who calls").
//!
//! A namespace isolates one end user of the customer (the tenant): the runs started for them and
//! everything that follows from those runs. The customer's keys and members reach every
//! namespace, and name the one a request acts for with [`crate::headers::NAMESPACE`]; an end
//! user's token reaches its own namespace only.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use crate::Error;
use crate::access::Permission;
use crate::common::{Duration, Labels, Timestamp};

// -------------------------------------------------------------------------------------------
// Names

/// A namespace's name: the customer's own id for the end user. 1 to 128 ASCII letters, digits,
/// `-`, `_`, `.`, `:` and `@`, case-sensitive. `*` is not a name: it stands for the tenant in
/// the store's usage and alerts.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, ToSchema)]
#[serde(try_from = "String", into = "String")]
#[schema(value_type = String, pattern = r"^[A-Za-z0-9._:@-]{1,128}$", example = "user_2fQ81")]
pub struct NamespaceName(String);

impl NamespaceName {
    pub const MAX_LEN: usize = 128;

    pub fn new(name: impl Into<String>) -> Result<Self, Error> {
        let name = name.into();
        if !is_namespace_name(&name) {
            return Err(Error::invalid(
                "namespace",
                format!(
                    "{name:?}: expected 1 to {} ASCII letters, digits, -, _, ., : and @",
                    Self::MAX_LEN
                ),
            ));
        }
        Ok(Self(name))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Whether `name` is a valid namespace name.
pub fn is_namespace_name(name: &str) -> bool {
    (1..=NamespaceName::MAX_LEN).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:@".contains(&byte))
}

impl fmt::Display for NamespaceName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for NamespaceName {
    type Err = Error;

    fn from_str(name: &str) -> Result<Self, Error> {
        Self::new(name)
    }
}

impl TryFrom<String> for NamespaceName {
    type Error = Error;

    fn try_from(name: String) -> Result<Self, Error> {
        Self::new(name)
    }
}

impl From<NamespaceName> for String {
    fn from(name: NamespaceName) -> Self {
        name.0
    }
}

// -------------------------------------------------------------------------------------------
// Quotas

/// Limits, each optional: an absent one does not limit. The same shape serves a tenant's
/// defaults for every namespace, a namespace's exceptions, the limits that apply to a
/// namespace, and the tenant's ceiling, which bounds its totals.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Quotas {
    /// CPU time of the isolates running the namespace's scripts in a calendar month (UTC),
    /// replays excluded. Never refused: crossing 80 % and 100 % sends `quota.alert`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_per_month: Option<Duration>,
    /// Runs active at once. Starting or forking one more is refused (429).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_runs: Option<u32>,
    /// Runs created in a calendar month, forks included (429 above it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runs_per_month: Option<u64>,
    /// Distinct scripts with at least one run kept in the namespace (429 for a new one above
    /// it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scripts: Option<u64>,
    /// The longest TTL a run may ask for; the default TTL is lowered to it (400 above it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_ttl: Option<Duration>,
    /// The most lines a script may have (422 `invalid-script` above it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_script_lines: Option<u32>,
}

/// A quota, as `quota-exceeded` and `quota.alert` name it.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum QuotaKind {
    CpuPerMonth,
    ActiveRuns,
    RunsPerMonth,
    Scripts,
    MaxTtl,
    MaxScriptLines,
}

impl QuotaKind {
    pub const ALL: [QuotaKind; 6] = [
        QuotaKind::CpuPerMonth,
        QuotaKind::ActiveRuns,
        QuotaKind::RunsPerMonth,
        QuotaKind::Scripts,
        QuotaKind::MaxTtl,
        QuotaKind::MaxScriptLines,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            QuotaKind::CpuPerMonth => "cpu_per_month",
            QuotaKind::ActiveRuns => "active_runs",
            QuotaKind::RunsPerMonth => "runs_per_month",
            QuotaKind::Scripts => "scripts",
            QuotaKind::MaxTtl => "max_ttl",
            QuotaKind::MaxScriptLines => "max_script_lines",
        }
    }
}

impl fmt::Display for QuotaKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which limit a quota belongs to: the namespace's (its exception, else the default), or the
/// tenant's ceiling.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum QuotaScope {
    Namespace,
    Organization,
}

/// What a namespace, or the tenant, used in a calendar month.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct QuotaUsage {
    /// CPU time of the month, replays excluded.
    pub cpu: Duration,
    /// Runs active now.
    pub active_runs: u64,
    /// Runs created in the month, forks included.
    pub runs_started: u64,
    /// Distinct scripts with at least one run kept.
    pub scripts: u64,
}

/// One UTC day of use.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct DayUsage {
    /// The day, `2026-11-14`.
    #[schema(value_type = String, format = Date)]
    pub day: jiff::civil::Date,
    pub cpu: Duration,
    pub runs_started: u64,
}

// -------------------------------------------------------------------------------------------
// Namespaces

/// A namespace with its limits and its use of a month.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Namespace {
    pub name: NamespaceName,
    /// The limits that apply: per quota, the namespace's exception, else the tenant's default.
    pub limits: Quotas,
    /// The namespace's exceptions to the defaults.
    pub exceptions: Quotas,
    /// Its use of the month listed.
    pub usage: QuotaUsage,
    pub created_at: Timestamp,
    /// The last run created in it, or the last CPU time counted for it.
    pub last_active_at: Timestamp,
    /// Set while the namespace is being erased: it takes no run and no token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleting_since: Option<Timestamp>,
}

/// How `GET /v1/namespaces` orders the namespaces: by a use of the month, the largest first, or
/// by name.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum NamespaceOrder {
    #[default]
    Cpu,
    Runs,
    Scripts,
    Name,
}

/// `GET /v1/namespaces`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, IntoParams)]
#[serde(rename_all = "camelCase")]
#[into_params(parameter_in = Query)]
pub struct NamespaceListQuery {
    /// Names starting with this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    /// Only the namespaces that used at least this share of one of their limits this month:
    /// `0.8` for 80 %.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub above: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(inline)]
    pub order: Option<NamespaceOrder>,
    /// The calendar month whose use is listed, `2026-11`; the current one by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub month: Option<String>,
    /// The `next` cursor of the previous page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    /// 50 by default, 500 at most.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// `GET /v1/namespaces`: a page of namespaces.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct NamespaceList {
    /// The month whose use is listed, `2026-11`.
    pub month: String,
    pub namespaces: Vec<Namespace>,
    /// How many namespaces match the query, across every page.
    pub total: u64,
    /// The cursor of the next page, absent on the last one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
}

/// `GET /v1/namespaces/{name}`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, IntoParams)]
#[serde(rename_all = "camelCase")]
#[into_params(parameter_in = Query)]
pub struct NamespaceQuery {
    /// The calendar month, `2026-11`; the current one by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub month: Option<String>,
}

/// `GET /v1/namespaces/{name}`: one namespace, its use day by day, its alerts and its tokens.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct NamespaceDetail {
    pub namespace: Namespace,
    /// The month, `2026-11`.
    pub month: String,
    /// The days of the month with some use, oldest first.
    pub days: Vec<DayUsage>,
    /// The alerts of the month.
    pub alerts: Vec<QuotaAlert>,
    /// Opaque tokens of the namespace neither revoked nor expired.
    pub tokens: u64,
    /// Signed tokens issued before this are refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_revoked_before: Option<Timestamp>,
}

/// `PUT /v1/namespaces/{name}`: creates the namespace, or replaces its exceptions. A quota
/// absent from `exceptions` follows the tenant's default.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PutNamespace {
    #[serde(default)]
    pub exceptions: Quotas,
}

/// `GET /v1/organization/namespace-defaults`: the limits of every namespace without an
/// exception, and the tenant's ceiling with its use of the month.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct NamespaceDefaults {
    pub defaults: Quotas,
    /// Set by the operator: the customer's offer, on its totals across namespaces.
    pub ceiling: Quotas,
    /// The month, `2026-11`.
    pub month: String,
    /// The tenant's use of the month, across its namespaces.
    pub usage: QuotaUsage,
}

/// `PUT /v1/organization/namespace-defaults`: replaces the defaults.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PutNamespaceDefaults {
    pub defaults: Quotas,
}

/// `PUT /v1/organizations/{tenant}/quotas`: the operator replaces a tenant's ceiling.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PutCeiling {
    pub ceiling: Quotas,
}

// -------------------------------------------------------------------------------------------
// Alerts

/// A limit crossed: sent once as the webhook `quota.alert`, and listed by the API.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct QuotaAlert {
    /// The namespace; absent (`null` in the webhook) for the tenant's ceiling.
    pub namespace: Option<NamespaceName>,
    pub quota: QuotaKind,
    /// 80 or 100: the share of the limit crossed; for `active_runs`, 100 at the first refusal
    /// of the month.
    pub threshold: u8,
    /// The use when the alert was raised: milliseconds for `cpu_per_month`, a count otherwise.
    pub used: u64,
    /// The limit, in the same unit.
    pub limit: u64,
    /// The calendar month, `2026-11`.
    pub period: String,
    pub raised_at: Timestamp,
    /// When it was delivered to the tenant's subscriptions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sent_at: Option<Timestamp>,
}

/// `GET /v1/organization/quota-alerts`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, IntoParams)]
#[serde(rename_all = "camelCase")]
#[into_params(parameter_in = Query)]
pub struct QuotaAlertQuery {
    /// The calendar month, `2026-11`; the current one by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub month: Option<String>,
    /// One namespace's alerts; every alert of the tenant without it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(value_type = Option<String>)]
    pub namespace: Option<NamespaceName>,
}

/// `GET /v1/organization/quota-alerts`: the alerts of a month, newest first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct QuotaAlertList {
    pub month: String,
    pub alerts: Vec<QuotaAlert>,
}

// -------------------------------------------------------------------------------------------
// Tokens

/// `POST /v1/namespaces/{name}/tokens`: an end user's opaque token.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateToken {
    /// How long the token lives: 1 hour by default, 30 days at most.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl: Option<Duration>,
    /// Among [`Permission::TOKEN`], each held by the caller; [`Permission::TOKEN_DEFAULT`]
    /// when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permissions: Option<Vec<Permission>>,
    /// The runs the token reaches carry all of these labels, and the runs it starts get them.
    #[serde(default, skip_serializing_if = "Labels::is_empty")]
    pub labels: Labels,
}

/// An end user's opaque token.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct Token {
    /// The SHA-256 of the secret, in hexadecimal (`DELETE /v1/tokens/{id}`).
    pub id: String,
    pub namespace: NamespaceName,
    pub permissions: Vec<Permission>,
    #[serde(default, skip_serializing_if = "Labels::is_empty")]
    pub labels: Labels,
    pub expires_at: Timestamp,
    pub created_at: Timestamp,
    /// `rewind_nt_…`: only in the answer of `POST /v1/namespaces/{name}/tokens`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

/// `DELETE /v1/namespaces/{name}/tokens`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RevokedTokens {
    /// Opaque tokens revoked.
    pub revoked: u64,
    /// Signed tokens issued before this are refused.
    pub revoked_before: Timestamp,
}

/// The algorithm of a token key, from its JWK: Ed25519 (`OKP`), P-256 (`EC`) or RSA.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
pub enum TokenAlgorithm {
    #[serde(rename = "EdDSA")]
    EdDsa,
    #[serde(rename = "ES256")]
    Es256,
    #[serde(rename = "RS256")]
    Rs256,
}

/// `POST /v1/organization/token-keys`: a public key the customer signs end users' tokens with.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateTokenKey {
    /// What the key is for.
    pub name: String,
    /// The public key as a JWK (RFC 7517): `OKP` with `crv` `Ed25519`, `EC` with `crv` `P-256`,
    /// or `RSA` of at least 2048 bits. A private part is refused.
    #[schema(value_type = Object)]
    pub jwk: serde_json::Value,
    /// The permissions its tokens may carry, among [`Permission::TOKEN`] and held by the caller;
    /// all of those the caller holds when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permissions: Option<Vec<Permission>>,
}

/// A registered token key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TokenKey {
    /// `rk_…`: the `kid` of the tokens it signs.
    pub kid: String,
    pub name: String,
    pub algorithm: TokenAlgorithm,
    #[schema(value_type = Object)]
    pub jwk: serde_json::Value,
    pub permissions: Vec<Permission>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
    pub created_at: Timestamp,
}

/// `GET /v1/organization/token-keys`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct TokenKeyList {
    pub keys: Vec<TokenKey>,
}

/// The audience of a signed token.
pub const TOKEN_AUDIENCE: &str = "rewind";

/// The longest a token lives: 30 days.
pub const TOKEN_MAX_TTL: std::time::Duration = std::time::Duration::from_secs(30 * 86_400);

/// The life of an opaque token minted without `ttl`: 1 hour.
pub const TOKEN_DEFAULT_TTL: std::time::Duration = std::time::Duration::from_secs(3_600);

/// The claims of a token the customer signs. Its header names the key: `kid` `rk_…`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SignedTokenClaims {
    /// The namespace.
    pub sub: NamespaceName,
    /// [`TOKEN_AUDIENCE`].
    pub aud: String,
    /// Issued at, in seconds since the Unix epoch.
    pub iat: i64,
    /// Expires at, in seconds since the Unix epoch, at most 30 days after `iat`.
    pub exp: i64,
    /// Among those the key may give; [`Permission::TOKEN_DEFAULT`] when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permissions: Option<Vec<Permission>>,
    #[serde(default, skip_serializing_if = "Labels::is_empty")]
    pub labels: Labels,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jti: Option<String>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn namespace_names_are_the_customers_ids() {
        for valid in ["alice", "user_2fQ81", "a.b:c@d-e", &"x".repeat(128)] {
            assert_eq!(NamespaceName::new(valid).unwrap().as_str(), valid);
        }
        for invalid in ["", "*", "a b", "a/b", "é", &"x".repeat(129)] {
            assert!(NamespaceName::new(invalid).is_err(), "{invalid:?}");
        }
        assert!(serde_json::from_value::<NamespaceName>(json!("a/b")).is_err());
    }

    #[test]
    fn quotas_are_camel_case_and_optional() {
        let quotas: Quotas = serde_json::from_value(json!({
            "cpuPerMonth": "2h",
            "activeRuns": 10,
            "maxScriptLines": 300
        }))
        .unwrap();
        assert_eq!(quotas.active_runs, Some(10));
        assert_eq!(quotas.runs_per_month, None);
        assert_eq!(
            serde_json::to_value(&quotas).unwrap(),
            json!({ "cpuPerMonth": "2h", "activeRuns": 10, "maxScriptLines": 300 })
        );
        assert_eq!(serde_json::to_value(Quotas::default()).unwrap(), json!({}));
        assert_eq!(
            serde_json::to_value(QuotaKind::CpuPerMonth).unwrap(),
            json!("cpu_per_month")
        );
    }

    #[test]
    fn a_signed_token_names_its_namespace_and_audience() {
        let claims: SignedTokenClaims = serde_json::from_value(json!({
            "sub": "alice",
            "aud": "rewind",
            "iat": 1_800_000_000,
            "exp": 1_800_003_600
        }))
        .unwrap();
        assert_eq!(claims.sub.as_str(), "alice");
        assert_eq!(claims.permissions, None);
    }
}
