-- Version 6: tests and the suites of workflow versions (`archi/06-storage-and-scale.md`, "Tables";
-- `archi/05-testing-and-observability.md`, "Where tests are stored").

-- The index row of a test, whose document is a blob of the `BlobStore` under its digest: the
-- namespace of the run a recorded test came from (null for a test written by hand), the size of
-- the document in bytes, when it was stored or reserved last (its grace starts there), and when
-- a cleanup started deleting it (its blob may be gone).
CREATE TABLE tests (
    tenant text COLLATE "C" NOT NULL,
    digest bytea NOT NULL,
    namespace text COLLATE "C",
    size bigint NOT NULL,
    reserved_at bigint NOT NULL,
    deletion_started_at bigint,
    PRIMARY KEY (tenant, digest)
);
-- The tests recorded from a namespace's runs, for its erasure.
CREATE INDEX tests_by_namespace ON tests (tenant, namespace);

-- The tests of each version's suite, written with the version.
CREATE TABLE workflow_tests (
    tenant text COLLATE "C" NOT NULL,
    version bytea NOT NULL,
    digest bytea NOT NULL,
    PRIMARY KEY (tenant, version, digest)
);
-- Whether a version references a test: a test is kept while one does.
CREATE INDEX workflow_tests_by_test ON workflow_tests (tenant, digest);
