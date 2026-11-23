//! The control plane's daily jobs (`archi/06-storage-and-scale.md`, "Retention and deletion"):
//! the end users' tokens and the connections to delete, and the tests no workflow version
//! references. They read nothing on a timer but once a day: one of the clocks that read no
//! store between their runs (`archi/07-code-architecture.md`, principle 11). Every node runs
//! them; each step is idempotent.

use std::sync::Arc;
use std::time::Duration;

use rewind_core::Timestamp;
use rewind_engine::ports::{AccessStore, StoreError};

use crate::app::App;

/// How often the jobs run.
const DAY: Duration = Duration::from_secs(86_400);
/// How long an end user's opaque token is kept once expired or revoked: a margin for the
/// clocks of the servers, and a day to read its row after an incident.
const TOKENS_KEPT: Duration = Duration::from_secs(24 * 3_600);
/// How many rows one statement of a purge deletes.
const PURGE_BATCH: usize = 1_000;
/// How long what ends of people's connections is kept: codes and tokens after they expired,
/// connections after they were revoked, registered clients no connection uses after their
/// last use (`archi/07-code-architecture.md`, "MCP and OAuth").
const CONNECTIONS_KEPT: Duration = Duration::from_secs(24 * 3_600);
/// How long a connection is kept after its last refresh token lapsed.
const LAPSED_CONNECTIONS_KEPT: Duration = Duration::from_secs(30 * 86_400);

/// Runs the jobs at start, then once a day, until the server stops.
pub(crate) async fn daily(app: Arc<App>) {
    let mut stop = app.stop.subscribe();
    while !app.stopping() {
        run(&app).await;
        if tokio::time::timeout(DAY, stop.changed()).await.is_ok() {
            return;
        }
    }
}

async fn run(app: &App) {
    match rewind_engine::suites::clean_tests(app.catalog(), app.blobs(), app.now()).await {
        Ok(0) => {}
        Ok(cleaned) => tracing::info!(cleaned, "tests no version references deleted"),
        Err(error) => tracing::error!(%error, "the tests no version references were not deleted"),
    }
    match purge_tokens(app).await {
        Ok(0) => {}
        Ok(purged) => tracing::info!(purged, "ended tokens deleted"),
        Err(error) => tracing::error!(%error, "the ended tokens were not deleted"),
    }
    match purge_connections(app).await {
        Ok(0) => {}
        Ok(purged) => tracing::info!(
            purged,
            "ended connections, codes, tokens and clients deleted"
        ),
        Err(error) => tracing::error!(%error, "the ended connections were not deleted"),
    }
}

/// Deletes the end users' opaque tokens expired or revoked more than [`TOKENS_KEPT`] ago, batch
/// after batch.
async fn purge_tokens(app: &App) -> Result<u64, StoreError> {
    let kept = u64::try_from(TOKENS_KEPT.as_millis()).unwrap_or(u64::MAX);
    let before = Timestamp::from_millis(app.now().as_millis().saturating_sub(kept));
    let mut purged = 0;
    loop {
        let batch = app.store.purge_tokens(before, PURGE_BATCH).await?;
        purged += batch;
        if batch < PURGE_BATCH as u64 || app.stopping() {
            return Ok(purged);
        }
    }
}

/// Deletes what ended of people's connections, in the world's time, batch after batch: the
/// connections revoked more than a day ago or whose last refresh token lapsed more than 30 days
/// ago (with their grants and tokens), the tokens and codes expired more than a day ago, and
/// the clients no connection uses, last used more than a day ago.
async fn purge_connections(app: &App) -> Result<u64, StoreError> {
    let now = crate::connections::world_now();
    let ago = |kept: Duration| {
        let kept = u64::try_from(kept.as_millis()).unwrap_or(u64::MAX);
        Timestamp::from_millis(now.as_millis().saturating_sub(kept))
    };
    let (day, month) = (ago(CONNECTIONS_KEPT), ago(LAPSED_CONNECTIONS_KEPT));
    let mut purged = 0;
    for step in 0..4 {
        loop {
            let batch = match step {
                0 => app.store.purge_connections(month, day, PURGE_BATCH).await?,
                1 => app.store.purge_connection_tokens(day, PURGE_BATCH).await?,
                2 => app.store.purge_oauth_codes(day, PURGE_BATCH).await?,
                _ => app.store.purge_oauth_clients(day, PURGE_BATCH).await?,
            };
            purged += batch;
            if batch < PURGE_BATCH as u64 || app.stopping() {
                break;
            }
        }
    }
    Ok(purged)
}
