//! The production store passes the conformance suites of `JournalStore`, `ReadModel` and
//! `SecretStore` against Postgres in Docker, each check on a new database. The journal's suite
//! passes again once `history` and `inbox` are partitioned by run, as the scaling path of
//! `archi/06-storage-and-scale.md` will partition them.

mod support;

use rewind_engine::conformance;
use rewind_store_postgres::PostgresStore;
use support::Postgres;

/// Creates a new, empty database for each check.
fn stores(postgres: &Postgres) -> impl Fn() -> PostgresStore + '_ {
    move || {
        // The suites create stores synchronously; connecting awaits its first I/O.
        tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(postgres.store()))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_store_conforms_on_postgres() {
    let Some(postgres) = Postgres::start("conformance") else {
        return;
    };
    conformance::journal::run_all(stores(&postgres)).await;
    conformance::read_model::run_all(stores(&postgres)).await;
    conformance::secrets::run_all(stores(&postgres)).await;
}

/// Replaces `history` and `inbox` with tables partitioned by hash of `run_id`, with the same
/// columns, defaults, keys and indexes: what an operator does when the tables grow.
const PARTITION: &str = "
BEGIN;
ALTER TABLE history RENAME TO history_unpartitioned;
CREATE TABLE history (LIKE history_unpartitioned INCLUDING ALL) PARTITION BY HASH (run_id);
DROP TABLE history_unpartitioned;
ALTER SEQUENCE inbox_ids OWNED BY NONE;
ALTER TABLE inbox RENAME TO inbox_unpartitioned;
CREATE TABLE inbox (LIKE inbox_unpartitioned INCLUDING ALL) PARTITION BY HASH (run_id);
DROP TABLE inbox_unpartitioned;
ALTER SEQUENCE inbox_ids OWNED BY inbox.id;
DO $$
BEGIN
    FOR n IN 0..3 LOOP
        EXECUTE format(
            'CREATE TABLE history_%s PARTITION OF history FOR VALUES WITH (MODULUS 4, REMAINDER %s)',
            n, n);
        EXECUTE format(
            'CREATE TABLE inbox_%s PARTITION OF inbox FOR VALUES WITH (MODULUS 4, REMAINDER %s)',
            n, n);
    END LOOP;
END $$;
COMMIT;
";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_journal_conforms_with_history_and_inbox_partitioned_by_run() {
    let Some(postgres) = Postgres::start("partitioned") else {
        return;
    };
    conformance::journal::run_all(|| {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let database = postgres.create_database();
                let store = postgres.connect(&database).await;
                postgres.psql(&database, PARTITION);
                assert_eq!(
                    postgres.psql(
                        &database,
                        "SELECT count(*) FROM pg_partitioned_table p \
                         JOIN pg_class c ON c.oid = p.partrelid \
                         WHERE c.relname IN ('history', 'inbox')"
                    ),
                    "2"
                );
                store
            })
        })
    })
    .await;
}
