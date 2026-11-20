-- Version 10: named secrets attached to runs, and each tenant's signing keys (`archi/04-tools.md`,
-- "Secrets" and "Signed calls"; `archi/06-storage-and-scale.md`, "Tables").
--
-- The credentials per toolset, per namespace and toolset, and per run are dropped: a secret now
-- has a name, which they cannot be mapped to, and production called only host tools. Their data
-- keys go with them, since nothing is encrypted under them any more: a tenant's next secret or
-- signing key makes a new data key under the current master key, whose id `data_keys.master_key`
-- records (the first 8 bytes of its SHA-256, in hex).
--
-- `secrets` keeps an organization's secrets under the namespace `*`, which is not a namespace's
-- name. `ciphertext` is the value sealed with the tenant's data key and bound to its tenant,
-- namespace and name; the names, labels and dates are in clear. `secret_reads` counts a secret's
-- reads per day (days since the Unix epoch). `signing_keys` keeps each tenant's Ed25519 keys: the
-- 32 bytes of the public key, and the seed sealed with the data key, bound to its tenant and
-- `kid`; at most one key of a tenant is not retired, the one that signs. `runs.secrets` holds a
-- run's attachments, `{"<server or API>": "org/<name>" | "ns/<name>"}`, never a value.

DROP TABLE secrets;
DELETE FROM data_keys;
ALTER TABLE data_keys ADD COLUMN master_key text COLLATE "C" NOT NULL;
-- The rewrap finds the data keys held under a previous master key.
CREATE INDEX data_keys_by_master_key ON data_keys (master_key, tenant);

CREATE TABLE secrets (
    tenant text COLLATE "C" NOT NULL,
    namespace text COLLATE "C" NOT NULL,
    name text COLLATE "C" NOT NULL,
    ciphertext bytea NOT NULL,
    labels jsonb NOT NULL,
    created_by text NOT NULL,
    created_at bigint NOT NULL,
    updated_by text NOT NULL,
    updated_at bigint NOT NULL,
    last_read_at bigint,
    last_read_run uuid,
    last_read_namespace text COLLATE "C",
    PRIMARY KEY (tenant, namespace, name)
);

CREATE TABLE secret_reads (
    tenant text COLLATE "C" NOT NULL,
    namespace text COLLATE "C" NOT NULL,
    name text COLLATE "C" NOT NULL,
    day bigint NOT NULL,
    reads bigint NOT NULL,
    PRIMARY KEY (tenant, namespace, name, day)
);

CREATE TABLE signing_keys (
    tenant text COLLATE "C" NOT NULL,
    kid text COLLATE "C" NOT NULL,
    public_key bytea NOT NULL,
    private_ciphertext bytea NOT NULL,
    created_at bigint NOT NULL,
    retired_at bigint,
    PRIMARY KEY (tenant, kid)
);
CREATE UNIQUE INDEX signing_keys_current ON signing_keys (tenant) WHERE retired_at IS NULL;

ALTER TABLE runs ADD COLUMN secrets jsonb NOT NULL DEFAULT '{}';
