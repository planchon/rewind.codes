//! The schema of `journal.db`, versioned: migrations are embedded in the binary, applied in
//! order when the database is opened, each in its own transaction with the new version. A
//! database whose version is above [`SCHEMA_VERSION`] was written by a newer build, and is
//! refused rather than misread. A migration is never edited once released: a change is a new
//! migration (version 2 moved access to grants and roles; version 3 added the workflows, the
//! watchers, and the runs' workflow, starter and scope; version 4 the workflow and starter of
//! the read model's runs; version 5 the namespaces; version 6 the tests and the suites of
//! workflow versions; version 7 people's connections and the OAuth clients and codes; version 8
//! the namespaces' display names; version 9 named secrets, their reads, the signing keys, the
//! master key of each data key and the runs' attachments; version 10 when each run's summary last
//! changed; version 11 the runs' origins and handle suffixes, the scripts' titles and the numbers
//! of workflow versions).
//!
//! The tables are those of `archi/06-storage-and-scale.md`, with these differences:
//!
//! - every key starts with the tenant, except where a run id is enough: run ids are unique
//!   across tenants, so `runs` is keyed by `id` (and indexed by tenant), and `run_watchers` by
//!   the run and its watcher (and indexed by watcher);
//! - composite values are spread over columns (a run's quarantine, lease, trace context, fork
//!   parent, workflow and starter; a task's route, policy and state), so that the store can
//!   query them. A run's `scope`, like its `labels`, is a JSON object;
//! - `workflows.stored_order` numbers each workflow's versions (by tenant and name) in the
//!   order they were stored, which breaks the ties of `created_at` when they are listed newest
//!   first; `workflows.number` is the version's number among them, as people name it
//!   (`triage v12`), unique by tenant and name;
//! - `runs.suffix_len` is how many characters the suffix of the run's handle has: the handle is
//!   `<namespace>/<origin>/<suffix>`, its suffix read from the run's id;
//! - a run keeps its oldest inbox item (`inbox_head`, received at `runnable_at`), and a task
//!   when it can be claimed (`due_at`): the picks of `acquire_runnable` and `claim_tool_tasks`
//!   read an index instead of grouping the inbox or scanning the tasks;
//! - `data_keys` holds each tenant's data key, encrypted by a master key whose id `master_key`
//!   records, apart from `tenants`: a tenant without settings (the embedded `local`) still has
//!   secrets;
//! - `signing_keys.public_key` holds the 32 bytes of an Ed25519 public key, which the API writes
//!   as a JWK; a run's `secrets`, like its `labels`, is a JSON object;
//! - `tool_calls.result_shape` holds the shape of a result whose value was projected, which
//!   `observed_shapes` merges per tool;
//! - `roles.permissions` holds a role's permissions as a JSON array of strings, sorted, and
//!   `grants.principal` and `grants.granted_by` a principal's text (`user:…`, `key:…`,
//!   `invitation:…`), whose order is the order of principals.
//! - a tenant's ceiling and its namespaces' defaults, and a namespace's exceptions, are a column
//!   per quota (`*_ms` for the durations; the ceiling's `active_runs` is `active_run_quota`);
//!   `usage` and `quota_alerts` hold the tenant's own rows under the namespace `*`, a day as days
//!   since the Unix epoch, a period as `2026-11` and a threshold as its percent; the permissions
//!   of `tokens` and `token_keys` are JSON arrays, as a role's;
//! - an OAuth client's `redirect_uris` is a JSON array of strings, a code's `organizations` a
//!   JSON object from tenant to access (`read`, `run`) and its `labels` a JSON object; a
//!   connection's `expires_at` is when its latest refresh token lapses, which its purge reads.
//!
//! The read model's tables, written only through `ReadModel`:
//!
//! - `run_summaries` keeps what the entries give (status, start and end, counts, history
//!   length), the metadata of the latest `apply` (headline, labels as JSON, parent, the
//!   workflow's name, `started_by`, the origin and suffix length of the run's handle), and the
//!   kept snapshot's columns: its length (`snapshot_len`), mode and ending, the ending's
//!   `fingerprint` among them. `wakes_at` is the earliest `until` of its waits while the run is
//!   active, kept for the `wakes` order;
//!   `changed_at` when a write last changed the summary, in milliseconds of the system clock,
//!   for the change stream (0 for the summaries of version 9, which no stream reads again);
//! - `run_labels` holds one row per label, for equality, prefix and presence;
//! - `run_waits` holds the kept snapshot's waits by `position`, the snapshot's order, without
//!   task states;
//! - `run_texts` holds what free text is searched in, lowercase, one row per field (the run's
//!   id, the headline, each label's value, the ending's error and message): a field matches on
//!   its own, whatever characters a quoted query holds;
//! - `tool_calls.task_attempt` and `task_phase` hold the furthest state recorded for a call's
//!   task, and `run_quarantine` the latest quarantine record (`kind` null: a release), each by
//!   run id alone: both may be recorded before the run is projected.

use turso::Connection;

use crate::Error;
use crate::sql::{self, Columns, opt_text, text};

/// The version of the schema this build writes.
pub const SCHEMA_VERSION: u32 = 11;

/// The `meta` key of the schema version.
const VERSION_KEY: &str = "schema_version";

/// Migration `n` brings the schema from version `n` to `n + 1`.
const MIGRATIONS: [&str; SCHEMA_VERSION as usize] = [V1, V2, V3, V4, V5, V6, V7, V8, V9, V10, V11];

const V1: &str = r#"
CREATE TABLE tenants (
    id TEXT PRIMARY KEY,
    active_run_quota INTEGER,
    journal_retention_ms INTEGER NOT NULL,
    archive_retention_ms INTEGER NOT NULL,
    read_model_retention_ms INTEGER NOT NULL,
    payload_grace_ms INTEGER NOT NULL
) STRICT;

CREATE TABLE data_keys (
    tenant TEXT PRIMARY KEY,
    wrapped_key BLOB NOT NULL
) STRICT;

CREATE TABLE api_keys (
    hash TEXT PRIMARY KEY,
    tenant TEXT NOT NULL,
    role TEXT NOT NULL,
    allowed_labels TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    revoked_at INTEGER
) STRICT;

CREATE TABLE scripts (
    tenant TEXT NOT NULL,
    digest TEXT NOT NULL,
    source TEXT NOT NULL,
    transpiled TEXT NOT NULL,
    source_map TEXT NOT NULL,
    PRIMARY KEY (tenant, digest)
) STRICT;

CREATE TABLE toolsets (
    tenant TEXT NOT NULL,
    digest TEXT NOT NULL,
    spec TEXT NOT NULL,
    declarations TEXT NOT NULL,
    prompt TEXT NOT NULL,
    PRIMARY KEY (tenant, digest)
) STRICT;

CREATE TABLE runs (
    id TEXT PRIMARY KEY,
    tenant TEXT NOT NULL,
    script TEXT NOT NULL,
    toolset TEXT NOT NULL,
    status TEXT NOT NULL,
    epoch INTEGER NOT NULL,
    history_len INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    expiry_enqueued INTEGER NOT NULL,
    quarantine_kind TEXT,
    quarantine_reason TEXT,
    quarantine_since INTEGER,
    quarantine_retried_by TEXT,
    failures INTEGER NOT NULL,
    attempts INTEGER NOT NULL,
    lease_owner TEXT,
    lease_until INTEGER,
    fence INTEGER NOT NULL,
    inbox_head INTEGER,
    runnable_at INTEGER,
    sticky_worker TEXT,
    labels TEXT NOT NULL,
    traceparent TEXT,
    tracestate TEXT,
    parent TEXT,
    parent_at INTEGER,
    idempotency_key TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    ended_at INTEGER,
    archive_document TEXT,
    archived_at INTEGER,
    UNIQUE (tenant, idempotency_key)
) STRICT;
CREATE INDEX runs_by_tenant ON runs (tenant, created_at, id);
CREATE INDEX runs_by_deadline ON runs (expires_at, id);
CREATE INDEX runs_by_end ON runs (ended_at, tenant, id);
CREATE INDEX runs_by_archive ON runs (archived_at, tenant, id);
CREATE INDEX runs_runnable ON runs (inbox_head);
CREATE INDEX runs_runnable_by_tenant ON runs (tenant, inbox_head);
CREATE INDEX runs_runnable_by_sticky_worker ON runs (sticky_worker, inbox_head);

