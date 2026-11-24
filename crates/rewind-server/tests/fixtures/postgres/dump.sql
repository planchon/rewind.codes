--
-- PostgreSQL database dump
--

\restrict K0IjfR5V7anrpHYHgFsoQRWudFnXFP2o5ONms5Kio5z4XDj6reAoyoyEvcYK6yH

-- Dumped from database version 17.12 (Debian 17.12-1.pgdg13+1)
-- Dumped by pg_dump version 17.12 (Debian 17.12-1.pgdg13+1)

SET statement_timeout = 0;
SET lock_timeout = 0;
SET idle_in_transaction_session_timeout = 0;
SET transaction_timeout = 0;
SET client_encoding = 'UTF8';
SET standard_conforming_strings = on;
SELECT pg_catalog.set_config('search_path', '', false);
SET check_function_bodies = false;
SET xmloption = content;
SET client_min_messages = warning;
SET row_security = off;

--
-- Name: pg_trgm; Type: EXTENSION; Schema: -; Owner: -
--

CREATE EXTENSION IF NOT EXISTS pg_trgm WITH SCHEMA public;


--
-- Name: EXTENSION pg_trgm; Type: COMMENT; Schema: -; Owner: -
--

COMMENT ON EXTENSION pg_trgm IS 'text similarity measurement and index searching based on trigrams';


SET default_tablespace = '';

SET default_table_access_method = heap;

--
-- Name: _sqlx_migrations; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public._sqlx_migrations (
    version bigint NOT NULL,
    description text NOT NULL,
    installed_on timestamp with time zone DEFAULT now() NOT NULL,
    success boolean NOT NULL,
    checksum bytea NOT NULL,
    execution_time bigint NOT NULL
);


--
-- Name: access_settings; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.access_settings (
    tenant text NOT NULL COLLATE pg_catalog."C",
    default_role text NOT NULL COLLATE pg_catalog."C"
);


--
-- Name: api_keys; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.api_keys (
    hash bytea NOT NULL,
    tenant text NOT NULL COLLATE pg_catalog."C",
    created_at bigint NOT NULL,
    revoked_at bigint,
    name text NOT NULL,
    created_by text COLLATE pg_catalog."C"
);


--
-- Name: connection_tokens; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.connection_tokens (
    hash bytea NOT NULL,
    connection_id text NOT NULL COLLATE pg_catalog."C",
    kind text NOT NULL,
    expires_at bigint NOT NULL,
    created_at bigint NOT NULL,
    used_at bigint
);


--
-- Name: connections; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.connections (
    id text NOT NULL COLLATE pg_catalog."C",
    user_id text NOT NULL COLLATE pg_catalog."C",
    client_id text NOT NULL COLLATE pg_catalog."C",
    client_name text NOT NULL,
    created_at bigint NOT NULL,
    last_used_at bigint,
    revoked_at bigint,
    expires_at bigint NOT NULL
);


--
-- Name: data_keys; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.data_keys (
    tenant text NOT NULL COLLATE pg_catalog."C",
    wrapped_key bytea NOT NULL,
    master_key text NOT NULL COLLATE pg_catalog."C"
);


--
-- Name: feed; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.feed (
    id bigint NOT NULL,
    consumer text NOT NULL,
    tenant text NOT NULL COLLATE pg_catalog."C",
    run_id uuid NOT NULL,
    fence bigint NOT NULL,
    from_idx bigint NOT NULL,
    to_idx bigint NOT NULL,
    lease_owner text,
    lease_until bigint,
    CONSTRAINT feed_consumer_check CHECK ((consumer = ANY (ARRAY['read_model'::text, 'otel'::text, 'webhooks'::text])))
);


--
-- Name: feed_id_seq; Type: SEQUENCE; Schema: public; Owner: -
--

ALTER TABLE public.feed ALTER COLUMN id ADD GENERATED ALWAYS AS IDENTITY (
    SEQUENCE NAME public.feed_id_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1
);


--
-- Name: grants; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.grants (
    tenant text NOT NULL COLLATE pg_catalog."C",
    principal text NOT NULL COLLATE pg_catalog."C",
    role text NOT NULL COLLATE pg_catalog."C",
    allowed_labels jsonb NOT NULL,
    granted_by text COLLATE pg_catalog."C",
    granted_at bigint NOT NULL
);


--
-- Name: history; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.history (
    run_id uuid NOT NULL,
    idx bigint NOT NULL,
    recorded_at bigint NOT NULL,
    event text NOT NULL
);


--
-- Name: inbox; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.inbox (
    run_id uuid NOT NULL,
    id bigint NOT NULL,
    event text NOT NULL,
    received_at bigint NOT NULL
);


--
-- Name: inbox_ids; Type: SEQUENCE; Schema: public; Owner: -
--

CREATE SEQUENCE public.inbox_ids
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1;


--
-- Name: inbox_ids; Type: SEQUENCE OWNED BY; Schema: public; Owner: -
--

ALTER SEQUENCE public.inbox_ids OWNED BY public.inbox.id;


--
-- Name: namespace_scripts; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.namespace_scripts (
    tenant text NOT NULL COLLATE pg_catalog."C",
    namespace text NOT NULL COLLATE pg_catalog."C",
    digest bytea NOT NULL,
    runs bigint NOT NULL,
    first_run_at bigint NOT NULL
);


--
-- Name: namespaces; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.namespaces (
    tenant text NOT NULL COLLATE pg_catalog."C",
    name text NOT NULL COLLATE pg_catalog."C",
    cpu_per_month_ms bigint,
    active_runs bigint,
    runs_per_month bigint,
    scripts bigint,
    max_ttl_ms bigint,
    max_script_lines bigint,
    tokens_revoked_before bigint,
    created_at bigint NOT NULL,
    last_active_at bigint NOT NULL,
    deleting_since bigint,
    display_name text,
    display_key text
);


--
-- Name: oauth_clients; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.oauth_clients (
    id text NOT NULL COLLATE pg_catalog."C",
    kind text NOT NULL,
    name text NOT NULL,
    uri text,
    logo_uri text,
    redirect_uris text[] NOT NULL,
    auth_method text NOT NULL,
    secret_hash bytea,
    fetched_at bigint,
    expires_at bigint,
    created_at bigint NOT NULL,
    last_used_at bigint
);


--
-- Name: oauth_codes; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.oauth_codes (
    hash bytea NOT NULL,
    client_id text NOT NULL COLLATE pg_catalog."C",
    user_id text NOT NULL COLLATE pg_catalog."C",
    redirect_uri text NOT NULL,
    code_challenge text NOT NULL,
    resource text NOT NULL,
    organizations jsonb NOT NULL,
    labels jsonb NOT NULL,
    expires_at bigint NOT NULL,
    used_at bigint,
    connection_id text COLLATE pg_catalog."C"
);


--
-- Name: payload_refs; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.payload_refs (
    tenant text NOT NULL COLLATE pg_catalog."C",
    digest bytea NOT NULL,
    run_id uuid NOT NULL
);


--
-- Name: payloads; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.payloads (
    tenant text NOT NULL COLLATE pg_catalog."C",
    digest bytea NOT NULL,
    bytes bigint NOT NULL,
    preview text NOT NULL,
    value text,
    unreferenced_since bigint,
    deletion_started_at bigint
);


--
-- Name: quota_alerts; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.quota_alerts (
    tenant text NOT NULL COLLATE pg_catalog."C",
    namespace text NOT NULL COLLATE pg_catalog."C",
    quota text NOT NULL,
    period text NOT NULL COLLATE pg_catalog."C",
    threshold bigint NOT NULL,
    used bigint NOT NULL,
    quota_limit bigint NOT NULL,
    raised_at bigint NOT NULL,
    lease_owner text,
    lease_until bigint,
    sent_at bigint,
    CONSTRAINT quota_alerts_quota_check CHECK ((quota = ANY (ARRAY['cpu_per_month'::text, 'active_runs'::text, 'runs_per_month'::text, 'scripts'::text, 'max_ttl'::text, 'max_script_lines'::text]))),
    CONSTRAINT quota_alerts_threshold_check CHECK ((threshold = ANY (ARRAY[(80)::bigint, (100)::bigint])))
);


--
-- Name: roles; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.roles (
    tenant text NOT NULL COLLATE pg_catalog."C",
    name text NOT NULL COLLATE pg_catalog."C",
    description text NOT NULL,
    permissions text[] NOT NULL,
    created_at bigint NOT NULL,
    updated_at bigint NOT NULL
);


--
-- Name: run_labels; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.run_labels (
    run_id uuid NOT NULL,
    key text NOT NULL COLLATE pg_catalog."C",
    value text NOT NULL COLLATE pg_catalog."C",
    tenant text NOT NULL COLLATE pg_catalog."C"
);


--
-- Name: run_quarantine; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.run_quarantine (
    run_id uuid NOT NULL,
    tenant text NOT NULL COLLATE pg_catalog."C",
    recorded_at bigint NOT NULL,
    kind text,
    reason text,
    since bigint,
    CONSTRAINT run_quarantine_check CHECK ((((kind IS NULL) = (reason IS NULL)) AND ((kind IS NULL) = (since IS NULL)))),
    CONSTRAINT run_quarantine_kind_check CHECK ((kind = ANY (ARRAY['non_determinism'::text, 'failures'::text])))
);


--
-- Name: run_search; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.run_search (
    run_id uuid NOT NULL,
    tenant text NOT NULL COLLATE pg_catalog."C",
    search_text text NOT NULL
);


--
-- Name: run_summaries; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.run_summaries (
    run_id uuid NOT NULL,
    tenant text NOT NULL COLLATE pg_catalog."C",
    script bytea NOT NULL,
    toolset bytea NOT NULL,
    headline text NOT NULL,
    parent uuid,
    labels jsonb NOT NULL,
    status text NOT NULL COLLATE pg_catalog."C",
    started_at bigint,
    ended_at bigint,
    last_activity_at bigint NOT NULL,
    tool_calls bigint NOT NULL,
    errors bigint NOT NULL,
    history_len bigint NOT NULL,
    snapshot_len bigint NOT NULL,
    mode text COLLATE pg_catalog."C",
    wakes_at bigint,
    error text COLLATE pg_catalog."C",
    message text,
    line bigint,
    signature text,
    fingerprint text COLLATE pg_catalog."C",
    workflow text COLLATE pg_catalog."C",
    started_by uuid,
    namespace text DEFAULT 'migrated'::text NOT NULL COLLATE pg_catalog."C",
    changed_at bigint DEFAULT 0 NOT NULL,
    CONSTRAINT run_summaries_check CHECK (((error IS NULL) = (fingerprint IS NULL))),
    CONSTRAINT run_summaries_check1 CHECK ((((error IS NULL) = (message IS NULL)) AND ((error IS NULL) = (signature IS NULL)))),
    CONSTRAINT run_summaries_mode_check CHECK ((mode = ANY (ARRAY['live'::text, 'mock'::text]))),
    CONSTRAINT run_summaries_status_check CHECK ((status = ANY (ARRAY['active'::text, 'completed'::text, 'failed'::text, 'cancelled'::text, 'aborted'::text])))
);


--
-- Name: run_waits; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.run_waits (
    run_id uuid NOT NULL,
    "position" integer NOT NULL,
    tenant text NOT NULL COLLATE pg_catalog."C",
    kind text NOT NULL COLLATE pg_catalog."C",
    seq bigint,
    name text COLLATE pg_catalog."C",
    since bigint NOT NULL,
    until bigint,
    callsite_line bigint,
    callsite_column bigint,
    CONSTRAINT run_waits_kind_check CHECK ((kind = ANY (ARRAY['signal'::text, 'timer'::text, 'call'::text, 'host'::text])))
);


--
-- Name: run_watchers; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.run_watchers (
    run_id uuid NOT NULL,
    watcher uuid NOT NULL,
    seq bigint NOT NULL,
    attached boolean NOT NULL
);


--
-- Name: runs; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.runs (
    tenant text NOT NULL COLLATE pg_catalog."C",
    id uuid NOT NULL,
    script bytea NOT NULL,
    toolset bytea NOT NULL,
    status text NOT NULL,
    epoch bigint NOT NULL,
    history_len bigint DEFAULT 0 NOT NULL,
    expires_at bigint NOT NULL,
    expiry_enqueued boolean DEFAULT false NOT NULL,
    quarantine_kind text,
    quarantine_reason text,
    quarantine_since bigint,
    quarantine_retried_by text,
    failures bigint DEFAULT 0 NOT NULL,
    attempts bigint DEFAULT 0 NOT NULL,
    lease_owner text,
    lease_until bigint,
    fence bigint DEFAULT 0 NOT NULL,
    inbox_head bigint,
    runnable_at bigint,
    sticky_worker text COLLATE pg_catalog."C",
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
    workflow text COLLATE pg_catalog."C",
    workflow_version bytea,
    started_by uuid,
    started_seq bigint,
    detached boolean,
    scope jsonb DEFAULT '{}'::jsonb NOT NULL,
    namespace text NOT NULL COLLATE pg_catalog."C",
    secrets jsonb DEFAULT '{}'::jsonb NOT NULL,
    CONSTRAINT runs_quarantine_kind_check CHECK ((quarantine_kind = ANY (ARRAY['non_determinism'::text, 'failures'::text]))),
    CONSTRAINT runs_started_by CHECK ((((started_by IS NULL) = (started_seq IS NULL)) AND ((started_by IS NULL) = (detached IS NULL)))),
    CONSTRAINT runs_status_check CHECK ((status = ANY (ARRAY['pending'::text, 'active'::text, 'completed'::text, 'failed'::text, 'cancelled'::text, 'aborted'::text]))),
    CONSTRAINT runs_workflow CHECK (((workflow IS NULL) = (workflow_version IS NULL)))
);


--
-- Name: scripts; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.scripts (
    tenant text NOT NULL COLLATE pg_catalog."C",
    digest bytea NOT NULL,
    source text NOT NULL,
    transpiled text NOT NULL,
    source_map text NOT NULL
);


--
-- Name: secret_reads; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.secret_reads (
    tenant text NOT NULL COLLATE pg_catalog."C",
    namespace text NOT NULL COLLATE pg_catalog."C",
    name text NOT NULL COLLATE pg_catalog."C",
    day bigint NOT NULL,
    reads bigint NOT NULL
);


--
-- Name: secrets; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.secrets (
    tenant text NOT NULL COLLATE pg_catalog."C",
    namespace text NOT NULL COLLATE pg_catalog."C",
    name text NOT NULL COLLATE pg_catalog."C",
    ciphertext bytea NOT NULL,
    labels jsonb NOT NULL,
    created_by text NOT NULL,
    created_at bigint NOT NULL,
    updated_by text NOT NULL,
    updated_at bigint NOT NULL,
    last_read_at bigint,
    last_read_run uuid,
    last_read_namespace text COLLATE pg_catalog."C"
);


--
-- Name: signal_keys; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.signal_keys (
    run_id uuid NOT NULL,
    key text NOT NULL
);


--
-- Name: signing_keys; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.signing_keys (
    tenant text NOT NULL COLLATE pg_catalog."C",
    kid text NOT NULL COLLATE pg_catalog."C",
    public_key bytea NOT NULL,
    private_ciphertext bytea NOT NULL,
    created_at bigint NOT NULL,
    retired_at bigint
);


--
-- Name: subscriptions; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.subscriptions (
    tenant text NOT NULL COLLATE pg_catalog."C",
    id text NOT NULL COLLATE pg_catalog."C",
    url text NOT NULL,
    secret text NOT NULL,
    events text NOT NULL,
    labels jsonb NOT NULL
);


