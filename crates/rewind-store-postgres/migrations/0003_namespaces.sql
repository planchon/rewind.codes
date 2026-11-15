-- Namespaces (archi/06-storage-and-scale.md, "Namespaces", "Quotas"): the end user a run belongs
-- to, with idempotency keys unique per namespace; the tenant's ceiling and its namespaces'
-- defaults, a column per quota (the ceiling's active runs is `active_run_quota`, of version 1);
-- namespaces with their exceptions, their use per day, their alerts and the runs kept per script;
-- end users' tokens and the keys that sign them.
--
-- `usage` and `quota_alerts` hold the tenant's own rows under the namespace `*`, which is not a
-- namespace's name. A day is days since the Unix epoch, a period `2026-11`, a threshold its
-- percent. Permissions are arrays of the catalog's names, as a role's.
--
-- The runs that existed before namespaces belong to `migrated`, which is created in each tenant
-- that has runs, with its script counts and the runs started per day, its own and its tenant's.

ALTER TABLE runs ADD COLUMN namespace text COLLATE "C" NOT NULL DEFAULT 'migrated';
ALTER TABLE runs ALTER COLUMN namespace DROP DEFAULT;
ALTER TABLE runs DROP CONSTRAINT runs_tenant_idempotency_key_key;
ALTER TABLE runs ADD CONSTRAINT runs_idempotency_key UNIQUE (tenant, namespace, idempotency_key);
CREATE INDEX runs_by_namespace ON runs (tenant, namespace, status);
ALTER TABLE run_summaries ADD COLUMN namespace text COLLATE "C" NOT NULL DEFAULT 'migrated';

ALTER TABLE tenants
    ADD COLUMN cpu_per_month_ms bigint,
    ADD COLUMN runs_per_month bigint,
    ADD COLUMN scripts bigint,
    ADD COLUMN max_ttl_ms bigint,
    ADD COLUMN max_script_lines bigint,
    ADD COLUMN default_cpu_per_month_ms bigint,
    ADD COLUMN default_active_runs bigint,
    ADD COLUMN default_runs_per_month bigint,
    ADD COLUMN default_scripts bigint,
    ADD COLUMN default_max_ttl_ms bigint,
    ADD COLUMN default_max_script_lines bigint;

CREATE TABLE namespaces (
    tenant text COLLATE "C" NOT NULL,
    name text COLLATE "C" NOT NULL,
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
    PRIMARY KEY (tenant, name)
);
CREATE INDEX namespaces_deleting ON namespaces (deleting_since, tenant, name)
    WHERE deleting_since IS NOT NULL;

CREATE TABLE usage (
    tenant text COLLATE "C" NOT NULL,
    namespace text COLLATE "C" NOT NULL,
    day bigint NOT NULL,
    cpu_ms bigint NOT NULL,
    runs_started bigint NOT NULL,
    PRIMARY KEY (tenant, namespace, day)
);

CREATE TABLE quota_alerts (
    tenant text COLLATE "C" NOT NULL,
    namespace text COLLATE "C" NOT NULL,
    quota text NOT NULL CHECK (quota IN ('cpu_per_month', 'active_runs', 'runs_per_month',
                                          'scripts', 'max_ttl', 'max_script_lines')),
    period text COLLATE "C" NOT NULL,
    threshold bigint NOT NULL CHECK (threshold IN (80, 100)),
    used bigint NOT NULL,
    quota_limit bigint NOT NULL,
    raised_at bigint NOT NULL,
    lease_owner text,
    lease_until bigint,
    sent_at bigint,
    PRIMARY KEY (tenant, namespace, quota, period, threshold)
);
CREATE INDEX quota_alerts_unsent ON quota_alerts (raised_at) WHERE sent_at IS NULL;
CREATE INDEX quota_alerts_by_period ON quota_alerts (tenant, period);

CREATE TABLE namespace_scripts (
    tenant text COLLATE "C" NOT NULL,
    namespace text COLLATE "C" NOT NULL,
    digest bytea NOT NULL,
    runs bigint NOT NULL,
    first_run_at bigint NOT NULL,
    PRIMARY KEY (tenant, namespace, digest)
);

CREATE TABLE tokens (
    hash bytea PRIMARY KEY,
    tenant text COLLATE "C" NOT NULL,
    namespace text COLLATE "C" NOT NULL,
    permissions text[] NOT NULL,
    labels jsonb NOT NULL,
    expires_at bigint NOT NULL,
    created_by text COLLATE "C" NOT NULL,
    created_at bigint NOT NULL,
    revoked_at bigint
);
CREATE INDEX tokens_by_namespace ON tokens (tenant, namespace);
-- The purge of tokens expired or revoked.
CREATE INDEX tokens_by_expiry ON tokens (expires_at);
CREATE INDEX tokens_by_revocation ON tokens (revoked_at) WHERE revoked_at IS NOT NULL;

CREATE TABLE token_keys (
    kid text COLLATE "C" PRIMARY KEY,
    tenant text COLLATE "C" NOT NULL,
    name text NOT NULL,
    jwk text NOT NULL,
    permissions text[] NOT NULL,
    created_by text COLLATE "C" NOT NULL,
    created_at bigint NOT NULL
);
CREATE INDEX token_keys_by_tenant ON token_keys (tenant, created_at, kid);

INSERT INTO namespaces (tenant, name, created_at, last_active_at)
SELECT tenant, 'migrated', min(created_at), max(created_at) FROM runs GROUP BY tenant;
INSERT INTO namespace_scripts (tenant, namespace, digest, runs, first_run_at)
SELECT tenant, 'migrated', script, count(*), min(created_at) FROM runs GROUP BY tenant, script;
INSERT INTO usage (tenant, namespace, day, cpu_ms, runs_started)
SELECT tenant, 'migrated', created_at / 86400000, 0, count(*) FROM runs
GROUP BY tenant, created_at / 86400000;
INSERT INTO usage (tenant, namespace, day, cpu_ms, runs_started)
SELECT tenant, '*', created_at / 86400000, 0, count(*) FROM runs
GROUP BY tenant, created_at / 86400000;