CREATE TABLE history (
    run_id TEXT NOT NULL,
    idx INTEGER NOT NULL,
    recorded_at INTEGER NOT NULL,
    event TEXT NOT NULL,
    PRIMARY KEY (run_id, idx)
) STRICT;

CREATE TABLE inbox (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id TEXT NOT NULL,
    event TEXT NOT NULL,
    received_at INTEGER NOT NULL
) STRICT;
CREATE INDEX inbox_by_run ON inbox (run_id, id);

CREATE TABLE signal_keys (
    run_id TEXT NOT NULL,
    key TEXT NOT NULL,
    PRIMARY KEY (run_id, key)
) STRICT;

CREATE TABLE timers (
    run_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    fire_at INTEGER NOT NULL,
    PRIMARY KEY (run_id, seq)
) STRICT;
CREATE INDEX timers_by_fire_at ON timers (fire_at, run_id, seq);

CREATE TABLE tool_tasks (
    run_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    tool TEXT NOT NULL,
    route TEXT NOT NULL,
    mcp_server TEXT,
    dispatched INTEGER NOT NULL,
    args_digest TEXT NOT NULL,
    args TEXT,
    on_interruption TEXT NOT NULL,
    approval INTEGER NOT NULL,
    execution_timeout_ms INTEGER,
    heartbeat_timeout_ms INTEGER NOT NULL,
    concurrency INTEGER NOT NULL,
    server_concurrency INTEGER,
    state TEXT NOT NULL,
    attempt INTEGER NOT NULL,
    token TEXT UNIQUE,
    lease_owner TEXT,
    lease_until INTEGER,
    next_attempt_at INTEGER,
    due_at INTEGER,
    parked_kind TEXT,
    parked_reason TEXT,
    schema_change_approved INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (run_id, seq)
) STRICT;
CREATE INDEX tool_tasks_due ON tool_tasks (due_at, run_id, seq);
CREATE INDEX tool_tasks_deferred ON tool_tasks (state, lease_until, run_id, seq);
CREATE INDEX tool_tasks_by_owner ON tool_tasks (lease_owner, mcp_server);

CREATE TABLE webhook_tokens (
    token TEXT PRIMARY KEY,
    run_id TEXT NOT NULL,
    name TEXT NOT NULL,
    UNIQUE (run_id, name)
) STRICT;

CREATE TABLE subscriptions (
    tenant TEXT NOT NULL,
    id TEXT NOT NULL,
    url TEXT NOT NULL,
    secret TEXT NOT NULL,
    events TEXT NOT NULL,
    labels TEXT NOT NULL,
    PRIMARY KEY (tenant, id)
) STRICT;

CREATE TABLE payloads (
    tenant TEXT NOT NULL,
    digest TEXT NOT NULL,
    bytes INTEGER NOT NULL,
    preview TEXT NOT NULL,
    value TEXT,
    unreferenced_since INTEGER,
    deletion_started_at INTEGER,
    PRIMARY KEY (tenant, digest)
) STRICT;

CREATE TABLE payload_refs (
    tenant TEXT NOT NULL,
    digest TEXT NOT NULL,
    run_id TEXT NOT NULL,
    PRIMARY KEY (tenant, digest, run_id)
) STRICT;
CREATE INDEX payload_refs_by_run ON payload_refs (run_id);

CREATE TABLE secrets (
    tenant TEXT NOT NULL,
    owner TEXT NOT NULL,
    ciphertext BLOB NOT NULL,
    PRIMARY KEY (tenant, owner)
) STRICT;

CREATE TABLE feed (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    consumer TEXT NOT NULL,
    tenant TEXT NOT NULL,
    run_id TEXT NOT NULL,
    fence INTEGER NOT NULL,
    from_idx INTEGER NOT NULL,
    to_idx INTEGER NOT NULL,
    lease_owner TEXT,
    lease_until INTEGER
) STRICT;
CREATE INDEX feed_by_consumer ON feed (consumer, id);
CREATE INDEX feed_by_run ON feed (run_id);

CREATE TABLE run_summaries (
    run_id TEXT PRIMARY KEY,
    tenant TEXT NOT NULL,
    script TEXT NOT NULL,
    headline TEXT NOT NULL,
    toolset TEXT NOT NULL,
    status TEXT NOT NULL,
    mode TEXT,
    labels TEXT NOT NULL,
    parent TEXT,
    started_at INTEGER,
    ended_at INTEGER,
    last_activity_at INTEGER NOT NULL,
    tool_calls INTEGER NOT NULL,
    errors INTEGER NOT NULL,
    history_len INTEGER NOT NULL,
    snapshot_len INTEGER NOT NULL,
    wakes_at INTEGER,
    ending_error TEXT,
    ending_message TEXT,
    ending_line INTEGER,
    ending_signature TEXT,
    fingerprint TEXT
) STRICT;
CREATE INDEX run_summaries_by_tenant ON run_summaries (tenant, run_id);
CREATE INDEX run_summaries_by_activity ON run_summaries (tenant, last_activity_at, run_id);

CREATE TABLE run_labels (
    tenant TEXT NOT NULL,
    run_id TEXT NOT NULL,
    key TEXT NOT NULL,
    value TEXT NOT NULL,
    PRIMARY KEY (run_id, key)
) STRICT;
CREATE INDEX run_labels_by_value ON run_labels (tenant, key, value);

CREATE TABLE run_waits (
    run_id TEXT NOT NULL,
    position INTEGER NOT NULL,
    kind TEXT NOT NULL,
    seq INTEGER,
    name TEXT,
    since INTEGER NOT NULL,
    until INTEGER,
    callsite_line INTEGER,
    callsite_column INTEGER,
    PRIMARY KEY (run_id, position)
) STRICT;

CREATE TABLE run_texts (
    run_id TEXT NOT NULL,
    field INTEGER NOT NULL,
    text TEXT NOT NULL,
    PRIMARY KEY (run_id, field)
) STRICT;

CREATE TABLE run_quarantine (
    run_id TEXT PRIMARY KEY,
    recorded_at INTEGER NOT NULL,
    kind TEXT,
    reason TEXT,
    since INTEGER
) STRICT;

CREATE TABLE tool_calls (
    run_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    tool TEXT,
    callsite_line INTEGER,
    scheduled_at INTEGER,
    completed_at INTEGER,
    outcome TEXT,
    error_kind TEXT,
    args_digest TEXT,
    result_digest TEXT,
    result_shape TEXT,
    task_attempt INTEGER,
    task_phase TEXT,
    PRIMARY KEY (run_id, seq)
) STRICT;
CREATE INDEX tool_calls_by_tool ON tool_calls (tool);
"#;

/// Access (`AccessStore`): API keys gain a name and their creator, and what a key may do moves
/// to its grant, held by the principal `key:<hash>`. Each key of version 1 gets a grant of its
/// role (`admin`, the operator's before roles became data, becomes `operator`) with its allowed
/// labels, granted when the key was created, and the name `unnamed <role> key`.
const V2: &str = r#"
CREATE TABLE roles (
    tenant TEXT NOT NULL,
    name TEXT NOT NULL,
    description TEXT NOT NULL,
    permissions TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (tenant, name)
) STRICT;

CREATE TABLE grants (
    tenant TEXT NOT NULL,
    principal TEXT NOT NULL,
    role TEXT NOT NULL,
    allowed_labels TEXT NOT NULL,
    granted_by TEXT,
    granted_at INTEGER NOT NULL,
    PRIMARY KEY (tenant, principal)
) STRICT;
CREATE INDEX grants_by_role ON grants (tenant, role);

CREATE TABLE access_settings (
    tenant TEXT PRIMARY KEY,
    default_role TEXT NOT NULL
) STRICT;

INSERT INTO grants (tenant, principal, role, allowed_labels, granted_by, granted_at)
SELECT tenant, 'key:' || hash, CASE role WHEN 'admin' THEN 'operator' ELSE role END,
       allowed_labels, NULL, created_at
FROM api_keys;

ALTER TABLE api_keys RENAME TO api_keys_v1;

