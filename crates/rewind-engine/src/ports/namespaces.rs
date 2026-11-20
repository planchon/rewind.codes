//! Namespaces: the end users of a tenant, their quotas, their use and their alerts
//! (`archi/06-storage-and-scale.md`, "Namespaces", "Quotas").
//!
//! The namespace is run metadata, like the tenant: it lives with the ports, never in the core,
//! and never in an event. The `JournalStore` keeps namespaces, their use and their alerts, since
//! it checks the quotas where it creates a run.

use std::fmt;
use std::time::Duration;

use rewind_core::Timestamp;

use super::tenancy::{Tenant, TenantId};

const DAY_MS: u64 = 86_400_000;

/// A namespace: the customer's own id for one of its end users. 1 to 128 ASCII letters,
/// digits, `-`, `_`, `.`, `:` and `@`, case-sensitive. `*` is not a name: the store uses it for
/// the tenant's own rows of use and alerts.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NamespaceName(String);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid namespace {0:?}: expected 1 to 128 ASCII letters, digits, -, _, ., : and @")]
pub struct InvalidNamespace(pub String);

impl NamespaceName {
    pub const MAX_LEN: usize = 128;

    /// The namespace the migration puts every run, toolset credential and count that existed
    /// before namespaces in (`archi/08-decisions.md`, "Taken for namespaces").
    pub const MIGRATED: &'static str = "migrated";

    /// The namespace the CLI uses against its own embedded server when none is given.
    pub const LOCAL: &'static str = "local";

    pub fn new(name: impl Into<String>) -> Result<Self, InvalidNamespace> {
        let name = name.into();
        let valid = (1..=Self::MAX_LEN).contains(&name.len())
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:@".contains(&byte));
        if valid {
            Ok(Self(name))
        } else {
            Err(InvalidNamespace(name))
        }
    }

    /// [`Self::MIGRATED`].
    pub fn migrated() -> Self {
        Self(Self::MIGRATED.to_owned())
    }

    /// `*`, which no namespace is named: the credentials resolved for it are a toolset's own.
    pub(crate) fn none() -> Self {
        Self("*".to_owned())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for NamespaceName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A namespace's display name: the customer's label for its end user (`Acme Corp`), shown
/// beside its name, which stays the key. Without surrounding whitespace, 1 to 128 characters,
/// none of them a control character; not unique.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DisplayName(String);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "invalid display name {0:?}: expected at most 128 characters, none of them a control character"
)]
pub struct InvalidDisplayName(pub String);

impl DisplayName {
    /// In characters (Unicode scalar values).
    pub const MAX_CHARS: usize = 128;

    /// A display name as stored: already trimmed and not empty.
    pub fn new(text: impl Into<String>) -> Result<Self, InvalidDisplayName> {
        let text = text.into();
        let valid = !text.is_empty()
            && text.trim() == text
            && text.chars().count() <= Self::MAX_CHARS
            && !text.chars().any(char::is_control);
        if valid {
            Ok(Self(text))
        } else {
            Err(InvalidDisplayName(text))
        }
    }

    /// A display name as given: trimmed of surrounding whitespace; `None` when nothing is left.
    pub fn parse(text: &str) -> Result<Option<Self>, InvalidDisplayName> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }
        Self::new(trimmed)
            .map(Some)
            .map_err(|_| InvalidDisplayName(text.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Its [`fold`](Self::fold): what the prefix of `list_namespaces`, folded too, matches.
    pub fn key(&self) -> String {
        Self::fold(&self.0)
    }

    /// Whether it starts with `prefix`, case aside.
    pub fn starts_with(&self, prefix: &str) -> bool {
        self.key().starts_with(&Self::fold(prefix))
    }

    /// `text` lowercase, character by character, so that the fold of a prefix is a prefix of
    /// the fold (unlike [`str::to_lowercase`], whose final sigma depends on what follows).
    pub fn fold(text: &str) -> String {
        text.chars().flat_map(char::to_lowercase).collect()
    }
}

impl fmt::Display for DisplayName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Six limits, each optional: an absent one does not limit. The same shape serves a tenant's
/// defaults for its namespaces, a namespace's exceptions, and the tenant's ceiling.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Quotas {
    /// CPU time of the isolates in a calendar month (UTC), replays excluded. Never refused:
    /// it only raises alerts.
    pub cpu_per_month: Option<Duration>,
    /// Runs active at once.
    pub active_runs: Option<u32>,
    /// Runs created in a calendar month, forks included.
    pub runs_per_month: Option<u64>,
    /// Distinct scripts with at least one run kept.
    pub scripts: Option<u64>,
    /// The longest TTL a run may ask for.
    pub max_ttl: Option<Duration>,
    /// The most lines a script may have.
    pub max_script_lines: Option<u32>,
}

