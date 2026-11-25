//! The schema of the embedded control plane's database, versioned: migrations are embedded in
//! the binary, applied in order when the database is opened, each in its own transaction with the
//! new version. A database whose version is above [`SCHEMA_VERSION`] was written by a newer
//! build, and is refused rather than misread. A migration is never edited once released: a
//! change is a new migration (version 2 moved access to grants and roles; version 3 added the
//! workflows; version 5 the namespaces; version 6 the tests and the suites of workflow versions;
//! version 7 people's connections and the OAuth clients and codes; version 8 the namespaces'
//! display names; version 9 named secrets, their reads, the signing keys and the master key of
//! each data key; version 11 the runs' origins and handle suffixes, the workflows' titles and the
//! numbers of workflow versions; version 12 each namespace's partition, the partition map, and
//! `legacy_runs`; version 13 set the tables of runs those versions also hold aside, as
//! `legacy_*`, which `rewind server migrate-runs` reads and drops (`archi/08-decisions.md`,
//! "Taken while writing the object-storage spec"; nothing else reads them); version 14 moved
//! the saved workflows and the titles of codes to the workflow model: workflows with an id and a
//! scope, their versions, the codes each scope has seen and their suites).
//!
//! The tables of the control plane are those of `archi/06-storage-and-scale.md`, with these
//! differences:
//!
//! - every key starts with the tenant;
//! - `workflows.namespace` is `*` for the organization's workflows; `workflows.latest` is the
//!   number of a workflow's latest version, which `workflow_versions` holds too;
//! - `workflow_versions.code` is the code of the version's document, which lists and lookups by
//!   code read; `workflow_codes.added_order` numbers the rows of a tenant in the order they were
//!   written, which tells the workflow that first had a code in a scope; `workflow_tests.reason`
//!   is why a test left its suite, its retirement's or its replacement's;
//! - `code_titles` holds the titles people gave codes before the workflow model, until
//!   `rewind server rename-data` turns them into names (`CatalogMigration`);
//! - `data_keys` holds each tenant's data key, encrypted by a master key whose id `master_key`
//!   records, apart from `tenants`: a tenant without settings (the embedded `local`) still has
//!   secrets;
//! - `signing_keys.public_key` holds the 32 bytes of an Ed25519 public key, which the API writes
//!   as a JWK;
//! - `roles.permissions` holds a role's permissions as a JSON array of strings, sorted, and
//!   `grants.principal` and `grants.granted_by` a principal's text (`member:…`, `key:…`,
//!   `invitation:…`), whose order is the order of principals;
//! - a tenant's ceiling and its namespaces' defaults, and a namespace's exceptions, are a column
//!   per quota (`*_ms` for the durations; the ceiling's `active_runs` is `active_run_quota`); the
//!   permissions of `tokens` and `token_keys` are JSON arrays, as a role's;
//! - an OAuth client's `redirect_uris` is a JSON array of strings, a code's `organizations` a
//!   JSON object from tenant to access (`read`, `run`) and its `labels` a JSON object; a
//!   connection's `expires_at` is when its latest refresh token lapses, which its purge reads.

use rewind_core::WorkflowId;
use turso::Connection;

use crate::Error;
use crate::sql::{self, Columns, opt_text, text};

/// The version of the schema this build writes.
pub const SCHEMA_VERSION: u32 = 15;

/// The `meta` key of the schema version.
const VERSION_KEY: &str = "schema_version";

/// Migration `n` brings the schema from version `n` to `n + 1`.
const MIGRATIONS: [&str; SCHEMA_VERSION as usize] = [
    V1, V2, V3, V4, V5, V6, V7, V8, V9, V10, V11, V12, V13, V14, V15,
];

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

/// The run summaries by workflow and by the run that started them
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
/// rows), their alerts and the runs kept per code; end users' tokens and the keys that sign
/// them.
///
/// The runs that existed before namespaces belong to `migrated`, which is created in each tenant
/// that has runs, with its code counts and the runs started per day, its own and its tenant's.
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

/// Named secrets that runs use, and each tenant's signing keys (`archi/04-tools.md`, "Secrets"
/// and "Signed calls"). The credentials per action set, per namespace and action set, and per run
/// are dropped, with the data keys that encrypted them: nothing is encrypted under those any more,
/// and a tenant's next secret or signing key makes a data key under the current master key, whose
/// id `data_keys.master_key` records. An organization's secrets are kept under the namespace `*`.
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

/// Run handles, code titles and the numbers of workflow versions
/// (`archi/06-storage-and-scale.md`, "Run handles"; `archi/04-tools.md`, "Script titles"). A run
/// keeps its origin, the middle of its handle, and the characters of its handle's suffix. A run
/// stored before handles takes its workflow's name, or `script`, and the short suffix, unless
/// another run of its tenant shares the first 64 bits of its id (the first 18 characters of its
/// text, `xxxxxxxx-xxxx-xxxx`): every run of such a group then takes the full suffix, which tells
/// distinct ids apart. A summary takes its run's; a summary whose run the history no longer holds
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

/// What the control plane keeps of the run plane (`archi/06-storage-and-scale.md`, "The control
/// plane"): the partition of each namespace, chosen when the catalog creates it (null for a
/// namespace created before the column, which the catalog places at its next use or with
/// `TursoCatalog::place_namespaces`); the partition map, each partition's owner and the generation of
/// its last move (the column `epoch`); the runs `migrate-runs` moved into a partition. The members of an embedded
/// cluster live in its process: no table holds them.
const V12: &str = r#"
ALTER TABLE namespaces ADD COLUMN partition INTEGER;

CREATE TABLE partitions (
    id INTEGER PRIMARY KEY,
    pool TEXT,
    node TEXT,
    epoch INTEGER NOT NULL
) STRICT;

CREATE TABLE legacy_runs (
    id TEXT PRIMARY KEY,
    tenant TEXT NOT NULL,
    partition INTEGER NOT NULL
) STRICT;
"#;