--
-- Name: tenants; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.tenants (
    id text NOT NULL COLLATE pg_catalog."C",
    active_run_quota bigint,
    journal_retention_ms bigint NOT NULL,
    archive_retention_ms bigint NOT NULL,
    read_model_retention_ms bigint NOT NULL,
    payload_grace_ms bigint NOT NULL,
    cpu_per_month_ms bigint,
    runs_per_month bigint,
    scripts bigint,
    max_ttl_ms bigint,
    max_script_lines bigint,
    default_cpu_per_month_ms bigint,
    default_active_runs bigint,
    default_runs_per_month bigint,
    default_scripts bigint,
    default_max_ttl_ms bigint,
    default_max_script_lines bigint
);


--
-- Name: tests; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.tests (
    tenant text NOT NULL COLLATE pg_catalog."C",
    digest bytea NOT NULL,
    namespace text COLLATE pg_catalog."C",
    size bigint NOT NULL,
    reserved_at bigint NOT NULL,
    deletion_started_at bigint
);


--
-- Name: timers; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.timers (
    run_id uuid NOT NULL,
    seq bigint NOT NULL,
    fire_at bigint NOT NULL
);


--
-- Name: token_keys; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.token_keys (
    kid text NOT NULL COLLATE pg_catalog."C",
    tenant text NOT NULL COLLATE pg_catalog."C",
    name text NOT NULL,
    jwk text NOT NULL,
    permissions text[] NOT NULL,
    created_by text NOT NULL COLLATE pg_catalog."C",
    created_at bigint NOT NULL
);


--
-- Name: tokens; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.tokens (
    hash bytea NOT NULL,
    tenant text NOT NULL COLLATE pg_catalog."C",
    namespace text NOT NULL COLLATE pg_catalog."C",
    permissions text[] NOT NULL,
    labels jsonb NOT NULL,
    expires_at bigint NOT NULL,
    created_by text NOT NULL COLLATE pg_catalog."C",
    created_at bigint NOT NULL,
    revoked_at bigint
);


--
-- Name: tool_calls; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.tool_calls (
    tenant text NOT NULL COLLATE pg_catalog."C",
    run_id uuid NOT NULL,
    seq bigint NOT NULL,
    tool text COLLATE pg_catalog."C",
    callsite_line bigint,
    scheduled_at bigint,
    completed_at bigint,
    outcome text COLLATE pg_catalog."C",
    error_kind text COLLATE pg_catalog."C",
    args_digest bytea,
    result_digest bytea,
    result_shape text,
    attempts bigint,
    task_state text COLLATE pg_catalog."C",
    CONSTRAINT tool_calls_check CHECK (((attempts IS NULL) = (task_state IS NULL))),
    CONSTRAINT tool_calls_outcome_check CHECK ((outcome = ANY (ARRAY['completed'::text, 'failed'::text]))),
    CONSTRAINT tool_calls_task_state_check CHECK ((task_state = ANY (ARRAY['approval'::text, 'ready'::text, 'claimed'::text, 'deferred'::text, 'parked'::text])))
);


--
-- Name: tool_tasks; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.tool_tasks (
    tenant text NOT NULL COLLATE pg_catalog."C",
    run_id uuid NOT NULL,
    seq bigint NOT NULL,
    toolset bytea NOT NULL,
    tool text NOT NULL COLLATE pg_catalog."C",
    route text NOT NULL,
    mcp_server text COLLATE pg_catalog."C",
    dispatched boolean NOT NULL,
    args_digest bytea NOT NULL,
    args text,
    on_interruption text NOT NULL,
    approval boolean NOT NULL,
    execution_timeout_ms bigint,
    heartbeat_timeout_ms bigint NOT NULL,
    concurrency bigint NOT NULL,
    server_concurrency bigint,
    state text NOT NULL,
    attempt bigint DEFAULT 0 NOT NULL,
    token text COLLATE pg_catalog."C",
    lease_owner text COLLATE pg_catalog."C",
    lease_until bigint,
    next_attempt_at bigint,
    due_at bigint,
    parked_kind text,
    parked_reason text,
    schema_change_approved boolean DEFAULT false NOT NULL,
    created_at bigint NOT NULL,
    CONSTRAINT tool_tasks_parked_kind_check CHECK ((parked_kind = ANY (ARRAY['schema_changed'::text, 'credentials_refused'::text, 'secret_missing'::text, 'credentials_expired'::text]))),
    CONSTRAINT tool_tasks_state_check CHECK ((state = ANY (ARRAY['approval'::text, 'ready'::text, 'claimed'::text, 'deferred'::text, 'parked'::text])))
);


--
-- Name: toolsets; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.toolsets (
    tenant text NOT NULL COLLATE pg_catalog."C",
    digest bytea NOT NULL,
    spec text NOT NULL,
    declarations text NOT NULL,
    prompt text NOT NULL
);


--
-- Name: usage; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.usage (
    tenant text NOT NULL COLLATE pg_catalog."C",
    namespace text NOT NULL COLLATE pg_catalog."C",
    day bigint NOT NULL,
    cpu_ms bigint NOT NULL,
    runs_started bigint NOT NULL
);


--
-- Name: webhook_tokens; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.webhook_tokens (
    token text NOT NULL COLLATE pg_catalog."C",
    run_id uuid NOT NULL,
    name text NOT NULL
);


--
-- Name: workflow_tests; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.workflow_tests (
    tenant text NOT NULL COLLATE pg_catalog."C",
    version bytea NOT NULL,
    digest bytea NOT NULL
);


--
-- Name: workflows; Type: TABLE; Schema: public; Owner: -
--

CREATE TABLE public.workflows (
    tenant text NOT NULL COLLATE pg_catalog."C",
    version bytea NOT NULL,
    name text NOT NULL COLLATE pg_catalog."C",
    document text NOT NULL,
    labels jsonb NOT NULL,
    created_at bigint NOT NULL,
    ordinal bigint NOT NULL
);


--
-- Name: workflows_ordinal_seq; Type: SEQUENCE; Schema: public; Owner: -
--

ALTER TABLE public.workflows ALTER COLUMN ordinal ADD GENERATED ALWAYS AS IDENTITY (
    SEQUENCE NAME public.workflows_ordinal_seq
    START WITH 1
    INCREMENT BY 1
    NO MINVALUE
    NO MAXVALUE
    CACHE 1
);


--
-- Name: inbox id; Type: DEFAULT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.inbox ALTER COLUMN id SET DEFAULT nextval('public.inbox_ids'::regclass);


--
-- Data for Name: _sqlx_migrations; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public._sqlx_migrations (version, description, installed_on, success, checksum, execution_time) FROM stdin;
1	schema	2026-11-23 03:10:30.888525+00	t	\\x438fe70d35e94df63bb223f31658bc1a8e02f6baaa473d0fa739d9137cb56847cd611c6b878cdeed3c0fba014ee00d6b	21849333
2	access	2026-11-23 03:10:30.91088+00	t	\\xde6f2b908d05e988dc1415a75400a46d668a088a4a18982eb8a289e7f641ea3289903a069f395fceea9d4872d271f33b	2411042
3	workflows	2026-11-23 03:10:30.913578+00	t	\\xd38858021f171c44b3cce2db53e5c3e1fa9ee7e46f746b509d5fb136be104362f4083f038e69e014cfcb8ee2cf240900	2350208
4	run workflows	2026-11-23 03:10:30.91618+00	t	\\x28b1c446f7fd9db29e8557d8b31087a6bda0dae05759843b8b1e7e339d1a4542aef7ffe9ebc03048b6e303ac38794e6e	1336791
5	namespaces	2026-11-23 03:10:30.917813+00	t	\\x51aea18391b288825da08e8fbcaaff7c64bb98cef0b3d63166fb904248a862c6ddcc919582fefad31087815fc5ed967e	5632125
6	tests	2026-11-23 03:10:30.923678+00	t	\\x355cde401ec996d07602c19fc2d5c43f3286947484736842654541e438b466b27b8ddc1e346dfe5aa54c9eabc66997be	1697875
7	connections	2026-11-23 03:10:30.92562+00	t	\\x224b7bfa30d048b576fe743305e5e8df72c5b7576903932bc8622b2cc52aad8738f1c2ccd8214a50e9f9e29140ecec19	3285000
8	namespace display names	2026-11-23 03:10:30.929349+00	t	\\xc193128b3a658f16f22fcddd40dd089ce5f8c74915324f5edcefcddaccee70da33cc9a296b450b220dff4733676060fa	900042
9	park kinds	2026-11-23 03:10:30.930391+00	t	\\xc088bf9e7541b4a59324c991c342c62c02a1c20613da05da03c271f10e6a8e42db9d2c5df72e1f255d7ac7f41f9c3daa	846333
10	secrets	2026-11-23 03:10:30.931416+00	t	\\x71a0c5689c79455dcdfe19b1fb5c48cad2397ad161e4fdea0198aca31d8fa9ccee1bc283bd54899e90fed13bcbd503e4	2813250
11	run orders	2026-11-23 03:10:30.934605+00	t	\\x933b28f04cedd164ed42e4b84d031d48ab212f33a9b16d3cc67a353857d2c6ad9aae5fcc38a9434ff338c8f2d6002ee9	1308791
12	run changes	2026-11-23 03:10:30.936076+00	t	\\xebe9c745ffc813429700e4118f928e21b506cc7640ca9fe9e939821d17913383bcf56c39e602be40c7c6c18be7438022	1234958
\.


--
-- Data for Name: access_settings; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.access_settings (tenant, default_role) FROM stdin;
\.


--
-- Data for Name: api_keys; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.api_keys (hash, tenant, created_at, revoked_at, name, created_by) FROM stdin;
\.


--
-- Data for Name: connection_tokens; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.connection_tokens (hash, connection_id, kind, expires_at, created_at, used_at) FROM stdin;
\.


--
-- Data for Name: connections; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.connections (id, user_id, client_id, client_name, created_at, last_used_at, revoked_at, expires_at) FROM stdin;
\.


--
-- Data for Name: data_keys; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.data_keys (tenant, wrapped_key, master_key) FROM stdin;
\.


--
-- Data for Name: feed; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.feed (id, consumer, tenant, run_id, fence, from_idx, to_idx, lease_owner, lease_until) FROM stdin;
\.


--
-- Data for Name: grants; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.grants (tenant, principal, role, allowed_labels, granted_by, granted_at) FROM stdin;
\.


