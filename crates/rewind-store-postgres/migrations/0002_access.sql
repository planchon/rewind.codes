-- Access, version 2 (`archi/06-storage-and-scale.md`, "Tables"; `AccessStore`): API keys gain a
-- name and their creator, and what a key may do moves to its grant, held by the principal
-- `key:<hex of the hash>`. The tenants' own roles, the grants and each tenant's default role get
-- their tables.
--
-- Each key of version 1 gets a grant of its role, with its allowed labels, granted when the key
-- was created and by no one; the role `admin`, the operator's before roles became data, becomes
-- `operator`. Version 1 kept no name: each key is named `unnamed <role> key`.
--
-- A role's permissions are an array of the catalog's names, which the store keeps without
-- reading them. Principals are their text, whose byte order is the order of principals.

CREATE TABLE roles (
    tenant text COLLATE "C" NOT NULL,
    name text COLLATE "C" NOT NULL,
    description text NOT NULL,
    permissions text[] NOT NULL,
    created_at bigint NOT NULL,
    updated_at bigint NOT NULL,
    PRIMARY KEY (tenant, name)
);

CREATE TABLE grants (
    tenant text COLLATE "C" NOT NULL,
    principal text COLLATE "C" NOT NULL,
    role text COLLATE "C" NOT NULL,
    allowed_labels jsonb NOT NULL,
    granted_by text COLLATE "C",
    granted_at bigint NOT NULL,
    PRIMARY KEY (tenant, principal)
);
CREATE INDEX grants_by_role ON grants (tenant, role);

CREATE TABLE access_settings (
    tenant text COLLATE "C" PRIMARY KEY,
    default_role text COLLATE "C" NOT NULL
);

INSERT INTO grants (tenant, principal, role, allowed_labels, granted_by, granted_at)
SELECT tenant, 'key:' || encode(hash, 'hex'),
       CASE role WHEN 'admin' THEN 'operator' ELSE role END,
       allowed_labels, NULL, created_at
FROM api_keys;

ALTER TABLE api_keys
    ADD COLUMN name text,
    ADD COLUMN created_by text COLLATE "C";
UPDATE api_keys
SET name = 'unnamed ' || CASE role WHEN 'admin' THEN 'operator' ELSE role END || ' key';
ALTER TABLE api_keys
    ALTER COLUMN name SET NOT NULL,
    DROP COLUMN role,
    DROP COLUMN allowed_labels;
CREATE INDEX api_keys_by_tenant ON api_keys (tenant, created_at, hash);