/// The tables of runs set aside (`archi/08-decisions.md`, "Taken while writing the object-storage
/// spec": the migration): runs live in the partition of the run plane and in the archive, and the
/// database holds the control plane only. The tables the old tables kept its runs in take the
/// prefix `legacy_`, their indexes with them (`runs` becomes `legacy_journal_runs`: `legacy_runs`
/// is the control plane's table of the runs moved): the runs, their histories, inboxes,
/// signal keys, timers, call tasks, signal URL tokens and watchers, the payloads and their
/// references, the feed, the read model's tables, and the namespaces' use per day, quota alerts
/// and code counts. `rewind server migrate-runs` reads them, moves each run into the partition
/// or the archive, and drops them once every run moved.
///
/// A tenant keeps one retention, how long an ended run is kept from its end: `retention_ms`,
/// which was the retention of archived histories; the other three go.
const V13: &str = r#"
ALTER TABLE runs RENAME TO legacy_journal_runs;
ALTER TABLE history RENAME TO legacy_history;
ALTER TABLE inbox RENAME TO legacy_inbox;
ALTER TABLE signal_keys RENAME TO legacy_signal_keys;
ALTER TABLE timers RENAME TO legacy_timers;
ALTER TABLE tool_tasks RENAME TO legacy_tool_tasks;
ALTER TABLE webhook_tokens RENAME TO legacy_webhook_tokens;
ALTER TABLE run_watchers RENAME TO legacy_run_watchers;
ALTER TABLE payloads RENAME TO legacy_payloads;
ALTER TABLE payload_refs RENAME TO legacy_payload_refs;
ALTER TABLE feed RENAME TO legacy_feed;
ALTER TABLE run_summaries RENAME TO legacy_run_summaries;
ALTER TABLE run_labels RENAME TO legacy_run_labels;
ALTER TABLE run_waits RENAME TO legacy_run_waits;
ALTER TABLE run_texts RENAME TO legacy_run_texts;
ALTER TABLE run_quarantine RENAME TO legacy_run_quarantine;
ALTER TABLE tool_calls RENAME TO legacy_tool_calls;
ALTER TABLE usage RENAME TO legacy_usage;
ALTER TABLE quota_alerts RENAME TO legacy_quota_alerts;
ALTER TABLE namespace_scripts RENAME TO legacy_namespace_scripts;

ALTER TABLE tenants DROP COLUMN journal_retention_ms;
ALTER TABLE tenants DROP COLUMN read_model_retention_ms;
ALTER TABLE tenants DROP COLUMN payload_grace_ms;
ALTER TABLE tenants RENAME COLUMN archive_retention_ms TO retention_ms;
"#;

/// The workflow model (`archi/06-storage-and-scale.md`, "The control plane" and "Rewriting the
/// stored names": workflows). A workflow has an id, a scope (a namespace, or the organization:
/// `*`), a name unique in its scope when it has one, its versions numbered from 1, the codes its
/// scope has seen under each set of labels (`workflow_codes`, in the order they were written) and
/// its suite: every test it holds or held, with when and why it left (`workflow_tests`).
///
/// What the saved workflows of version 13 become (`rewind server rename-data` does the rest, from
/// the runs):
///
/// - each saved workflow, by its tenant and name, is an organization's workflow whose id is
///   [`WorkflowId::of_saved`], computed in Rust before these statements (`saved_workflow_ids`:
///   SQL has no SHA-256); its description is its latest version's document's, its labels and
///   creation its first version's, its creator `rename-data`;
/// - its versions keep their numbers, digests, documents (in the names they were saved in, which
///   `rename-data` rewrites), labels and creation; a version's code is its document's `code`, or
///   `script` in a document saved before that rename;
/// - each code of a version, under the version's labels, belongs in the organization's
///   `workflow_codes` to the first workflow created among those that have it there, from that
///   workflow's first version with it;
/// - a workflow's suite is its latest version's tests, each from the first version of the run of
///   versions up to the latest that lists it; a test an older version listed and the latest does
///   not left with the first later version that no longer lists it, for the reason that version
///   gives its name, if any. Only tests that have a row are kept.
///
/// The titles people gave codes wait in `code_titles` for `rename-data`, without who set them
/// and when. Every stored permission set (roles', tokens', token keys') holding `scripts:write`
/// holds `workflows:write` instead, and one holding `toolsets:write` or `actions:write` holds
/// `workflows:write` too (`archi/06-storage-and-scale.md`, "Rewriting the stored names").
const V14: &str = r#"
ALTER TABLE workflows RENAME TO saved_versions;
ALTER TABLE workflow_tests RENAME TO saved_version_tests;

CREATE TABLE workflows (
    tenant TEXT NOT NULL,
    id TEXT NOT NULL,
    namespace TEXT NOT NULL,
    name TEXT,
    description TEXT,
    labels TEXT NOT NULL,
    source_workflow TEXT,
    source_version INTEGER,
    created_by TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    promoted_at INTEGER,
    latest INTEGER NOT NULL,
    PRIMARY KEY (tenant, id)
) STRICT;

CREATE TABLE workflow_versions (
    tenant TEXT NOT NULL,
    workflow TEXT NOT NULL,
    version INTEGER NOT NULL,
    digest TEXT NOT NULL,
    code TEXT NOT NULL,
    document TEXT NOT NULL,
    labels TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (tenant, workflow, version)
) STRICT;

CREATE TABLE workflow_codes (
    tenant TEXT NOT NULL,
    namespace TEXT NOT NULL,
    labels TEXT NOT NULL,
    code TEXT NOT NULL,
    workflow TEXT NOT NULL,
    added_at INTEGER NOT NULL,
    added_order INTEGER NOT NULL,
    PRIMARY KEY (tenant, namespace, labels, code)
) STRICT;

CREATE TABLE workflow_tests (
    tenant TEXT NOT NULL,
    workflow TEXT NOT NULL,
    name TEXT NOT NULL,
    digest TEXT NOT NULL,
    added_at INTEGER NOT NULL,
    retired_at INTEGER,
    reason TEXT,
    PRIMARY KEY (tenant, workflow, name, digest)
) STRICT;

-- The versions, numbered as they were, under the id of their workflow.
INSERT INTO workflow_versions (tenant, workflow, version, digest, code, document, labels,
                               created_at)
SELECT v.tenant, ids.id, v.number, v.version,
       COALESCE(json_extract(v.document, '$.code'), json_extract(v.document, '$.script')),
       v.document, v.labels, v.created_at