impl Quotas {
    /// Per quota, `self`'s if it has one, else `fallback`'s: a namespace's exceptions over the
    /// tenant's defaults.
    pub fn over(&self, fallback: &Quotas) -> Quotas {
        Quotas {
            cpu_per_month: self.cpu_per_month.or(fallback.cpu_per_month),
            active_runs: self.active_runs.or(fallback.active_runs),
            runs_per_month: self.runs_per_month.or(fallback.runs_per_month),
            scripts: self.scripts.or(fallback.scripts),
            max_ttl: self.max_ttl.or(fallback.max_ttl),
            max_script_lines: self.max_script_lines.or(fallback.max_script_lines),
        }
    }

    /// Per quota, the smaller of the two limits: what a request must pass when it is bounded by
    /// both its namespace's limits and the tenant's ceiling (`max_ttl`, `max_script_lines`).
    pub fn tightest(&self, other: &Quotas) -> Quotas {
        fn min<T: Ord + Copy>(a: Option<T>, b: Option<T>) -> Option<T> {
            match (a, b) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            }
        }
        Quotas {
            cpu_per_month: min(self.cpu_per_month, other.cpu_per_month),
            active_runs: min(self.active_runs, other.active_runs),
            runs_per_month: min(self.runs_per_month, other.runs_per_month),
            scripts: min(self.scripts, other.scripts),
            max_ttl: min(self.max_ttl, other.max_ttl),
            max_script_lines: min(self.max_script_lines, other.max_script_lines),
        }
    }

    /// The limit of `quota`, in its unit: milliseconds for the durations, a count otherwise.
    pub fn limit(&self, quota: QuotaKind) -> Option<u64> {
        let millis = |duration: Duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX);
        match quota {
            QuotaKind::CpuPerMonth => self.cpu_per_month.map(millis),
            QuotaKind::ActiveRuns => self.active_runs.map(u64::from),
            QuotaKind::RunsPerMonth => self.runs_per_month,
            QuotaKind::Scripts => self.scripts,
            QuotaKind::MaxTtl => self.max_ttl.map(millis),
            QuotaKind::MaxScriptLines => self.max_script_lines.map(u64::from),
        }
    }
}

/// The limits of a namespace: its exceptions over the tenant's defaults. The tenant's ceiling
/// bounds the tenant's totals; for `max_ttl` and `max_script_lines`, a request passes
/// `limits(..).tightest(&tenant.quotas)`.
pub fn limits(tenant: &Tenant, namespace: Option<&Namespace>) -> Quotas {
    match namespace {
        Some(namespace) => namespace.exceptions.over(&tenant.namespace_defaults),
        None => tenant.namespace_defaults.clone(),
    }
}

/// A quota.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
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

    /// As the API and the store write it: `cpu_per_month`.
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

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|quota| quota.as_str() == text)
    }
}

impl fmt::Display for QuotaKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Whose limit a refusal or an alert is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum QuotaScope {
    /// The namespace's limit: its exception, else the tenant's default.
    Namespace,
    /// The tenant's ceiling, on its totals.
    Tenant,
}

/// What `create_run` refused a run for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuotaExceeded {
    /// `ActiveRuns`, `RunsPerMonth` or `Scripts`.
    pub quota: QuotaKind,
    pub scope: QuotaScope,
    pub limit: u64,
    pub used: u64,
    /// The start of the next month, for a monthly quota.
    pub resets_at: Option<Timestamp>,
}

/// A calendar month, in UTC.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Month {
    year: i16,
    /// 1 to 12.
    month: i8,
}

impl Month {
    pub fn new(year: i16, month: i8) -> Option<Self> {
        jiff::civil::Date::new(year, month, 1)
            .ok()
            .map(|_| Self { year, month })
    }

    /// The month `at` falls in.
    pub fn of(at: Timestamp) -> Self {
        Self::of_day(Day::of(at))
    }

    /// The month a day falls in.
    pub fn of_day(day: Day) -> Self {
        let date = day.date();
        Self {
            year: date.year(),
            month: date.month(),
        }
    }

    pub fn year(self) -> i16 {
        self.year
    }

    pub fn month(self) -> i8 {
        self.month
    }

