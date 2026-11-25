-- Version 17: the catalog in the words of the rename release (`archi/06-storage-and-scale.md`,
-- "Rewriting the stored names"; `archi/08-decisions.md`, "Taken for the vocabulary": the set-aside
-- legacy tables keep their names; the catalog's are renamed).
--
-- What SQL can rename is renamed here, once, when the new build opens the store: the tables
-- and columns that still said toolset, script (the quotas of scripts and script lines too),
-- epoch (a partition's) or events (a subscription's), the principals `user:` that are members
-- now, the permissions renamed and the types of the webhooks a subscription lists. The documents whose digest is their identity
-- (the pinned action sets, the workflow versions, the tests) are rewritten by
-- `rewind server rename-data`, which computes their new digests: until it ran, they are refused
-- when read, as the workflow model's versions are.

-- The tables of the code and the pinned action sets.
ALTER TABLE scripts RENAME TO code;
ALTER TABLE code RENAME CONSTRAINT scripts_pkey TO code_pkey;
ALTER TABLE toolsets RENAME TO action_sets;
ALTER TABLE action_sets RENAME CONSTRAINT toolsets_pkey TO action_sets_pkey;

-- A partition's move counter is its generation; the determinism epochs of `nodes` keep their
-- name.
ALTER TABLE partitions RENAME COLUMN epoch TO generation;

-- The quotas count workflows and a code's lines, for an organization, its namespaces' defaults
-- and each namespace.
ALTER TABLE tenants RENAME COLUMN scripts TO workflows;
ALTER TABLE tenants RENAME COLUMN max_script_lines TO max_code_lines;
ALTER TABLE tenants RENAME COLUMN default_scripts TO default_workflows;
ALTER TABLE tenants RENAME COLUMN default_max_script_lines TO default_max_code_lines;
ALTER TABLE namespaces RENAME COLUMN scripts TO workflows;
ALTER TABLE namespaces RENAME COLUMN max_script_lines TO max_code_lines;

-- A subscription lists webhook types.
ALTER TABLE subscriptions RENAME COLUMN events TO types;
UPDATE subscriptions SET types = replace(replace(replace(types,
    '"run.suspended"', '"run.waiting"'),
    '"credentials.expired"', '"secret.expired"'),
    '"tool.schema_changed"', '"action.schema_changed"')
WHERE types LIKE '%"run.suspended"%'
   OR types LIKE '%"credentials.expired"%'
   OR types LIKE '%"tool.schema_changed"%';

-- A person in an organization is a member: `user:<id>` is `member:<id>`, wherever a principal is
-- named.
UPDATE grants SET principal = 'member:' || substr(principal, 6) WHERE principal LIKE 'user:%';
UPDATE grants SET granted_by = 'member:' || substr(granted_by, 6) WHERE granted_by LIKE 'user:%';
UPDATE api_keys SET created_by = 'member:' || substr(created_by, 6) WHERE created_by LIKE 'user:%';
UPDATE tokens SET created_by = 'member:' || substr(created_by, 6) WHERE created_by LIKE 'user:%';
UPDATE token_keys SET created_by = 'member:' || substr(created_by, 6)
WHERE created_by LIKE 'user:%';
UPDATE secrets SET created_by = 'member:' || substr(created_by, 6) WHERE created_by LIKE 'user:%';
UPDATE secrets SET updated_by = 'member:' || substr(updated_by, 6) WHERE updated_by LIKE 'user:%';
UPDATE workflows SET created_by = 'member:' || substr(created_by, 6)
WHERE created_by LIKE 'user:%';

-- The permissions renamed: `toolsets:read` and `toolsets:write` are `actions:read` and
-- `actions:write`, `tasks:work` and `tasks:approve` are `calls:work` and `calls:approve`, in the
-- roles, the end users' tokens and the token keys, each set sorted by its bytes, as the store
-- writes them.
CREATE FUNCTION pg_temp.renamed_permissions(permissions text[]) RETURNS text[] AS $$
    SELECT ARRAY(
        SELECT DISTINCT CASE p
            WHEN 'toolsets:read' THEN 'actions:read'
            WHEN 'toolsets:write' THEN 'actions:write'
            WHEN 'tasks:work' THEN 'calls:work'
            WHEN 'tasks:approve' THEN 'calls:approve'
            ELSE p
        END COLLATE "C" AS renamed
        FROM unnest(permissions) AS p
        ORDER BY renamed)
$$ LANGUAGE sql IMMUTABLE;
UPDATE roles SET permissions = pg_temp.renamed_permissions(permissions)
WHERE permissions && '{toolsets:read,toolsets:write,tasks:work,tasks:approve}'::text[];
UPDATE tokens SET permissions = pg_temp.renamed_permissions(permissions)
WHERE permissions && '{toolsets:read,toolsets:write,tasks:work,tasks:approve}'::text[];
UPDATE token_keys SET permissions = pg_temp.renamed_permissions(permissions)
WHERE permissions && '{toolsets:read,toolsets:write,tasks:work,tasks:approve}'::text[];