--
-- Data for Name: history; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.history (run_id, idx, recorded_at, event) FROM stdin;
031deb6d-6ddc-7cb5-8e1a-a6f8b428d15a	0	1769904000000	{"v":1,"type":"run_started","script":"4525aac7fa4ad45d61eb58150e78593a80037287f06a0048aefd5345dbe113b3","action_set":"6e15f8f3f4e53953f4b554fcb718f5c3fa277d27c38682103d5e832ba7b3ba64","input":{"value":{"repo":"acme/api"}},"seed":"37df3e4937ae01f877ca3f1ab5d9e99e9e29b85942f410c9539dda8261dd09aa","epoch":1,"expires_at":3346704000000,"mode":"live"}
031deb6d-6ddc-7cb5-8e1a-a6f8b428d15a	1	1769904000000	{"v":1,"type":"schedule_tool_call","seq":1,"tool":"small","args":{"value":{"repo":"acme/api"}},"callsite":{"line":4,"column":19}}
2af4fb3b-d947-7ebb-a55c-0d794c298dc9	0	1769904000000	{"v":1,"type":"run_started","script":"bf5854b90fb9854d41d3eacd0af8cb662d9ab2fe5b47b3c6793d77d4ca840fd3","action_set":"6e15f8f3f4e53953f4b554fcb718f5c3fa277d27c38682103d5e832ba7b3ba64","input":{"value":{}},"seed":"b14e3f67a4a2a18762adb99b5f37e356275e3482821ed1f0489760bf9484f58e","epoch":1,"expires_at":3346704000000,"mode":"live"}
2af4fb3b-d947-7ebb-a55c-0d794c298dc9	1	1769904000000	{"v":1,"type":"schedule_tool_call","seq":1,"tool":"small","args":{"value":{}},"callsite":{"line":4,"column":9}}
551d70f7-e034-761a-b0bb-4c12308730b1	0	1769904000000	{"v":1,"type":"run_started","script":"ba6fba436b6a05d223f60d5960b8149fcc189a8a3f4ec21c3bc27e9b83b3a2f1","action_set":"6e15f8f3f4e53953f4b554fcb718f5c3fa277d27c38682103d5e832ba7b3ba64","input":{"value":{}},"seed":"faacc819b429e25a14532740257e12e4a2c25920ca11c42a79f16647a97c463d","epoch":1,"expires_at":3346704000000,"mode":"live"}
551d70f7-e034-761a-b0bb-4c12308730b1	1	1769904000000	{"v":1,"type":"signal_awaited","name":"never","callsite":{"line":4,"column":9}}
1f2f83db-2016-79eb-9387-81ecf0cd83a5	0	1769904000000	{"v":1,"type":"run_started","script":"19686ac5a58ac0dceeadf97d1c1a1bf582f961b3ec1308a1085f197aecf1cf2a","action_set":"6e15f8f3f4e53953f4b554fcb718f5c3fa277d27c38682103d5e832ba7b3ba64","input":{"digest":"479c8bf992635862d76895e6c1c22321d19aef2bc27b3471437de1e49f036bfa"},"seed":"a383029a2103bf26bdf767c725239dd50781c86cf4065101a177f9ec50be3e3c","epoch":1,"expires_at":3346704000000,"mode":"live"}
1f2f83db-2016-79eb-9387-81ecf0cd83a5	1	1769904000000	{"v":1,"type":"schedule_tool_call","seq":1,"tool":"small","args":{"value":{}},"callsite":{"line":5,"column":9}}
a54b0d91-1850-7392-919d-d7d434ec2c56	0	1769904000000	{"v":1,"type":"run_started","script":"d799f339ffa3c027081384c1db7ac8226dd4ed9c5bbd4ea21db628269fba29ec","action_set":"6e15f8f3f4e53953f4b554fcb718f5c3fa277d27c38682103d5e832ba7b3ba64","input":{"value":{}},"seed":"af916940ba300be28d2b59644691f6dd004f2bc4d3cf993660b77e06adae6fe4","epoch":1,"expires_at":3346704000000,"mode":"live"}
a54b0d91-1850-7392-919d-d7d434ec2c56	1	1769904000000	{"v":1,"type":"signal_awaited","name":"approval","callsite":{"line":4,"column":26}}
18b72207-70d2-77c5-8642-0c8599a04aef	0	1769904000000	{"v":1,"type":"run_started","script":"9e3b12524f88a77e812357e179138181f58e453a7b8d96c7211c8b0a518dfdd9","action_set":"6e15f8f3f4e53953f4b554fcb718f5c3fa277d27c38682103d5e832ba7b3ba64","input":{"value":{}},"seed":"a81acf8bea30bae685c1abc49f3232068252e1c26b1df867c4c5a14cbb2722fe","epoch":1,"expires_at":3346704000000,"mode":"live"}
18b72207-70d2-77c5-8642-0c8599a04aef	1	1769904000000	{"v":1,"type":"signal_awaited","name":"first","callsite":{"line":4,"column":23}}
2c582b9e-1961-750f-be22-215ba1ff1918	0	1769904000000	{"v":1,"type":"run_started","script":"9219c361e48fae3c31a965a92ea5c5e3d05a063273a1d2592b0053d495e599cb","action_set":"6e15f8f3f4e53953f4b554fcb718f5c3fa277d27c38682103d5e832ba7b3ba64","input":{"value":{}},"seed":"c04e7345d3a5742d9a8c32eae8e538cecd118a191441a227725c095336ba2124","epoch":1,"expires_at":3346704000000,"mode":"live"}
2c582b9e-1961-750f-be22-215ba1ff1918	1	1769904000000	{"v":1,"type":"schedule_tool_call","seq":1,"tool":"big","args":{"value":{}},"callsite":{"line":5,"column":19}}
6e4cacb5-4098-7d30-88c3-933c639bdaad	0	1769904000000	{"v":1,"type":"run_started","script":"da713890f0ceadac70b6a9a9886d93e95d6f0e6b03c4dac0db47da880f5b46ad","action_set":"6e15f8f3f4e53953f4b554fcb718f5c3fa277d27c38682103d5e832ba7b3ba64","input":{"value":{}},"seed":"dc74b069e81cf6c9cf78e388ff1b48131c304a4d4f02fa3b26934a1a5844956f","epoch":1,"expires_at":3346704000000,"mode":"live"}
6e4cacb5-4098-7d30-88c3-933c639bdaad	1	1769904000000	{"v":1,"type":"schedule_tool_call","seq":1,"tool":"webhook","args":{"value":{"name":"pr_merged"}},"callsite":{"line":4,"column":22}}
85df88e9-7e9c-7d21-8f60-46463925e0b1	0	1769904000000	{"v":1,"type":"run_started","script":"b7dcf637e3e2344ff84d62a0e40508279d7922fedae8fb9bde8cfa69a666b1e7","action_set":"6e15f8f3f4e53953f4b554fcb718f5c3fa277d27c38682103d5e832ba7b3ba64","input":{"value":{}},"seed":"383d2450170994740193ebeb4df11fd57300bd432d6681c7821ec19372147de0","epoch":1,"expires_at":3346704000000,"mode":"live"}
85df88e9-7e9c-7d21-8f60-46463925e0b1	1	1769904000000	{"v":1,"type":"schedule_tool_call","seq":1,"tool":"workflows.triage.start","args":{"value":{"input":{"repo":"acme/api"}}},"callsite":{"line":4,"column":28}}
55872074-cf3c-79e1-8326-dad575e80391	0	1769904000000	{"v":1,"type":"run_started","script":"1015eb715f6e4bfd752f9e290f65c3e4b01d2b0f99e7d3b3e26ceade3c64908a","action_set":"6e15f8f3f4e53953f4b554fcb718f5c3fa277d27c38682103d5e832ba7b3ba64","input":{"value":{}},"seed":"b1a2a4296e18ac483420b1372b825ee327e42787ba4f54715864a0646361f59d","epoch":1,"expires_at":3346704000000,"mode":"live"}
55872074-cf3c-79e1-8326-dad575e80391	1	1769904000000	{"v":1,"type":"signal_awaited","name":"never","callsite":{"line":4,"column":16}}
f9c322e4-79c4-735d-ae70-d0f08fa8a8b1	0	1769904000000	{"v":1,"type":"run_started","script":"20ede820a73da79169660b91916dc0dd21cde1c148b14887f436c4c43fffc4b4","action_set":"6e15f8f3f4e53953f4b554fcb718f5c3fa277d27c38682103d5e832ba7b3ba64","input":{"value":{}},"seed":"d07c10011aeea35e8038e9563b77f63ada47b75db1169803fd9f51b2f691992f","epoch":1,"expires_at":3346704000000,"mode":"live"}
f9c322e4-79c4-735d-ae70-d0f08fa8a8b1	1	1769904000000	{"v":1,"type":"schedule_tool_call","seq":1,"tool":"slow","args":{"value":{}},"callsite":{"line":4,"column":16}}
031deb6d-6ddc-7cb5-8e1a-a6f8b428d15a	2	1769904000000	{"v":1,"type":"tool_call_completed","seq":1,"result":{"value":{"ok":true}}}
031deb6d-6ddc-7cb5-8e1a-a6f8b428d15a	3	1769904000000	{"v":1,"type":"schedule_tool_call","seq":2,"tool":"medium","args":{"value":{}},"callsite":{"line":5,"column":19}}
1f2f83db-2016-79eb-9387-81ecf0cd83a5	2	1769904000000	{"v":1,"type":"tool_call_completed","seq":1,"result":{"value":{"ok":true}}}
1f2f83db-2016-79eb-9387-81ecf0cd83a5	3	1769904000000	{"v":1,"type":"start_timer","seq":2,"duration_ms":3600000,"fire_at":1769907600000,"callsite":{"line":6,"column":9}}
2af4fb3b-d947-7ebb-a55c-0d794c298dc9	2	1769904000000	{"v":1,"type":"tool_call_completed","seq":1,"result":{"value":{"ok":true}}}
2af4fb3b-d947-7ebb-a55c-0d794c298dc9	3	1769904000000	{"v":1,"type":"schedule_tool_call","seq":2,"tool":"flaky","args":{"value":{}},"callsite":{"line":5,"column":9}}
2c582b9e-1961-750f-be22-215ba1ff1918	2	1769904000000	{"v":1,"type":"tool_call_completed","seq":1,"result":{"digest":"fb3a224297affd9c23852a92179568da187a161043a76d58ca6b12ef6e529618"}}
2c582b9e-1961-750f-be22-215ba1ff1918	3	1769904000000	{"v":1,"type":"schedule_tool_call","seq":2,"tool":"medium","args":{"value":{}},"callsite":{"line":6,"column":19}}
6e4cacb5-4098-7d30-88c3-933c639bdaad	2	1769904000000	{"v":1,"type":"tool_call_completed","seq":1,"result":{"value":{"url":"https://rewind.example.com/hooks/rewind_wh_81725bdabd033d5d67585bf74945a5503c37674f32e41ee7f64c5e850580a118"}}}
6e4cacb5-4098-7d30-88c3-933c639bdaad	3	1769904000000	{"v":1,"type":"signal_awaited","name":"pr_merged","callsite":{"line":5,"column":23}}
85df88e9-7e9c-7d21-8f60-46463925e0b1	2	1769904000000	{"v":1,"type":"tool_call_completed","seq":1,"result":{"value":{"runId":"e1dced5d-e291-7195-be97-f0d0ea8e5bf6"}}}
85df88e9-7e9c-7d21-8f60-46463925e0b1	3	1769904000000	{"v":1,"type":"signal_awaited","name":"rewind:ended:e1dced5d-e291-7195-be97-f0d0ea8e5bf6","callsite":{"line":5,"column":28}}
e1dced5d-e291-7195-be97-f0d0ea8e5bf6	0	1769904000000	{"v":1,"type":"run_started","script":"c7f3ce310055574d84fd13a786ef2e5f9fcf7edc5a796b23bc7f0cca691a4af4","action_set":"f3e36efa9bae959bf0e62cf9b1da2adc53de0a970ef5b69f92dcb984a9175843","input":{"value":{"repo":"acme/api"}},"seed":"ae6f8a589ee2fb99572a81f757b1c49acc98dd54284427d35a801eeac47012a0","epoch":1,"expires_at":1772496000000,"mode":"live"}
e1dced5d-e291-7195-be97-f0d0ea8e5bf6	1	1769904000000	{"v":1,"type":"signal_awaited","name":"approval","callsite":{"line":5,"column":26}}
031deb6d-6ddc-7cb5-8e1a-a6f8b428d15a	4	1769904000000	{"v":1,"type":"tool_call_completed","seq":2,"result":{"digest":"a4492762822e7f869dc57428969f068f1b0b6d660acc8096a40ee4e9ee6b4a74"}}
031deb6d-6ddc-7cb5-8e1a-a6f8b428d15a	5	1769904000000	{"v":1,"type":"schedule_tool_call","seq":3,"tool":"big","args":{"value":{}},"callsite":{"line":6,"column":19}}
2c582b9e-1961-750f-be22-215ba1ff1918	4	1769904000000	{"v":1,"type":"tool_call_completed","seq":2,"result":{"digest":"a4492762822e7f869dc57428969f068f1b0b6d660acc8096a40ee4e9ee6b4a74"}}
2c582b9e-1961-750f-be22-215ba1ff1918	5	1769904000000	{"v":1,"type":"signal_awaited","name":"done","callsite":{"line":7,"column":22}}
18b72207-70d2-77c5-8642-0c8599a04aef	2	1769904000000	{"v":1,"type":"signal_received","name":"first","payload":{"value":{"n":1}}}
18b72207-70d2-77c5-8642-0c8599a04aef	3	1769904000000	{"v":1,"type":"signal_awaited","name":"second","callsite":{"line":5,"column":24}}
e206c2c7-f3cd-701f-b2d5-81899e11fb5d	0	1769904000000	{"v":1,"type":"run_started","script":"e4f02399335086d8f990de1200bc96742e12ea9f407712306d7d435cdfea78a6","action_set":"6e15f8f3f4e53953f4b554fcb718f5c3fa277d27c38682103d5e832ba7b3ba64","input":{"value":{}},"seed":"b14e3f67a4a2a18762adb99b5f37e356275e3482821ed1f0489760bf9484f58e","epoch":1,"expires_at":3346704000000,"mode":"live"}
e206c2c7-f3cd-701f-b2d5-81899e11fb5d	1	1769904000000	{"v":1,"type":"schedule_tool_call","seq":1,"tool":"small","args":{"value":{}},"callsite":{"line":4,"column":9}}
e206c2c7-f3cd-701f-b2d5-81899e11fb5d	2	1769904000000	{"v":1,"type":"tool_call_completed","seq":1,"result":{"value":{"ok":true}}}
e206c2c7-f3cd-701f-b2d5-81899e11fb5d	3	1769904000000	{"v":1,"type":"schedule_tool_call","seq":2,"tool":"flaky","args":{"value":{}},"callsite":{"line":6,"column":11}}
e206c2c7-f3cd-701f-b2d5-81899e11fb5d	4	1769904000000	{"v":1,"type":"tool_call_failed","seq":2,"error":{"kind":"error","message":"it broke"}}
e206c2c7-f3cd-701f-b2d5-81899e11fb5d	5	1769904000000	{"v":1,"type":"complete_run","output":{"value":{"fixed":true}}}
2af4fb3b-d947-7ebb-a55c-0d794c298dc9	4	1769904000000	{"v":1,"type":"tool_call_failed","seq":2,"error":{"kind":"error","message":"it broke"}}
2af4fb3b-d947-7ebb-a55c-0d794c298dc9	5	1769904000000	{"v":1,"type":"fail_run","error":{"name":"ToolError","message":"flaky failed: it broke","stack":"ToolError: flaky failed: it broke"}}
031deb6d-6ddc-7cb5-8e1a-a6f8b428d15a	6	1769904000000	{"v":1,"type":"tool_call_completed","seq":3,"result":{"digest":"fb3a224297affd9c23852a92179568da187a161043a76d58ca6b12ef6e529618"}}
031deb6d-6ddc-7cb5-8e1a-a6f8b428d15a	7	1769904000000	{"v":1,"type":"complete_run","output":{"value":{"a":{"ok":true},"b":100000,"m":2000,"tail":"bbbbb"}}}
551d70f7-e034-761a-b0bb-4c12308730b1	2	1769904000000	{"v":1,"type":"cancel_requested","reason":{"kind":"requested","message":"not needed"}}
551d70f7-e034-761a-b0bb-4c12308730b1	3	1769904000000	{"v":1,"type":"fail_run","error":{"name":"CancelledError","message":"run cancelled: not needed","stack":"CancelledError: run cancelled: not needed"}}
9e5651b0-ef95-7636-82c9-c08ce3ca2634	0	1769904000000	{"v":1,"type":"run_started","script":"22bff039f17f95307821bf817968bf896c40c9658b6a60a4ecaad28a92d01670","action_set":"6e15f8f3f4e53953f4b554fcb718f5c3fa277d27c38682103d5e832ba7b3ba64","input":{"value":{}},"seed":"7b318fa8f3a10d957917208574482f1d8cb8d40e1622e4808657087027245678","epoch":1,"expires_at":3346704000000,"mode":"live"}
9e5651b0-ef95-7636-82c9-c08ce3ca2634	1	1769904000000	{"v":1,"type":"schedule_tool_call","seq":1,"tool":"ask_human","args":{"value":{"question":"ship it?"}},"callsite":{"line":4,"column":24}}
\.


--
-- Data for Name: inbox; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.inbox (run_id, id, event, received_at) FROM stdin;
18b72207-70d2-77c5-8642-0c8599a04aef	31	{"v":1,"type":"signal_received","name":"second","payload":{"value":{"n":2}}}	1769904000000
\.


--
-- Data for Name: namespace_scripts; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.namespace_scripts (tenant, namespace, digest, runs, first_run_at) FROM stdin;
acme	alice	\\x2f5be40838a5e7d7f9d69bcf2d175e62d1e173c88ad75ff9b6d6ba0057229126	1	1767225600000
acme	alice	\\x4525aac7fa4ad45d61eb58150e78593a80037287f06a0048aefd5345dbe113b3	1	1769904000000
acme	alice	\\xbf5854b90fb9854d41d3eacd0af8cb662d9ab2fe5b47b3c6793d77d4ca840fd3	1	1769904000000
acme	bob	\\xba6fba436b6a05d223f60d5960b8149fcc189a8a3f4ec21c3bc27e9b83b3a2f1	1	1769904000000
acme	alice	\\x19686ac5a58ac0dceeadf97d1c1a1bf582f961b3ec1308a1085f197aecf1cf2a	1	1769904000000
acme	bob	\\xd799f339ffa3c027081384c1db7ac8226dd4ed9c5bbd4ea21db628269fba29ec	1	1769904000000
acme	alice	\\x9e3b12524f88a77e812357e179138181f58e453a7b8d96c7211c8b0a518dfdd9	1	1769904000000
acme	bob	\\x9219c361e48fae3c31a965a92ea5c5e3d05a063273a1d2592b0053d495e599cb	1	1769904000000
acme	alice	\\xda713890f0ceadac70b6a9a9886d93e95d6f0e6b03c4dac0db47da880f5b46ad	1	1769904000000
acme	alice	\\xb7dcf637e3e2344ff84d62a0e40508279d7922fedae8fb9bde8cfa69a666b1e7	1	1769904000000
acme	bob	\\x1015eb715f6e4bfd752f9e290f65c3e4b01d2b0f99e7d3b3e26ceade3c64908a	1	1769904000000
acme	alice	\\x20ede820a73da79169660b91916dc0dd21cde1c148b14887f436c4c43fffc4b4	1	1769904000000
acme	alice	\\xc7f3ce310055574d84fd13a786ef2e5f9fcf7edc5a796b23bc7f0cca691a4af4	1	1769904000000
acme	alice	\\xe4f02399335086d8f990de1200bc96742e12ea9f407712306d7d435cdfea78a6	1	1769904000000
acme	bob	\\x22bff039f17f95307821bf817968bf896c40c9658b6a60a4ecaad28a92d01670	1	1769904000000
\.


--
-- Data for Name: namespaces; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.namespaces (tenant, name, cpu_per_month_ms, active_runs, runs_per_month, scripts, max_ttl_ms, max_script_lines, tokens_revoked_before, created_at, last_active_at, deleting_since, display_name, display_key) FROM stdin;
acme	alice	\N	\N	\N	\N	\N	\N	\N	1767225600000	1769904000000	\N	\N	\N
acme	bob	\N	\N	\N	\N	\N	\N	\N	1769904000000	1769904000000	\N	\N	\N
\.


--
-- Data for Name: oauth_clients; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.oauth_clients (id, kind, name, uri, logo_uri, redirect_uris, auth_method, secret_hash, fetched_at, expires_at, created_at, last_used_at) FROM stdin;
\.


--
-- Data for Name: oauth_codes; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.oauth_codes (hash, client_id, user_id, redirect_uri, code_challenge, resource, organizations, labels, expires_at, used_at, connection_id) FROM stdin;
\.


