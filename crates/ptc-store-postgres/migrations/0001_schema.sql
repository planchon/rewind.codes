-- The schema of the production store, version 1 (`archi/06-storage-and-scale.md`, "Tables").
--
-- Times are milliseconds since the Unix epoch and durations milliseconds, as `bigint`, like the
-- domain's `Timestamp`. Digests are their 32 bytes, run ids `uuid`, labels `jsonb`. Events and
-- payload values are the exact text `ptc-format` and RFC 8785 write. Text that is compared or
-- ordered (tenants, names, tokens) uses the "C" collation: byte order, the order of the domain.
--
-- `history` and `inbox` are ready to be partitioned by `run_id`: their primary keys start with
-- it and every statement on them names a run.

CREATE TABLE tenants (
    id text COLLATE "C" PRIMARY KEY,
    active_run_quota bigint,
    journal_retention_ms bigint NOT NULL,
    archive_retention_ms bigint NOT NULL,
    read_model_retention_ms bigint NOT NULL,
    payload_grace_ms bigint NOT NULL
);

-- A tenant's data key, encrypted by the deployment's master key. Apart from `tenants`: a tenant
-- without settings still has secrets.
CREATE TABLE data_keys (
    tenant text COLLATE "C" PRIMARY KEY,
    wrapped_key bytea NOT NULL
);

CREATE TABLE api_keys (
    hash bytea PRIMARY KEY,
    tenant text COLLATE "C" NOT NULL,
    role text NOT NULL,
    allowed_labels jsonb NOT NULL,
    created_at bigint NOT NULL,
    revoked_at bigint
);

CREATE TABLE scripts (
    tenant text COLLATE "C" NOT NULL,
    digest bytea NOT NULL,
    source text NOT NULL,
    transpiled text NOT NULL,
    source_map text NOT NULL,
    PRIMARY KEY (tenant, digest)
);

CREATE TABLE toolsets (
    tenant text COLLATE "C" NOT NULL,
    digest bytea NOT NULL,
    spec text NOT NULL,
    declarations text NOT NULL,
    prompt text NOT NULL,
    PRIMARY KEY (tenant, digest)
);

-- A run keeps its oldest inbox item (`inbox_head`, received at `runnable_at`): the picks of
-- `acquire_runnable` read the partial indexes of runnable runs in order.
CREATE TABLE runs (
    tenant text COLLATE "C" NOT NULL,
    id uuid NOT NULL,
    script bytea NOT NULL,
    toolset bytea NOT NULL,
    status text NOT NULL
        CHECK (status IN ('pending', 'active', 'completed', 'failed', 'cancelled', 'aborted')),
    epoch bigint NOT NULL,
    history_len bigint NOT NULL DEFAULT 0,
    expires_at bigint NOT NULL,
    expiry_enqueued boolean NOT NULL DEFAULT false,
    quarantine_kind text CHECK (quarantine_kind IN ('non_determinism', 'failures')),
    quarantine_reason text,
    quarantine_since bigint,
    quarantine_retried_by text,
    failures bigint NOT NULL DEFAULT 0,
    attempts bigint NOT NULL DEFAULT 0,
    lease_owner text,
    lease_until bigint,
    fence bigint NOT NULL DEFAULT 0,
    inbox_head bigint,
    runnable_at bigint,
    sticky_worker text COLLATE "C",
    labels jsonb NOT NULL,
    traceparent text,
    tracestate text,
    parent uuid,
    parent_at bigint,
    idempotency_key text,
    created_at bigint NOT NULL,
    updated_at bigint NOT NULL,
    ended_at bigint,
    archive_document bytea,
    archived_at bigint,
    PRIMARY KEY (tenant, id),
    -- Run ids are unique across tenants, and the tables of a run name it by its id alone.
    UNIQUE (id),
    UNIQUE (tenant, idempotency_key)
);
CREATE INDEX runs_by_creation ON runs (tenant, created_at, id);
CREATE INDEX runs_by_labels ON runs USING gin (labels jsonb_path_ops);
CREATE INDEX runs_active ON runs (tenant)
    WHERE status NOT IN ('completed', 'failed', 'cancelled', 'aborted');
CREATE INDEX runs_by_deadline ON runs (expires_at, id)
    WHERE status NOT IN ('completed', 'failed', 'cancelled', 'aborted');
