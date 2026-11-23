-- Version 13: run handles, script titles and the numbers of workflow versions
-- (`archi/06-storage-and-scale.md`, "Run handles"; `archi/04-tools.md`, "Script titles").

-- A run's handle is `<namespace>/<origin>/<suffix>`. Its origin is what ran (`NewRun::origin`);
-- a run stored before handles takes its workflow's name, or `script`. Its suffix is the start of
-- its id in base32, `suffix_len` characters: 12 (its millisecond and the 12 bits after it, which
-- the engine never repeats within a process), more for a run that shares them with another run
-- of its tenant.
ALTER TABLE runs
    ADD COLUMN origin text COLLATE "C" NOT NULL DEFAULT 'script',
    ADD COLUMN suffix_len smallint NOT NULL DEFAULT 12 CHECK (suffix_len BETWEEN 12 AND 25);
ALTER TABLE runs ALTER COLUMN origin DROP DEFAULT;
UPDATE runs SET origin = workflow WHERE workflow IS NOT NULL;
-- The runs stored before handles may share the first 64 bits of their id (the millisecond, the
-- version and the 12 bits after it) with another run of their tenant: every run of such a group
-- takes the full suffix, 25 characters, which two ids never share.
UPDATE runs SET suffix_len = 25
WHERE (tenant, left(replace(id::text, '-', ''), 16)) IN (
    SELECT tenant, left(replace(id::text, '-', ''), 16) FROM runs
    GROUP BY 1, 2 HAVING count(*) > 1
);

-- A summary keeps the handle of its run as the run's record has it; only the summaries whose
-- run differs from the defaults are written. A summary kept longer than its run takes its
-- workflow's name, or `script`, and the full suffix when it shares the first 64 bits of its id
-- with another summary of its tenant.
ALTER TABLE run_summaries
    ADD COLUMN origin text COLLATE "C" NOT NULL DEFAULT 'script',
    ADD COLUMN suffix_len smallint NOT NULL DEFAULT 12 CHECK (suffix_len BETWEEN 12 AND 25);
ALTER TABLE run_summaries ALTER COLUMN origin DROP DEFAULT;
UPDATE run_summaries s SET origin = r.origin, suffix_len = r.suffix_len
FROM runs r
WHERE r.id = s.run_id AND (r.origin <> 'script' OR r.suffix_len <> 12);
UPDATE run_summaries s
SET origin = COALESCE(s.workflow, 'script'),
    suffix_len = CASE WHEN shared.summaries > 1 THEN 25 ELSE 12 END
FROM (
    SELECT run_id, count(*) OVER (PARTITION BY tenant, left(replace(run_id::text, '-', ''), 16))
        AS summaries
    FROM run_summaries
) shared
WHERE shared.run_id = s.run_id AND NOT EXISTS (SELECT 1 FROM runs r WHERE r.id = s.run_id);

-- The title and description a person gave a stored script, over the comment at its top
-- (`ScriptTitle`): `updated_by` names a principal as the audit log does.
CREATE TABLE script_titles (
    tenant text COLLATE "C" NOT NULL,
    digest bytea NOT NULL,
    title text NOT NULL,
    description text,
    updated_by text NOT NULL,
    updated_at bigint NOT NULL,
    PRIMARY KEY (tenant, digest)
);

-- A version's number among the versions of its workflow in its tenant, from 1 in the order they
-- were stored (`triage v12`): those stored before are numbered by their creation, then by
-- `ordinal`. A new version takes the next number under a lock of its workflow, which the unique
-- index backs; it also finds a workflow's highest number.
ALTER TABLE workflows ADD COLUMN number bigint;
UPDATE workflows w SET number = n.number
FROM (
    SELECT tenant, version,
        row_number() OVER (PARTITION BY tenant, name ORDER BY created_at, ordinal) AS number
    FROM workflows
) n
WHERE n.tenant = w.tenant AND n.version = w.version;
ALTER TABLE workflows ALTER COLUMN number SET NOT NULL;
CREATE UNIQUE INDEX workflows_by_number ON workflows (tenant, name, number);