--
-- Data for Name: payload_refs; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.payload_refs (tenant, digest, run_id) FROM stdin;
acme	\\xa4492762822e7f869dc57428969f068f1b0b6d660acc8096a40ee4e9ee6b4a74	63cbe1e4-5932-7dd7-a7cf-1707c6e1d01f
acme	\\xfb3a224297affd9c23852a92179568da187a161043a76d58ca6b12ef6e529618	63cbe1e4-5932-7dd7-a7cf-1707c6e1d01f
acme	\\x479c8bf992635862d76895e6c1c22321d19aef2bc27b3471437de1e49f036bfa	1f2f83db-2016-79eb-9387-81ecf0cd83a5
acme	\\xfb3a224297affd9c23852a92179568da187a161043a76d58ca6b12ef6e529618	2c582b9e-1961-750f-be22-215ba1ff1918
acme	\\xa4492762822e7f869dc57428969f068f1b0b6d660acc8096a40ee4e9ee6b4a74	031deb6d-6ddc-7cb5-8e1a-a6f8b428d15a
acme	\\xa4492762822e7f869dc57428969f068f1b0b6d660acc8096a40ee4e9ee6b4a74	2c582b9e-1961-750f-be22-215ba1ff1918
acme	\\xfb3a224297affd9c23852a92179568da187a161043a76d58ca6b12ef6e529618	031deb6d-6ddc-7cb5-8e1a-a6f8b428d15a
\.


