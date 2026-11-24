-- Version 16: the workflow model (`archi/06-storage-and-scale.md`, "The control plane" and
-- "Rewriting the stored names": workflows; `archi/08-decisions.md`, "Taken while reviewing the
-- vocabulary against main").
--
-- A workflow is one entity: an id the API gives it, a scope (a namespace, or `*`, the
-- organization), a name unique in its scope or none, a description, versions numbered 1, 2, 3…,
-- the codes each scope has seen and its suite. The saved workflows move into it: each saved
-- workflow, by its tenant and name, becomes an organization's workflow whose id is derived from
-- them (`WorkflowId::of_saved`), with its versions numbered as they were, their digests and their
-- documents as they are (`rewind server rename-data` rewrites the documents in the new names),
-- the codes they ran and the suite of its latest version. The titles people gave codes wait in
-- `code_titles` until `rename-data` makes names of them. The permission to write scripts
-- becomes the permission to write workflows, which the permission to write toolsets or actions
-- now brings too.

-- The saved versions step aside while their rows move, then go. Their suites' rows go now: the
-- versions' documents list the same tests.
ALTER TABLE workflows RENAME TO saved_versions;
ALTER TABLE saved_versions RENAME CONSTRAINT workflows_pkey TO saved_versions_pkey;
ALTER INDEX workflows_by_name RENAME TO saved_versions_by_name;
ALTER INDEX workflows_by_number RENAME TO saved_versions_by_number;
DROP TABLE workflow_tests;

-- A workflow: its id (`wf_` and 12 characters), its scope (`namespace`, `*` for the
-- organization's), its name (unique in its scope; null for none) and description, the labels of
-- the grant that created it, for a fork the organization's workflow it copies and the version it
-- last took, who created it (a principal, as the audit log names it), when, when it was promoted
-- to the organization, and the number of its latest version, after which a new version takes its
-- number under the lock of the workflow's row.
CREATE TABLE workflows (
    tenant text COLLATE "C" NOT NULL,
    id text COLLATE "C" NOT NULL,
    namespace text COLLATE "C" NOT NULL,
    name text COLLATE "C",
    description text,
    labels jsonb NOT NULL,
    source_workflow text COLLATE "C",
    source_version bigint,
    created_by text NOT NULL,
    created_at bigint NOT NULL,
    promoted_at bigint,
    latest bigint NOT NULL,
    PRIMARY KEY (tenant, id),
    CONSTRAINT workflows_source CHECK ((source_workflow IS NULL) = (source_version IS NULL))
);
-- A name is unique in its scope (workflows without a name do not collide); a scope's workflows,
-- for its erasure.
CREATE UNIQUE INDEX workflows_by_name ON workflows (tenant, namespace, name);

-- A version of a workflow: its number, the digest of its document, the code its document runs
-- (its `code`, or `script` in the documents written before the code was named so), its
-- `rewind-format` document, the labels it was made under, and when. Immutable.
CREATE TABLE workflow_versions (
    tenant text COLLATE "C" NOT NULL,
    workflow text COLLATE "C" NOT NULL,
    version bigint NOT NULL,
    digest bytea NOT NULL,
    code bytea NOT NULL,
    document text NOT NULL,
    labels jsonb NOT NULL,
    created_at bigint NOT NULL,
    PRIMARY KEY (tenant, workflow, version)
);
-- A version by its digest: `triage@<digest>`, and the runs set aside in `legacy`.
CREATE INDEX workflow_versions_by_digest ON workflow_versions (tenant, digest);

-- Each code a scope has seen under a set of labels, and the workflow that first had it there.
-- `ordinal` is the order the rows were written in: among the rows of a code whose labels carry a
-- caller's, the first written wins. A row keeps its `ordinal` when its workflow moves scope.
CREATE TABLE workflow_codes (
    tenant text COLLATE "C" NOT NULL,
    namespace text COLLATE "C" NOT NULL,
    labels jsonb NOT NULL,
    code bytea NOT NULL,
    workflow text COLLATE "C" NOT NULL,
    added_at bigint NOT NULL,
    ordinal bigint GENERATED ALWAYS AS IDENTITY,
    PRIMARY KEY (tenant, namespace, labels, code)
);
-- A code's workflow in a scope, the first written first.
CREATE INDEX workflow_codes_by_code ON workflow_codes (tenant, code, namespace, ordinal);
-- A workflow's codes, which move with it.
CREATE INDEX workflow_codes_by_workflow ON workflow_codes (tenant, workflow);

-- A workflow's suite: every test it holds or held, by name and digest, when it joined, and when
-- and why it left (retired, or replaced). A test that joins again under its name and digest
-- takes its row back.
CREATE TABLE workflow_tests (
    tenant text COLLATE "C" NOT NULL,
    workflow text COLLATE "C" NOT NULL,
    name text COLLATE "C" NOT NULL,
    digest bytea NOT NULL,
    added_at bigint NOT NULL,
    retired_at bigint,
    reason text,
    PRIMARY KEY (tenant, workflow, name, digest)
);
-- Whether a suite references a test: a test is kept while one does, current or retired.
CREATE INDEX workflow_tests_by_test ON workflow_tests (tenant, digest);

-- Each saved workflow's id: `wf_` and the low 60 bits of the first 8 bytes of the SHA-256 of
-- `rewind:workflow:<tenant>/<name>`, read big-endian, five bits a character from the highest, in Crockford's base32 in lower case, as `WorkflowId::of_saved` computes it.
CREATE TEMPORARY TABLE saved_workflows ON COMMIT DROP AS
SELECT tenant, name, 'wf_' || (
    SELECT string_agg(
        substr('0123456789abcdefghjkmnpqrstvwxyz', ((bits >> (5 * k)) & 31)::integer + 1, 1),
        '' ORDER BY k DESC)
    FROM generate_series(0, 11) AS k
) AS id
FROM (
    SELECT DISTINCT tenant, name,
        ('x' || encode(substring(
            sha256(convert_to('rewind:workflow:' || tenant || '/' || name, 'UTF8'))
            FROM 1 FOR 8), 'hex'))::bit(64)::bigint AS bits
    FROM saved_versions
) saved;

-- A saved workflow is the organization's, created by `rename-data` when its first version was:
-- its labels are its first version's, its description its latest version's.
INSERT INTO workflows (tenant, id, namespace, name, description, labels, created_by, created_at,
    latest)
SELECT s.tenant, s.id, '*', s.name, latest.document::jsonb ->> 'description', first.labels,
    'rename-data', first.created_at, latest.number
FROM saved_workflows s
CROSS JOIN LATERAL (
    SELECT labels, created_at FROM saved_versions v
    WHERE v.tenant = s.tenant AND v.name = s.name ORDER BY number LIMIT 1
) first
CROSS JOIN LATERAL (
    SELECT document, number FROM saved_versions v
    WHERE v.tenant = s.tenant AND v.name = s.name ORDER BY number DESC LIMIT 1
) latest;

INSERT INTO workflow_versions (tenant, workflow, version, digest, code, document, labels,
    created_at)
SELECT v.tenant, s.id, v.number, v.version,
    decode(COALESCE(v.document::jsonb ->> 'code', v.document::jsonb ->> 'script'), 'hex'),
    v.document, v.labels, v.created_at
FROM saved_versions v
JOIN saved_workflows s ON s.tenant = v.tenant AND s.name = v.name;

-- Each code under each set of labels belongs to the workflow whose version had it first, written
-- in the order the versions were: as the versions' writes would have added them.
INSERT INTO workflow_codes (tenant, namespace, labels, code, workflow, added_at)
SELECT tenant, '*', labels, code, workflow, created_at
FROM (
    SELECT DISTINCT ON (v.tenant, v.labels, v.code)
        v.tenant, v.labels, v.code, v.workflow, v.created_at, o.ordinal
    FROM workflow_versions v
    JOIN saved_versions o ON o.tenant = v.tenant AND o.version = v.digest
    ORDER BY v.tenant, v.labels, v.code, v.created_at, o.ordinal
) first
ORDER BY created_at, ordinal;

-- The suites. Each test a version lists, by name and digest; a test whose row is gone (its
-- namespace was erased) is left out.
CREATE TEMPORARY TABLE listed ON COMMIT DROP AS
SELECT v.tenant, v.workflow, v.version, t.name, decode(t.test, 'hex') AS digest
FROM workflow_versions v
CROSS JOIN LATERAL jsonb_to_recordset(COALESCE(v.document::jsonb -> 'tests', '[]'))
    AS t(name text, test text)
WHERE EXISTS (SELECT 1 FROM tests WHERE tests.tenant = v.tenant
              AND tests.digest = decode(t.test, 'hex'));

-- A test listed by the latest version is in the suite; one an older version listed left it with
-- the version after the last that listed it, for the reason that version gave for its name, if
-- any (`retired`, then `replaced`, `updated` in the documents written before). Either joined
-- with the first version of the run of versions that lists it up to its last.
INSERT INTO workflow_tests (tenant, workflow, name, digest, added_at, retired_at, reason)
SELECT l.tenant, l.workflow, l.name, l.digest, joined.created_at, leaving.created_at,
    COALESCE(
        (SELECT r ->> 'reason' FROM jsonb_array_elements(leaving.document::jsonb -> 'retired') r
         WHERE r ->> 'name' = l.name LIMIT 1),
        (SELECT r ->> 'reason'
         FROM jsonb_array_elements(COALESCE(leaving.document::jsonb -> 'replaced',
                                            leaving.document::jsonb -> 'updated')) r
         WHERE r ->> 'name' = l.name LIMIT 1))
FROM (
    SELECT tenant, workflow, name, digest, max(version) AS last
    FROM listed GROUP BY tenant, workflow, name, digest
) l
JOIN workflow_versions joined ON joined.tenant = l.tenant AND joined.workflow = l.workflow
    AND joined.version = 1 + COALESCE((
        SELECT max(v.version) FROM workflow_versions v
        WHERE v.tenant = l.tenant AND v.workflow = l.workflow AND v.version < l.last
        AND NOT EXISTS (SELECT 1 FROM listed x
                        WHERE x.tenant = v.tenant AND x.workflow = v.workflow
                        AND x.version = v.version AND x.name = l.name AND x.digest = l.digest)
    ), 0)
LEFT JOIN workflow_versions leaving ON leaving.tenant = l.tenant
    AND leaving.workflow = l.workflow AND leaving.version = l.last + 1;

DROP TABLE saved_versions;

-- The titles people gave codes, kept for `rename-data` (`CatalogMigration::code_titles`), which
-- deletes them once each became a name or was listed.
ALTER TABLE script_titles RENAME TO code_titles;
ALTER TABLE code_titles RENAME CONSTRAINT script_titles_pkey TO code_titles_pkey;
ALTER TABLE code_titles DROP COLUMN updated_by, DROP COLUMN updated_at;

-- `workflows:write` where `scripts:write` was, and beside `toolsets:write` and `actions:write`,
-- which saved workflows: in the roles, the end users' tokens and the token keys, each set sorted
-- by its bytes, as the store writes them.
UPDATE roles SET permissions = ARRAY(
    SELECT p FROM unnest(array_remove(permissions, 'scripts:write') || '{workflows:write}'::text[])
        AS p
    GROUP BY p ORDER BY p COLLATE "C")
WHERE permissions && '{scripts:write,toolsets:write,actions:write}'::text[];
UPDATE tokens SET permissions = ARRAY(
    SELECT p FROM unnest(array_remove(permissions, 'scripts:write') || '{workflows:write}'::text[])
        AS p
    GROUP BY p ORDER BY p COLLATE "C")
WHERE permissions && '{scripts:write,toolsets:write,actions:write}'::text[];
UPDATE token_keys SET permissions = ARRAY(
    SELECT p FROM unnest(array_remove(permissions, 'scripts:write') || '{workflows:write}'::text[])
        AS p
    GROUP BY p ORDER BY p COLLATE "C")
WHERE permissions && '{scripts:write,toolsets:write,actions:write}'::text[];