FROM saved_versions v
JOIN saved_workflow_ids ids ON ids.tenant = v.tenant AND ids.name = v.name;

-- The organization's workflows, from their first and latest versions.
INSERT INTO workflows (tenant, id, namespace, name, description, labels, source_workflow,
                       source_version, created_by, created_at, promoted_at, latest)
SELECT ids.tenant, ids.id, '*', ids.name, json_extract(latest.document, '$.description'),
       first.labels, NULL, NULL, 'rename-data', first.created_at, NULL, latest.version
FROM saved_workflow_ids ids
JOIN workflow_versions first ON first.tenant = ids.tenant AND first.workflow = ids.id
    AND first.version = (SELECT MIN(v.version) FROM workflow_versions v
                         WHERE v.tenant = ids.tenant AND v.workflow = ids.id)
JOIN workflow_versions latest ON latest.tenant = ids.tenant AND latest.workflow = ids.id
    AND latest.version = (SELECT MAX(v.version) FROM workflow_versions v
                          WHERE v.tenant = ids.tenant AND v.workflow = ids.id);

-- Each code under each set of labels, to the first workflow created among those that have it.
INSERT INTO workflow_codes (tenant, namespace, labels, code, workflow, added_at, added_order)
SELECT tenant, '*', labels, code, workflow, added_at,
       row_number() OVER (PARTITION BY tenant ORDER BY added_at, labels, code)
FROM (
    SELECT v.tenant, v.labels, v.code, v.workflow, MIN(v.created_at) AS added_at,
           row_number() OVER (PARTITION BY v.tenant, v.labels, v.code
                              ORDER BY w.created_at, w.name) AS rank
    FROM workflow_versions v
    JOIN workflows w ON w.tenant = v.tenant AND w.id = v.workflow
    GROUP BY v.tenant, v.labels, v.code, v.workflow, w.created_at, w.name
)
WHERE rank = 1;

-- The tests each version lists, by name and digest.
CREATE TABLE saved_suite_tests (
    tenant TEXT NOT NULL,
    workflow TEXT NOT NULL,
    version INTEGER NOT NULL,
    name TEXT NOT NULL,
    digest TEXT NOT NULL,
    PRIMARY KEY (tenant, workflow, name, digest, version)
) STRICT;
INSERT INTO saved_suite_tests (tenant, workflow, version, name, digest)
SELECT v.tenant, v.workflow, v.version, json_extract(t.value, '$.name'),
       json_extract(t.value, '$.test')
FROM workflow_versions v, json_each(v.document, '$.tests') AS t;

-- Each test of a suite, from the first version of the last run of versions that list it; one
-- the latest version does not list left with the version after that run.
INSERT INTO workflow_tests (tenant, workflow, name, digest, added_at, retired_at, reason)
SELECT s.tenant, s.workflow, s.name, s.digest, joined.created_at, left_by.created_at,
       COALESCE(
           (SELECT json_extract(r.value, '$.reason')
            FROM json_each(left_by.document, '$.retired') AS r
            WHERE json_extract(r.value, '$.name') = s.name),
           (SELECT json_extract(r.value, '$.reason')
            FROM json_each(left_by.document, '$.replaced') AS r
            WHERE json_extract(r.value, '$.name') = s.name))
FROM (
    SELECT last.tenant, last.workflow, last.name, last.digest, last.version,
           1 + COALESCE((
               SELECT MAX(v.version) FROM workflow_versions v
               WHERE v.tenant = last.tenant AND v.workflow = last.workflow
                 AND v.version < last.version
                 AND NOT EXISTS (
                     SELECT 1 FROM saved_suite_tests x
                     WHERE x.tenant = v.tenant AND x.workflow = v.workflow
                       AND x.version = v.version AND x.name = last.name
                       AND x.digest = last.digest)), 0) AS first
    FROM (
        SELECT tenant, workflow, name, digest, MAX(version) AS version
        FROM saved_suite_tests
        GROUP BY tenant, workflow, name, digest
    ) last
) s
JOIN workflow_versions joined ON joined.tenant = s.tenant AND joined.workflow = s.workflow
    AND joined.version = s.first
LEFT JOIN workflow_versions left_by ON left_by.tenant = s.tenant
    AND left_by.workflow = s.workflow AND left_by.version = s.version + 1
WHERE EXISTS (SELECT 1 FROM tests t WHERE t.tenant = s.tenant AND t.digest = s.digest);

DROP TABLE saved_suite_tests;
DROP TABLE saved_version_tests;
DROP TABLE saved_versions;
DROP TABLE saved_workflow_ids;

CREATE UNIQUE INDEX workflows_by_name ON workflows (tenant, namespace, name)
    WHERE name IS NOT NULL;
CREATE INDEX workflows_by_namespace ON workflows (tenant, namespace, id);
CREATE INDEX workflow_versions_by_digest ON workflow_versions (tenant, digest);
CREATE INDEX workflow_codes_by_code ON workflow_codes (tenant, namespace, code, added_order);
CREATE INDEX workflow_tests_by_test ON workflow_tests (tenant, digest);

ALTER TABLE script_titles RENAME TO code_titles;
ALTER TABLE code_titles DROP COLUMN updated_by;
ALTER TABLE code_titles DROP COLUMN updated_at;

-- workflows:write replaces scripts:write and saving under toolsets:write (actions:write).
UPDATE roles SET permissions = (
    SELECT json_group_array(value) FROM (
        SELECT CASE value WHEN 'scripts:write' THEN 'workflows:write' ELSE value END AS value
        FROM json_each(roles.permissions)
        UNION
        SELECT 'workflows:write' WHERE EXISTS (
            SELECT 1 FROM json_each(roles.permissions)
            WHERE value IN ('toolsets:write', 'actions:write'))
        ORDER BY value))
WHERE EXISTS (SELECT 1 FROM json_each(roles.permissions)
              WHERE value IN ('scripts:write', 'toolsets:write', 'actions:write'));
UPDATE tokens SET permissions = (
    SELECT json_group_array(value) FROM (
        SELECT CASE value WHEN 'scripts:write' THEN 'workflows:write' ELSE value END AS value
        FROM json_each(tokens.permissions)
        UNION
        SELECT 'workflows:write' WHERE EXISTS (
            SELECT 1 FROM json_each(tokens.permissions)
            WHERE value IN ('toolsets:write', 'actions:write'))
        ORDER BY value))