--
-- Data for Name: payloads; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.payloads (tenant, digest, bytes, preview, value, unreferenced_since, deletion_started_at) FROM stdin;
acme	\\xa4492762822e7f869dc57428969f068f1b0b6d660acc8096a40ee4e9ee6b4a74	2011	{"text":"mmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmm…	{"text":"mmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmm"}	\N	\N
acme	\\xfb3a224297affd9c23852a92179568da187a161043a76d58ca6b12ef6e529618	100011	{"text":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb…	\N	\N	\N
acme	\\x479c8bf992635862d76895e6c1c22321d19aef2bc27b3471437de1e49f036bfa	3011	{"text":"iiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiii…	{"text":"iiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiii"}	\N	\N
\.


--
-- Data for Name: quota_alerts; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.quota_alerts (tenant, namespace, quota, period, threshold, used, quota_limit, raised_at, lease_owner, lease_until, sent_at) FROM stdin;
\.


--
-- Data for Name: roles; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.roles (tenant, name, description, permissions, created_at, updated_at) FROM stdin;
\.


--
-- Data for Name: run_labels; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.run_labels (run_id, key, value, tenant) FROM stdin;
63cbe1e4-5932-7dd7-a7cf-1707c6e1d01f	agent	triage	acme
63cbe1e4-5932-7dd7-a7cf-1707c6e1d01f	fixture	archived	acme
031deb6d-6ddc-7cb5-8e1a-a6f8b428d15a	agent	triage	acme
031deb6d-6ddc-7cb5-8e1a-a6f8b428d15a	fixture	completed	acme
2af4fb3b-d947-7ebb-a55c-0d794c298dc9	agent	triage	acme
2af4fb3b-d947-7ebb-a55c-0d794c298dc9	fixture	failed	acme
551d70f7-e034-761a-b0bb-4c12308730b1	agent	triage	acme
551d70f7-e034-761a-b0bb-4c12308730b1	fixture	cancelled	acme
1f2f83db-2016-79eb-9387-81ecf0cd83a5	agent	triage	acme
1f2f83db-2016-79eb-9387-81ecf0cd83a5	fixture	timer	acme
a54b0d91-1850-7392-919d-d7d434ec2c56	agent	triage	acme
a54b0d91-1850-7392-919d-d7d434ec2c56	fixture	signal	acme
18b72207-70d2-77c5-8642-0c8599a04aef	agent	triage	acme
18b72207-70d2-77c5-8642-0c8599a04aef	fixture	inbox	acme
2c582b9e-1961-750f-be22-215ba1ff1918	agent	triage	acme
2c582b9e-1961-750f-be22-215ba1ff1918	fixture	big-live	acme
6e4cacb5-4098-7d30-88c3-933c639bdaad	agent	triage	acme
6e4cacb5-4098-7d30-88c3-933c639bdaad	fixture	webhook	acme
85df88e9-7e9c-7d21-8f60-46463925e0b1	agent	triage	acme
85df88e9-7e9c-7d21-8f60-46463925e0b1	fixture	parent	acme
55872074-cf3c-79e1-8326-dad575e80391	agent	triage	acme
55872074-cf3c-79e1-8326-dad575e80391	fixture	quarantined	acme
f9c322e4-79c4-735d-ae70-d0f08fa8a8b1	agent	triage	acme
f9c322e4-79c4-735d-ae70-d0f08fa8a8b1	fixture	deferred	acme
e1dced5d-e291-7195-be97-f0d0ea8e5bf6	agent	triage	acme
e1dced5d-e291-7195-be97-f0d0ea8e5bf6	fixture	parent	acme
e206c2c7-f3cd-701f-b2d5-81899e11fb5d	fixture	fork	acme
9e5651b0-ef95-7636-82c9-c08ce3ca2634	agent	triage	acme
9e5651b0-ef95-7636-82c9-c08ce3ca2634	fixture	host	acme
\.


--
-- Data for Name: run_quarantine; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.run_quarantine (run_id, tenant, recorded_at, kind, reason, since) FROM stdin;
63cbe1e4-5932-7dd7-a7cf-1707c6e1d01f	acme	1767225600000	\N	\N	\N
031deb6d-6ddc-7cb5-8e1a-a6f8b428d15a	acme	1769904000000	\N	\N	\N
2af4fb3b-d947-7ebb-a55c-0d794c298dc9	acme	1769904000000	\N	\N	\N
551d70f7-e034-761a-b0bb-4c12308730b1	acme	1769904000000	\N	\N	\N
55872074-cf3c-79e1-8326-dad575e80391	acme	1769904000000	failures	its activations never finished	1769904000000
e206c2c7-f3cd-701f-b2d5-81899e11fb5d	acme	1769904000000	\N	\N	\N
\.


--
-- Data for Name: run_search; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.run_search (run_id, tenant, search_text) FROM stdin;
63cbe1e4-5932-7dd7-a7cf-1707c6e1d01f	acme	63cbe1e4-5932-7dd7-a7cf-1707c6e1d01fAimport { small, big, medium } from "rewind:tools";AtriageAarchived
031deb6d-6ddc-7cb5-8e1a-a6f8b428d15a	acme	031deb6d-6ddc-7cb5-8e1a-a6f8b428d15aAimport { small, medium, big } from "rewind:tools";AtriageAcompleted
1f2f83db-2016-79eb-9387-81ecf0cd83a5	acme	1f2f83db-2016-79eb-9387-81ecf0cd83a5Aimport { sleep } from "rewind";AtriageAtimer
a54b0d91-1850-7392-919d-d7d434ec2c56	acme	a54b0d91-1850-7392-919d-d7d434ec2c56Aimport { signal } from "rewind";AtriageAsignal
18b72207-70d2-77c5-8642-0c8599a04aef	acme	18b72207-70d2-77c5-8642-0c8599a04aefAimport { signal } from "rewind";AtriageAinbox
2c582b9e-1961-750f-be22-215ba1ff1918	acme	2c582b9e-1961-750f-be22-215ba1ff1918Aimport { signal } from "rewind";AtriageAbig-live
6e4cacb5-4098-7d30-88c3-933c639bdaad	acme	6e4cacb5-4098-7d30-88c3-933c639bdaadAimport { webhook, signal } from "rewind";AtriageAwebhook
85df88e9-7e9c-7d21-8f60-46463925e0b1	acme	85df88e9-7e9c-7d21-8f60-46463925e0b1Aimport { triage } from "rewind:workflows";AtriageAparent
55872074-cf3c-79e1-8326-dad575e80391	acme	55872074-cf3c-79e1-8326-dad575e80391Aimport { signal } from "rewind";AtriageAquarantined
f9c322e4-79c4-735d-ae70-d0f08fa8a8b1	acme	f9c322e4-79c4-735d-ae70-d0f08fa8a8b1Aimport { slow } from "rewind:tools";AtriageAdeferred
e1dced5d-e291-7195-be97-f0d0ea8e5bf6	acme	e1dced5d-e291-7195-be97-f0d0ea8e5bf6Aimport { signal } from "rewind";AtriageAparent
2af4fb3b-d947-7ebb-a55c-0d794c298dc9	acme	2af4fb3b-d947-7ebb-a55c-0d794c298dc9Aimport { small, flaky } from "rewind:tools";AtoolerrorAflaky failed: it brokeAtriageAfailed
551d70f7-e034-761a-b0bb-4c12308730b1	acme	551d70f7-e034-761a-b0bb-4c12308730b1Aimport { signal } from "rewind";AcancellederrorArun cancelled: not neededAtriageAcancelled
e206c2c7-f3cd-701f-b2d5-81899e11fb5d	acme	e206c2c7-f3cd-701f-b2d5-81899e11fb5dAimport { small, flaky } from "rewind:tools";Afork
9e5651b0-ef95-7636-82c9-c08ce3ca2634	acme	9e5651b0-ef95-7636-82c9-c08ce3ca2634Aimport { ask_human } from "rewind:tools";AtriageAhost
\.


--
-- Data for Name: run_summaries; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.run_summaries (run_id, tenant, script, toolset, headline, parent, labels, status, started_at, ended_at, last_activity_at, tool_calls, errors, history_len, snapshot_len, mode, wakes_at, error, message, line, signature, fingerprint, workflow, started_by, namespace, changed_at) FROM stdin;
63cbe1e4-5932-7dd7-a7cf-1707c6e1d01f	acme	\\x2f5be40838a5e7d7f9d69bcf2d175e62d1e173c88ad75ff9b6d6ba0057229126	\\x6e15f8f3f4e53953f4b554fcb718f5c3fa277d27c38682103d5e832ba7b3ba64	import { small, big, medium } from "rewind:tools";	\N	{"agent": "triage", "fixture": "archived"}	completed	1767225600000	1767225600000	1767225600000	3	0	8	8	live	\N	\N	\N	\N	\N	\N	\N	\N	alice	1795403431209
a54b0d91-1850-7392-919d-d7d434ec2c56	acme	\\xd799f339ffa3c027081384c1db7ac8226dd4ed9c5bbd4ea21db628269fba29ec	\\x6e15f8f3f4e53953f4b554fcb718f5c3fa277d27c38682103d5e832ba7b3ba64	import { signal } from "rewind";	\N	{"agent": "triage", "fixture": "signal"}	active	1769904000000	\N	1769904000000	0	0	2	2	live	\N	\N	\N	\N	\N	\N	\N	\N	bob	1795403431734
55872074-cf3c-79e1-8326-dad575e80391	acme	\\x1015eb715f6e4bfd752f9e290f65c3e4b01d2b0f99e7d3b3e26ceade3c64908a	\\x6e15f8f3f4e53953f4b554fcb718f5c3fa277d27c38682103d5e832ba7b3ba64	import { signal } from "rewind";	\N	{"agent": "triage", "fixture": "quarantined"}	active	1769904000000	\N	1769904000000	0	0	2	2	live	\N	\N	\N	\N	\N	\N	\N	\N	bob	1795403431759
f9c322e4-79c4-735d-ae70-d0f08fa8a8b1	acme	\\x20ede820a73da79169660b91916dc0dd21cde1c148b14887f436c4c43fffc4b4	\\x6e15f8f3f4e53953f4b554fcb718f5c3fa277d27c38682103d5e832ba7b3ba64	import { slow } from "rewind:tools";	\N	{"agent": "triage", "fixture": "deferred"}	active	1769904000000	\N	1769904000000	1	0	2	2	live	\N	\N	\N	\N	\N	\N	\N	\N	alice	1795403431765
1f2f83db-2016-79eb-9387-81ecf0cd83a5	acme	\\x19686ac5a58ac0dceeadf97d1c1a1bf582f961b3ec1308a1085f197aecf1cf2a	\\x6e15f8f3f4e53953f4b554fcb718f5c3fa277d27c38682103d5e832ba7b3ba64	import { sleep } from "rewind";	\N	{"agent": "triage", "fixture": "timer"}	active	1769904000000	\N	1769904000000	1	0	4	4	live	1769907600000	\N	\N	\N	\N	\N	\N	\N	alice	1795403431772
6e4cacb5-4098-7d30-88c3-933c639bdaad	acme	\\xda713890f0ceadac70b6a9a9886d93e95d6f0e6b03c4dac0db47da880f5b46ad	\\x6e15f8f3f4e53953f4b554fcb718f5c3fa277d27c38682103d5e832ba7b3ba64	import { webhook, signal } from "rewind";	\N	{"agent": "triage", "fixture": "webhook"}	active	1769904000000	\N	1769904000000	1	0	4	4	live	\N	\N	\N	\N	\N	\N	\N	\N	alice	1795403431783
85df88e9-7e9c-7d21-8f60-46463925e0b1	acme	\\xb7dcf637e3e2344ff84d62a0e40508279d7922fedae8fb9bde8cfa69a666b1e7	\\x6e15f8f3f4e53953f4b554fcb718f5c3fa277d27c38682103d5e832ba7b3ba64	import { triage } from "rewind:workflows";	\N	{"agent": "triage", "fixture": "parent"}	active	1769904000000	\N	1769904000000	1	0	4	4	live	\N	\N	\N	\N	\N	\N	\N	\N	alice	1795403431786
e1dced5d-e291-7195-be97-f0d0ea8e5bf6	acme	\\xc7f3ce310055574d84fd13a786ef2e5f9fcf7edc5a796b23bc7f0cca691a4af4	\\xf3e36efa9bae959bf0e62cf9b1da2adc53de0a970ef5b69f92dcb984a9175843	import { signal } from "rewind";	\N	{"agent": "triage", "fixture": "parent"}	active	1769904000000	\N	1769904000000	0	0	2	2	live	\N	\N	\N	\N	\N	\N	triage	85df88e9-7e9c-7d21-8f60-46463925e0b1	alice	1795403431790
2af4fb3b-d947-7ebb-a55c-0d794c298dc9	acme	\\xbf5854b90fb9854d41d3eacd0af8cb662d9ab2fe5b47b3c6793d77d4ca840fd3	\\x6e15f8f3f4e53953f4b554fcb718f5c3fa277d27c38682103d5e832ba7b3ba64	import { small, flaky } from "rewind:tools";	\N	{"agent": "triage", "fixture": "failed"}	failed	1769904000000	1769904000000	1769904000000	2	1	6	6	live	\N	ToolError	flaky failed: it broke	\N	ToolError: flaky failed: it broke	7b7ec80ba2c85638	\N	\N	alice	1795403431802
2c582b9e-1961-750f-be22-215ba1ff1918	acme	\\x9219c361e48fae3c31a965a92ea5c5e3d05a063273a1d2592b0053d495e599cb	\\x6e15f8f3f4e53953f4b554fcb718f5c3fa277d27c38682103d5e832ba7b3ba64	import { signal } from "rewind";	\N	{"agent": "triage", "fixture": "big-live"}	active	1769904000000	\N	1769904000000	2	0	6	6	live	\N	\N	\N	\N	\N	\N	\N	\N	bob	1795403431805
031deb6d-6ddc-7cb5-8e1a-a6f8b428d15a	acme	\\x4525aac7fa4ad45d61eb58150e78593a80037287f06a0048aefd5345dbe113b3	\\x6e15f8f3f4e53953f4b554fcb718f5c3fa277d27c38682103d5e832ba7b3ba64	import { small, medium, big } from "rewind:tools";	\N	{"agent": "triage", "fixture": "completed"}	completed	1769904000000	1769904000000	1769904000000	3	0	8	8	live	\N	\N	\N	\N	\N	\N	\N	\N	alice	1795403431808
18b72207-70d2-77c5-8642-0c8599a04aef	acme	\\x9e3b12524f88a77e812357e179138181f58e453a7b8d96c7211c8b0a518dfdd9	\\x6e15f8f3f4e53953f4b554fcb718f5c3fa277d27c38682103d5e832ba7b3ba64	import { signal } from "rewind";	\N	{"agent": "triage", "fixture": "inbox"}	active	1769904000000	\N	1769904000000	0	0	4	4	live	\N	\N	\N	\N	\N	\N	\N	\N	alice	1795403431811
551d70f7-e034-761a-b0bb-4c12308730b1	acme	\\xba6fba436b6a05d223f60d5960b8149fcc189a8a3f4ec21c3bc27e9b83b3a2f1	\\x6e15f8f3f4e53953f4b554fcb718f5c3fa277d27c38682103d5e832ba7b3ba64	import { signal } from "rewind";	\N	{"agent": "triage", "fixture": "cancelled"}	cancelled	1769904000000	1769904000000	1769904000000	0	0	4	4	live	\N	CancelledError	run cancelled: not needed	\N	CancelledError: run cancelled: not needed	6d167c9f61862508	\N	\N	bob	1795403431813
e206c2c7-f3cd-701f-b2d5-81899e11fb5d	acme	\\xe4f02399335086d8f990de1200bc96742e12ea9f407712306d7d435cdfea78a6	\\x6e15f8f3f4e53953f4b554fcb718f5c3fa277d27c38682103d5e832ba7b3ba64	import { small, flaky } from "rewind:tools";	2af4fb3b-d947-7ebb-a55c-0d794c298dc9	{"fixture": "fork"}	completed	1769904000000	1769904000000	1769904000000	2	1	6	6	live	\N	\N	\N	\N	\N	\N	\N	\N	alice	1795403431818
9e5651b0-ef95-7636-82c9-c08ce3ca2634	acme	\\x22bff039f17f95307821bf817968bf896c40c9658b6a60a4ecaad28a92d01670	\\x6e15f8f3f4e53953f4b554fcb718f5c3fa277d27c38682103d5e832ba7b3ba64	import { ask_human } from "rewind:tools";	\N	{"agent": "triage", "fixture": "host"}	active	1769904000000	\N	1769904000000	1	0	2	2	live	\N	\N	\N	\N	\N	\N	\N	\N	bob	1795403431824
\.


--
-- Data for Name: run_waits; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.run_waits (run_id, "position", tenant, kind, seq, name, since, until, callsite_line, callsite_column) FROM stdin;
a54b0d91-1850-7392-919d-d7d434ec2c56	0	acme	signal	\N	approval	1769904000000	\N	4	26
55872074-cf3c-79e1-8326-dad575e80391	0	acme	signal	\N	never	1769904000000	\N	4	16
f9c322e4-79c4-735d-ae70-d0f08fa8a8b1	0	acme	host	1	slow	1769904000000	\N	4	16
1f2f83db-2016-79eb-9387-81ecf0cd83a5	0	acme	timer	2	\N	1769904000000	1769907600000	6	9
6e4cacb5-4098-7d30-88c3-933c639bdaad	0	acme	signal	\N	pr_merged	1769904000000	\N	5	23
85df88e9-7e9c-7d21-8f60-46463925e0b1	0	acme	signal	\N	rewind:ended:e1dced5d-e291-7195-be97-f0d0ea8e5bf6	1769904000000	\N	5	28
e1dced5d-e291-7195-be97-f0d0ea8e5bf6	0	acme	signal	\N	approval	1769904000000	\N	5	26
2c582b9e-1961-750f-be22-215ba1ff1918	0	acme	signal	\N	done	1769904000000	\N	7	22
18b72207-70d2-77c5-8642-0c8599a04aef	0	acme	signal	\N	second	1769904000000	\N	5	24
9e5651b0-ef95-7636-82c9-c08ce3ca2634	0	acme	host	1	ask_human	1769904000000	\N	4	24
\.


--
-- Data for Name: run_watchers; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.run_watchers (run_id, watcher, seq, attached) FROM stdin;
e1dced5d-e291-7195-be97-f0d0ea8e5bf6	85df88e9-7e9c-7d21-8f60-46463925e0b1	1	t
\.


--
-- Data for Name: runs; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.runs (tenant, id, script, toolset, status, epoch, history_len, expires_at, expiry_enqueued, quarantine_kind, quarantine_reason, quarantine_since, quarantine_retried_by, failures, attempts, lease_owner, lease_until, fence, inbox_head, runnable_at, sticky_worker, labels, traceparent, tracestate, parent, parent_at, idempotency_key, created_at, updated_at, ended_at, archive_document, archived_at, workflow, workflow_version, started_by, started_seq, detached, scope, namespace, secrets) FROM stdin;
acme	63cbe1e4-5932-7dd7-a7cf-1707c6e1d01f	\\x2f5be40838a5e7d7f9d69bcf2d175e62d1e173c88ad75ff9b6d6ba0057229126	\\x6e15f8f3f4e53953f4b554fcb718f5c3fa277d27c38682103d5e832ba7b3ba64	completed	1	8	3344025600000	f	\N	\N	\N	\N	0	0	\N	\N	4	\N	\N	worker	{"agent": "triage", "fixture": "archived"}	\N	\N	\N	\N	\N	1767225600000	1767225600000	1767225600000	\\x19496568cf10c4979d4e5a3212348be1434673e0138e629996c69eefeac13b85	1769904000000	\N	\N	\N	\N	\N	{}	alice	{}
acme	6e4cacb5-4098-7d30-88c3-933c639bdaad	\\xda713890f0ceadac70b6a9a9886d93e95d6f0e6b03c4dac0db47da880f5b46ad	\\x6e15f8f3f4e53953f4b554fcb718f5c3fa277d27c38682103d5e832ba7b3ba64	active	1	4	3346704000000	f	\N	\N	\N	\N	0	0	\N	\N	2	\N	\N	worker	{"agent": "triage", "fixture": "webhook"}	\N	\N	\N	\N	\N	1769904000000	1769904000000	\N	\N	\N	\N	\N	\N	\N	\N	{}	alice	{}
acme	1f2f83db-2016-79eb-9387-81ecf0cd83a5	\\x19686ac5a58ac0dceeadf97d1c1a1bf582f961b3ec1308a1085f197aecf1cf2a	\\x6e15f8f3f4e53953f4b554fcb718f5c3fa277d27c38682103d5e832ba7b3ba64	active	1	4	3346704000000	f	\N	\N	\N	\N	0	0	\N	\N	2	\N	\N	worker	{"agent": "triage", "fixture": "timer"}	\N	\N	\N	\N	\N	1769904000000	1769904000000	\N	\N	\N	\N	\N	\N	\N	\N	{}	alice	{}
acme	f9c322e4-79c4-735d-ae70-d0f08fa8a8b1	\\x20ede820a73da79169660b91916dc0dd21cde1c148b14887f436c4c43fffc4b4	\\x6e15f8f3f4e53953f4b554fcb718f5c3fa277d27c38682103d5e832ba7b3ba64	active	1	2	3346704000000	f	\N	\N	\N	\N	0	0	\N	\N	1	\N	\N	worker	{"agent": "triage", "fixture": "deferred"}	\N	\N	\N	\N	\N	1769904000000	1769904000000	\N	\N	\N	\N	\N	\N	\N	\N	{}	alice	{}
acme	a54b0d91-1850-7392-919d-d7d434ec2c56	\\xd799f339ffa3c027081384c1db7ac8226dd4ed9c5bbd4ea21db628269fba29ec	\\x6e15f8f3f4e53953f4b554fcb718f5c3fa277d27c38682103d5e832ba7b3ba64	active	1	2	3346704000000	f	\N	\N	\N	\N	0	0	\N	\N	1	\N	\N	worker	{"agent": "triage", "fixture": "signal"}	\N	\N	\N	\N	start-signal	1769904000000	1769904000000	\N	\N	\N	\N	\N	\N	\N	\N	{}	bob	{}
acme	551d70f7-e034-761a-b0bb-4c12308730b1	\\xba6fba436b6a05d223f60d5960b8149fcc189a8a3f4ec21c3bc27e9b83b3a2f1	\\x6e15f8f3f4e53953f4b554fcb718f5c3fa277d27c38682103d5e832ba7b3ba64	cancelled	1	4	3346704000000	f	\N	\N	\N	\N	0	0	\N	\N	2	\N	\N	worker	{"agent": "triage", "fixture": "cancelled"}	\N	\N	\N	\N	\N	1769904000000	1769904000000	1769904000000	\N	\N	\N	\N	\N	\N	\N	{}	bob	{}
acme	85df88e9-7e9c-7d21-8f60-46463925e0b1	\\xb7dcf637e3e2344ff84d62a0e40508279d7922fedae8fb9bde8cfa69a666b1e7	\\x6e15f8f3f4e53953f4b554fcb718f5c3fa277d27c38682103d5e832ba7b3ba64	active	1	4	3346704000000	f	\N	\N	\N	\N	0	0	\N	\N	2	\N	\N	worker	{"agent": "triage", "fixture": "parent"}	\N	\N	\N	\N	\N	1769904000000	1769904000000	\N	\N	\N	\N	\N	\N	\N	\N	{}	alice	{}
acme	e1dced5d-e291-7195-be97-f0d0ea8e5bf6	\\xc7f3ce310055574d84fd13a786ef2e5f9fcf7edc5a796b23bc7f0cca691a4af4	\\xf3e36efa9bae959bf0e62cf9b1da2adc53de0a970ef5b69f92dcb984a9175843	active	1	2	1772496000000	f	\N	\N	\N	\N	0	0	\N	\N	1	\N	\N	worker	{"agent": "triage", "fixture": "parent"}	00-85df88e97e9c7d218f6046463925e0b1-72066c9b002f15ac-01	\N	\N	\N	rewind:85df88e9-7e9c-7d21-8f60-46463925e0b1/1	1769904000000	1769904000000	\N	\N	\N	triage	\\x5739537ce2f07ea2ba822a564517c7e9d5035582143579548c6b4701a6a19e87	85df88e9-7e9c-7d21-8f60-46463925e0b1	1	f	{}	alice	{}
acme	031deb6d-6ddc-7cb5-8e1a-a6f8b428d15a	\\x4525aac7fa4ad45d61eb58150e78593a80037287f06a0048aefd5345dbe113b3	\\x6e15f8f3f4e53953f4b554fcb718f5c3fa277d27c38682103d5e832ba7b3ba64	completed	1	8	3346704000000	f	\N	\N	\N	\N	0	0	\N	\N	4	\N	\N	worker	{"agent": "triage", "fixture": "completed"}	\N	\N	\N	\N	start-completed	1769904000000	1769904000000	1769904000000	\N	\N	\N	\N	\N	\N	\N	{}	alice	{}
acme	2af4fb3b-d947-7ebb-a55c-0d794c298dc9	\\xbf5854b90fb9854d41d3eacd0af8cb662d9ab2fe5b47b3c6793d77d4ca840fd3	\\x6e15f8f3f4e53953f4b554fcb718f5c3fa277d27c38682103d5e832ba7b3ba64	failed	1	6	3346704000000	f	\N	\N	\N	\N	0	0	\N	\N	3	\N	\N	worker	{"agent": "triage", "fixture": "failed"}	\N	\N	\N	\N	\N	1769904000000	1769904000000	1769904000000	\N	\N	\N	\N	\N	\N	\N	{}	alice	{}
acme	2c582b9e-1961-750f-be22-215ba1ff1918	\\x9219c361e48fae3c31a965a92ea5c5e3d05a063273a1d2592b0053d495e599cb	\\x6e15f8f3f4e53953f4b554fcb718f5c3fa277d27c38682103d5e832ba7b3ba64	active	1	6	3346704000000	f	\N	\N	\N	\N	0	0	\N	\N	3	\N	\N	worker	{"agent": "triage", "fixture": "big-live"}	\N	\N	\N	\N	\N	1769904000000	1769904000000	\N	\N	\N	\N	\N	\N	\N	\N	{}	bob	{}
acme	9e5651b0-ef95-7636-82c9-c08ce3ca2634	\\x22bff039f17f95307821bf817968bf896c40c9658b6a60a4ecaad28a92d01670	\\x6e15f8f3f4e53953f4b554fcb718f5c3fa277d27c38682103d5e832ba7b3ba64	active	1	2	3346704000000	f	\N	\N	\N	\N	0	0	\N	\N	1	\N	\N	worker	{"agent": "triage", "fixture": "host"}	\N	\N	\N	\N	\N	1769904000000	1769904000000	\N	\N	\N	\N	\N	\N	\N	\N	{}	bob	{}
acme	18b72207-70d2-77c5-8642-0c8599a04aef	\\x9e3b12524f88a77e812357e179138181f58e453a7b8d96c7211c8b0a518dfdd9	\\x6e15f8f3f4e53953f4b554fcb718f5c3fa277d27c38682103d5e832ba7b3ba64	active	1	4	3346704000000	f	\N	\N	\N	\N	0	0	\N	\N	2	31	1769904000000	worker	{"agent": "triage", "fixture": "inbox"}	\N	\N	\N	\N	\N	1769904000000	1769904000000	\N	\N	\N	\N	\N	\N	\N	\N	{}	alice	{}
acme	e206c2c7-f3cd-701f-b2d5-81899e11fb5d	\\xe4f02399335086d8f990de1200bc96742e12ea9f407712306d7d435cdfea78a6	\\x6e15f8f3f4e53953f4b554fcb718f5c3fa277d27c38682103d5e832ba7b3ba64	completed	1	6	3346704000000	f	\N	\N	\N	\N	0	0	\N	\N	1	\N	\N	worker	{"fixture": "fork"}	\N	\N	2af4fb3b-d947-7ebb-a55c-0d794c298dc9	\N	\N	1769904000000	1769904000000	1769904000000	\N	\N	\N	\N	\N	\N	\N	{}	alice	{}
acme	55872074-cf3c-79e1-8326-dad575e80391	\\x1015eb715f6e4bfd752f9e290f65c3e4b01d2b0f99e7d3b3e26ceade3c64908a	\\x6e15f8f3f4e53953f4b554fcb718f5c3fa277d27c38682103d5e832ba7b3ba64	active	1	2	3346704000000	f	failures	its activations never finished	1769904000000	\N	0	0	\N	\N	2	\N	\N	worker	{"agent": "triage", "fixture": "quarantined"}	\N	\N	\N	\N	\N	1769904000000	1769904000000	\N	\N	\N	\N	\N	\N	\N	\N	{}	bob	{}
\.


--
-- Data for Name: scripts; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.scripts (tenant, digest, source, transpiled, source_map) FROM stdin;
acme	\\xc7f3ce310055574d84fd13a786ef2e5f9fcf7edc5a796b23bc7f0cca691a4af4	\nimport { signal } from "rewind";\n\nexport default async function main(input: { repo: string }) {\n  const approval = await signal<{ close: boolean }>("approval");\n  return { closed: approval.close ? 3 : 0, repo: input.repo };\n}\n	import { signal } from "rewind";\nexport default async function main(input) {\n  const approval = await signal("approval");\n  return {\n    closed: approval.close ? 3 : 0,\n    repo: input.repo\n  };\n}\n	{"version":3,"sources":["file:///script.ts"],"sourcesContent":["\\nimport { signal } from \\"rewind\\";\\n\\nexport default async function main(input: { repo: string }) {\\n  const approval = await signal<{ close: boolean }>(\\"approval\\");\\n  return { closed: approval.close ? 3 : 0, repo: input.repo };\\n}\\n"],"names":[],"mappings":"AACA,SAAS,MAAM,QAAQ,SAAS;AAEhC,eAAe,eAAe,KAAK,KAAuB;EACxD,MAAM,WAAW,MAAM,OAA2B;EAClD,OAAO;IAAE,QAAQ,SAAS,KAAK,GAAG,IAAI;IAAG,MAAM,MAAM,IAAI;EAAC;AAC5D"}
acme	\\x2f5be40838a5e7d7f9d69bcf2d175e62d1e173c88ad75ff9b6d6ba0057229126	\nimport { small, big, medium } from "rewind:tools";\nexport default async function main(input: { n: number }) {\n  const a = await small({});\n  const m = await medium({});\n  const b = await big({});\n  return { a, m: m.text.length, b: b.text.length, n: input.n };\n}\n	import { small, big, medium } from "rewind:tools";\nexport default async function main(input) {\n  const a = await small({});\n  const m = await medium({});\n  const b = await big({});\n  return {\n    a,\n    m: m.text.length,\n    b: b.text.length,\n    n: input.n\n  };\n}\n	{"version":3,"sources":["file:///script.ts"],"sourcesContent":["\\nimport { small, big, medium } from \\"rewind:tools\\";\\nexport default async function main(input: { n: number }) {\\n  const a = await small({});\\n  const m = await medium({});\\n  const b = await big({});\\n  return { a, m: m.text.length, b: b.text.length, n: input.n };\\n}\\n"],"names":[],"mappings":"AACA,SAAS,KAAK,EAAE,GAAG,EAAE,MAAM,QAAQ,eAAe;AAClD,eAAe,eAAe,KAAK,KAAoB;EACrD,MAAM,IAAI,MAAM,MAAM,CAAC;EACvB,MAAM,IAAI,MAAM,OAAO,CAAC;EACxB,MAAM,IAAI,MAAM,IAAI,CAAC;EACrB,OAAO;IAAE;IAAG,GAAG,EAAE,IAAI,CAAC,MAAM;IAAE,GAAG,EAAE,IAAI,CAAC,MAAM;IAAE,GAAG,MAAM,CAAC;EAAC;AAC7D"}
acme	\\x4525aac7fa4ad45d61eb58150e78593a80037287f06a0048aefd5345dbe113b3	\nimport { small, medium, big } from "rewind:tools";\nexport default async function main(input: { repo: string }) {\n  const a = await small({ repo: input.repo });\n  const m = await medium({});\n  const b = await big({});\n  return { a, m: m.text.length, b: b.text.length, tail: b.text.slice(0, 5) };\n}\n	import { small, medium, big } from "rewind:tools";\nexport default async function main(input) {\n  const a = await small({\n    repo: input.repo\n  });\n  const m = await medium({});\n  const b = await big({});\n  return {\n    a,\n    m: m.text.length,\n    b: b.text.length,\n    tail: b.text.slice(0, 5)\n  };\n}\n	{"version":3,"sources":["file:///script.ts"],"sourcesContent":["\\nimport { small, medium, big } from \\"rewind:tools\\";\\nexport default async function main(input: { repo: string }) {\\n  const a = await small({ repo: input.repo });\\n  const m = await medium({});\\n  const b = await big({});\\n  return { a, m: m.text.length, b: b.text.length, tail: b.text.slice(0, 5) };\\n}\\n"],"names":[],"mappings":"AACA,SAAS,KAAK,EAAE,MAAM,EAAE,GAAG,QAAQ,eAAe;AAClD,eAAe,eAAe,KAAK,KAAuB;EACxD,MAAM,IAAI,MAAM,MAAM;IAAE,MAAM,MAAM,IAAI;EAAC;EACzC,MAAM,IAAI,MAAM,OAAO,CAAC;EACxB,MAAM,IAAI,MAAM,IAAI,CAAC;EACrB,OAAO;IAAE;IAAG,GAAG,EAAE,IAAI,CAAC,MAAM;IAAE,GAAG,EAAE,IAAI,CAAC,MAAM;IAAE,MAAM,EAAE,IAAI,CAAC,KAAK,CAAC,GAAG;EAAG;AAC3E"}
acme	\\xbf5854b90fb9854d41d3eacd0af8cb662d9ab2fe5b47b3c6793d77d4ca840fd3	\nimport { small, flaky } from "rewind:tools";\nexport default async function main() {\n  await small({});\n  await flaky({});\n  return "unreachable";\n}\n	import { small, flaky } from "rewind:tools";\nexport default async function main() {\n  await small({});\n  await flaky({});\n  return "unreachable";\n}\n	{"version":3,"sources":["file:///script.ts"],"sourcesContent":["\\nimport { small, flaky } from \\"rewind:tools\\";\\nexport default async function main() {\\n  await small({});\\n  await flaky({});\\n  return \\"unreachable\\";\\n}\\n"],"names":[],"mappings":"AACA,SAAS,KAAK,EAAE,KAAK,QAAQ,eAAe;AAC5C,eAAe,eAAe;EAC5B,MAAM,MAAM,CAAC;EACb,MAAM,MAAM,CAAC;EACb,OAAO;AACT"}
acme	\\xba6fba436b6a05d223f60d5960b8149fcc189a8a3f4ec21c3bc27e9b83b3a2f1	\nimport { signal } from "rewind";\nexport default async function main() {\n  await signal("never");\n}\n	import { signal } from "rewind";\nexport default async function main() {\n  await signal("never");\n}\n	{"version":3,"sources":["file:///script.ts"],"sourcesContent":["\\nimport { signal } from \\"rewind\\";\\nexport default async function main() {\\n  await signal(\\"never\\");\\n}\\n"],"names":[],"mappings":"AACA,SAAS,MAAM,QAAQ,SAAS;AAChC,eAAe,eAAe;EAC5B,MAAM,OAAO;AACf"}
acme	\\x19686ac5a58ac0dceeadf97d1c1a1bf582f961b3ec1308a1085f197aecf1cf2a	\nimport { sleep } from "rewind";\nimport { small } from "rewind:tools";\nexport default async function main(input: { text: string }) {\n  await small({});\n  await sleep("1h");\n  return { slept: true, size: input.text.length };\n}\n	import { sleep } from "rewind";\nimport { small } from "rewind:tools";\nexport default async function main(input) {\n  await small({});\n  await sleep("1h");\n  return {\n    slept: true,\n    size: input.text.length\n  };\n}\n	{"version":3,"sources":["file:///script.ts"],"sourcesContent":["\\nimport { sleep } from \\"rewind\\";\\nimport { small } from \\"rewind:tools\\";\\nexport default async function main(input: { text: string }) {\\n  await small({});\\n  await sleep(\\"1h\\");\\n  return { slept: true, size: input.text.length };\\n}\\n"],"names":[],"mappings":"AACA,SAAS,KAAK,QAAQ,SAAS;AAC/B,SAAS,KAAK,QAAQ,eAAe;AACrC,eAAe,eAAe,KAAK,KAAuB;EACxD,MAAM,MAAM,CAAC;EACb,MAAM,MAAM;EACZ,OAAO;IAAE,OAAO;IAAM,MAAM,MAAM,IAAI,CAAC,MAAM;EAAC;AAChD"}
acme	\\xd799f339ffa3c027081384c1db7ac8226dd4ed9c5bbd4ea21db628269fba29ec	\nimport { signal } from "rewind";\nexport default async function main() {\n  const approval = await signal<{ ok: boolean }>("approval");\n  return { approval };\n}\n	import { signal } from "rewind";\nexport default async function main() {\n  const approval = await signal("approval");\n  return {\n    approval\n  };\n}\n	{"version":3,"sources":["file:///script.ts"],"sourcesContent":["\\nimport { signal } from \\"rewind\\";\\nexport default async function main() {\\n  const approval = await signal<{ ok: boolean }>(\\"approval\\");\\n  return { approval };\\n}\\n"],"names":[],"mappings":"AACA,SAAS,MAAM,QAAQ,SAAS;AAChC,eAAe,eAAe;EAC5B,MAAM,WAAW,MAAM,OAAwB;EAC/C,OAAO;IAAE;EAAS;AACpB"}
acme	\\x9e3b12524f88a77e812357e179138181f58e453a7b8d96c7211c8b0a518dfdd9	\nimport { signal } from "rewind";\nexport default async function main() {\n  const first = await signal("first");\n  const second = await signal("second");\n  return { first, second };\n}\n	import { signal } from "rewind";\nexport default async function main() {\n  const first = await signal("first");\n  const second = await signal("second");\n  return {\n    first,\n    second\n  };\n}\n	{"version":3,"sources":["file:///script.ts"],"sourcesContent":["\\nimport { signal } from \\"rewind\\";\\nexport default async function main() {\\n  const first = await signal(\\"first\\");\\n  const second = await signal(\\"second\\");\\n  return { first, second };\\n}\\n"],"names":[],"mappings":"AACA,SAAS,MAAM,QAAQ,SAAS;AAChC,eAAe,eAAe;EAC5B,MAAM,QAAQ,MAAM,OAAO;EAC3B,MAAM,SAAS,MAAM,OAAO;EAC5B,OAAO;IAAE;IAAO;EAAO;AACzB"}
acme	\\x9219c361e48fae3c31a965a92ea5c5e3d05a063273a1d2592b0053d495e599cb	\nimport { signal } from "rewind";\nimport { big, medium } from "rewind:tools";\nexport default async function main() {\n  const b = await big({});\n  const m = await medium({});\n  const done = await signal("done");\n  return { b: b.text.length, m: m.text.length, done };\n}\n	import { signal } from "rewind";\nimport { big, medium } from "rewind:tools";\nexport default async function main() {\n  const b = await big({});\n  const m = await medium({});\n  const done = await signal("done");\n  return {\n    b: b.text.length,\n    m: m.text.length,\n    done\n  };\n}\n	{"version":3,"sources":["file:///script.ts"],"sourcesContent":["\\nimport { signal } from \\"rewind\\";\\nimport { big, medium } from \\"rewind:tools\\";\\nexport default async function main() {\\n  const b = await big({});\\n  const m = await medium({});\\n  const done = await signal(\\"done\\");\\n  return { b: b.text.length, m: m.text.length, done };\\n}\\n"],"names":[],"mappings":"AACA,SAAS,MAAM,QAAQ,SAAS;AAChC,SAAS,GAAG,EAAE,MAAM,QAAQ,eAAe;AAC3C,eAAe,eAAe;EAC5B,MAAM,IAAI,MAAM,IAAI,CAAC;EACrB,MAAM,IAAI,MAAM,OAAO,CAAC;EACxB,MAAM,OAAO,MAAM,OAAO;EAC1B,OAAO;IAAE,GAAG,EAAE,IAAI,CAAC,MAAM;IAAE,GAAG,EAAE,IAAI,CAAC,MAAM;IAAE;EAAK;AACpD"}
acme	\\xb7dcf637e3e2344ff84d62a0e40508279d7922fedae8fb9bde8cfa69a666b1e7	\nimport { triage } from "rewind:workflows";\nexport default async function main() {\n  const run = await triage.start({ repo: "acme/api" });\n  const result = await run.result();\n  return { result };\n}\n	import { triage } from "rewind:workflows";\nexport default async function main() {\n  const run = await triage.start({\n    repo: "acme/api"\n  });\n  const result = await run.result();\n  return {\n    result\n  };\n}\n	{"version":3,"sources":["file:///script.ts"],"sourcesContent":["\\nimport { triage } from \\"rewind:workflows\\";\\nexport default async function main() {\\n  const run = await triage.start({ repo: \\"acme/api\\" });\\n  const result = await run.result();\\n  return { result };\\n}\\n"],"names":[],"mappings":"AACA,SAAS,MAAM,QAAQ,mBAAmB;AAC1C,eAAe,eAAe;EAC5B,MAAM,MAAM,MAAM,OAAO,KAAK,CAAC;IAAE,MAAM;EAAW;EAClD,MAAM,SAAS,MAAM,IAAI,MAAM;EAC/B,OAAO;IAAE;EAAO;AAClB"}
acme	\\x20ede820a73da79169660b91916dc0dd21cde1c148b14887f436c4c43fffc4b4	\nimport { slow } from "rewind:tools";\nexport default async function main() {\n  return await slow({});\n}\n	import { slow } from "rewind:tools";\nexport default async function main() {\n  return await slow({});\n}\n	{"version":3,"sources":["file:///script.ts"],"sourcesContent":["\\nimport { slow } from \\"rewind:tools\\";\\nexport default async function main() {\\n  return await slow({});\\n}\\n"],"names":[],"mappings":"AACA,SAAS,IAAI,QAAQ,eAAe;AACpC,eAAe,eAAe;EAC5B,OAAO,MAAM,KAAK,CAAC;AACrB"}
acme	\\xe4f02399335086d8f990de1200bc96742e12ea9f407712306d7d435cdfea78a6	\nimport { small, flaky } from "rewind:tools";\nexport default async function main() {\n  await small({});\n  try {\n    await flaky({});\n  } catch (error) {\n    return { fixed: true };\n  }\n  return "unreachable";\n}\n	import { small, flaky } from "rewind:tools";\nexport default async function main() {\n  await small({});\n  try {\n    await flaky({});\n  } catch (error) {\n    return {\n      fixed: true\n    };\n  }\n  return "unreachable";\n}\n	{"version":3,"sources":["file:///script.ts"],"sourcesContent":["\\nimport { small, flaky } from \\"rewind:tools\\";\\nexport default async function main() {\\n  await small({});\\n  try {\\n    await flaky({});\\n  } catch (error) {\\n    return { fixed: true };\\n  }\\n  return \\"unreachable\\";\\n}\\n"],"names":[],"mappings":"AACA,SAAS,KAAK,EAAE,KAAK,QAAQ,eAAe;AAC5C,eAAe,eAAe;EAC5B,MAAM,MAAM,CAAC;EACb,IAAI;IACF,MAAM,MAAM,CAAC;EACf,EAAE,OAAO,OAAO;IACd,OAAO;MAAE,OAAO;IAAK;EACvB;EACA,OAAO;AACT"}
acme	\\x22bff039f17f95307821bf817968bf896c40c9658b6a60a4ecaad28a92d01670	\nimport { ask_human } from "rewind:tools";\nexport default async function main() {\n  const answer = await ask_human({ question: "ship it?" });\n  return { answer };\n}\n	import { ask_human } from "rewind:tools";\nexport default async function main() {\n  const answer = await ask_human({\n    question: "ship it?"\n  });\n  return {\n    answer\n  };\n}\n	{"version":3,"sources":["file:///script.ts"],"sourcesContent":["\\nimport { ask_human } from \\"rewind:tools\\";\\nexport default async function main() {\\n  const answer = await ask_human({ question: \\"ship it?\\" });\\n  return { answer };\\n}\\n"],"names":[],"mappings":"AACA,SAAS,SAAS,QAAQ,eAAe;AACzC,eAAe,eAAe;EAC5B,MAAM,SAAS,MAAM,UAAU;IAAE,UAAU;EAAW;EACtD,OAAO;IAAE;EAAO;AAClB"}
acme	\\xda713890f0ceadac70b6a9a9886d93e95d6f0e6b03c4dac0db47da880f5b46ad	\nimport { webhook, signal } from "rewind";\nexport default async function main() {\n  const hook = await webhook("pr_merged");\n  const event = await signal<{ merged: boolean }>("pr_merged");\n  return { url: hook.url, merged: event.merged };\n}\n	import { webhook, signal } from "rewind";\nexport default async function main() {\n  const hook = await webhook("pr_merged");\n  const event = await signal("pr_merged");\n  return {\n    url: hook.url,\n    merged: event.merged\n  };\n}\n	{"version":3,"sources":["file:///script.ts"],"sourcesContent":["\\nimport { webhook, signal } from \\"rewind\\";\\nexport default async function main() {\\n  const hook = await webhook(\\"pr_merged\\");\\n  const event = await signal<{ merged: boolean }>(\\"pr_merged\\");\\n  return { url: hook.url, merged: event.merged };\\n}\\n"],"names":[],"mappings":"AACA,SAAS,OAAO,EAAE,MAAM,QAAQ,SAAS;AACzC,eAAe,eAAe;EAC5B,MAAM,OAAO,MAAM,QAAQ;EAC3B,MAAM,QAAQ,MAAM,OAA4B;EAChD,OAAO;IAAE,KAAK,KAAK,GAAG;IAAE,QAAQ,MAAM,MAAM;EAAC;AAC/C"}
acme	\\x1015eb715f6e4bfd752f9e290f65c3e4b01d2b0f99e7d3b3e26ceade3c64908a	\nimport { signal } from "rewind";\nexport default async function main() {\n  return await signal("never");\n}\n	import { signal } from "rewind";\nexport default async function main() {\n  return await signal("never");\n}\n	{"version":3,"sources":["file:///script.ts"],"sourcesContent":["\\nimport { signal } from \\"rewind\\";\\nexport default async function main() {\\n  return await signal(\\"never\\");\\n}\\n"],"names":[],"mappings":"AACA,SAAS,MAAM,QAAQ,SAAS;AAChC,eAAe,eAAe;EAC5B,OAAO,MAAM,OAAO;AACtB"}
\.


--
-- Data for Name: secret_reads; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.secret_reads (tenant, namespace, name, day, reads) FROM stdin;
\.


--
-- Data for Name: secrets; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.secrets (tenant, namespace, name, ciphertext, labels, created_by, created_at, updated_by, updated_at, last_read_at, last_read_run, last_read_namespace) FROM stdin;
\.


--
-- Data for Name: signal_keys; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.signal_keys (run_id, key) FROM stdin;
18b72207-70d2-77c5-8642-0c8599a04aef	key-first
18b72207-70d2-77c5-8642-0c8599a04aef	key-second
\.


--
-- Data for Name: signing_keys; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.signing_keys (tenant, kid, public_key, private_ciphertext, created_at, retired_at) FROM stdin;
\.


--
-- Data for Name: subscriptions; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.subscriptions (tenant, id, url, secret, events, labels) FROM stdin;
\.


--
-- Data for Name: tenants; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.tenants (id, active_run_quota, journal_retention_ms, archive_retention_ms, read_model_retention_ms, payload_grace_ms, cpu_per_month_ms, runs_per_month, scripts, max_ttl_ms, max_script_lines, default_cpu_per_month_ms, default_active_runs, default_runs_per_month, default_scripts, default_max_ttl_ms, default_max_script_lines) FROM stdin;
acme	\N	2592000000	1576800000000	7776000000	86400000	\N	\N	\N	\N	\N	\N	\N	\N	\N	\N	\N
\.


--
-- Data for Name: tests; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.tests (tenant, digest, namespace, size, reserved_at, deletion_started_at) FROM stdin;
acme	\\xfa89ec08d9fd85b9458a351547bb1c3262ebcc720090b21ba2a448a8a4710016	\N	24	1769904000000	\N
\.


--
-- Data for Name: timers; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.timers (run_id, seq, fire_at) FROM stdin;
1f2f83db-2016-79eb-9387-81ecf0cd83a5	2	1769907600000
\.


--
-- Data for Name: token_keys; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.token_keys (kid, tenant, name, jwk, permissions, created_by, created_at) FROM stdin;
\.


--
-- Data for Name: tokens; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.tokens (hash, tenant, namespace, permissions, labels, expires_at, created_by, created_at, revoked_at) FROM stdin;
\.


--
-- Data for Name: tool_calls; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.tool_calls (tenant, run_id, seq, tool, callsite_line, scheduled_at, completed_at, outcome, error_kind, args_digest, result_digest, result_shape, attempts, task_state) FROM stdin;
acme	63cbe1e4-5932-7dd7-a7cf-1707c6e1d01f	1	small	4	1767225600000	1767225600000	completed	\N	\\x44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a	\\x4062edaf750fb8074e7e83e0c9028c94e32468a8b6f1614774328ef045150f93	{"object":{"ok":{"optional":false,"shape":"boolean"}}}	\N	\N
acme	63cbe1e4-5932-7dd7-a7cf-1707c6e1d01f	2	medium	5	1767225600000	1767225600000	completed	\N	\\x44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a	\\xa4492762822e7f869dc57428969f068f1b0b6d660acc8096a40ee4e9ee6b4a74	{"object":{"text":{"optional":false,"shape":"string"}}}	\N	\N
acme	63cbe1e4-5932-7dd7-a7cf-1707c6e1d01f	3	big	6	1767225600000	1767225600000	completed	\N	\\x44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a	\\xfb3a224297affd9c23852a92179568da187a161043a76d58ca6b12ef6e529618	\N	\N	\N
acme	f9c322e4-79c4-735d-ae70-d0f08fa8a8b1	1	slow	4	1769904000000	\N	\N	\N	\\x44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a	\N	\N	1	deferred
acme	031deb6d-6ddc-7cb5-8e1a-a6f8b428d15a	1	small	4	1769904000000	1769904000000	completed	\N	\\x8dd1a390ef1282d683f233877c83288e159521806b8bc72bd59ddb14f9a95908	\\x4062edaf750fb8074e7e83e0c9028c94e32468a8b6f1614774328ef045150f93	{"object":{"ok":{"optional":false,"shape":"boolean"}}}	\N	\N
acme	1f2f83db-2016-79eb-9387-81ecf0cd83a5	1	small	5	1769904000000	1769904000000	completed	\N	\\x44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a	\\x4062edaf750fb8074e7e83e0c9028c94e32468a8b6f1614774328ef045150f93	{"object":{"ok":{"optional":false,"shape":"boolean"}}}	\N	\N
acme	2af4fb3b-d947-7ebb-a55c-0d794c298dc9	1	small	4	1769904000000	1769904000000	completed	\N	\\x44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a	\\x4062edaf750fb8074e7e83e0c9028c94e32468a8b6f1614774328ef045150f93	{"object":{"ok":{"optional":false,"shape":"boolean"}}}	\N	\N
acme	2c582b9e-1961-750f-be22-215ba1ff1918	1	big	5	1769904000000	1769904000000	completed	\N	\\x44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a	\\xfb3a224297affd9c23852a92179568da187a161043a76d58ca6b12ef6e529618	\N	\N	\N
acme	6e4cacb5-4098-7d30-88c3-933c639bdaad	1	webhook	4	1769904000000	1769904000000	completed	\N	\\xc819ee1e7170a2db1bda1226c77b8092e7eaa70433b545fefb631179e74dd288	\\x719cf51bfa9e958c4d328813087d3970cfac51bfed0a03263e02a2c82a013fa9	{"object":{"url":{"optional":false,"shape":"string"}}}	\N	\N
acme	85df88e9-7e9c-7d21-8f60-46463925e0b1	1	workflows.triage.start	4	1769904000000	1769904000000	completed	\N	\\x7549668f000889662218efaa7fc73f13e7a8b6c1d05906b6c26ced8b5c9f7eff	\\x5fbcd5c2ca0dd1b5b683934b0f60c9616df01082469f66567d88b1bfaebf075a	{"object":{"runId":{"optional":false,"shape":"string"}}}	\N	\N
acme	031deb6d-6ddc-7cb5-8e1a-a6f8b428d15a	2	medium	5	1769904000000	1769904000000	completed	\N	\\x44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a	\\xa4492762822e7f869dc57428969f068f1b0b6d660acc8096a40ee4e9ee6b4a74	{"object":{"text":{"optional":false,"shape":"string"}}}	\N	\N
acme	2af4fb3b-d947-7ebb-a55c-0d794c298dc9	2	flaky	5	1769904000000	1769904000000	failed	error	\\x44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a	\N	\N	\N	\N
acme	2c582b9e-1961-750f-be22-215ba1ff1918	2	medium	6	1769904000000	1769904000000	completed	\N	\\x44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a	\\xa4492762822e7f869dc57428969f068f1b0b6d660acc8096a40ee4e9ee6b4a74	{"object":{"text":{"optional":false,"shape":"string"}}}	\N	\N
acme	031deb6d-6ddc-7cb5-8e1a-a6f8b428d15a	3	big	6	1769904000000	1769904000000	completed	\N	\\x44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a	\\xfb3a224297affd9c23852a92179568da187a161043a76d58ca6b12ef6e529618	\N	\N	\N
acme	e206c2c7-f3cd-701f-b2d5-81899e11fb5d	1	small	4	1769904000000	1769904000000	completed	\N	\\x44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a	\\x4062edaf750fb8074e7e83e0c9028c94e32468a8b6f1614774328ef045150f93	{"object":{"ok":{"optional":false,"shape":"boolean"}}}	\N	\N
acme	e206c2c7-f3cd-701f-b2d5-81899e11fb5d	2	flaky	6	1769904000000	1769904000000	failed	error	\\x44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a	\N	\N	\N	\N
acme	9e5651b0-ef95-7636-82c9-c08ce3ca2634	1	ask_human	4	1769904000000	\N	\N	\N	\\x650570d3efee5eca580b451ace51705dc150eb5a516b5d4ddaec9c5214df40fc	\N	\N	0	ready
\.


--
-- Data for Name: tool_tasks; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.tool_tasks (tenant, run_id, seq, toolset, tool, route, mcp_server, dispatched, args_digest, args, on_interruption, approval, execution_timeout_ms, heartbeat_timeout_ms, concurrency, server_concurrency, state, attempt, token, lease_owner, lease_until, next_attempt_at, due_at, parked_kind, parked_reason, schema_change_approved, created_at) FROM stdin;
acme	f9c322e4-79c4-735d-ae70-d0f08fa8a8b1	1	\\x6e15f8f3f4e53953f4b554fcb718f5c3fa277d27c38682103d5e832ba7b3ba64	slow	{"host":null}	\N	f	\\x44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a	{}	retry	f	\N	60000	16	\N	deferred	1	rewind_tt_cdb3fab6cabf1753efd9c82a3934559e5560754c226eb930e4ccff8b01de8bc1	\N	3346704000000	\N	\N	\N	\N	f	1769904000000
acme	9e5651b0-ef95-7636-82c9-c08ce3ca2634	1	\\x6e15f8f3f4e53953f4b554fcb718f5c3fa277d27c38682103d5e832ba7b3ba64	ask_human	{"host":null}	\N	f	\\x650570d3efee5eca580b451ace51705dc150eb5a516b5d4ddaec9c5214df40fc	{"question":"ship it?"}	retry	f	\N	60000	16	\N	ready	0	\N	\N	\N	1769904000000	1769904000000	\N	\N	f	1769904000000
\.


--
-- Data for Name: toolsets; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.toolsets (tenant, digest, spec, declarations, prompt) FROM stdin;
acme	\\xf3e36efa9bae959bf0e62cf9b1da2adc53de0a970ef5b69f92dcb984a9175843	{"servers":[],"tools":[],"v":1}		
acme	\\x6e15f8f3f4e53953f4b554fcb718f5c3fa277d27c38682103d5e832ba7b3ba64	{"servers":[],"tools":[{"name":"ask_human","policy":{"approval":false,"concurrency":16,"heartbeat_timeout_ms":60000,"on_interruption":"retry"},"route":{"type":"host"},"schemas":{"input":{}}},{"name":"big","policy":{"approval":false,"concurrency":16,"heartbeat_timeout_ms":60000,"on_interruption":"retry"},"route":{"type":"host"},"schemas":{"input":{}}},{"name":"flaky","policy":{"approval":false,"concurrency":16,"heartbeat_timeout_ms":60000,"on_interruption":"retry"},"route":{"type":"host"},"schemas":{"input":{}}},{"name":"medium","policy":{"approval":false,"concurrency":16,"heartbeat_timeout_ms":60000,"on_interruption":"retry"},"route":{"type":"host"},"schemas":{"input":{}}},{"name":"slow","policy":{"approval":false,"concurrency":16,"heartbeat_timeout_ms":60000,"on_interruption":"retry"},"route":{"type":"host"},"schemas":{"input":{}}},{"name":"small","policy":{"approval":false,"concurrency":16,"heartbeat_timeout_ms":60000,"on_interruption":"retry"},"route":{"type":"host"},"schemas":{"input":{}}},{"name":"webhook","policy":{"approval":false,"concurrency":16,"execution_timeout_ms":300000,"heartbeat_timeout_ms":60000,"on_interruption":"retry"},"route":{"builtin":"webhook","type":"builtin"},"schemas":{"input":{"additionalProperties":false,"properties":{"name":{"type":"string"}},"required":["name"],"type":"object"}}}],"v":1,"workflows":[{"name":"triage","schemas":{"input":{"properties":{"repo":{"type":"string"}},"required":["repo"],"type":"object"},"output":{"type":"object"},"signals":{"approval":{"properties":{"close":{"type":"boolean"}},"required":["close"],"type":"object"}}},"version":"5739537ce2f07ea2ba822a564517c7e9d5035582143579548c6b4701a6a19e87"}]}		
\.


--
-- Data for Name: usage; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.usage (tenant, namespace, day, cpu_ms, runs_started) FROM stdin;
acme	alice	20454	0	1
acme	*	20454	0	1
acme	alice	20485	0	9
acme	bob	20485	0	5
acme	*	20485	0	14
\.


--
-- Data for Name: webhook_tokens; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.webhook_tokens (token, run_id, name) FROM stdin;
rewind_wh_81725bdabd033d5d67585bf74945a5503c37674f32e41ee7f64c5e850580a118	6e4cacb5-4098-7d30-88c3-933c639bdaad	pr_merged
\.


--
-- Data for Name: workflow_tests; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.workflow_tests (tenant, version, digest) FROM stdin;
\.


--
-- Data for Name: workflows; Type: TABLE DATA; Schema: public; Owner: -
--

COPY public.workflows (tenant, version, name, document, labels, created_at, ordinal) FROM stdin;
acme	\\x5739537ce2f07ea2ba822a564517c7e9d5035582143579548c6b4701a6a19e87	triage	{"action_set":"f3e36efa9bae959bf0e62cf9b1da2adc53de0a970ef5b69f92dcb984a9175843","name":"triage","schemas":{"input":{"properties":{"repo":{"type":"string"}},"required":["repo"],"type":"object"},"output":{"type":"object"},"signals":{"approval":{"properties":{"close":{"type":"boolean"}},"required":["close"],"type":"object"}}},"script":"c7f3ce310055574d84fd13a786ef2e5f9fcf7edc5a796b23bc7f0cca691a4af4","v":1}	{}	1767225600000	1
\.


--
-- Name: feed_id_seq; Type: SEQUENCE SET; Schema: public; Owner: -
--

SELECT pg_catalog.setval('public.feed_id_seq', 90, true);


--
-- Name: inbox_ids; Type: SEQUENCE SET; Schema: public; Owner: -
--

SELECT pg_catalog.setval('public.inbox_ids', 31, true);


--
-- Name: workflows_ordinal_seq; Type: SEQUENCE SET; Schema: public; Owner: -
--

SELECT pg_catalog.setval('public.workflows_ordinal_seq', 1, true);


--
-- Name: _sqlx_migrations _sqlx_migrations_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public._sqlx_migrations
    ADD CONSTRAINT _sqlx_migrations_pkey PRIMARY KEY (version);


--
-- Name: access_settings access_settings_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.access_settings
    ADD CONSTRAINT access_settings_pkey PRIMARY KEY (tenant);


--
-- Name: api_keys api_keys_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.api_keys
    ADD CONSTRAINT api_keys_pkey PRIMARY KEY (hash);


--
-- Name: connection_tokens connection_tokens_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.connection_tokens
    ADD CONSTRAINT connection_tokens_pkey PRIMARY KEY (hash);


--
-- Name: connections connections_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.connections
    ADD CONSTRAINT connections_pkey PRIMARY KEY (id);


--
-- Name: data_keys data_keys_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.data_keys
    ADD CONSTRAINT data_keys_pkey PRIMARY KEY (tenant);


--
-- Name: feed feed_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.feed
    ADD CONSTRAINT feed_pkey PRIMARY KEY (id);


--
-- Name: grants grants_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.grants
    ADD CONSTRAINT grants_pkey PRIMARY KEY (tenant, principal);


--
-- Name: history history_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.history
    ADD CONSTRAINT history_pkey PRIMARY KEY (run_id, idx);


--
-- Name: inbox inbox_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.inbox
    ADD CONSTRAINT inbox_pkey PRIMARY KEY (run_id, id);


--
-- Name: namespace_scripts namespace_scripts_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.namespace_scripts
    ADD CONSTRAINT namespace_scripts_pkey PRIMARY KEY (tenant, namespace, digest);


--
-- Name: namespaces namespaces_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.namespaces
    ADD CONSTRAINT namespaces_pkey PRIMARY KEY (tenant, name);


--
-- Name: oauth_clients oauth_clients_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.oauth_clients
    ADD CONSTRAINT oauth_clients_pkey PRIMARY KEY (id);


--
-- Name: oauth_codes oauth_codes_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.oauth_codes
    ADD CONSTRAINT oauth_codes_pkey PRIMARY KEY (hash);


--
-- Name: payload_refs payload_refs_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.payload_refs
    ADD CONSTRAINT payload_refs_pkey PRIMARY KEY (tenant, digest, run_id);


--
-- Name: payloads payloads_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.payloads
    ADD CONSTRAINT payloads_pkey PRIMARY KEY (tenant, digest);


--
-- Name: quota_alerts quota_alerts_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.quota_alerts
    ADD CONSTRAINT quota_alerts_pkey PRIMARY KEY (tenant, namespace, quota, period, threshold);


--
-- Name: roles roles_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.roles
    ADD CONSTRAINT roles_pkey PRIMARY KEY (tenant, name);


--
-- Name: run_labels run_labels_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.run_labels
    ADD CONSTRAINT run_labels_pkey PRIMARY KEY (run_id, key);


--
-- Name: run_quarantine run_quarantine_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.run_quarantine
    ADD CONSTRAINT run_quarantine_pkey PRIMARY KEY (run_id);


--
-- Name: run_search run_search_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.run_search
    ADD CONSTRAINT run_search_pkey PRIMARY KEY (run_id);


--
-- Name: run_summaries run_summaries_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.run_summaries
    ADD CONSTRAINT run_summaries_pkey PRIMARY KEY (run_id);


--
-- Name: run_waits run_waits_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.run_waits
    ADD CONSTRAINT run_waits_pkey PRIMARY KEY (run_id, "position");


--
-- Name: run_watchers run_watchers_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.run_watchers
    ADD CONSTRAINT run_watchers_pkey PRIMARY KEY (run_id, watcher);


--
-- Name: runs runs_id_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.runs
    ADD CONSTRAINT runs_id_key UNIQUE (id);


--
-- Name: runs runs_idempotency_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.runs
    ADD CONSTRAINT runs_idempotency_key UNIQUE (tenant, namespace, idempotency_key);


--
-- Name: runs runs_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.runs
    ADD CONSTRAINT runs_pkey PRIMARY KEY (tenant, id);


--
-- Name: scripts scripts_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.scripts
    ADD CONSTRAINT scripts_pkey PRIMARY KEY (tenant, digest);


--
-- Name: secret_reads secret_reads_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.secret_reads
    ADD CONSTRAINT secret_reads_pkey PRIMARY KEY (tenant, namespace, name, day);


--
-- Name: secrets secrets_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.secrets
    ADD CONSTRAINT secrets_pkey PRIMARY KEY (tenant, namespace, name);


--
-- Name: signal_keys signal_keys_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.signal_keys
    ADD CONSTRAINT signal_keys_pkey PRIMARY KEY (run_id, key);


--
-- Name: signing_keys signing_keys_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.signing_keys
    ADD CONSTRAINT signing_keys_pkey PRIMARY KEY (tenant, kid);


--
-- Name: subscriptions subscriptions_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.subscriptions
    ADD CONSTRAINT subscriptions_pkey PRIMARY KEY (tenant, id);


--
-- Name: tenants tenants_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.tenants
    ADD CONSTRAINT tenants_pkey PRIMARY KEY (id);


--
-- Name: tests tests_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.tests
    ADD CONSTRAINT tests_pkey PRIMARY KEY (tenant, digest);


--
-- Name: timers timers_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.timers
    ADD CONSTRAINT timers_pkey PRIMARY KEY (run_id, seq);


--
-- Name: token_keys token_keys_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.token_keys
    ADD CONSTRAINT token_keys_pkey PRIMARY KEY (kid);


--
-- Name: tokens tokens_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.tokens
    ADD CONSTRAINT tokens_pkey PRIMARY KEY (hash);


--
-- Name: tool_calls tool_calls_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.tool_calls
    ADD CONSTRAINT tool_calls_pkey PRIMARY KEY (run_id, seq);


--
-- Name: tool_tasks tool_tasks_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.tool_tasks
    ADD CONSTRAINT tool_tasks_pkey PRIMARY KEY (run_id, seq);


--
-- Name: tool_tasks tool_tasks_token_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.tool_tasks
    ADD CONSTRAINT tool_tasks_token_key UNIQUE (token);


--
-- Name: toolsets toolsets_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.toolsets
    ADD CONSTRAINT toolsets_pkey PRIMARY KEY (tenant, digest);


--
-- Name: usage usage_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.usage
    ADD CONSTRAINT usage_pkey PRIMARY KEY (tenant, namespace, day);


--
-- Name: webhook_tokens webhook_tokens_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.webhook_tokens
    ADD CONSTRAINT webhook_tokens_pkey PRIMARY KEY (token);


--
-- Name: webhook_tokens webhook_tokens_run_id_name_key; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.webhook_tokens
    ADD CONSTRAINT webhook_tokens_run_id_name_key UNIQUE (run_id, name);


--
-- Name: workflow_tests workflow_tests_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.workflow_tests
    ADD CONSTRAINT workflow_tests_pkey PRIMARY KEY (tenant, version, digest);


--
-- Name: workflows workflows_pkey; Type: CONSTRAINT; Schema: public; Owner: -
--

ALTER TABLE ONLY public.workflows
    ADD CONSTRAINT workflows_pkey PRIMARY KEY (tenant, version);


--
-- Name: api_keys_by_tenant; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX api_keys_by_tenant ON public.api_keys USING btree (tenant, created_at, hash);


--
-- Name: connection_tokens_by_connection; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX connection_tokens_by_connection ON public.connection_tokens USING btree (connection_id);


--
-- Name: connection_tokens_by_expiry; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX connection_tokens_by_expiry ON public.connection_tokens USING btree (expires_at);


--
-- Name: connections_by_client; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX connections_by_client ON public.connections USING btree (client_id);


--
-- Name: connections_by_expiry; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX connections_by_expiry ON public.connections USING btree (expires_at);


--
-- Name: connections_by_user; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX connections_by_user ON public.connections USING btree (user_id, created_at, id);


--
-- Name: connections_live; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX connections_live ON public.connections USING btree (user_id, client_id) WHERE (revoked_at IS NULL);


--
-- Name: data_keys_by_master_key; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX data_keys_by_master_key ON public.data_keys USING btree (master_key, tenant);


--
-- Name: feed_by_consumer; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX feed_by_consumer ON public.feed USING btree (consumer, id);


--
-- Name: feed_by_run; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX feed_by_run ON public.feed USING btree (run_id);


--
-- Name: grants_by_principal; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX grants_by_principal ON public.grants USING btree (principal);


--
-- Name: grants_by_role; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX grants_by_role ON public.grants USING btree (tenant, role);


--
-- Name: namespaces_deleting; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX namespaces_deleting ON public.namespaces USING btree (deleting_since, tenant, name) WHERE (deleting_since IS NOT NULL);


--
-- Name: oauth_clients_by_kind; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX oauth_clients_by_kind ON public.oauth_clients USING btree (kind);


--
-- Name: oauth_codes_by_expiry; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX oauth_codes_by_expiry ON public.oauth_codes USING btree (expires_at);


--
-- Name: payload_refs_by_run; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX payload_refs_by_run ON public.payload_refs USING btree (run_id);


--
-- Name: payloads_unreferenced; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX payloads_unreferenced ON public.payloads USING btree (unreferenced_since) WHERE (unreferenced_since IS NOT NULL);


--
-- Name: quota_alerts_by_period; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX quota_alerts_by_period ON public.quota_alerts USING btree (tenant, period);


--
-- Name: quota_alerts_unsent; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX quota_alerts_unsent ON public.quota_alerts USING btree (raised_at) WHERE (sent_at IS NULL);


--
-- Name: run_labels_by_value; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX run_labels_by_value ON public.run_labels USING btree (tenant, key, value, run_id);


--
-- Name: run_quarantine_by_tenant; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX run_quarantine_by_tenant ON public.run_quarantine USING btree (tenant) WHERE (kind IS NOT NULL);


--
-- Name: run_search_by_text; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX run_search_by_text ON public.run_search USING gin (search_text public.gin_trgm_ops);


--
-- Name: run_summaries_active; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX run_summaries_active ON public.run_summaries USING btree (tenant, wakes_at, run_id) WHERE (status = 'active'::text);


--
-- Name: run_summaries_by_activity; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX run_summaries_by_activity ON public.run_summaries USING btree (tenant, last_activity_at, run_id);


--
-- Name: run_summaries_by_calls; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX run_summaries_by_calls ON public.run_summaries USING btree (tenant, tool_calls, run_id);


--
-- Name: run_summaries_by_change; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX run_summaries_by_change ON public.run_summaries USING btree (tenant, changed_at);


--
-- Name: run_summaries_by_duration; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX run_summaries_by_duration ON public.run_summaries USING btree (tenant, (
CASE
    WHEN (ended_at >= started_at) THEN (ended_at - started_at)
    WHEN (ended_at < started_at) THEN (0)::bigint
    ELSE NULL::bigint
END) DESC NULLS LAST, run_id DESC);


--
-- Name: run_summaries_by_end; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX run_summaries_by_end ON public.run_summaries USING btree (tenant, ended_at) WHERE (ended_at IS NOT NULL);


--
-- Name: run_summaries_by_fingerprint; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX run_summaries_by_fingerprint ON public.run_summaries USING btree (tenant, fingerprint) WHERE (fingerprint IS NOT NULL);


--
-- Name: run_summaries_by_parent; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX run_summaries_by_parent ON public.run_summaries USING btree (tenant, parent) WHERE (parent IS NOT NULL);


--
-- Name: run_summaries_by_script; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX run_summaries_by_script ON public.run_summaries USING btree (tenant, script);


--
-- Name: run_summaries_by_start; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX run_summaries_by_start ON public.run_summaries USING btree (tenant, started_at);


--
-- Name: run_summaries_by_started_by; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX run_summaries_by_started_by ON public.run_summaries USING btree (tenant, started_by) WHERE (started_by IS NOT NULL);


--
-- Name: run_summaries_by_tenant; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX run_summaries_by_tenant ON public.run_summaries USING btree (tenant, run_id);


--
-- Name: run_summaries_by_wake; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX run_summaries_by_wake ON public.run_summaries USING btree (tenant, wakes_at, run_id);


--
-- Name: run_summaries_by_workflow; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX run_summaries_by_workflow ON public.run_summaries USING btree (tenant, workflow) WHERE (workflow IS NOT NULL);


--
-- Name: run_summaries_to_purge; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX run_summaries_to_purge ON public.run_summaries USING btree (ended_at) WHERE (ended_at IS NOT NULL);


--
-- Name: run_waits_by_name; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX run_waits_by_name ON public.run_waits USING btree (tenant, kind, name);


--
-- Name: run_watchers_by_watcher; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX run_watchers_by_watcher ON public.run_watchers USING btree (watcher, run_id);


--
-- Name: runs_active; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX runs_active ON public.runs USING btree (tenant) WHERE (status <> ALL (ARRAY['completed'::text, 'failed'::text, 'cancelled'::text, 'aborted'::text]));


--
-- Name: runs_archived; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX runs_archived ON public.runs USING btree (archived_at, tenant, id) WHERE (archive_document IS NOT NULL);


--
-- Name: runs_by_creation; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX runs_by_creation ON public.runs USING btree (tenant, created_at, id);


--
-- Name: runs_by_deadline; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX runs_by_deadline ON public.runs USING btree (expires_at, id) WHERE (status <> ALL (ARRAY['completed'::text, 'failed'::text, 'cancelled'::text, 'aborted'::text]));


--
-- Name: runs_by_labels; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX runs_by_labels ON public.runs USING gin (labels jsonb_path_ops);


--
-- Name: runs_by_namespace; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX runs_by_namespace ON public.runs USING btree (tenant, namespace, status);


--
-- Name: runs_quarantined; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX runs_quarantined ON public.runs USING btree (quarantine_since, tenant, id) WHERE (quarantine_kind IS NOT NULL);


--
-- Name: runs_runnable; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX runs_runnable ON public.runs USING btree (tenant, inbox_head) WHERE (inbox_head IS NOT NULL);


--
-- Name: runs_runnable_by_sticky_worker; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX runs_runnable_by_sticky_worker ON public.runs USING btree (sticky_worker, inbox_head) WHERE ((inbox_head IS NOT NULL) AND (sticky_worker IS NOT NULL));


--
-- Name: runs_to_archive; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX runs_to_archive ON public.runs USING btree (ended_at, tenant, id) WHERE ((ended_at IS NOT NULL) AND (archive_document IS NULL));


--
-- Name: signing_keys_current; Type: INDEX; Schema: public; Owner: -
--

CREATE UNIQUE INDEX signing_keys_current ON public.signing_keys USING btree (tenant) WHERE (retired_at IS NULL);


--
-- Name: tests_by_namespace; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX tests_by_namespace ON public.tests USING btree (tenant, namespace);


--
-- Name: timers_by_fire_at; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX timers_by_fire_at ON public.timers USING btree (fire_at, run_id, seq);


--
-- Name: token_keys_by_tenant; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX token_keys_by_tenant ON public.token_keys USING btree (tenant, created_at, kid);


--
-- Name: tokens_by_expiry; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX tokens_by_expiry ON public.tokens USING btree (expires_at);


--
-- Name: tokens_by_namespace; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX tokens_by_namespace ON public.tokens USING btree (tenant, namespace);


--
-- Name: tokens_by_revocation; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX tokens_by_revocation ON public.tokens USING btree (revoked_at) WHERE (revoked_at IS NOT NULL);


--
-- Name: tool_calls_by_schedule; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX tool_calls_by_schedule ON public.tool_calls USING btree (tenant, scheduled_at);


--
-- Name: tool_calls_by_tool; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX tool_calls_by_tool ON public.tool_calls USING btree (tenant, tool);


--
-- Name: tool_tasks_claimed_by_owner; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX tool_tasks_claimed_by_owner ON public.tool_tasks USING btree (lease_owner, tenant, mcp_server) WHERE (state = 'claimed'::text);


--
-- Name: tool_tasks_deferred; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX tool_tasks_deferred ON public.tool_tasks USING btree (lease_until, run_id, seq) WHERE (state = 'deferred'::text);


--
-- Name: tool_tasks_due; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX tool_tasks_due ON public.tool_tasks USING btree (due_at, run_id, seq) WHERE (due_at IS NOT NULL);


--
-- Name: tool_tasks_host_due; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX tool_tasks_host_due ON public.tool_tasks USING btree (tenant, due_at, run_id, seq) WHERE ((due_at IS NOT NULL) AND (NOT dispatched));


--
-- Name: tool_tasks_parked; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX tool_tasks_parked ON public.tool_tasks USING btree (tenant, toolset) WHERE (state = 'parked'::text);


--
-- Name: workflow_tests_by_test; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX workflow_tests_by_test ON public.workflow_tests USING btree (tenant, digest);


--
-- Name: workflows_by_name; Type: INDEX; Schema: public; Owner: -
--

CREATE INDEX workflows_by_name ON public.workflows USING btree (tenant, name, created_at, ordinal);


--
-- Name: run_labels_per_tenant; Type: STATISTICS; Schema: public; Owner: -
--

CREATE STATISTICS public.run_labels_per_tenant (mcv) ON key, value, tenant FROM public.run_labels;


--
-- Name: tool_calls_per_tenant; Type: STATISTICS; Schema: public; Owner: -
--

CREATE STATISTICS public.tool_calls_per_tenant (mcv) ON tenant, tool FROM public.tool_calls;


--
-- PostgreSQL database dump complete
--

\unrestrict K0IjfR5V7anrpHYHgFsoQRWudFnXFP2o5ONms5Kio5z4XDj6reAoyoyEvcYK6yH