    /// Its first day.
    pub fn first_day(self) -> Day {
        Day::from_date(jiff::civil::date(self.year, self.month, 1))
    }

    /// The month after.
    pub fn next(self) -> Month {
        if self.month == 12 {
            Month {
                year: self.year.saturating_add(1),
                month: 1,
            }
        } else {
            Month {
                year: self.year,
                month: self.month + 1,
            }
        }
    }

    /// The first instant of the month.
    pub fn start(self) -> Timestamp {
        self.first_day().start()
    }

    /// The first instant of the next month: when a monthly quota frees.
    pub fn end(self) -> Timestamp {
        self.next().start()
    }

    /// Whether `day` falls in this month.
    pub fn contains(self, day: Day) -> bool {
        Month::of_day(day) == self
    }

    /// `2026-11`.
    pub fn parse(text: &str) -> Option<Self> {
        let (year, month) = text.split_once('-')?;
        if year.len() != 4 || month.len() != 2 {
            return None;
        }
        Month::new(year.parse().ok()?, month.parse().ok()?)
    }
}

/// `2026-11`.
impl fmt::Display for Month {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}", self.year, self.month)
    }
}

/// A UTC day: days since the Unix epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Day(pub u32);

impl Day {
    /// The day `at` falls in.
    pub fn of(at: Timestamp) -> Self {
        Self(u32::try_from(at.as_millis() / DAY_MS).unwrap_or(u32::MAX))
    }

    fn from_date(date: jiff::civil::Date) -> Self {
        let epoch = jiff::civil::date(1970, 1, 1);
        let days = date
            .since((jiff::Unit::Day, epoch))
            .map(|span| span.get_days())
            .unwrap_or(0);
        Self(u32::try_from(days).unwrap_or(0))
    }

    /// The civil date.
    pub fn date(self) -> jiff::civil::Date {
        jiff::civil::date(1970, 1, 1)
            .checked_add(jiff::Span::new().days(i64::from(self.0)))
            .unwrap_or(jiff::civil::Date::MAX)
    }

    /// Its first instant.
    pub fn start(self) -> Timestamp {
        Timestamp::from_millis(u64::from(self.0) * DAY_MS)
    }
}

/// `2026-11-14`.
impl fmt::Display for Day {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.date())
    }
}

/// A namespace as the store keeps it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Namespace {
    pub tenant: TenantId,
    pub name: NamespaceName,
    /// The customer's label for it, if any.
    pub display_name: Option<DisplayName>,
    /// Its exceptions to the tenant's defaults.
    pub exceptions: Quotas,
    /// Signed tokens of the namespace issued before this are refused.
    pub tokens_revoked_before: Option<Timestamp>,
    pub created_at: Timestamp,
    /// The last run created in it, or the last CPU time counted for it.
    pub last_active_at: Timestamp,
    /// Set while it is being erased: it takes no run and no token.
    pub deleting_since: Option<Timestamp>,
}

impl Namespace {
    /// A namespace without exceptions, created at `now`.
    pub fn new(tenant: TenantId, name: NamespaceName, now: Timestamp) -> Self {
        Self {
            tenant,
            name,
            display_name: None,
            exceptions: Quotas::default(),
            tokens_revoked_before: None,
            created_at: now,
            last_active_at: now,
            deleting_since: None,
        }
    }
}

/// Whose use to read.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum UsageOf {
    Namespace(NamespaceName),
    /// The tenant's totals, across its namespaces.
    Tenant,
}

/// CPU time a worker counted for a namespace on a day, to add to its use.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsageDelta {
    pub tenant: TenantId,
    pub namespace: NamespaceName,
    pub day: Day,
    pub cpu: Duration,
}

/// One day of use.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DayUsage {
    pub cpu: Duration,
    pub runs_started: u64,
}

/// What a namespace, or the tenant, used in a month.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub cpu: Duration,
    pub runs_started: u64,
    /// Runs active now.
    pub active_runs: u64,
    /// Distinct scripts with at least one run kept; for the tenant, the sum over its
    /// namespaces.
    pub scripts: u64,
    /// The days of the month with some use, oldest first.
    pub days: Vec<(Day, DayUsage)>,
}

impl Usage {
    /// The use of `quota`, in its unit: milliseconds for CPU time, a count otherwise. `None` for
    /// the limits that are not counted (`max_ttl`, `max_script_lines`).
    pub fn of(&self, quota: QuotaKind) -> Option<u64> {
        match quota {
            QuotaKind::CpuPerMonth => Some(u64::try_from(self.cpu.as_millis()).unwrap_or(u64::MAX)),
            QuotaKind::ActiveRuns => Some(self.active_runs),
            QuotaKind::RunsPerMonth => Some(self.runs_started),
            QuotaKind::Scripts => Some(self.scripts),
            QuotaKind::MaxTtl | QuotaKind::MaxScriptLines => None,
        }
    }
}

