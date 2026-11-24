//! The change stream (`GET /v1/changes`): what changes in the organization, as the caller may
//! see it (`archi/07-code-architecture.md`, "HTTP API `/v1`"; `archi/05-testing-and-observability.md`,
//! "Live").
//!
//! ```text
//!  owners of the scope's partitions ── Changed (once durable) ──► run {summary}
//!  Membership::changes ───────────── ControlPlaneChange ────────► code, workflow, secret,
//!                                                                 namespace, connection
//!  a subscription lost, changes lost ────────────────────────────► resync, then closed
//! ```

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::Stream;
use rewind_api::access::Permission;
use rewind_api::events::{
    Change, ConnectionChanged, NamespaceChanged, Resync, ScriptChanged, SecretChanged,
    WorkflowChanged,
};
use rewind_core::Digest;
use rewind_engine::adapters::SystemClock;
use rewind_engine::ports::link::{Changed, SubscriptionKey};
use rewind_engine::ports::{
    CatalogStore, Clock, ControlPlaneChange, Day, SecretNamespace, SecretStore,
};
use tokio::sync::broadcast::{self, error::RecvError};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::app::{App, Control};
use crate::auth::{self, Caller, TokenExpiry};
use crate::error::ApiError;
use crate::follow;
use crate::http::search;
use crate::idle::Busy;
use crate::streams::Rights;

/// `GET /v1/changes`. Each `run` event holds the run's summary as `GET /v1/runs` lists it, for
/// the runs the caller may see: of its tenant, in the namespace its request is bound to (an end
/// user's token's own), carrying every label of its grant. No other run shows, not even its id.
/// The control plane's events name what changed, for what the caller may read. `resync` says the
/// client should read again what it shows. There is no resume. Closed at a member's or a
/// connection's access token's expiry (an access token already expired is refused with 401);
/// once an end user's token or a connection no longer authenticates; once a key is revoked or
/// the grant of a key, a member or a connection is removed, changes its labels or no longer
/// gives `runs:read`; and after `resync` when a subscription is lost.
pub(super) async fn stream(
    State(app): State<Arc<App>>,
    caller: Caller,
    TokenExpiry(expiry): TokenExpiry,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    caller.require(Permission::RunsRead)?;
    // Tokens expire in the world's time, whatever the server's clock.
    if let Some(expires_at) = expiry
        && expires_at.as_millis() <= SystemClock.now().as_millis()
    {
        return Err(auth::unauthorized(
            "the access token has expired: POST /auth/session gives a new one, then open the \
             stream again",
        ));
    }
    let key = match &caller.namespace {
        Some(namespace) => SubscriptionKey::Namespace {
            tenant: caller.tenant.clone(),
            namespace: namespace.clone(),
        },
        None => SubscriptionKey::Tenant(caller.tenant.clone()),
    };
    let follow = Follow {
        control: app.control(),
        runs: follow::follow(&app, key),
        rights: Rights::new(&app, caller, Permission::RunsRead, expiry),
        _busy: app.idle.busy(),
        app,
        pending: VecDeque::new(),
        titles: HashMap::new(),
        closing: false,
    };
    let events = futures_util::stream::unfold(follow, |mut follow| async move {
        follow.next_event().await.map(|event| (Ok(event), follow))
    });
    Ok(Sse::new(events).keep_alive(KeepAlive::default()))
}

/// Where a stream stands.
struct Follow {
    app: Arc<App>,
    rights: Rights,
    runs: UnboundedReceiver<Changed>,
    control: broadcast::Receiver<Control>,
    pending: VecDeque<Event>,
    /// The titles of the workflows the stream's runs ran, read once and forgotten when the
    /// code's `script` change comes (a title given or removed).
    titles: HashMap<Digest, Option<String>>,
    /// A subscription was lost: the stream ends once `pending` is sent.
    closing: bool,
    /// A stream keeps the embedded server from stopping on idle, as a followed run's does.
    _busy: Busy,
}

