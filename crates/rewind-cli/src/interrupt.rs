//! Ctrl-C (`archi/08-decisions.md`, "The embedded CLI"): a first Ctrl-C lets the command finish
//! the call in flight, a second one exits at once with 130.
//!
//! A call that changes something (a start, a signal, a fork, a cancellation, a release, a
//! local scenario or replay) is always finished: abandoning it would leave the user not knowing
//! whether it happened. No such call starts after the first Ctrl-C: `rewind run` interrupted while
//! it registers its action set starts no run, and exits with 130. What only reads or waits (waiting
//! for a run to end, listing pages) is abandoned at the first Ctrl-C, and the command reports
//! what it knows.

use std::future::Future;
use std::sync::OnceLock;

use tokio::sync::watch;

use crate::error::CliError;
use crate::exit::Exit;

fn flag() -> &'static watch::Sender<bool> {
    static FLAG: OnceLock<watch::Sender<bool>> = OnceLock::new();
    FLAG.get_or_init(|| watch::channel(false).0)
}

/// Listens to Ctrl-C for the life of the process. Called once, by [`crate::main`], within the
/// Tokio runtime.
pub(crate) fn install() {
    tokio::spawn(async {
        if tokio::signal::ctrl_c().await.is_err() {
            return;
        }
        request();
        eprintln!("interrupted: finishing the call in flight (Ctrl-C again to exit at once)");
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("interrupted");
            std::process::exit(i32::from(Exit::Interrupted.code()));
        }
    });
}

/// Records a first Ctrl-C.
pub(crate) fn request() {
    flag().send_replace(true);
}

/// Whether Ctrl-C was pressed: the command starts nothing new.
pub(crate) fn requested() -> bool {
    *flag().borrow()
}

/// Before a call that changes something: refused once Ctrl-C was pressed, so that the command
/// finishes the call in flight and sends nothing new. `what` says what was not done.
pub(crate) fn before_change(what: &str) -> Result<(), CliError> {
    if requested() {
        return Err(CliError::Interrupted(format!("{what} was not sent")));
    }
    Ok(())
}

/// Resolves at the first Ctrl-C.
pub(crate) async fn requested_now() {
    let mut receiver = flag().subscribe();
    // The sender lives in a static: it is never dropped, so `wait_for` only ends with `true`.
    let _ = receiver.wait_for(|interrupted| *interrupted).await;
}

/// Runs a read or a wait unless Ctrl-C comes first: `None` when it was abandoned.
pub(crate) async fn abandonable<F: Future>(future: F) -> Option<F::Output> {
    if requested() {
        return None;
    }
    tokio::select! {
        output = future => Some(output),
        () = requested_now() => None,
    }
}
