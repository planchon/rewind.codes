//! What closes a stream held open for a caller (`GET /v1/runs/{id}/events`, `GET /v1/changes`,
//! `GET /v1/calls/stream`; `archi/07-code-architecture.md`, "HTTP API `/v1`"): the expiry of the
//! token that opened it, the server stopping, and a change of the control plane after which the
//! caller no longer holds the permission it read with. Rights are read again when the control
//! plane says something of the caller's tenant changed, never on a timer.

use std::sync::Arc;
use std::time::Duration;

use rewind_api::access::Permission;
use rewind_core::Timestamp;
use rewind_engine::adapters::SystemClock;
use rewind_engine::ports::{Clock, ControlPlaneChange};
use tokio::sync::{broadcast, watch};

use crate::app::{App, Control};
use crate::auth::{self, Caller};

/// The watch kept by a stream over its caller's rights.
pub(crate) struct Rights {
    app: Arc<App>,
    caller: Caller,
    permission: Permission,
    control: broadcast::Receiver<Control>,
    stop: watch::Receiver<bool>,
    deadline: Option<tokio::time::Instant>,
    /// A change concerned the caller and its rights are not read again yet: kept across the
    /// waits a stream drops for another event.
    recheck: bool,
}

impl Rights {
    /// `expiry`: the access token's, in the world's time, if any.
    pub(crate) fn new(
        app: &Arc<App>,
        caller: Caller,
        permission: Permission,
        expiry: Option<Timestamp>,
    ) -> Self {
        let token = caller.token.as_ref().map(|token| token.expires_at);
        let expires_at = [expiry, token].into_iter().flatten().min();
        // Tokens expire in the world's time, whatever the server's clock.
        let deadline = expires_at.map(|expires_at| {
            let left = expires_at
                .as_millis()
                .saturating_sub(SystemClock.now().as_millis());
            tokio::time::Instant::now() + Duration::from_millis(left)
        });
        Self {
            control: app.control(),
            stop: app.stop.subscribe(),
            app: Arc::clone(app),
            caller,
            permission,
            deadline,
            recheck: false,
        }
    }

    pub(crate) fn caller(&self) -> &Caller {
        &self.caller
    }

    /// Returns once the stream must close. Cancel-safe: a stream waits on it beside its source.
    pub(crate) async fn lost(&mut self) {
        loop {
            if *self.stop.borrow() {
                return;
            }
            if self.recheck {
                if !auth::still_holds(&self.app, &self.caller, self.permission).await {
                    return;
                }
                self.recheck = false;
            }
            let deadline = self.deadline;
            let expired = async move {
                match deadline {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                () = expired => return,
                _ = self.stop.changed() => return,
                control = self.control.recv() => {
                    let concerned = match control {
                        Ok(Control::Change(change)) => self.concerns(&change),
                        Ok(Control::Lost) | Err(broadcast::error::RecvError::Lagged(_)) => true,
                        Err(broadcast::error::RecvError::Closed) => return,
                    };
                    self.recheck |= concerned;
                }
            }
        }
    }

    /// Whether a change may change what the caller holds.
    fn concerns(&self, change: &ControlPlaneChange) -> bool {
        let tenant = &self.caller.tenant;
        match change {
            ControlPlaneChange::Key { tenant: of, .. }
            | ControlPlaneChange::Token { tenant: of, .. }
            | ControlPlaneChange::TokenKey { tenant: of, .. }
            | ControlPlaneChange::Grant { tenant: of, .. }
            | ControlPlaneChange::Role { tenant: of, .. }
            | ControlPlaneChange::Namespace { tenant: of, .. }
            | ControlPlaneChange::Tenant { tenant: of } => of == tenant,
            ControlPlaneChange::Connection { .. } => self.caller.connection.is_some(),
            _ => false,
        }
    }
}
