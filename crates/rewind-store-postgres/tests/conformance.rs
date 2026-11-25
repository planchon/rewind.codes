//! The production store passes the conformance suites of `AccessStore` and `SecretStore`
//! against Postgres in Docker, each check on a new database. Those of `CatalogStore` and
//! `Cluster` run in `tests/control_plane.rs`.

use crate::support;

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
    conformance::access::run_all(stores(&postgres)).await;
    conformance::secrets::run_all(stores(&postgres)).await;
}