CREATE INDEX runs_runnable ON runs (tenant, inbox_head) WHERE inbox_head IS NOT NULL;
CREATE INDEX runs_runnable_by_sticky_worker ON runs (sticky_worker, inbox_head)
    WHERE inbox_head IS NOT NULL AND sticky_worker IS NOT NULL;
CREATE INDEX runs_quarantined ON runs (quarantine_since, tenant, id)
    WHERE quarantine_kind IS NOT NULL;
CREATE INDEX runs_to_archive ON runs (ended_at, tenant, id)
    WHERE ended_at IS NOT NULL AND archive_document IS NULL;
CREATE INDEX runs_archived ON runs (archived_at, tenant, id) WHERE archive_document IS NOT NULL;

CREATE TABLE history (
    run_id uuid NOT NULL,
    idx bigint NOT NULL,
    recorded_at bigint NOT NULL,
    event text NOT NULL,
    PRIMARY KEY (run_id, idx)
);

-- Inbox ids come from a sequence of their own rather than an identity column, which a
-- partitioned table only supports from Postgres 17.
CREATE SEQUENCE inbox_ids AS bigint;
CREATE TABLE inbox (
    run_id uuid NOT NULL,
    id bigint NOT NULL DEFAULT nextval('inbox_ids'),
    event text NOT NULL,
    received_at bigint NOT NULL,
    PRIMARY KEY (run_id, id)
);
ALTER SEQUENCE inbox_ids OWNED BY inbox.id;

CREATE TABLE signal_keys (
    run_id uuid NOT NULL,
    key text NOT NULL,
    PRIMARY KEY (run_id, key)
);

CREATE TABLE timers (
    run_id uuid NOT NULL,
    seq bigint NOT NULL,
    fire_at bigint NOT NULL,
    PRIMARY KEY (run_id, seq)
);
CREATE INDEX timers_by_fire_at ON timers (fire_at, run_id, seq);

-- A task keeps its run's tenant and toolset, and when it can be claimed (`due_at`: a ready task
-- at its next attempt, a claimed one when its claim expires).
CREATE TABLE tool_tasks (
    tenant text COLLATE "C" NOT NULL,
    run_id uuid NOT NULL,
    seq bigint NOT NULL,
    toolset bytea NOT NULL,
    tool text COLLATE "C" NOT NULL,
    route text NOT NULL,
    mcp_server text COLLATE "C",
    dispatched boolean NOT NULL,
    args_digest bytea NOT NULL,
    args text,
    on_interruption text NOT NULL,
    approval boolean NOT NULL,
    execution_timeout_ms bigint,
    heartbeat_timeout_ms bigint NOT NULL,
    concurrency bigint NOT NULL,
    server_concurrency bigint,
    state text NOT NULL
        CHECK (state IN ('approval', 'ready', 'claimed', 'deferred', 'parked')),
    attempt bigint NOT NULL DEFAULT 0,
    token text COLLATE "C" UNIQUE,
    lease_owner text COLLATE "C",
    lease_until bigint,
    next_attempt_at bigint,
    due_at bigint,
    parked_kind text CHECK (parked_kind IN ('schema_changed', 'credentials_expired')),
    parked_reason text,
    schema_change_approved boolean NOT NULL DEFAULT false,
    created_at bigint NOT NULL,
    PRIMARY KEY (run_id, seq)
);
CREATE INDEX tool_tasks_due ON tool_tasks (due_at, run_id, seq) WHERE due_at IS NOT NULL;
CREATE INDEX tool_tasks_host_due ON tool_tasks (tenant, due_at, run_id, seq)
    WHERE due_at IS NOT NULL AND NOT dispatched;
CREATE INDEX tool_tasks_deferred ON tool_tasks (lease_until, run_id, seq)
    WHERE state = 'deferred';
CREATE INDEX tool_tasks_claimed_by_owner ON tool_tasks (lease_owner, tenant, mcp_server)
    WHERE state = 'claimed';
CREATE INDEX tool_tasks_parked ON tool_tasks (tenant, toolset) WHERE state = 'parked';

CREATE TABLE webhook_tokens (
    token text COLLATE "C" PRIMARY KEY,
    run_id uuid NOT NULL,
    name text NOT NULL,
    UNIQUE (run_id, name)
);

CREATE TABLE subscriptions (
    tenant text COLLATE "C" NOT NULL,
    id text COLLATE "C" NOT NULL,
    url text NOT NULL,
    secret text NOT NULL,
    events text NOT NULL,
    labels jsonb NOT NULL,
    PRIMARY KEY (tenant, id)
);