CREATE TABLE api_keys (
    hash TEXT PRIMARY KEY,
    tenant TEXT NOT NULL,
    name TEXT NOT NULL,
    created_by TEXT,
    created_at INTEGER NOT NULL,
    revoked_at INTEGER
) STRICT;
CREATE INDEX api_keys_by_tenant ON api_keys (tenant, created_at, hash);

INSERT INTO api_keys (hash, tenant, name, created_by, created_at, revoked_at)
SELECT hash, tenant, 'unnamed ' || CASE role WHEN 'admin' THEN 'operator' ELSE role END || ' key',
       NULL, created_at, revoked_at
FROM api_keys_v1;

DROP TABLE api_keys_v1;
"#;

/// Workflows and the runs that watch a run's end (`archi/03-execution.md`, "Workflows across
/// runs"). A run's `workflow` and `workflow_version` are null together, as are `started_by`,
/// `started_seq` and `detached`.
const V3: &str = r#"
ALTER TABLE runs ADD COLUMN workflow TEXT;
ALTER TABLE runs ADD COLUMN workflow_version TEXT;
ALTER TABLE runs ADD COLUMN started_by TEXT;
ALTER TABLE runs ADD COLUMN started_seq INTEGER;
ALTER TABLE runs ADD COLUMN detached INTEGER;
ALTER TABLE runs ADD COLUMN scope TEXT NOT NULL DEFAULT '{}';

CREATE TABLE workflows (
    tenant TEXT NOT NULL,
    version TEXT NOT NULL,
    name TEXT NOT NULL,
    document TEXT NOT NULL,
    labels TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    stored_order INTEGER NOT NULL,
    PRIMARY KEY (tenant, version)
) STRICT;
CREATE INDEX workflows_by_name ON workflows (tenant, name, created_at, stored_order);

CREATE TABLE run_watchers (
    run_id TEXT NOT NULL,
    watcher TEXT NOT NULL,
    seq INTEGER NOT NULL,
    attached INTEGER NOT NULL,
    PRIMARY KEY (run_id, watcher)
) STRICT;
CREATE INDEX run_watchers_by_watcher ON run_watchers (watcher, attached);
"#;

/// The read model's runs by workflow and by the run that started them
/// (`archi/05-testing-and-observability.md`, "Query language": `workflow:`, `started_by:`): a
/// summary keeps the workflow's name and its starter, null for a run of neither, like `parent`.
/// No index: a query's terms are `CASE` expressions, which an index does not serve.
const V4: &str = r#"
ALTER TABLE run_summaries ADD COLUMN workflow TEXT;
ALTER TABLE run_summaries ADD COLUMN started_by TEXT;
"#;

/// Namespaces (`archi/06-storage-and-scale.md`, "Namespaces", "Quotas"): the end user a run
/// belongs to, with idempotency keys unique per namespace (`runs` is rebuilt for its new key,
/// with the workflow columns of version 3); the tenant's ceiling and its namespaces' defaults, a
/// column per quota; namespaces with their exceptions, their use per day (`*` the tenant's own
/// rows), their alerts and the runs kept per script; end users' tokens and the keys that sign
/// them.
///
/// The runs that existed before namespaces belong to `migrated`, which is created in each tenant
/// that has runs, with its script counts and the runs started per day, its own and its tenant's.
const V5: &str = r#"
CREATE TABLE runs_v5 (
    id TEXT PRIMARY KEY,
    tenant TEXT NOT NULL,
    namespace TEXT NOT NULL,
    script TEXT NOT NULL,
    toolset TEXT NOT NULL,
    status TEXT NOT NULL,
    epoch INTEGER NOT NULL,
    history_len INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    expiry_enqueued INTEGER NOT NULL,
    quarantine_kind TEXT,
    quarantine_reason TEXT,
    quarantine_since INTEGER,
    quarantine_retried_by TEXT,
    failures INTEGER NOT NULL,
    attempts INTEGER NOT NULL,
    lease_owner TEXT,
    lease_until INTEGER,
    fence INTEGER NOT NULL,
    inbox_head INTEGER,
    runnable_at INTEGER,
    sticky_worker TEXT,
    labels TEXT NOT NULL,
    traceparent TEXT,
    tracestate TEXT,
    parent TEXT,
    parent_at INTEGER,
    idempotency_key TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    ended_at INTEGER,
    archive_document TEXT,
    archived_at INTEGER,
    workflow TEXT,
    workflow_version TEXT,
    started_by TEXT,
    started_seq INTEGER,
    detached INTEGER,
    scope TEXT NOT NULL DEFAULT '{}',
    UNIQUE (tenant, namespace, idempotency_key)
) STRICT;
INSERT INTO runs_v5 (namespace,
    id, tenant, script, toolset, status, epoch, history_len, expires_at, expiry_enqueued,
    quarantine_kind, quarantine_reason, quarantine_since, quarantine_retried_by, failures,
    attempts, lease_owner, lease_until, fence, inbox_head, runnable_at, sticky_worker, labels,
    traceparent, tracestate, parent, parent_at, idempotency_key, created_at, updated_at,
    ended_at, archive_document, archived_at, workflow, workflow_version, started_by, started_seq,
    detached, scope)
SELECT 'migrated',
    id, tenant, script, toolset, status, epoch, history_len, expires_at, expiry_enqueued,
    quarantine_kind, quarantine_reason, quarantine_since, quarantine_retried_by, failures,
    attempts, lease_owner, lease_until, fence, inbox_head, runnable_at, sticky_worker, labels,
    traceparent, tracestate, parent, parent_at, idempotency_key, created_at, updated_at,
    ended_at, archive_document, archived_at, workflow, workflow_version, started_by, started_seq,
    detached, scope
FROM runs;
DROP TABLE runs;
ALTER TABLE runs_v5 RENAME TO runs;
CREATE INDEX runs_by_tenant ON runs (tenant, created_at, id);
CREATE INDEX runs_by_namespace ON runs (tenant, namespace, status);
CREATE INDEX runs_by_deadline ON runs (expires_at, id);
CREATE INDEX runs_by_end ON runs (ended_at, tenant, id);
CREATE INDEX runs_by_archive ON runs (archived_at, tenant, id);
CREATE INDEX runs_runnable ON runs (inbox_head);
CREATE INDEX runs_runnable_by_tenant ON runs (tenant, inbox_head);
CREATE INDEX runs_runnable_by_sticky_worker ON runs (sticky_worker, inbox_head);

ALTER TABLE run_summaries ADD COLUMN namespace TEXT NOT NULL DEFAULT 'migrated';

ALTER TABLE tenants ADD COLUMN cpu_per_month_ms INTEGER;
ALTER TABLE tenants ADD COLUMN runs_per_month INTEGER;
ALTER TABLE tenants ADD COLUMN scripts INTEGER;
ALTER TABLE tenants ADD COLUMN max_ttl_ms INTEGER;
ALTER TABLE tenants ADD COLUMN max_script_lines INTEGER;
ALTER TABLE tenants ADD COLUMN default_cpu_per_month_ms INTEGER;
ALTER TABLE tenants ADD COLUMN default_active_runs INTEGER;
ALTER TABLE tenants ADD COLUMN default_runs_per_month INTEGER;
ALTER TABLE tenants ADD COLUMN default_scripts INTEGER;
ALTER TABLE tenants ADD COLUMN default_max_ttl_ms INTEGER;
ALTER TABLE tenants ADD COLUMN default_max_script_lines INTEGER;

CREATE TABLE namespaces (
    tenant TEXT NOT NULL,
    name TEXT NOT NULL,
    cpu_per_month_ms INTEGER,
    active_runs INTEGER,
    runs_per_month INTEGER,
    scripts INTEGER,
    max_ttl_ms INTEGER,
    max_script_lines INTEGER,
    tokens_revoked_before INTEGER,
    created_at INTEGER NOT NULL,
    last_active_at INTEGER NOT NULL,
    deleting_since INTEGER,
    PRIMARY KEY (tenant, name)
) STRICT;
CREATE INDEX namespaces_deleting ON namespaces (deleting_since, tenant, name);

CREATE TABLE usage (
    tenant TEXT NOT NULL,
    namespace TEXT NOT NULL,
    day INTEGER NOT NULL,
    cpu_ms INTEGER NOT NULL,
    runs_started INTEGER NOT NULL,
    PRIMARY KEY (tenant, namespace, day)
) STRICT;