/// How `list_namespaces` orders: by a use of the month, the largest first, then by name; or by
/// name.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NamespaceOrder {
    #[default]
    Cpu,
    Runs,
    Scripts,
    Name,
}

/// A position in a listing of namespaces: the last one of the previous page, and its key in
/// the order (0 when ordered by name).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamespaceCursor {
    pub key: u64,
    pub name: NamespaceName,
}

/// Namespaces to list, in one tenant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamespaceFilter {
    pub tenant: TenantId,
    /// This namespace alone.
    pub name: Option<NamespaceName>,
    /// Names starting with this, or display names starting with it case aside
    /// ([`DisplayName::starts_with`]).
    pub prefix: Option<String>,
    /// Only those that used at least this many percent of one of their counted limits
    /// (`cpu_per_month`, `active_runs`, `runs_per_month`, `scripts`) in `month`.
    pub above_percent: Option<u32>,
    pub order: NamespaceOrder,
    /// The month whose use is read.
    pub month: Month,
    pub after: Option<NamespaceCursor>,
    pub limit: usize,
}

/// A page of [`JournalStore::list_namespaces`](super::JournalStore::list_namespaces).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NamespacePage {
    pub namespaces: Vec<NamespaceUsage>,
    /// How many namespaces match the filter, across every page: `after` and `limit` aside.
    pub total: u64,
}

/// A namespace with the limits that apply to it and its use of a month (without days).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamespaceUsage {
    pub namespace: Namespace,
    /// Its exceptions over the tenant's defaults.
    pub limits: Quotas,
    pub usage: Usage,
}

/// The share of a limit an alert is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Threshold {
    /// 80 % of the limit.
    Near,
    /// 100 % of the limit; for `active_runs`, its first refusal of the month.
    Reached,
}

impl Threshold {
    pub fn percent(self) -> u8 {
        match self {
            Threshold::Near => 80,
            Threshold::Reached => 100,
        }
    }

    /// The thresholds `before` and `after` cross, of a limit: those `after` reaches and `before`
    /// did not. Exact, in integers: a use reaches 80 % of a limit when `use × 100 ≥ limit × 80`,
    /// so 80 % of 4 is reached at 4, not 3, and 80 % of 1 at 1.
    pub fn crossed(before: u64, after: u64, limit: u64) -> Vec<Threshold> {
        [Threshold::Near, Threshold::Reached]
            .into_iter()
            .filter(|threshold| {
                let at = u128::from(limit) * u128::from(threshold.percent());
                u128::from(before) * 100 < at && u128::from(after) * 100 >= at
            })
            .collect()
    }
}

/// Which alert: at most one per key.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct QuotaAlertKey {
    pub tenant: TenantId,
    /// `None` for the tenant's ceiling.
    pub namespace: Option<NamespaceName>,
    pub quota: QuotaKind,
    pub period: Month,
    pub threshold: Threshold,
}

impl QuotaAlertKey {
    /// The id of its webhook delivery: `quota.alert:{namespace or *}:{quota}:{period}:{threshold}`.
    pub fn event_id(&self) -> String {
        format!(
            "quota.alert:{}:{}:{}:{}",
            self.namespace.as_ref().map_or("*", NamespaceName::as_str),
            self.quota,
            self.period,
            self.threshold.percent()
        )
    }
}

/// A limit crossed, recorded once by the write that crossed it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuotaAlert {
    pub key: QuotaAlertKey,
    /// The use when it was raised, in the quota's unit (milliseconds for CPU time).
    pub used: u64,
    pub limit: u64,
    pub raised_at: Timestamp,
    /// When it was delivered to the tenant's subscriptions.
    pub sent_at: Option<Timestamp>,
}

/// What `put_namespace` and `set_namespace_display_name` did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NamespaceWrite {
    /// The namespace as stored now.
    Written(Namespace),
    /// The namespace is being erased: nothing changed.
    Deleting,
}

/// What `delete_namespace` did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeleteNamespace {
    /// The namespace is marked: it takes no run and no token, and its erasure goes on.
    Started,
    NotFound,
}