CREATE TABLE payloads (
    tenant text COLLATE "C" NOT NULL,
    digest bytea NOT NULL,
    bytes bigint NOT NULL,
    preview text NOT NULL,
    value text,
    unreferenced_since bigint,
    -- When a cleanup started deleting the payload: its blob may be gone.
    deletion_started_at bigint,
    PRIMARY KEY (tenant, digest)
);
CREATE INDEX payloads_unreferenced ON payloads (unreferenced_since)
    WHERE unreferenced_since IS NOT NULL;

CREATE TABLE payload_refs (
    tenant text COLLATE "C" NOT NULL,
    digest bytea NOT NULL,
    run_id uuid NOT NULL,
    PRIMARY KEY (tenant, digest, run_id)
);
CREATE INDEX payload_refs_by_run ON payload_refs (run_id);

CREATE TABLE secrets (
    tenant text COLLATE "C" NOT NULL,
    owner text COLLATE "C" NOT NULL,
    ciphertext bytea NOT NULL,
    PRIMARY KEY (tenant, owner)
);

CREATE TABLE feed (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    consumer text NOT NULL CHECK (consumer IN ('read_model', 'otel', 'webhooks')),
    tenant text COLLATE "C" NOT NULL,
    run_id uuid NOT NULL,
    fence bigint NOT NULL,
    from_idx bigint NOT NULL,
    to_idx bigint NOT NULL,
    lease_owner text,
    lease_until bigint
);
CREATE INDEX feed_by_consumer ON feed (consumer, id);
CREATE INDEX feed_by_run ON feed (run_id);

-- The read model (`archi/05-testing-and-observability.md`, "Read model"), written by the
-- projector, and by whoever changes a task's state or a run's quarantine. Every table names its
-- run's tenant: every query is scoped by it, and the indexes that serve them start with it.

-- Free text is found with trigrams (`run_search`): a trusted extension since Postgres 13, which
-- the owner of the database may create.
CREATE EXTENSION IF NOT EXISTS pg_trgm;

-- One row per run. Its status, times and counts come from the entries projected, keyed by
-- `(run, index)`; its mode, ending and `wakes_at` from the snapshot of the longest history
-- (`snapshot_len`), whose waits are in `run_waits`. `wakes_at` is the earliest `until` of those
-- waits while the run is active. An ending has all of its columns, or none.
CREATE TABLE run_summaries (
    run_id uuid PRIMARY KEY,
    tenant text COLLATE "C" NOT NULL,
    script bytea NOT NULL,
    toolset bytea NOT NULL,
    headline text NOT NULL,
    parent uuid,
    labels jsonb NOT NULL,
    status text COLLATE "C" NOT NULL
        CHECK (status IN ('active', 'completed', 'failed', 'cancelled', 'aborted')),
    started_at bigint,
    ended_at bigint,
    last_activity_at bigint NOT NULL,
    tool_calls bigint NOT NULL,
    errors bigint NOT NULL,
    history_len bigint NOT NULL,
    snapshot_len bigint NOT NULL,
    mode text COLLATE "C" CHECK (mode IN ('live', 'mock')),
    wakes_at bigint,
    error text COLLATE "C",
    message text,
    line bigint,
    signature text,
    fingerprint text COLLATE "C",
    CHECK ((error IS NULL) = (fingerprint IS NULL)),
    CHECK ((error IS NULL) = (message IS NULL) AND (error IS NULL) = (signature IS NULL))
);
-- The orders of a search: newest run, latest activity, earliest wake (nulls last).
CREATE INDEX run_summaries_by_tenant ON run_summaries (tenant, run_id);
CREATE INDEX run_summaries_by_activity ON run_summaries (tenant, last_activity_at, run_id);
CREATE INDEX run_summaries_by_wake ON run_summaries (tenant, wakes_at, run_id);
-- The runs that may wait, few among the others, in the order of their wake.
CREATE INDEX run_summaries_active ON run_summaries (tenant, wakes_at, run_id)
    WHERE status = 'active';
-- Windows and predicates: started (statistics, scripts), ended (failure groups), script,
-- failure group, forks.
CREATE INDEX run_summaries_by_start ON run_summaries (tenant, started_at);
CREATE INDEX run_summaries_by_end ON run_summaries (tenant, ended_at) WHERE ended_at IS NOT NULL;
CREATE INDEX run_summaries_by_script ON run_summaries (tenant, script);
CREATE INDEX run_summaries_by_fingerprint ON run_summaries (tenant, fingerprint)
    WHERE fingerprint IS NOT NULL;