CREATE TABLE quota_alerts (
    tenant TEXT NOT NULL,
    namespace TEXT NOT NULL,
    quota TEXT NOT NULL,
    period TEXT NOT NULL,
    threshold INTEGER NOT NULL,
    used INTEGER NOT NULL,
    quota_limit INTEGER NOT NULL,
    raised_at INTEGER NOT NULL,
    lease_owner TEXT,
    lease_until INTEGER,
    sent_at INTEGER,
    PRIMARY KEY (tenant, namespace, quota, period, threshold)
) STRICT;
CREATE INDEX quota_alerts_unsent ON quota_alerts (sent_at, raised_at);
CREATE INDEX quota_alerts_by_period ON quota_alerts (tenant, period);

CREATE TABLE namespace_scripts (
    tenant TEXT NOT NULL,
    namespace TEXT NOT NULL,
    digest TEXT NOT NULL,
    runs INTEGER NOT NULL,
    first_run_at INTEGER NOT NULL,
    PRIMARY KEY (tenant, namespace, digest)
) STRICT;

CREATE TABLE tokens (
    hash TEXT PRIMARY KEY,
    tenant TEXT NOT NULL,
    namespace TEXT NOT NULL,
    permissions TEXT NOT NULL,
    labels TEXT NOT NULL,
    expires_at INTEGER NOT NULL,
    created_by TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    revoked_at INTEGER
) STRICT;
CREATE INDEX tokens_by_namespace ON tokens (tenant, namespace);
CREATE INDEX tokens_by_expiry ON tokens (expires_at);
CREATE INDEX tokens_by_revocation ON tokens (revoked_at);

CREATE TABLE token_keys (
    kid TEXT PRIMARY KEY,
    tenant TEXT NOT NULL,
    name TEXT NOT NULL,
    jwk TEXT NOT NULL,
    permissions TEXT NOT NULL,
    created_by TEXT NOT NULL,
    created_at INTEGER NOT NULL
) STRICT;
CREATE INDEX token_keys_by_tenant ON token_keys (tenant, created_at, kid);

INSERT INTO namespaces (tenant, name, created_at, last_active_at)
SELECT tenant, 'migrated', MIN(created_at), MAX(created_at) FROM runs GROUP BY tenant;
INSERT INTO namespace_scripts (tenant, namespace, digest, runs, first_run_at)
SELECT tenant, 'migrated', script, COUNT(*), MIN(created_at) FROM runs GROUP BY tenant, script;
INSERT INTO usage (tenant, namespace, day, cpu_ms, runs_started)
SELECT tenant, 'migrated', created_at / 86400000, 0, COUNT(*) FROM runs
GROUP BY tenant, created_at / 86400000;
INSERT INTO usage (tenant, namespace, day, cpu_ms, runs_started)
SELECT tenant, '*', created_at / 86400000, 0, COUNT(*) FROM runs
GROUP BY tenant, created_at / 86400000;
"#;

/// Tests and the suites of workflow versions (`archi/05-testing-and-observability.md`, "Where
/// tests are stored"). A `tests` row indexes a test whose document is a blob under its digest:
/// the namespace of the run a recorded test came from (null for a test written by hand), its
/// size, when it was stored or reserved last (its grace starts there), and when a cleanup started
/// deleting it (its blob may be gone). `workflow_tests` holds the tests of each version's suite,
/// written with the version, and is indexed by test: a test is kept while a version references
/// it.
const V6: &str = r#"
CREATE TABLE tests (
    tenant TEXT NOT NULL,
    digest TEXT NOT NULL,
    namespace TEXT,
    size INTEGER NOT NULL,
    reserved_at INTEGER NOT NULL,
    deletion_started_at INTEGER,
    PRIMARY KEY (tenant, digest)
) STRICT;
CREATE INDEX tests_by_namespace ON tests (tenant, namespace);

CREATE TABLE workflow_tests (
    tenant TEXT NOT NULL,
    version TEXT NOT NULL,
    digest TEXT NOT NULL,
    PRIMARY KEY (tenant, version, digest)
) STRICT;
CREATE INDEX workflow_tests_by_test ON workflow_tests (tenant, digest);
"#;

/// People's connections (`archi/06-storage-and-scale.md`, "Connections"), outside every tenant:
/// the OAuth clients (metadata documents cached under their URL, registered clients `rc_…`),
/// the authorization codes, the connections and their tokens, all hashed. A connection's grants
/// are rows of `grants` (principal `connection:cn_…`), found by principal across tenants.
const V7: &str = r#"
CREATE INDEX grants_by_principal ON grants (principal);

CREATE TABLE oauth_clients (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    name TEXT NOT NULL,
    uri TEXT,
    logo_uri TEXT,
    redirect_uris TEXT NOT NULL,
    auth_method TEXT NOT NULL,
    secret_hash TEXT,
    fetched_at INTEGER,
    expires_at INTEGER,
    created_at INTEGER NOT NULL,
    last_used_at INTEGER
) STRICT;
CREATE INDEX oauth_clients_by_kind ON oauth_clients (kind);

CREATE TABLE oauth_codes (
    hash TEXT PRIMARY KEY,
    client_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    redirect_uri TEXT NOT NULL,
    code_challenge TEXT NOT NULL,
    resource TEXT NOT NULL,
    organizations TEXT NOT NULL,
    labels TEXT NOT NULL,
    expires_at INTEGER NOT NULL,
    used_at INTEGER,
    connection_id TEXT
) STRICT;
CREATE INDEX oauth_codes_by_expiry ON oauth_codes (expires_at);

CREATE TABLE connections (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL,
    client_id TEXT NOT NULL,
    client_name TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    last_used_at INTEGER,
    revoked_at INTEGER,
    expires_at INTEGER NOT NULL
) STRICT;
CREATE INDEX connections_by_user ON connections (user_id, client_id);
CREATE INDEX connections_by_client ON connections (client_id);

CREATE TABLE connection_tokens (
    hash TEXT PRIMARY KEY,
    connection_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    expires_at INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    used_at INTEGER
) STRICT;
CREATE INDEX connection_tokens_by_connection ON connection_tokens (connection_id);
CREATE INDEX connection_tokens_by_expiry ON connection_tokens (expires_at);
"#;

/// A namespace's display name (`archi/06-storage-and-scale.md`, "Namespaces"): the customer's
/// label for the end user. A listing's prefix matches it case aside in the store's code.
const V8: &str = r#"
ALTER TABLE namespaces ADD COLUMN display_name TEXT;
"#;

/// Named secrets attached to runs, and each tenant's signing keys (`archi/04-tools.md`, "Secrets"
/// and "Signed calls"). The credentials per toolset, per namespace and toolset, and per run are
/// dropped, with the data keys that encrypted them: nothing is encrypted under those any more, and
/// a tenant's next secret or signing key makes a data key under the current master key, whose id
/// `data_keys.master_key` records. An organization's secrets are kept under the namespace `*`.
const V9: &str = r#"
DROP TABLE secrets;
DROP TABLE data_keys;

CREATE TABLE data_keys (
    tenant TEXT PRIMARY KEY,
    wrapped_key BLOB NOT NULL,
    master_key TEXT NOT NULL
) STRICT;

CREATE TABLE secrets (
    tenant TEXT NOT NULL,
    namespace TEXT NOT NULL,
    name TEXT NOT NULL,
    ciphertext BLOB NOT NULL,
    labels TEXT NOT NULL,
    created_by TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_by TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    last_read_at INTEGER,
    last_read_run TEXT,
    last_read_namespace TEXT,
    PRIMARY KEY (tenant, namespace, name)
) STRICT;

CREATE TABLE secret_reads (
    tenant TEXT NOT NULL,
    namespace TEXT NOT NULL,
    name TEXT NOT NULL,
    day INTEGER NOT NULL,
    reads INTEGER NOT NULL,
    PRIMARY KEY (tenant, namespace, name, day)
) STRICT;

CREATE TABLE signing_keys (
    tenant TEXT NOT NULL,
    kid TEXT NOT NULL,
    public_key BLOB NOT NULL,
    private_ciphertext BLOB NOT NULL,
    created_at INTEGER NOT NULL,
    retired_at INTEGER,
    PRIMARY KEY (tenant, kid)
) STRICT;