impl Follow {
    async fn next_event(&mut self) -> Option<Event> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Some(event);
            }
            if self.closing {
                return None;
            }
            tokio::select! {
                () = self.rights.lost() => return None,
                changed = self.runs.recv() => match changed {
                    Some(changed) => self.run(changed).await,
                    None => {
                        self.pending.push_back(resync());
                        self.closing = true;
                    }
                },
                control = self.control.recv() => match control {
                    Ok(Control::Change(change)) => self.control_plane(change).await,
                    Ok(Control::Lost) | Err(RecvError::Lagged(_)) => self.pending.push_back(resync()),
                    Err(RecvError::Closed) => return None,
                },
            }
        }
    }

    /// A run's summary, with its code's title, if the caller may see the run.
    async fn run(&mut self, changed: Changed) {
        let caller = self.rights.caller();
        let Some(summary) = changed.summary else {
            return;
        };
        if summary.run.tenant != caller.tenant
            || !caller.reaches(&summary.namespace)
            || !caller.sees(&summary.labels)
        {
            return;
        }
        let code = summary.code;
        let title = match self.titles.get(&code) {
            Some(title) => title.clone(),
            None => {
                // A title that cannot be read now is left out, and read again next time.
                match self.app.titles(&caller.tenant, [code]).await {
                    Ok(mut titles) => {
                        let title = titles.remove(&code).map(|title| title.title);
                        self.titles.insert(code, title.clone());
                        title
                    }
                    Err(_) => None,
                }
            }
        };
        let mut shown = search::summary(&summary);
        shown.script_title = title;
        let event = Change::Run(Box::new(shown));
        self.pending.push_back(sse(&event));
    }

    /// What changed in the control plane, if the caller may read it.
    async fn control_plane(&mut self, change: ControlPlaneChange) {
        let caller = self.rights.caller();
        let tenant = caller.tenant.clone();
        let unscoped = caller.namespace.is_none() && caller.labels.is_empty();
        if let ControlPlaneChange::Script { tenant: of, digest } = &change
            && *of == tenant
        {
            self.titles.remove(digest);
        }
        let event = match change {
            ControlPlaneChange::Script { tenant: of, digest } if of == tenant && unscoped => {
                Some(Change::Script(ScriptChanged {
                    digest: digest.to_hex(),
                }))
            }
            ControlPlaneChange::Workflow {
                tenant: of,
                name,
                version,
            } if of == tenant => {
                let labels_seen = match self.app.catalog().get_workflow(&tenant, version).await {
                    Ok(Some(stored)) => caller
                        .labels
                        .iter()
                        .all(|(key, value)| stored.workflow.labels().get(key) == Some(value)),
                    _ => false,
                };
                labels_seen.then(|| {
                    Change::Workflow(WorkflowChanged {
                        name,
                        digest: version.to_hex(),
                    })
                })
            }
            ControlPlaneChange::Secret { tenant: of, secret } if of == tenant => {
                let seen = match &secret.namespace {
                    SecretNamespace::Namespace(namespace) => caller.reaches(namespace),
                    SecretNamespace::Organization if caller.labels.is_empty() => true,
                    SecretNamespace::Organization => {
                        match self
                            .app
                            .store
                            .meta(&tenant, &secret, Day::of(self.app.now()))
                            .await
                        {
                            Ok(Some(meta)) => caller.sees(&meta.labels),
                            // Deleted: what it carried is gone with it.
                            Ok(None) => false,
                            Err(_) => false,
                        }
                    }
                };
                seen.then(|| {
                    Change::Secret(SecretChanged {
                        name: secret.name.to_string(),
                        namespace: match &secret.namespace {
                            SecretNamespace::Namespace(namespace) => {
                                Some(crate::views::namespace(namespace))
                            }
                            SecretNamespace::Organization => None,
                        },
                    })
                })
            }
            ControlPlaneChange::Namespace { tenant: of, name } if of == tenant => {
                caller.reaches(&name).then(|| {
                    Change::Namespace(NamespaceChanged {
                        name: Some(crate::views::namespace(&name)),
                    })
                })
            }
            ControlPlaneChange::Tenant { tenant: of } if of == tenant => {
                Some(Change::Namespace(NamespaceChanged { name: None }))
            }
            ControlPlaneChange::Connection { id }
                if caller.require(Permission::MembersRead).is_ok() =>
            {
                Some(Change::Connection(ConnectionChanged { id }))
            }
            _ => None,
        };
        if let Some(event) = event {
            self.pending.push_back(sse(&event));
        }
    }
}

fn sse(event: &Change) -> Event {
    let data = serde_json::to_string(event).unwrap_or_else(|_| "{}".to_owned());
    Event::default().event(event.sse_event()).data(data)
}

fn resync() -> Event {
    sse(&Change::Resync(Resync {}))
}
