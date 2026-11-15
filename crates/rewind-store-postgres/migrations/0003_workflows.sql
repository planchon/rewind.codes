-- Version 3: workflow versions, the workflow metadata of runs, and the watchers of a run's end
-- (`archi/06-storage-and-scale.md`, "Tables"; `archi/03-execution.md`, "Workflows across runs").

-- A workflow version: its `rewind-format` document, whose bytes hash to `version`, with the labels
-- of the key that saved it. Immutable. `ordinal` is the order versions were stored in, which
-- orders those created at the same time.
CREATE TABLE workflows (
    tenant text COLLATE "C" NOT NULL,
    version bytea NOT NULL,
    name text COLLATE "C" NOT NULL,
    document text NOT NULL,
    labels jsonb NOT NULL,
    created_at bigint NOT NULL,
    ordinal bigint GENERATED ALWAYS AS IDENTITY,
    PRIMARY KEY (tenant, version)
);
-- The versions of a workflow, newest first, and the latest of each.
CREATE INDEX workflows_by_name ON workflows (tenant, name, created_at, ordinal);

-- The workflow version a run executes; the run and the call that started it, and whether it was
-- started detached; the labels every run its scripts reach carries. A part of the metadata has
-- all of its columns, or none.
ALTER TABLE runs
    ADD COLUMN workflow text COLLATE "C",
    ADD COLUMN workflow_version bytea,
    ADD COLUMN started_by uuid,
    ADD COLUMN started_seq bigint,
    ADD COLUMN detached boolean,
    ADD COLUMN scope jsonb NOT NULL DEFAULT '{}',
    ADD CONSTRAINT runs_workflow CHECK ((workflow IS NULL) = (workflow_version IS NULL)),
    ADD CONSTRAINT runs_started_by
        CHECK ((started_by IS NULL) = (started_seq IS NULL)
               AND (started_by IS NULL) = (detached IS NULL));

-- The runs that receive a run's end: the run that started it (`attached` unless it was started
-- detached), and the forks that copied that start. Named by run id alone, like the other tables
-- of a run. A watcher row stays until one of its runs is deleted.
CREATE TABLE run_watchers (
    run_id uuid NOT NULL,
    watcher uuid NOT NULL,
    seq bigint NOT NULL,
    attached boolean NOT NULL,
    PRIMARY KEY (run_id, watcher)
);
-- The runs a run watches: those its end cancels (attached), and those to forget when it is
-- deleted.
CREATE INDEX run_watchers_by_watcher ON run_watchers (watcher, run_id);