ALTER TABLE runs ADD COLUMN secrets TEXT NOT NULL DEFAULT '{}';
"#;

/// When each run's summary last changed (`archi/08-decisions.md`, "Taken for the live
/// dashboard"): the change stream reads the summaries stamped since its last pass. The summaries
/// kept are stamped 0, before any stream.
const V10: &str = r#"
ALTER TABLE run_summaries ADD COLUMN changed_at INTEGER NOT NULL DEFAULT 0;
CREATE INDEX run_summaries_by_change ON run_summaries (tenant, changed_at);
"#;

/// Run handles, script titles and the numbers of workflow versions
/// (`archi/06-storage-and-scale.md`, "Run handles"; `archi/04-tools.md`, "Script titles"). A run
/// keeps its origin, the middle of its handle, and the characters of its handle's suffix. A run
/// stored before handles takes its workflow's name, or `script`, and the short suffix, unless
/// another run of its tenant shares the first 64 bits of its id (the first 18 characters of its
/// text, `xxxxxxxx-xxxx-xxxx`): every run of such a group then takes the full suffix, which tells
/// distinct ids apart. A summary takes its run's; a summary whose run the journal no longer holds
/// follows the same rules among the summaries of its tenant. The versions of each workflow are
/// numbered from 1 in the order they were created, ties in the order they were stored.
///
/// `+twin.tenant` keeps the planner from searching the twins by tenant (`runs_by_tenant`, a scan
/// of the tenant's runs for each run) rather than by the range of their ids.
const V11: &str = r#"
ALTER TABLE runs ADD COLUMN origin TEXT NOT NULL DEFAULT 'script';
ALTER TABLE runs ADD COLUMN suffix_len INTEGER NOT NULL DEFAULT 12;
UPDATE runs SET origin = workflow WHERE workflow IS NOT NULL;
UPDATE runs SET suffix_len = 25 WHERE EXISTS (
    SELECT 1 FROM runs twin
    WHERE twin.id BETWEEN substr(runs.id, 1, 18) || '-0000-000000000000'
                      AND substr(runs.id, 1, 18) || '-ffff-ffffffffffff'
      AND +twin.tenant = runs.tenant AND twin.id <> runs.id
);

ALTER TABLE run_summaries ADD COLUMN origin TEXT NOT NULL DEFAULT 'script';
ALTER TABLE run_summaries ADD COLUMN suffix_len INTEGER NOT NULL DEFAULT 12;
UPDATE run_summaries SET origin = workflow WHERE workflow IS NOT NULL;
UPDATE run_summaries SET suffix_len = 25 WHERE EXISTS (
    SELECT 1 FROM run_summaries twin
    WHERE twin.run_id BETWEEN substr(run_summaries.run_id, 1, 18) || '-0000-000000000000'
                          AND substr(run_summaries.run_id, 1, 18) || '-ffff-ffffffffffff'
      AND twin.tenant = run_summaries.tenant AND twin.run_id <> run_summaries.run_id
);
UPDATE run_summaries SET
    origin = (SELECT runs.origin FROM runs WHERE runs.id = run_summaries.run_id),
    suffix_len = (SELECT runs.suffix_len FROM runs WHERE runs.id = run_summaries.run_id)
WHERE run_id IN (SELECT id FROM runs);

CREATE TABLE script_titles (
    tenant TEXT NOT NULL,
    digest TEXT NOT NULL,
    title TEXT NOT NULL,
    description TEXT,
    updated_by TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (tenant, digest)
) STRICT;

ALTER TABLE workflows ADD COLUMN number INTEGER NOT NULL DEFAULT 0;
CREATE TABLE workflow_numbers (
    tenant TEXT NOT NULL,
    version TEXT NOT NULL,
    number INTEGER NOT NULL,
    PRIMARY KEY (tenant, version)
) STRICT;
INSERT INTO workflow_numbers (tenant, version, number)
SELECT tenant, version,
       row_number() OVER (PARTITION BY tenant, name ORDER BY created_at, stored_order)
FROM workflows;
UPDATE workflows SET number = (
    SELECT n.number FROM workflow_numbers n
    WHERE n.tenant = workflows.tenant AND n.version = workflows.version
);
DROP TABLE workflow_numbers;
CREATE UNIQUE INDEX workflows_by_number ON workflows (tenant, name, number);
"#;

/// Brings the database's schema to [`SCHEMA_VERSION`], or refuses a newer one.
pub(crate) async fn migrate(connection: &Connection) -> Result<(), Error> {
    migrate_to(connection, SCHEMA_VERSION).await
}

/// Brings the database's schema to `target`, at most [`SCHEMA_VERSION`], or refuses a newer one.
async fn migrate_to(connection: &Connection, target: u32) -> Result<(), Error> {
    sql::execute(
        connection,
        "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL) STRICT",
        Vec::new(),
    )
    .await?;
    let found = version(connection).await?;
    if found > SCHEMA_VERSION {
        return Err(Error::NewerSchema {
            found,
            supported: SCHEMA_VERSION,
        });
    }
    let migrations = MIGRATIONS.iter().take(target as usize);
    for (from, migration) in migrations.enumerate().skip(found as usize) {
        let to = (from + 1).to_string();
        sql::transaction(connection, async {
            connection.execute_batch(migration).await?;
            set_meta(connection, VERSION_KEY, Some(&to)).await
        })
        .await?;
        tracing::info!(version = %to, "migrated the schema of the embedded store");
    }
    Ok(())
}

/// The schema version recorded in `meta`; 0 for a new database.
pub(crate) async fn version(connection: &Connection) -> Result<u32, Error> {
    match meta(connection, VERSION_KEY).await? {
        None => Ok(0),
        Some(value) => value
            .parse()
            .map_err(|_| Error::Corrupt(format!("schema version {value:?}"))),
    }
}

pub(crate) async fn meta(connection: &Connection, key: &str) -> Result<Option<String>, Error> {
    let row = sql::query_row(
        connection,
        "SELECT value FROM meta WHERE key = ?",
        vec![text(key)],
    )
    .await?;
    row.map(|row| row.text(0)).transpose()
}

pub(crate) async fn set_meta(
    connection: &Connection,
    key: &str,
    value: Option<&str>,
) -> Result<(), Error> {
    match value {
        Some(value) => {
            sql::execute(
                connection,
                "INSERT OR REPLACE INTO meta (key, value) VALUES (?, ?)",
                vec![text(key), opt_text(Some(value))],
            )
            .await?
        }
        None => {
            sql::execute(
                connection,
                "DELETE FROM meta WHERE key = ?",
                vec![text(key)],
            )
            .await?
        }
    };
    Ok(())
}