WHERE EXISTS (SELECT 1 FROM json_each(tokens.permissions)
              WHERE value IN ('scripts:write', 'toolsets:write', 'actions:write'));
UPDATE token_keys SET permissions = (
    SELECT json_group_array(value) FROM (
        SELECT CASE value WHEN 'scripts:write' THEN 'workflows:write' ELSE value END AS value
        FROM json_each(token_keys.permissions)
        UNION
        SELECT 'workflows:write' WHERE EXISTS (
            SELECT 1 FROM json_each(token_keys.permissions)
            WHERE value IN ('toolsets:write', 'actions:write'))
        ORDER BY value))
WHERE EXISTS (SELECT 1 FROM json_each(token_keys.permissions)
              WHERE value IN ('scripts:write', 'toolsets:write', 'actions:write'));
"#;

/// The catalog in the words of the rename release (`archi/06-storage-and-scale.md`, "Rewriting the
/// stored names"), as Postgres's migration 17: the tables of the code and the pinned action sets,
/// a partition's `generation`, a subscription's `types` and the new names of its webhook types, the
/// principals `member:` that were `user:`, the permissions renamed. The documents whose digest is
/// their identity are `rename-data`'s.
const V15: &str = r#"
ALTER TABLE scripts RENAME TO code;
ALTER TABLE toolsets RENAME TO action_sets;
ALTER TABLE partitions RENAME COLUMN epoch TO generation;
ALTER TABLE subscriptions RENAME COLUMN events TO types;
UPDATE subscriptions SET types = replace(replace(replace(types,
    '"run.suspended"', '"run.waiting"'),
    '"credentials.expired"', '"secret.expired"'),
    '"tool.schema_changed"', '"action.schema_changed"');
UPDATE grants SET principal = 'member:' || substr(principal, 6) WHERE principal LIKE 'user:%';
UPDATE grants SET granted_by = 'member:' || substr(granted_by, 6) WHERE granted_by LIKE 'user:%';
UPDATE api_keys SET created_by = 'member:' || substr(created_by, 6) WHERE created_by LIKE 'user:%';
UPDATE tokens SET created_by = 'member:' || substr(created_by, 6) WHERE created_by LIKE 'user:%';
UPDATE token_keys SET created_by = 'member:' || substr(created_by, 6) WHERE created_by LIKE 'user:%';
UPDATE secrets SET created_by = 'member:' || substr(created_by, 6) WHERE created_by LIKE 'user:%';
UPDATE secrets SET updated_by = 'member:' || substr(updated_by, 6) WHERE updated_by LIKE 'user:%';
UPDATE workflows SET created_by = 'member:' || substr(created_by, 6) WHERE created_by LIKE 'user:%';
UPDATE roles SET permissions = (
    SELECT json_group_array(value) FROM (
        SELECT DISTINCT CASE value
            WHEN 'toolsets:read' THEN 'actions:read'
            WHEN 'toolsets:write' THEN 'actions:write'
            WHEN 'tasks:work' THEN 'calls:work'
            WHEN 'tasks:approve' THEN 'calls:approve'
            ELSE value END AS value
        FROM json_each(roles.permissions)
        ORDER BY value))
WHERE EXISTS (SELECT 1 FROM json_each(roles.permissions)
              WHERE value IN ('toolsets:read', 'toolsets:write', 'tasks:work', 'tasks:approve'));
UPDATE tokens SET permissions = (
    SELECT json_group_array(value) FROM (
        SELECT DISTINCT CASE value
            WHEN 'toolsets:read' THEN 'actions:read'
            WHEN 'toolsets:write' THEN 'actions:write'
            WHEN 'tasks:work' THEN 'calls:work'
            WHEN 'tasks:approve' THEN 'calls:approve'
            ELSE value END AS value
        FROM json_each(tokens.permissions)
        ORDER BY value))
WHERE EXISTS (SELECT 1 FROM json_each(tokens.permissions)
              WHERE value IN ('toolsets:read', 'toolsets:write', 'tasks:work', 'tasks:approve'));
UPDATE token_keys SET permissions = (
    SELECT json_group_array(value) FROM (
        SELECT DISTINCT CASE value
            WHEN 'toolsets:read' THEN 'actions:read'
            WHEN 'toolsets:write' THEN 'actions:write'
            WHEN 'tasks:work' THEN 'calls:work'
            WHEN 'tasks:approve' THEN 'calls:approve'
            ELSE value END AS value
        FROM json_each(token_keys.permissions)
        ORDER BY value))
WHERE EXISTS (SELECT 1 FROM json_each(token_keys.permissions)
              WHERE value IN ('toolsets:read', 'toolsets:write', 'tasks:work', 'tasks:approve'));
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
        let to = from + 1;
        sql::transaction(connection, async {
            prepare(connection, to).await?;
            connection.execute_batch(migration).await?;
            set_meta(connection, VERSION_KEY, Some(&to.to_string())).await
        })
        .await?;
        tracing::info!(version = %to, "migrated the schema of the embedded store");
    }
    Ok(())
}

/// What a migration needs that SQL cannot compute, written in Rust and run in its transaction
/// before its statements: the ids of the saved workflows, for version 14.
async fn prepare(connection: &Connection, to: usize) -> Result<(), Error> {
    if to == 14 {
        saved_workflow_ids(connection).await?;
    }
    Ok(())
}

