//! The embedded store passes the conformance suites of `JournalStore`, `ReadModel` and
//! `SecretStore`, each check on a new database file.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use ptc_engine::conformance;
use ptc_store_turso::{MasterKey, TursoStore};
use tempfile::TempDir;

/// Opens a new, empty database in `dir` for each check.
fn stores(dir: &TempDir) -> impl Fn() -> TursoStore + '_ {
    let next = AtomicUsize::new(0);
    move || {
        let path: PathBuf = dir.path().join(format!(
            "journal-{}.db",
            next.fetch_add(1, Ordering::SeqCst)
        ));
        // The suites create stores synchronously; opening one awaits its first I/O.
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current()
                .block_on(TursoStore::open(path, MasterKey::from_bytes([1; 32])))
        })
        .expect("open a new store")
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_journal_conforms() {
    let dir = TempDir::new().expect("temporary directory");
    conformance::journal::run_all(stores(&dir)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_read_model_conforms() {
    let dir = TempDir::new().expect("temporary directory");
    conformance::read_model::run_all(stores(&dir)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_secrets_conform() {
    let dir = TempDir::new().expect("temporary directory");
    conformance::secrets::run_all(stores(&dir)).await;
}