/// What `drop_namespace` did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropNamespace {
    /// The namespace, its use and its alerts are gone.
    Dropped,
    /// It still has runs: erase them first.
    RunsLeft,
    /// No such namespace marked for erasure.
    NotFound,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_the_customers_ids() {
        for valid in ["alice", "user_2fQ81", "a.b:c@d-e", &"x".repeat(128)] {
            assert_eq!(NamespaceName::new(valid).unwrap().as_str(), valid);
        }
        for invalid in ["", "*", "a b", "a/b", "é", &"x".repeat(129)] {
            assert_eq!(
                NamespaceName::new(invalid),
                Err(InvalidNamespace(invalid.to_owned()))
            );
        }
    }

    #[test]
    fn display_names_are_trimmed_labels() {
        let parsed = |text: &str| DisplayName::parse(text).map(|name| name.map(|n| n.0));
        assert_eq!(parsed("  Acme Corp "), Ok(Some("Acme Corp".to_owned())));
        assert_eq!(
            parsed("Élodie & Co · 🦀"),
            Ok(Some("Élodie & Co · 🦀".to_owned()))
        );
        assert_eq!(parsed(&"é".repeat(128)), Ok(Some("é".repeat(128))));
        assert_eq!(parsed(""), Ok(None));
        assert_eq!(parsed(" \t\n "), Ok(None));
        for invalid in [&"x".repeat(129), "a\nb", "a\u{7}b", "tab\there"] {
            assert_eq!(
                DisplayName::parse(invalid),
                Err(InvalidDisplayName(invalid.to_owned()))
            );
        }
        assert!(DisplayName::new(" padded").is_err());
        assert!(DisplayName::new("").is_err());
        let name = DisplayName::new("ÉLODIE Σ").unwrap();
        assert_eq!(name.key(), "élodie σ");
        assert!(name.starts_with("élo"));
        assert!(name.starts_with("ÉLODIE Σ"));
        assert!(!name.starts_with("lodie"));
    }

    #[test]
    fn exceptions_override_defaults_quota_by_quota() {
        let defaults = Quotas {
            active_runs: Some(10),
            cpu_per_month: Some(Duration::from_secs(7_200)),
            ..Quotas::default()
        };
        let exceptions = Quotas {
            cpu_per_month: Some(Duration::from_secs(36_000)),
            ..Quotas::default()
        };
        let limits = exceptions.over(&defaults);
        assert_eq!(limits.active_runs, Some(10));
        assert_eq!(limits.cpu_per_month, Some(Duration::from_secs(36_000)));
        assert_eq!(limits.limit(QuotaKind::CpuPerMonth), Some(36_000_000));
        let ceiling = Quotas {
            active_runs: Some(5),
            max_ttl: Some(Duration::from_secs(60)),
            ..Quotas::default()
        };
        let tightest = limits.tightest(&ceiling);
        assert_eq!(tightest.active_runs, Some(5));
        assert_eq!(tightest.max_ttl, Some(Duration::from_secs(60)));
    }

    #[test]
    fn months_and_days_are_utc() {
        // 2026-11-14T17:00:00Z.
        let at = Timestamp::from_millis(1_794_675_600_000);
        let month = Month::of(at);
        assert_eq!(month.to_string(), "2026-11");
        assert_eq!(Day::of(at).to_string(), "2026-11-14");
        assert_eq!(month.start().as_millis(), 1_793_491_200_000);
        assert_eq!(month.end(), Month::new(2026, 12).unwrap().start());
        assert_eq!(Month::new(2026, 12).unwrap().next().to_string(), "2027-01");
        assert_eq!(Month::parse("2026-11"), Some(month));
        assert_eq!(Month::parse("2026-13"), None);
        assert!(month.contains(Day::of(at)));
        assert_eq!(Month::of_day(month.first_day()), month);
    }

    #[test]
    fn thresholds_are_crossed_once() {
        assert_eq!(Threshold::crossed(0, 79, 100), vec![]);
        assert_eq!(Threshold::crossed(79, 80, 100), vec![Threshold::Near]);
        assert_eq!(
            Threshold::crossed(50, 120, 100),
            vec![Threshold::Near, Threshold::Reached]
        );
        assert_eq!(Threshold::crossed(100, 150, 100), vec![]);
        let key = QuotaAlertKey {
            tenant: TenantId::new("acme").unwrap(),
            namespace: None,
            quota: QuotaKind::CpuPerMonth,
            period: Month::new(2026, 11).unwrap(),
            threshold: Threshold::Reached,
        };
        assert_eq!(key.event_id(), "quota.alert:*:cpu_per_month:2026-11:100");
    }
}