/// The id of each workflow saved before the workflow model, by its tenant and name, in
/// `saved_workflow_ids`, which [`V14`] reads and drops: [`WorkflowId::of_saved`] hashes with
/// SHA-256, which SQL does not have.
async fn saved_workflow_ids(connection: &Connection) -> Result<(), Error> {
    sql::execute(
        connection,
        "CREATE TABLE saved_workflow_ids (tenant TEXT NOT NULL, name TEXT NOT NULL, \
         id TEXT NOT NULL, PRIMARY KEY (tenant, name)) STRICT",
        Vec::new(),
    )
    .await?;
    let saved = sql::query(
        connection,
        "SELECT DISTINCT tenant, name FROM workflows ORDER BY tenant, name",
        Vec::new(),
    )
    .await?;
    for row in &saved {
        let (tenant, name) = (row.text(0)?, row.text(1)?);
        let id = WorkflowId::of_saved(&tenant, &name);
        sql::execute(
            connection,
            "INSERT INTO saved_workflow_ids (tenant, name, id) VALUES (?, ?, ?)",
            vec![text(tenant), text(name), text(id.as_str())],
        )
        .await?;
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

    use rewind_core::{Digest, RunId, Timestamp};
    use rewind_engine::ports::{AccessStore, ApiKey, Grant, Principal, TenantId};

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

    /// The versions the migration wrote, as their rows hold them, newest first: their number,
    /// digest, code, whether their document has `schemas`, labels and creation. A moved version
    /// keeps the document it was saved with, in the names of before the rename release, which
    /// the catalog reads once `rewind server rename-data` rewrote it: its row is read here.
    async fn version_rows(
        store: &TursoStore,
        tenant: &str,
        workflow: &rewind_core::WorkflowId,
    ) -> Vec<(
        u32,
        Digest,
        Digest,
        bool,
        BTreeMap<String, String>,
        Timestamp,
    )> {
        let connection = store.connection().await.expect("connection");
        sql::query(
            &connection,
            "SELECT version, digest, code, json_extract(document, '$.schemas') IS NOT NULL, \
             labels, created_at FROM workflow_versions WHERE tenant = ? AND workflow = ? \
             ORDER BY version DESC",
            vec![text(tenant), text(workflow.as_str())],
        )
        .await
        .expect("the versions' rows")
        .iter()
        .map(|row| {
            (
                row.u32(0).expect("version"),
                row.digest(1).expect("digest"),
                row.digest(2).expect("code"),
                row.u32(3).expect("interface") == 1,
                crate::codec::decode_labels(&row.text(4).expect("labels")).expect("labels"),
                row.ts(5).expect("created_at"),
            )
        })
        .collect()
    }

    /// The codes the migration wrote: each code of the organization, its labels and its
    /// workflow, in the order they were written.
    async fn code_rows(store: &TursoStore, tenant: &str) -> Vec<(Digest, String, String)> {
        let connection = store.connection().await.expect("connection");
        sql::query(
            &connection,
            "SELECT code, labels, workflow FROM workflow_codes WHERE tenant = ? \
             AND namespace = '*' ORDER BY added_order",
            vec![text(tenant)],
        )
        .await
        .expect("the codes' rows")
        .iter()
        .map(|row| {
            (
                row.digest(0).expect("code"),
                row.text(1).expect("labels"),
                row.text(2).expect("workflow"),
            )
        })
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
        let path = dir.path().join("control.db");
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

    #[tokio::test]
    async fn the_credentials_of_version_8_are_dropped() {
        use rewind_core::SecretName;
        use rewind_engine::ports::{
            Day, NewSecret, Secret, SecretId, SecretNamespace, SecretStore,
        };

        let dir = tempfile::TempDir::new().expect("temporary directory");
        let path = dir.path().join("control.db");
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
            by: "member:ada".into(),
        };
        store.put(&tenant, &secret, T0).await.expect("put");
        assert_eq!(store.master_keys_in_use().await, Ok(vec![key.id()]));
    }
    /// A run as version 10 kept it: before handles, with the name and digest of the saved
    /// workflow version it ran, if any.
    async fn run_of_version_10(
        connection: &turso::Connection,
        id: RunId,
        tenant: &str,
        workflow: Option<(&str, Digest)>,
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
                digest(Digest::of(b"action_set")),
                ts(at(86_400 * 30)),
                ts(T0),
                ts(T0),
                opt_text(workflow.map(|(name, _)| name)),
                workflow.map_or(turso::Value::Null, |(_, version)| digest(version)),
            ],
        )
        .await
        .expect("a run of version 10");
    }

    /// A workflow version as the builds before the workflow model saved it, in the names of
    /// then: its document's text, with its workflow's name, and its digest, which the text
    /// hashes to.
    fn saved_document(document: &serde_json::Value) -> (String, Digest) {
        let text = rewind_core::canonical_json(document);
        let digest = Digest::of(text.as_bytes());
        (text, digest)
    }

    /// The document of a saved version of `name` whose code is the digest of `code`.
    fn saved(name: &str, code: &str) -> serde_json::Value {
        serde_json::json!({
            "v": 1,
            "name": name,
            "code": Digest::of(code.as_bytes()).to_hex(),
            "action_set": Digest::of(b"action_set").to_hex(),
            "schemas": { "input": { "type": "object" }, "output": {}, "signals": {} },
        })
    }

    /// The labels of a saved document, as its row keeps them.
    fn labels_of(document: &serde_json::Value) -> String {
        let labels: BTreeMap<String, String> = document
            .get("labels")
            .map(|labels| serde_json::from_value(labels.clone()).expect("labels"))
            .unwrap_or_default();
        encode_labels(&labels)
    }

    /// A workflow version as version 10 kept it: without its number.
    async fn version_of_version_10(
        connection: &turso::Connection,
        tenant: &str,
        document: &serde_json::Value,
        created_at: Timestamp,
        stored_order: u64,
    ) -> Digest {
        let (json, version) = saved_document(document);
        sql::execute(
            connection,
            "INSERT INTO workflows (tenant, version, name, document, labels, created_at, \
             stored_order) VALUES (?, ?, ?, ?, ?, ?, ?)",
            vec![
                text(tenant),
                digest(version),
                text(document["name"].as_str().expect("a name")),
                text(json),
                text(labels_of(document)),
                ts(created_at),
                sql::int(stored_order),
            ],
        )
        .await
        .expect("a workflow version of version 10");
        version
    }

    /// The runs and workflow versions of a database of version 10 get handles and numbers
    /// (version 11, as released), and the runs keep them set aside for `migrate-runs`, each with
    /// the workflow version it ran as the workflow model moved it. Runs of a tenant whose ids
    /// share their first 64 bits take the full suffix; a run's origin is its workflow, or
    /// `script`. Versions are numbered by creation, ties in the order they were stored, and a
    /// new one takes the next number.
    #[tokio::test]
    async fn runs_and_versions_of_version_10_get_handles_and_numbers() {
        use rewind_core::{SUFFIX_LEN, WorkflowId, WorkflowVersion, run_suffix};
        use rewind_engine::ports::legacy::LegacyRuns;
        use rewind_engine::ports::{CatalogStore, NewVersion, RunWorkflow};

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
        let dir = tempfile::TempDir::new().expect("temporary directory");
        let path = dir.path().join("control.db");
        let (late, early, tied) = {
            // The database as a build of version 10 left it, with versions created out of the
            // order they were stored in, two of them at the same time.
            let database = turso::Builder::new_local(path.to_str().expect("UTF-8 path"))
                .build()
                .await
                .expect("create the file");
            let connection = database.connect().expect("connect");
            migrate_to(&connection, 10).await.expect("version 10");
            let late =
                version_of_version_10(&connection, "acme", &saved("triage", "v1"), at(5), 1).await;
            let early =
                version_of_version_10(&connection, "acme", &saved("triage", "v2"), at(2), 2).await;
            let tied =
                version_of_version_10(&connection, "acme", &saved("triage", "v3"), at(5), 3).await;
            version_of_version_10(&connection, "acme", &saved("digest", "d1"), at(1), 1).await;
            version_of_version_10(&connection, "beta", &saved("triage", "x"), at(1), 1).await;
            run_of_version_10(&connection, triage, "acme", Some(("triage", late))).await;
            run_of_version_10(&connection, plain, "acme", None).await;
            run_of_version_10(&connection, apart, "acme", None).await;
            let unknown = Some(("triage", Digest::of(b"no such version")));
            run_of_version_10(&connection, theirs, "beta", unknown).await;
            (late, early, tied)
        };

        let store = TursoStore::open(&path, MasterKey::from_bytes([1; 32]))
            .await
            .expect("open and migrate");
        assert_eq!(store.schema_version().await, Ok(SCHEMA_VERSION));
        let (acme, beta) = (tenant_id("acme"), tenant_id("beta"));
        let set_aside: BTreeMap<RunId, (String, String, u8)> = store
            .legacy()
            .runs(None, 10)
            .await
            .expect("the runs set aside")
            .into_iter()
            .map(|run| {
                (
                    run.run.id,
                    (run.run.tenant.to_string(), run.origin, run.suffix_len),
                )
            })
            .collect();
        let handle =
            |tenant: &str, origin: &str, len: u8| (tenant.to_owned(), origin.to_owned(), len);
        assert_eq!(
            set_aside,
            BTreeMap::from([
                (triage, handle("acme", "triage", 25)),
                (plain, handle("acme", "script", 25)),
                (apart, handle("acme", "script", 12)),
                (theirs, handle("beta", "triage", 12)),
            ]),
            "the workflow's name; another tenant's run does not lengthen a suffix"
        );
        let ran: BTreeMap<RunId, Option<RunWorkflow>> = store
            .legacy()
            .runs(None, 10)
            .await
            .expect("the runs set aside")
            .into_iter()
            .map(|run| (run.run.id, run.metadata.workflow))
            .collect();
        let saved_triage = WorkflowId::of_saved("acme", "triage");
        assert_eq!(
            ran[&triage],
            Some(RunWorkflow {
                id: saved_triage.clone(),
                name: Some("triage".into()),
                version: 2,
                digest: late,
                interface: true,
            }),
            "the version it ran, by its digest"
        );
        assert_eq!(ran[&plain], None);
        assert_eq!(ran[&theirs], None, "a digest no version has");

        let catalog = store.catalog();
        let numbered: Vec<(Digest, u32)> = version_rows(&store, "acme", &saved_triage)
            .await
            .into_iter()
            .map(|(version, digest, ..)| (digest, version))
            .collect();
        assert_eq!(
            numbered,
            [(tied, 3), (late, 2), (early, 1)],
            "numbered by creation, ties in the order they were stored"
        );
        let first = |tenant: &TenantId, name: &str| {
            let (store, tenant) = (&store, tenant.clone());
            let id = WorkflowId::of_saved(tenant.as_str(), name);
            async move {
                version_rows(store, tenant.as_str(), &id)
                    .await
                    .into_iter()
                    .find(|(version, ..)| *version == 1)
                    .map(|(_, _, code, ..)| code)
            }
        };
        assert_eq!(first(&acme, "digest").await, Some(Digest::of(b"d1")));
        assert_eq!(first(&beta, "triage").await, Some(Digest::of(b"x")));
        let next = catalog
            .put_version(
                &acme,
                &saved_triage,
                &NewVersion {
                    document: WorkflowVersion::new(Digest::of(b"v4"), Digest::of(b"action_set")),
                    pulled: None,
                },
                at(6),
            )
            .await
            .expect("put_version");
        assert_eq!(next.version, 4, "a new version takes the next number");
    }

    /// A workflow version as version 13 kept it, numbered, with the rows of its suite.
    async fn version_of_version_13(
        connection: &turso::Connection,
        document: &serde_json::Value,
        created_at: Timestamp,
        number: u32,
    ) -> Digest {
        let (json, version) = saved_document(document);
        sql::execute(
            connection,
            "INSERT INTO workflows (tenant, version, name, document, labels, created_at, \
             stored_order, number) VALUES ('acme', ?, ?, ?, ?, ?, ?, ?)",
            vec![
                digest(version),
                text(document["name"].as_str().expect("a name")),
                text(json),
                text(labels_of(document)),
                ts(created_at),
                sql::int(u64::from(number)),
                sql::int(u64::from(number)),
            ],
        )
        .await
        .expect("a workflow version of version 13");
        for test in document["tests"].as_array().into_iter().flatten() {
            sql::execute(
                connection,
                "INSERT INTO workflow_tests (tenant, version, digest) VALUES ('acme', ?, ?)",
                vec![
                    digest(version),
                    text(test["test"].as_str().expect("a test")),
                ],
            )
            .await
            .expect("a test of its suite");
        }
        version
    }

    /// The saved workflows of version 13 move to the workflow model (version 14): each is an
    /// organization's workflow with the id its tenant and name give it, its versions numbered as
    /// they were (a document saved with `script` or `code`), its codes, and its suite as its
    /// versions made it, the tests that left it with when and why. The titles of codes wait for
    /// `rename-data`, and the permissions to save scripts or under toolsets become
    /// `workflows:write`.
    #[tokio::test]
    async fn the_saved_workflows_of_version_13_move_to_the_workflow_model() {
        use rewind_core::{WorkflowId, WorkflowVersion};
        use rewind_engine::ports::{
            CatalogMigration, CatalogStore, CodeTitle, NewVersion, ScopeFilter, StoredWorkflow,
            SuiteMember, WorkflowFilter, WorkflowScope,
        };
        use serde_json::json;

        let test = |name: &str| Digest::of(name.as_bytes());
        let code = |text: &str| Digest::of(text.as_bytes());
        let dev = labels(&[("env", "dev")]);
        let dir = tempfile::TempDir::new().expect("temporary directory");
        let path = dir.path().join("control.db");
        let (gym_v1, triage_v1, triage_v2, triage_v3) = {
            let database = turso::Builder::new_local(path.to_str().expect("UTF-8 path"))
                .build()
                .await
                .expect("create the file");
            let connection = database.connect().expect("connect");
            migrate_to(&connection, 13).await.expect("version 13");
            for name in ["a", "b", "b2", "c"] {
                sql::execute(
                    &connection,
                    "INSERT INTO tests (tenant, digest, namespace, size, reserved_at) \
                     VALUES ('acme', ?, NULL, 10, ?)",
                    vec![digest(test(name)), ts(T0)],
                )
                .await
                .expect("a test");
            }
            // `gym`, created first, has the code of triage's first version.
            let gym_v1 =
                version_of_version_13(&connection, &saved("gym", "triage v1"), T0, 1).await;
            // A document saved before `code` was named so.
            let mut first = saved("triage", "triage v1");
            let fields = first.as_object_mut().expect("an object");
            let script = fields.remove("code").expect("a code");
            fields.insert("script".into(), script);
            fields.insert("description".into(), json!("First"));
            fields.insert(
                "tests".into(),
                json!([
                    { "name": "a", "test": test("a").to_hex() },
                    { "name": "b", "test": test("b").to_hex() },
                ]),
            );
            let triage_v1 = version_of_version_13(&connection, &first, at(1), 1).await;
            let mut second = saved("triage", "triage v2");
            let fields = second.as_object_mut().expect("an object");
            fields.insert("base".into(), json!(triage_v1.to_hex()));
            fields.insert(
                "tests".into(),
                json!([
                    { "name": "b", "test": test("b2").to_hex() },
                    { "name": "c", "test": test("c").to_hex() },
                ]),
            );
            fields.insert(
                "retired".into(),
                json!([{ "name": "a", "test": test("a").to_hex(), "reason": "deprecated" }]),
            );
            fields.insert(
                "replaced".into(),
                json!([{
                    "name": "b",
                    "old": test("b").to_hex(),
                    "new": test("b2").to_hex(),
                    "reason": "out of date",
                }]),
            );
            let triage_v2 = version_of_version_13(&connection, &second, at(2), 2).await;
            // The latest, under labels: `a` back, `c` retired, a test whose row is gone.
            let mut third = saved("triage", "triage v1");
            let fields = third.as_object_mut().expect("an object");
            fields.insert("description".into(), json!("Triage the stale issues"));
            fields.insert("labels".into(), json!({ "env": "dev" }));
            fields.insert("base".into(), json!(triage_v2.to_hex()));
            fields.insert(
                "tests".into(),
                json!([
                    { "name": "a", "test": test("a").to_hex() },
                    { "name": "b", "test": test("b2").to_hex() },
                    { "name": "gone", "test": test("gone").to_hex() },
                ]),
            );
            fields.insert(
                "retired".into(),
                json!([{ "name": "c", "test": test("c").to_hex(), "reason": "flaky" }]),
            );
            let triage_v3 = version_of_version_13(&connection, &third, at(3), 3).await;

            sql::execute(
                &connection,
                "INSERT INTO script_titles (tenant, digest, title, description, updated_by, \
                 updated_at) VALUES ('acme', ?, 'Triage', 'Triage the issues', 'member:ada', ?)",
                vec![digest(code("triage v2")), ts(T0)],
            )
            .await
            .expect("a title");
            for (name, permissions) in [
                ("writer", r#"["runs:read","scripts:write"]"#),
                ("builder", r#"["runs:read","toolsets:write"]"#),
                ("reader", r#"["runs:read"]"#),
            ] {
                sql::execute(
                    &connection,
                    "INSERT INTO roles (tenant, name, description, permissions, created_at, \
                     updated_at) VALUES ('acme', ?, '', ?, ?, ?)",
                    vec![text(name), text(permissions), ts(T0), ts(T0)],
                )
                .await
                .expect("a role");
            }
            sql::execute(
                &connection,
                "INSERT INTO tokens (hash, tenant, namespace, permissions, labels, expires_at, \
                 created_by, created_at) VALUES (?, 'acme', 'alice', \
                 '[\"actions:write\",\"scripts:write\"]', '{}', ?, 'member:ada', ?)",
                vec![digest(Digest::of(b"token")), ts(at(3_600)), ts(T0)],
            )
            .await
            .expect("a token");
            sql::execute(
                &connection,
                "INSERT INTO token_keys (kid, tenant, name, jwk, permissions, created_by, \
                 created_at) VALUES ('k1', 'acme', 'signer', '{}', '[\"scripts:write\"]', \
                 'member:ada', ?)",
                vec![ts(T0)],
            )
            .await
            .expect("a token key");
            (gym_v1, triage_v1, triage_v2, triage_v3)
        };

        let store = TursoStore::open(&path, MasterKey::from_bytes([1; 32]))
            .await
            .expect("open and migrate");
        assert_eq!(store.schema_version().await, Ok(SCHEMA_VERSION));
        let catalog = store.catalog();
        let acme = tenant_id("acme");
        let none = BTreeMap::new();
        let (triage, gym) = (
            WorkflowId::of_saved("acme", "triage"),
            WorkflowId::of_saved("acme", "gym"),
        );
        assert_eq!(triage.as_str(), "wf_e2nzwv04tf3f");
        assert_eq!(
            catalog.workflow(&acme, &triage).await.expect("workflow"),
            Some(StoredWorkflow {
                id: triage.clone(),
                scope: WorkflowScope::Organization,
                name: Some("triage".into()),
                description: Some("Triage the stale issues".into()),
                labels: none.clone(),
                source: None,
                created_by: "rename-data".into(),
                created_at: at(1),
                promoted_at: None,
                latest: 3,
            }),
            "its latest version's description, its first version's labels"
        );
        let gym_workflow = catalog
            .workflow(&acme, &gym)
            .await
            .expect("workflow")
            .expect("gym");
        assert_eq!(
            (
                gym_workflow.description,
                gym_workflow.created_at,
                gym_workflow.latest
            ),
            (None, T0, 1)
        );

        let read = version_rows(&store, "acme", &triage).await;
        assert_eq!(
            read,
            [
                (3, triage_v3, code("triage v1"), true, dev.clone(), at(3)),
                (2, triage_v2, code("triage v2"), true, none.clone(), at(2)),
                (1, triage_v1, code("triage v1"), true, none.clone(), at(1)),
            ],
            "numbered as they were, under their digests; a code saved as `script` reads as one"
        );
        assert_eq!(
            version_rows(&store, "acme", &gym)
                .await
                .into_iter()
                .map(|(version, digest, ..)| (version, digest))
                .collect::<Vec<_>>(),
            [(1, gym_v1)]
        );

        // Each code to the first workflow created among those that have it, under its labels.
        let workflow = |id: &WorkflowId| id.to_string();
        assert_eq!(
            code_rows(&store, "acme").await,
            [
                (code("triage v1"), "{}".to_owned(), workflow(&gym)),
                (code("triage v2"), "{}".to_owned(), workflow(&triage)),
                (
                    code("triage v1"),
                    r#"{"env":"dev"}"#.to_owned(),
                    workflow(&triage)
                ),
            ]
        );
        let mut both = vec![gym.clone(), triage.clone()];
        both.sort();
        assert_eq!(
            catalog
                .list_workflows(&WorkflowFilter {
                    tenant: acme.clone(),
                    scope: ScopeFilter::Organization,
                    code: Some(code("triage v1")),
                    labels: none.clone(),
                    after: None,
                    limit: 10,
                })
                .await
                .expect("list_workflows")
                .into_iter()
                .map(|workflow| workflow.id)
                .collect::<Vec<_>>(),
            both
        );

        // The suite as the versions made it: `a` joined again with the latest, `b` replaced,
        // `c` retired, the test without a row left out.
        let member =
            |name: &str, test: Digest, added: u64, left: Option<(u64, &str)>| SuiteMember {
                name: name.into(),
                digest: test,
                added_at: at(added),
                retired_at: left.map(|(at_second, _)| at(at_second)),
                reason: left.map(|(_, reason)| reason.into()),
            };
        assert_eq!(
            catalog.suite(&acme, &triage).await.expect("suite"),
            [
                member("a", test("a"), 3, None),
                member("b", test("b"), 1, Some((2, "out of date"))),
                member("b", test("b2"), 2, None),
                member("c", test("c"), 2, Some((3, "flaky"))),
            ]
        );
        assert_eq!(catalog.suite(&acme, &gym).await.expect("suite"), []);

        assert_eq!(
            catalog.code_titles().await.expect("code_titles"),
            [CodeTitle {
                tenant: acme.clone(),
                code: code("triage v2"),
                title: "Triage".into(),
                description: Some("Triage the issues".into()),
            }]
        );
        catalog.drop_code_titles().await.expect("drop_code_titles");
        assert_eq!(catalog.code_titles().await.expect("code_titles"), []);

        let permissions = |names: &[&str]| -> std::collections::BTreeSet<String> {
            names.iter().map(|name| (*name).to_owned()).collect()
        };
        let role = |name: &'static str| {
            let (store, acme) = (&store, acme.clone());
            async move {
                store
                    .get_role(&acme, name)
                    .await
                    .expect("get_role")
                    .expect("the role")
                    .permissions
            }
        };
        assert_eq!(
            role("writer").await,
            permissions(&["runs:read", "workflows:write"])
        );
        assert_eq!(
            role("builder").await,
            permissions(&["actions:write", "runs:read", "workflows:write"]),
            "and version 15 renames toolsets:write"
        );
        assert_eq!(role("reader").await, permissions(&["runs:read"]));
        assert_eq!(
            store
                .find_token(Digest::of(b"token"))
                .await
                .expect("find_token")
                .expect("the token")
                .permissions,
            permissions(&["actions:write", "workflows:write"])
        );
        assert_eq!(
            store
                .find_token_key("k1")
                .await
                .expect("find_token_key")
                .expect("the key")
                .permissions,
            permissions(&["workflows:write"])
        );
        let stored = {
            let connection = store.connection().await.expect("connection");
            sql::query_row(
                &connection,
                "SELECT permissions FROM roles WHERE tenant = 'acme' AND name = 'builder'",
                Vec::new(),
            )
            .await
            .expect("the role's row")
            .expect("a row")
            .text(0)
            .expect("text")
        };
        assert_eq!(
            stored, r#"["actions:write","runs:read","workflows:write"]"#,
            "sorted, as the store writes them"
        );
        let left: Vec<String> = {
            let connection = store.connection().await.expect("connection");
            sql::query(
                &connection,
                "SELECT name FROM sqlite_master WHERE name LIKE 'saved_%' \
                 OR name IN ('script_titles', 'workflows_by_number') ORDER BY name",
                Vec::new(),
            )
            .await
            .expect("the schema")
            .iter()
            .map(|row| row.text(0).expect("a name"))
            .collect()
        };
        assert_eq!(
            left,
            Vec::<String>::new(),
            "the tables of version 13 are gone"
        );

        let next = catalog
            .put_version(
                &acme,
                &triage,
                &NewVersion {
                    document: WorkflowVersion::new(code("triage v4"), Digest::of(b"action_set")),
                    pulled: None,
                },
                at(4),
            )
            .await
            .expect("put_version");
        assert_eq!(next.version, 4, "a moved workflow takes new versions");
        assert_eq!(
            catalog
                .suite(&acme, &triage)
                .await
                .expect("suite")
                .iter()
                .filter(|member| member.is_current())
                .count(),
            0,
            "the new version has no tests"
        );
    }
}