/// The schema version is the store's own: the server cannot set it.
pub(crate) fn check_setting_name(name: &str) -> Result<(), Error> {
    if name == VERSION_KEY {
        return Err(Error::ReservedSetting(name.to_owned()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use rewind_core::{Digest, RunId, RunStatus, Seq, Timestamp};
    use rewind_engine::ports::{
        AccessStore, ApiKey, Grant, JournalStore, Predicate, Principal, ProjectedRun, ReadModel,
        RunMetadata, RunOrder, RunQuery, RunRef, RunSnapshot, TenantId, TextMatch, Watched,
    };

    use super::*;
    use crate::codec::encode_labels;
    use crate::sql::{digest, opt_ts, ts};
    use crate::{MasterKey, TursoStore};

    const T0: Timestamp = Timestamp(1_767_225_600_000);

    fn at(seconds: u64) -> Timestamp {
        Timestamp(T0.0 + seconds * 1_000)
    }

    fn tenant_id(id: &str) -> TenantId {
        TenantId::new(id).expect("tenant")
    }

    /// A key as version 1 kept it: its role and allowed labels on the key itself.
    struct KeyOfVersion1 {
        secret: &'static str,
        tenant: &'static str,
        role: &'static str,
        labels: &'static [(&'static str, &'static str)],
        created_at: Timestamp,
        revoked_at: Option<Timestamp>,
    }

    fn labels(labels: &[(&str, &str)]) -> BTreeMap<String, String> {
        labels
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[tokio::test]
    async fn keys_of_version_1_become_keys_and_grants() {
        let keys = [
            KeyOfVersion1 {
                secret: "rewind_sk_admin",
                tenant: "acme",
                role: "admin",
                labels: &[],
                created_at: T0,
                revoked_at: None,
            },
            KeyOfVersion1 {
                secret: "rewind_sk_agent",
                tenant: "acme",
                role: "harness",
                labels: &[("conversation", "c-1")],
                created_at: at(1),
                revoked_at: None,
            },
            KeyOfVersion1 {
                secret: "rewind_sk_gone",
                tenant: "beta",
                role: "reader",
                labels: &[],
                created_at: at(2),
                revoked_at: Some(at(3)),
            },
        ];
        let dir = tempfile::TempDir::new().expect("temporary directory");
        let path = dir.path().join("journal.db");
        {
            let database = turso::Builder::new_local(path.to_str().expect("UTF-8 path"))
                .build()
                .await
                .expect("open the file");
            let connection = database.connect().expect("connect");
            migrate_to(&connection, 1).await.expect("version 1");
            for key in &keys {
                sql::execute(
                    &connection,
                    "INSERT INTO api_keys (hash, tenant, role, allowed_labels, created_at, \
                     revoked_at) VALUES (?, ?, ?, ?, ?, ?)",
                    vec![
                        digest(Digest::of(key.secret.as_bytes())),
                        text(key.tenant),
                        text(key.role),
                        text(encode_labels(&labels(key.labels))),
                        ts(key.created_at),
                        opt_ts(key.revoked_at),
                    ],
                )
                .await
                .expect("a key of version 1");
            }
        }

        let store = TursoStore::open(&path, MasterKey::from_bytes([1; 32]))
            .await
            .expect("migrate to the current version");
        assert_eq!(store.schema_version().await, Ok(SCHEMA_VERSION));
        for (key, role) in keys.iter().zip(["operator", "harness", "reader"]) {
            let hash = Digest::of(key.secret.as_bytes());
            let tenant = tenant_id(key.tenant);
            assert_eq!(
                store.find_key(hash).await.expect("find_key"),
                Some(ApiKey {
                    hash,
                    tenant: tenant.clone(),
                    name: format!("unnamed {role} key"),
                    created_by: None,
                    created_at: key.created_at,
                    revoked_at: key.revoked_at,
                }),
                "{}",
                key.secret
            );
            assert_eq!(
                store
                    .grant(&tenant, &Principal::Key(hash))
                    .await
                    .expect("grant"),
                Some(Grant {
                    tenant,
                    principal: Principal::Key(hash),
                    role: role.to_owned(),
                    allowed_labels: labels(key.labels),
                    granted_by: None,
                    granted_at: key.created_at,
                }),
                "{}",
                key.secret
            );
        }
        let acme: Vec<Digest> = store
            .list_keys(&tenant_id("acme"))
            .await
            .expect("list_keys")
            .into_iter()
            .map(|key| key.hash)
            .collect();
        assert_eq!(
            acme,
            [
                Digest::of(b"rewind_sk_admin"),
                Digest::of(b"rewind_sk_agent")
            ]
        );
    }

    /// A run as version 2 kept it: before namespaces.
    async fn run_of_version_2(
        connection: &turso::Connection,
        n: u128,
        tenant: &str,
        script: &[u8],
        key: Option<&str>,
        created_at: Timestamp,
    ) {
        sql::execute(
            connection,
            "INSERT INTO runs (id, tenant, script, toolset, status, epoch, history_len, \
             expires_at, expiry_enqueued, failures, attempts, fence, labels, idempotency_key, \
             created_at, updated_at) \
             VALUES (?, ?, ?, ?, 'completed', 1, 0, ?, 0, 0, 0, 0, '{}', ?, ?, ?)",
            vec![
                text(rewind_core::RunId::from_u128(n).to_string()),
                text(tenant),
                digest(Digest::of(script)),
                digest(Digest::of(b"toolset")),
                ts(at(86_400 * 30)),
                crate::sql::opt_text(key),
                ts(created_at),
                ts(created_at),
            ],
        )
        .await
        .expect("a run of version 2");
    }

    #[tokio::test]
    async fn runs_of_version_2_belong_to_the_namespace_migrated() {
        use rewind_engine::ports::{
            CreateRun, Day, JournalStore, Month, NamespaceName, NewRun, RunMetadata, RunRef,
            UsageOf,
        };

        let dir = tempfile::TempDir::new().expect("temporary directory");
        let path = dir.path().join("journal.db");
        {
            let database = turso::Builder::new_local(path.to_str().expect("UTF-8 path"))
                .build()
                .await
                .expect("open the file");
            let connection = database.connect().expect("connect");
            migrate_to(&connection, 2).await.expect("version 2");
            run_of_version_2(&connection, 1, "acme", b"a", Some("k"), T0).await;
            run_of_version_2(&connection, 2, "acme", b"a", None, at(1)).await;
            run_of_version_2(&connection, 3, "acme", b"b", None, at(86_400)).await;
            run_of_version_2(&connection, 4, "beta", b"a", None, at(2)).await;
        }

        let store = TursoStore::open(&path, MasterKey::from_bytes([1; 32]))
            .await
            .expect("migrate to the current version");
        let acme = tenant_id("acme");
        let migrated = NamespaceName::migrated();
        let run = RunRef::new(acme.clone(), rewind_core::RunId::from_u128(1));
        assert_eq!(
            store
                .get_run(&run)
                .await
                .expect("get_run")
                .expect("run")
                .namespace,
            migrated
        );
        let namespace = store
            .get_namespace(&acme, &migrated)
            .await
            .expect("get_namespace")
            .expect("the namespace of the runs before namespaces");
        assert_eq!(
            (namespace.created_at, namespace.last_active_at),
            (T0, at(86_400))
        );
        assert_eq!(
            namespace.display_name, None,
            "version 8 adds no display name"
        );
        assert!(
            store
                .namespace_has_script(&acme, &migrated, Digest::of(b"b"))
                .await
                .expect("namespace_has_script")
        );
        let month = Month::of(T0);
        let usage = store
            .namespace_usage(&acme, &UsageOf::Namespace(migrated.clone()), month)
            .await
            .expect("namespace_usage");
        assert_eq!((usage.runs_started, usage.scripts), (3, 2));
        assert_eq!(
            usage
                .days
                .iter()
                .map(|(day, used)| (*day, used.runs_started))
                .collect::<Vec<_>>(),
            [(Day::of(T0), 2), (Day::of(at(86_400)), 1)]
        );
        let tenant = store
            .namespace_usage(&acme, &UsageOf::Tenant, month)
            .await
            .expect("namespace_usage");
        assert_eq!(tenant.runs_started, 3);

        // Idempotency keys are unique per namespace now: `k` is free in another one.
        let started = rewind_core::RunStarted {
            script: Digest::of(b"a"),
            toolset: Digest::of(b"toolset"),
            input: serde_json::json!(null).into(),
            seed: rewind_core::Seed::from_bytes([0; 32]),
            epoch: rewind_core::Epoch(1),
            expires_at: at(86_400 * 30),
            mode: rewind_core::RunMode::Live,
        };
        let new_run = |n: u128, namespace: NamespaceName| NewRun {
            run: RunRef::new(acme.clone(), rewind_core::RunId::from_u128(n)),
            namespace,
            origin: "script".into(),
            started: started.clone(),
            metadata: RunMetadata::default(),
            created_at: at(3),
        };
        let again = store
            .create_run(new_run(5, migrated.clone()), Some("k".into()))
            .await
            .expect("create_run");
        assert!(matches!(again, CreateRun::Created(ref c) if !c.created && c.run == run));
        let other = store
            .create_run(
                new_run(6, NamespaceName::new("alice").expect("valid")),
                Some("k".into()),
            )
            .await
            .expect("create_run");
        assert!(matches!(other, CreateRun::Created(ref c) if c.created));
    }

    #[tokio::test]
    async fn a_database_of_version_2_is_migrated_with_its_runs() {
        let dir = tempfile::TempDir::new().expect("temporary directory");
        let path = dir.path().join("journal.db");
        let run = RunRef::new(TenantId::local(), RunId::from_u128(1));
        {
            // The database as a build of version 2 left it, with a run.
            let database = turso::Builder::new_local(path.to_str().expect("UTF-8 path"))
                .build()
                .await
                .expect("create the file");
            let connection = database.connect().expect("connect");
            migrate_to(&connection, 2).await.expect("version 2");
            sql::execute(
                &connection,
                "INSERT INTO runs (id, tenant, script, toolset, status, epoch, history_len, \
                 expires_at, expiry_enqueued, failures, attempts, fence, labels, created_at, \
                 updated_at) VALUES (?, ?, ?, ?, 'pending', 1, 0, 2, 0, 0, 0, 0, \
                 '{\"agent\":\"triage\"}', 1, 1)",
                vec![
                    sql::run_id(run.id),
                    sql::tenant(&run.tenant),
                    sql::digest(Digest::of(b"script")),
                    sql::digest(Digest::of(b"toolset")),
                ],
            )
            .await
            .expect("a run");
        }

        let store = TursoStore::open(&path, MasterKey::from_bytes([1; 32]))
            .await
            .expect("open and migrate");
        assert_eq!(store.schema_version().await, Ok(SCHEMA_VERSION));
        let record = store
            .get_run(&run)
            .await
            .expect("get_run")
            .expect("the run is kept");
        assert_eq!(
            record.metadata,
            RunMetadata {
                labels: BTreeMap::from([("agent".to_owned(), "triage".to_owned())]),
                ..RunMetadata::default()
            },
            "no workflow, no starter, an empty scope"
        );
        assert_eq!(
            store
                .watch_run(&run, RunId::from_u128(2), Seq(1))
                .await
                .expect("watch_run"),
            Some(Watched { ended: false })
        );
    }

    #[tokio::test]
    async fn the_credentials_of_version_8_are_dropped() {
        use rewind_core::SecretName;
        use rewind_engine::ports::{
            Day, NewSecret, Secret, SecretId, SecretNamespace, SecretStore,
        };

        let dir = tempfile::TempDir::new().expect("temporary directory");
        let path = dir.path().join("journal.db");
        let tenant = TenantId::local();
        {
            // The database as a build of version 8 left it, with a credential and its data key.
            let database = turso::Builder::new_local(path.to_str().expect("UTF-8 path"))
                .build()
                .await
                .expect("create the file");
            let connection = database.connect().expect("connect");
            migrate_to(&connection, 8).await.expect("version 8");
            sql::execute(
                &connection,
                "INSERT INTO data_keys (tenant, wrapped_key) VALUES (?, ?)",
                vec![sql::tenant(&tenant), turso::Value::Blob(vec![0; 40])],
            )
            .await
            .expect("a data key");
            sql::execute(
                &connection,
                "INSERT INTO secrets (tenant, owner, ciphertext) VALUES (?, 'toolset:ab', ?)",
                vec![sql::tenant(&tenant), turso::Value::Blob(vec![0; 40])],
            )
            .await
            .expect("a credential");
        }

        let key = MasterKey::from_bytes([1; 32]);
        let store = TursoStore::open(&path, key.clone())
            .await
            .expect("open and migrate");
        assert_eq!(store.schema_version().await, Ok(SCHEMA_VERSION));
        assert_eq!(store.master_keys_in_use().await, Ok(Vec::new()));
        assert_eq!(
            store
                .list(
                    &tenant,
                    &SecretNamespace::Organization,
                    &BTreeMap::new(),
                    Day(0)
                )
                .await,
            Ok(Vec::new())
        );
        let secret = NewSecret {
            id: SecretId::organization(SecretName::new("github").expect("valid")),
            value: Secret::new("v"),
            labels: BTreeMap::new(),
            by: "user:ada".into(),
        };
        store.put(&tenant, &secret, T0).await.expect("put");
        assert_eq!(store.master_keys_in_use().await, Ok(vec![key.id()]));
    }

    #[tokio::test]
    async fn a_database_of_version_3_is_migrated_with_its_summaries() {
        let dir = tempfile::TempDir::new().expect("temporary directory");
        let path = dir.path().join("journal.db");
        let tenant = TenantId::local();
        let (starter, started) = (RunId::from_u128(1), RunId::from_u128(2));
        {
            // The database as a build of version 3 left it, with a run projected.
            let database = turso::Builder::new_local(path.to_str().expect("UTF-8 path"))
                .build()
                .await
                .expect("create the file");
            let connection = database.connect().expect("connect");
            migrate_to(&connection, 3).await.expect("version 3");
            sql::execute(
                &connection,
                "INSERT INTO run_summaries (run_id, tenant, script, headline, toolset, status, \
                 labels, last_activity_at, tool_calls, errors, history_len, snapshot_len) \
                 VALUES (?, ?, ?, 'a run', ?, 'active', '{}', 5, 0, 0, 1, 1)",
                vec![
                    sql::run_id(starter),
                    sql::tenant(&tenant),
                    sql::digest(Digest::of(b"script")),
                    sql::digest(Digest::of(b"toolset")),
                ],
            )
            .await
            .expect("a summary");
        }

        let store = TursoStore::open(&path, MasterKey::from_bytes([1; 32]))
            .await
            .expect("open and migrate");
        assert_eq!(store.schema_version().await, Ok(SCHEMA_VERSION));
        let kept = store
            .search_runs(&RunQuery::new(tenant.clone()), RunOrder::Started, None, 10)
            .await
            .expect("search_runs");
        assert_eq!(
            kept.iter()
                .map(|summary| (
                    summary.run.id,
                    summary.workflow.as_deref(),
                    summary.started_by
                ))
                .collect::<Vec<_>>(),
            [(starter, None, None)],
            "the summary is kept, of no workflow, started by no run"
        );

        // A run of a workflow, started by the migrated one.
        store
            .apply(
                &ProjectedRun {
                    run: RunRef::new(tenant.clone(), started),
                    namespace: rewind_engine::ports::NamespaceName::migrated(),
                    origin: "script".into(),
                    suffix_len: 12,
                    script: Digest::of(b"script"),
                    headline: "a run".into(),
                    toolset: Digest::of(b"toolset"),
                    labels: BTreeMap::new(),
                    parent: None,
                    workflow: Some("triage".into()),
                    started_by: Some(starter),
                },
                &[],
                &RunSnapshot {
                    history_len: 0,
                    status: RunStatus::Active,
                    mode: None,
                    waits: Vec::new(),
                    ending: None,
                },
            )
            .await
            .expect("apply");
        let found = |query: RunQuery| {
            let store = &store;
            async move {
                let runs = store
                    .search_runs(&query, RunOrder::Started, None, 10)
                    .await
                    .expect("search_runs");
                runs.iter()
                    .map(|summary| summary.run.id)
                    .collect::<Vec<_>>()
            }
        };
        let any_workflow = || Predicate::Workflow(vec![TextMatch::Any]);
        assert_eq!(
            found(RunQuery::new(tenant.clone()).with(any_workflow())).await,
            [started]
        );
        assert_eq!(
            found(RunQuery::new(tenant.clone()).without(any_workflow())).await,
            [starter]
        );
        assert_eq!(
            found(RunQuery::new(tenant.clone()).with(Predicate::StartedBy(starter))).await,
            [started]
        );
    }

    /// A run as version 10 kept it: before handles.
    async fn run_of_version_10(
        connection: &turso::Connection,
        id: RunId,
        tenant: &str,
        workflow: Option<&str>,
    ) {
        sql::execute(
            connection,
            "INSERT INTO runs (id, tenant, namespace, script, toolset, status, epoch, history_len, \
             expires_at, expiry_enqueued, failures, attempts, fence, labels, created_at, \
             updated_at, workflow, workflow_version) \
             VALUES (?, ?, 'migrated', ?, ?, 'completed', 1, 0, ?, 0, 0, 0, 0, '{}', ?, ?, ?, ?)",
            vec![
                sql::run_id(id),
                text(tenant),
                digest(Digest::of(b"script")),
                digest(Digest::of(b"toolset")),
                ts(at(86_400 * 30)),
                ts(T0),
                ts(T0),
                opt_text(workflow),
                workflow.map_or(turso::Value::Null, |name| {
                    digest(Digest::of(name.as_bytes()))
                }),
            ],
        )
        .await
        .expect("a run of version 10");
    }

    /// A run's summary as version 10 kept it.
    async fn summary_of_version_10(
        connection: &turso::Connection,
        id: RunId,
        tenant: &str,
        workflow: Option<&str>,
    ) {
        sql::execute(
            connection,
            "INSERT INTO run_summaries (run_id, tenant, script, headline, toolset, status, \
             labels, last_activity_at, tool_calls, errors, history_len, snapshot_len, workflow) \
             VALUES (?, ?, ?, 'a run', ?, 'completed', '{}', 5, 0, 0, 1, 1, ?)",
            vec![
                sql::run_id(id),
                text(tenant),
                digest(Digest::of(b"script")),
                digest(Digest::of(b"toolset")),
                opt_text(workflow),
            ],
        )
        .await
        .expect("a summary of version 10");
    }

    /// A workflow version as version 10 kept it: without its number.
    async fn version_of_version_10(
        connection: &turso::Connection,
        tenant: &str,
        workflow: &rewind_core::Workflow,
        created_at: Timestamp,
        stored_order: u64,
    ) {
        sql::execute(
            connection,
            "INSERT INTO workflows (tenant, version, name, document, labels, created_at, \
             stored_order) VALUES (?, ?, ?, ?, ?, ?, ?)",
            vec![
                text(tenant),
                digest(workflow.version()),
                text(workflow.name()),
                text(rewind_format::encode_workflow(workflow)),
                text(encode_labels(workflow.labels())),
                ts(created_at),
                sql::int(stored_order),
            ],
        )
        .await
        .expect("a workflow version of version 10");
    }

    fn workflow(name: &str, script: &str) -> rewind_core::Workflow {
        rewind_core::Workflow::new(
            name,
            None,
            Digest::of(script.as_bytes()),
            Digest::of(b"toolset"),
            rewind_core::WorkflowSchemas {
                input: serde_json::json!({ "type": "object" }),
                output: serde_json::json!({}),
                signals: BTreeMap::new(),
            },
        )
        .expect("a valid workflow")
    }

    #[tokio::test]
    async fn runs_and_versions_of_version_10_get_handles_and_numbers() {
        use rewind_core::{RunHandle, SUFFIX_LEN, run_suffix, suffix_range};

        let ms = 1_795_383_896_000;
        // Two runs of `acme` whose first 64 bits are the same, a third apart from them, and a run
        // of `beta` with the same first bits as the pair.
        let (triage, plain) = (
            RunId::from_v7_parts(ms, 0x0c5, 1),
            RunId::from_v7_parts(ms, 0x0c5, 2),
        );
        let apart = RunId::from_v7_parts(ms + 1, 0x0c5, 1);
        let theirs = RunId::from_v7_parts(ms, 0x0c5, 3);
        assert_eq!(
            run_suffix(triage, SUFFIX_LEN),
            run_suffix(plain, SUFFIX_LEN),
            "the short suffix does not tell the pair apart"
        );
        // Two summaries of runs the journal no longer holds, with the same first 64 bits.
        let (gone, gone_twin) = (
            RunId::from_v7_parts(ms + 2, 7, 1),
            RunId::from_v7_parts(ms + 2, 7, 2),
        );
        // Versions created out of the order they were stored in, two of them at the same time.
        let (late, early, tied) = (
            workflow("triage", "v1"),
            workflow("triage", "v2"),
            workflow("triage", "v3"),
        );
        let dir = tempfile::TempDir::new().expect("temporary directory");
        let path = dir.path().join("journal.db");
        {
            // The database as a build of version 10 left it.
            let database = turso::Builder::new_local(path.to_str().expect("UTF-8 path"))
                .build()
                .await
                .expect("create the file");
            let connection = database.connect().expect("connect");
            migrate_to(&connection, 10).await.expect("version 10");
            run_of_version_10(&connection, triage, "acme", Some("triage")).await;
            run_of_version_10(&connection, plain, "acme", None).await;
            run_of_version_10(&connection, apart, "acme", None).await;
            run_of_version_10(&connection, theirs, "beta", None).await;
            summary_of_version_10(&connection, triage, "acme", Some("triage")).await;
            summary_of_version_10(&connection, apart, "acme", None).await;
            summary_of_version_10(&connection, gone, "acme", Some("digest")).await;
            summary_of_version_10(&connection, gone_twin, "acme", None).await;
            version_of_version_10(&connection, "acme", &late, at(5), 1).await;
            version_of_version_10(&connection, "acme", &early, at(2), 2).await;
            version_of_version_10(&connection, "acme", &tied, at(5), 3).await;
            version_of_version_10(&connection, "acme", &workflow("digest", "d1"), at(1), 1).await;
            version_of_version_10(&connection, "beta", &workflow("triage", "x"), at(1), 1).await;
        }

        let store = TursoStore::open(&path, MasterKey::from_bytes([1; 32]))
            .await
            .expect("open and migrate");
        assert_eq!(store.schema_version().await, Ok(SCHEMA_VERSION));
        let (acme, beta) = (tenant_id("acme"), tenant_id("beta"));
        let record = |tenant: &TenantId, id: RunId| {
            let (store, run) = (&store, RunRef::new(tenant.clone(), id));
            async move {
                store
                    .get_run(&run)
                    .await
                    .expect("get_run")
                    .expect("the run is kept")
            }
        };
        let handle =
            |record: &rewind_engine::ports::RunRecord| (record.origin.clone(), record.suffix_len);
        let (first, second) = (record(&acme, triage).await, record(&acme, plain).await);
        assert_eq!(
            handle(&first),
            ("triage".to_owned(), 25),
            "the workflow's name"
        );
        assert_eq!(handle(&second), ("script".to_owned(), 25));
        assert_eq!(
            handle(&record(&acme, apart).await),
            ("script".to_owned(), 12)
        );
        assert_eq!(
            handle(&record(&beta, theirs).await),
            ("script".to_owned(), 12),
            "another tenant's run does not lengthen a suffix"
        );
        // Each of the pair's full suffixes names it alone.
        for record in [&first, &second] {
            let RunHandle { suffix, .. } = record.handle();
            let (low, high) = suffix_range(&suffix).expect("a range");
            let named: Vec<RunId> = store
                .runs_in_range(&acme, low, high, 10)
                .await
                .expect("runs_in_range")
                .iter()
                .map(|named| named.run.id)
                .collect();
            assert_eq!(named, [record.run.id]);
        }

        let summaries: BTreeMap<RunId, (String, u8)> = store
            .search_runs(&RunQuery::new(acme.clone()), RunOrder::Started, None, 10)
            .await
            .expect("search_runs")
            .into_iter()
            .map(|summary| (summary.run.id, (summary.origin, summary.suffix_len)))
            .collect();
        assert_eq!(
            summaries,
            BTreeMap::from([
                (triage, ("triage".to_owned(), 25)),
                (apart, ("script".to_owned(), 12)),
                (gone, ("digest".to_owned(), 25)),
                (gone_twin, ("script".to_owned(), 25)),
            ]),
            "a summary takes its run's, or follows the rules among the summaries"
        );

        let none = BTreeMap::new();
        let numbered: Vec<(Digest, u32)> = store
            .workflow_versions(&acme, "triage", &none)
            .await
            .expect("workflow_versions")
            .iter()
            .map(|stored| (stored.workflow.version(), stored.number))
            .collect();
        assert_eq!(
            numbered,
            [
                (tied.version(), 3),
                (late.version(), 2),
                (early.version(), 1)
            ],
            "numbered by creation, ties in the order they were stored"
        );
        let number = |tenant: &TenantId, name: &str| {
            let (store, tenant, name) = (&store, tenant.clone(), name.to_owned());
            async move {
                store
                    .workflow_number(&tenant, &name, 1)
                    .await
                    .expect("workflow_number")
                    .map(|stored| stored.workflow.name().to_owned())
            }
        };
        assert_eq!(number(&acme, "digest").await.as_deref(), Some("digest"));
        assert_eq!(number(&beta, "triage").await.as_deref(), Some("triage"));
        let next = store
            .put_workflow(&acme, &workflow("triage", "v4"), at(6))
            .await
            .expect("put_workflow");
        assert_eq!(next.number, 4, "a new version takes the next number");
        let again = store
            .put_workflow(&acme, &late, at(7))
            .await
            .expect("put_workflow");
        assert_eq!(again.number, 2, "a version put again keeps its number");
    }
}
