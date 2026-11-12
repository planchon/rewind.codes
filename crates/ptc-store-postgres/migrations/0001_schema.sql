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

-- The read model, written only by the projector.

CREATE TABLE run_summaries (
    run_id uuid PRIMARY KEY,
    tenant text COLLATE "C" NOT NULL,
    script bytea NOT NULL,
    toolset bytea NOT NULL,
    status text NOT NULL,
    labels jsonb NOT NULL,
    started_at bigint,
    ended_at bigint,
    last_activity_at bigint NOT NULL,
    tool_calls bigint NOT NULL,
    errors bigint NOT NULL,
    history_len bigint NOT NULL
);
CREATE INDEX run_summaries_by_tenant ON run_summaries (tenant, run_id);
CREATE INDEX run_summaries_by_end ON run_summaries (ended_at) WHERE ended_at IS NOT NULL;

-- One row per call, each column set by the entry that knows it; `result_shape` is the shape of a
-- result whose value was projected.
CREATE TABLE tool_calls (
    tenant text COLLATE "C" NOT NULL,
    run_id uuid NOT NULL,
    seq bigint NOT NULL,
    tool text COLLATE "C",
    callsite_line bigint,
    scheduled_at bigint,
    completed_at bigint,
    outcome text CHECK (outcome IN ('completed', 'failed')),
    error_kind text,
    args_digest bytea,
    result_digest bytea,
    result_shape text,
    -- The highest attempt recorded by whoever claimed the call's task.
    attempts bigint,
    PRIMARY KEY (run_id, seq)
);
CREATE INDEX tool_calls_by_tool ON tool_calls (tenant, tool);