CREATE INDEX run_summaries_by_parent ON run_summaries (tenant, parent) WHERE parent IS NOT NULL;
-- Retention, across tenants.
CREATE INDEX run_summaries_to_purge ON run_summaries (ended_at) WHERE ended_at IS NOT NULL;

-- One row per label of a run: equality, prefix (the "C" collation lets `LIKE 'prefix%'` use the
-- index) and presence.
CREATE TABLE run_labels (
    run_id uuid NOT NULL,
    key text COLLATE "C" NOT NULL,
    value text COLLATE "C" NOT NULL,
    tenant text COLLATE "C" NOT NULL,
    PRIMARY KEY (run_id, key)
);
CREATE INDEX run_labels_by_value ON run_labels (tenant, key, value, run_id);
-- Label values, like tools, are a tenant's own: the planner learns the frequent ones per tenant,
-- and pages through a frequent one in order rather than reading all its runs.
CREATE STATISTICS run_labels_per_tenant (mcv) ON tenant, key, value FROM run_labels;

-- The waits of a run's latest snapshot, at their position in it. A call's task state is not
-- here but in `tool_calls`: it outlives the snapshot.
CREATE TABLE run_waits (
    run_id uuid NOT NULL,
    position integer NOT NULL,
    tenant text COLLATE "C" NOT NULL,
    kind text COLLATE "C" NOT NULL CHECK (kind IN ('signal', 'timer', 'call', 'host')),
    seq bigint,
    name text COLLATE "C",
    since bigint NOT NULL,
    until bigint,
    callsite_line bigint,
    callsite_column bigint,
    PRIMARY KEY (run_id, position)
);
CREATE INDEX run_waits_by_name ON run_waits (tenant, kind, name);

-- What free text is searched in: the run's id, the script's first line, the ending's error and
-- message, the labels' values, each in lowercase, joined by an uppercase A, which no lowercased
-- text holds. Apart from `run_summaries`, whose row each batch of entries rewrites: this one
-- changes when the run is projected first and when it ends, and so does its trigram index.
CREATE TABLE run_search (
    run_id uuid PRIMARY KEY,
    tenant text COLLATE "C" NOT NULL,
    search_text text NOT NULL
);
CREATE INDEX run_search_by_text ON run_search USING gin (search_text gin_trgm_ops);

-- The latest record of a run's quarantine, at `recorded_at`: quarantined with its kind, reason
-- and time, or released (no kind). Recorded before or after the run is projected.
CREATE TABLE run_quarantine (
    run_id uuid PRIMARY KEY,
    tenant text COLLATE "C" NOT NULL,
    recorded_at bigint NOT NULL,
    kind text CHECK (kind IN ('non_determinism', 'failures')),
    reason text,
    since bigint,
    CHECK ((kind IS NULL) = (reason IS NULL) AND (kind IS NULL) = (since IS NULL))
);
CREATE INDEX run_quarantine_by_tenant ON run_quarantine (tenant) WHERE kind IS NOT NULL;

-- One row per call, each column set by the entry that knows it; `result_shape` is the shape of a
-- result whose value was projected. The task's furthest state, recorded by whoever changed it
-- before or after the call is projected: its highest attempt, and the state of the highest rank
-- (by attempt, then approval or claim, parked or deferred, ready).
CREATE TABLE tool_calls (
    tenant text COLLATE "C" NOT NULL,
    run_id uuid NOT NULL,
    seq bigint NOT NULL,
    tool text COLLATE "C",
    callsite_line bigint,
    scheduled_at bigint,
    completed_at bigint,
    outcome text COLLATE "C" CHECK (outcome IN ('completed', 'failed')),
    error_kind text COLLATE "C",
    args_digest bytea,
    result_digest bytea,
    result_shape text,
    attempts bigint,
    task_state text COLLATE "C"
        CHECK (task_state IN ('approval', 'ready', 'claimed', 'deferred', 'parked')),
    PRIMARY KEY (run_id, seq),
    CHECK ((attempts IS NULL) = (task_state IS NULL))
);
CREATE INDEX tool_calls_by_tool ON tool_calls (tenant, tool);
CREATE STATISTICS tool_calls_per_tenant (mcv) ON tenant, tool FROM tool_calls;
CREATE INDEX tool_calls_by_schedule ON tool_calls (tenant, scheduled_at);
