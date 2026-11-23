//! The engine's `NodeLink` over HTTP/2 (`archi/07-code-architecture.md`, "`Cluster` and
//! `NodeLink`").
//!
//! [`HttpLink`] keeps the addresses of the members it reached (`NodeInfo::address`), and reads
//! the members again when it is asked for a node it does not know, or when a node it knew could
//! not be reached: a node that restarted may listen elsewhere. One `reqwest` client holds one
//! HTTP/2 connection per node, without TLS (prior knowledge), and checks it with HTTP/2 pings, so
//! that a node that vanished breaks its requests and its subscriptions.

use std::collections::BTreeMap;
use std::fmt;
use std::future::{Future, ready};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::{Stream, StreamExt as _};
use rewind_engine::ports::cluster::Membership;
use rewind_engine::ports::link::{
    ActivationRequest, ActivationResult, Changed, NodeReply, NodeRequest, SubscriptionKey,
};
use rewind_engine::ports::{
    ClusterError, Departed, LinkError, Moves, NodeId, NodeInfo, NodeLink, PartitionMap,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use sse_stream::SseByteStream;

use super::dto::{self, LINK_VERSION, VERSION_HEADER};
use super::{ACTIVATE, CALL, ClusterSecret, Error, SUBSCRIBE};

/// How long a connection to a node may take to open.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// How often an HTTP/2 ping checks a connection, and how long its answer may take: a node that
/// vanished is seen gone within both.
const PING_INTERVAL: Duration = Duration::from_secs(10);
const PING_TIMEOUT: Duration = Duration::from_secs(5);

/// Where the link reads the members of the cluster, and their addresses.
pub trait Members: Send + Sync + 'static {
    /// The members, as [`Membership::nodes`] reads them.
    fn nodes(&self) -> impl Future<Output = Result<Vec<NodeInfo>, ClusterError>> + Send;
}

/// A node's membership, shared between the node, which holds it for its liveness, the server,
/// which reads the members and the changes, and its link, which reads the members through
/// [`SharedMembership::members`] without keeping it alive. The membership ends when the last
/// holder drops it.
#[derive(Debug)]
pub struct SharedMembership<M>(Arc<M>);

impl<M> Clone for SharedMembership<M> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<M: Membership + 'static> SharedMembership<M> {
    pub fn new(membership: M) -> Self {
        Self(Arc::new(membership))
    }

    /// The members as the link reads them: through the membership while the node holds it.
    pub fn members(&self) -> MembersOf<M> {
        MembersOf(Arc::downgrade(&self.0))
    }
}

impl<M: Membership + 'static> Membership for SharedMembership<M> {
    type Changes = M::Changes;

    fn node(&self) -> &NodeId {
        self.0.node()
    }

    fn watch(&self) -> impl Future<Output = Result<Departed, ClusterError>> + Send {
        self.0.watch()
    }

    fn assign(
        &self,
        moves: &Moves,
    ) -> impl Future<Output = Result<PartitionMap, ClusterError>> + Send {
        self.0.assign(moves)
    }

    fn partition_map(&self) -> impl Future<Output = Result<PartitionMap, ClusterError>> + Send {
        self.0.partition_map()
    }

    fn nodes(&self) -> impl Future<Output = Result<Vec<NodeInfo>, ClusterError>> + Send {
        self.0.nodes()
    }

    fn changes(&self) -> impl Future<Output = Result<Self::Changes, ClusterError>> + Send {
        self.0.changes()
    }

    /// Leaves the cluster. While the link reads the members through it, the membership is
    /// dropped instead once that read ends, which ends its liveness as well.
    async fn leave(self) -> Result<(), ClusterError> {
        match Arc::try_unwrap(self.0) {
            Ok(membership) => membership.leave().await,
            Err(_) => Ok(()),
        }
    }
}

/// The members, read through a node's membership while the node holds it.
#[derive(Debug)]
pub struct MembersOf<M>(Weak<M>);

impl<M> Clone for MembersOf<M> {
    fn clone(&self) -> Self {
        Self(Weak::clone(&self.0))
    }
}

impl<M: Membership + 'static> Members for MembersOf<M> {
    async fn nodes(&self) -> Result<Vec<NodeInfo>, ClusterError> {
        let membership = self
            .0
            .upgrade()
            .ok_or_else(|| ClusterError::Unavailable("the node has left the cluster".into()))?;
        membership.nodes().await
    }
}

/// `NodeLink` over HTTP/2: requests, activations and subscriptions sent to the other nodes of
/// the deployment, authenticated by its secret.
pub struct HttpLink<M> {
    members: M,
    secret: ClusterSecret,
    http: reqwest::Client,
    /// The addresses of the members, as last read.
    addresses: Mutex<BTreeMap<NodeId, String>>,
    version: u32,
}

impl<M> fmt::Debug for HttpLink<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpLink")
            .field("addresses", &*self.book())
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}

impl<M> HttpLink<M> {
    fn book(&self) -> MutexGuard<'_, BTreeMap<NodeId, String>> {
        self.addresses
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

impl<M: Members> HttpLink<M> {
    /// A link that finds the nodes among `members` and carries `secret` on every request.
    pub fn new(members: M, secret: ClusterSecret) -> Result<Self, Error> {
        let http = reqwest::Client::builder()
            .http2_prior_knowledge()
            // The private network: never through the proxy of the environment.
            .no_proxy()
            .connect_timeout(CONNECT_TIMEOUT)
            .http2_keep_alive_interval(PING_INTERVAL)
            .http2_keep_alive_timeout(PING_TIMEOUT)
            .http2_keep_alive_while_idle(true)
            .build()
            .map_err(|error| Error::Client(error.to_string()))?;
        Ok(Self {
            members,
            secret,
            http,
            addresses: Mutex::new(BTreeMap::new()),
            version: LINK_VERSION,
        })
    }

    /// The same link, speaking another version: what a node of another build sends.
    #[cfg(test)]
    pub(crate) fn speaking(mut self, version: u32) -> Self {
        self.version = version;
        self
    }

    /// The node's address: as last read, or read from the members.
    async fn address(&self, node: &NodeId) -> Result<String, LinkError> {
        if let Some(address) = self.book().get(node) {
            return Ok(address.clone());
        }
        let members = self.members.nodes().await.map_err(|error| {
            tracing::warn!(%node, %error, "the members of the cluster cannot be read");
            LinkError::Unreachable(node.clone())
        })?;
        let mut book = self.book();
        *book = members
            .into_iter()
            .map(|member| (member.id, member.address))
            .collect();
        book.get(node)
            .cloned()
            .ok_or_else(|| LinkError::Unreachable(node.clone()))
    }

    /// A node that could not be reached: its address is read again next time.
    fn forget(&self, node: &NodeId) {
        self.book().remove(node);
    }

    /// Posts `body` to `route` of `node`; a success, or what the node answered instead.
    async fn post(
        &self,
        node: &NodeId,
        route: &str,
        body: &impl Serialize,
    ) -> Result<reqwest::Response, LinkError> {
        let address = self.address(node).await?;
        let response = self
            .http
            .post(format!("http://{address}{route}"))
            .bearer_auth(self.secret.expose())
            .header(VERSION_HEADER, self.version)
            .json(body)
            .send()
            .await
            .map_err(|error| self.unreachable(node, &error))?;
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let message = response.text().await.unwrap_or_default();
        Err(LinkError::Failed(format!(
            "node {node} refused the request ({status}): {message}"
        )))
    }

    fn unreachable(&self, node: &NodeId, error: &reqwest::Error) -> LinkError {
        tracing::debug!(%node, %error, "a node of the cluster cannot be reached");
        self.forget(node);
        LinkError::Unreachable(node.clone())
    }

    /// Posts `body` and reads the JSON it is answered with.
    async fn exchange<T: DeserializeOwned>(
        &self,
        node: &NodeId,
        route: &str,
        body: &impl Serialize,
    ) -> Result<T, LinkError> {
        let response = self.post(node, route, body).await?;
        let bytes = response
            .bytes()
            .await
            .map_err(|error| self.unreachable(node, &error))?;
        serde_json::from_slice(&bytes).map_err(|error| {
            LinkError::Failed(format!(
                "node {node} answered what this node cannot read: {error}"
            ))
        })
    }
}

fn unreadable(node: &NodeId, invalid: &dto::Invalid) -> LinkError {
    LinkError::Failed(format!("node {node} answered {invalid}"))
}

impl<M: Members> NodeLink for HttpLink<M> {
    type Changes = RemoteChanges;

    async fn call(&self, node: &NodeId, request: NodeRequest) -> Result<NodeReply, LinkError> {
        let reply: dto::NodeReply = self
            .exchange(node, CALL, &dto::NodeRequest::from(&request))
            .await?;
        reply
            .try_into()
            .map_err(|invalid| unreadable(node, &invalid))
    }

    async fn subscribe(
        &self,
        node: &NodeId,
        key: SubscriptionKey,
        from: Option<u64>,
    ) -> Result<RemoteChanges, LinkError> {
        let subscribe = dto::Subscribe {
            key: (&key).into(),
            from,
        };
        let response = self.post(node, SUBSCRIBE, &subscribe).await?;
        let node = node.clone();
        let changes = SseByteStream::new(response.bytes_stream())
            .map(move |event| match event {
                // A comment: the keep-alive.
                Ok(event) => match event.data {
                    None => Step::Skip,
                    Some(data) => match decode(&data) {
                        Ok(changed) => Step::Changed(Box::new(changed)),
                        Err(error) => {
                            tracing::warn!(%node, %error, "a change the subscription cannot read");
                            Step::End
                        }
                    },
                },
                Err(error) => {
                    tracing::debug!(%node, %error, "a subscription was cut");
                    Step::End
                }
            })
            .take_while(|step| ready(!matches!(step, Step::End)))
            .filter_map(|step| {
                ready(match step {
                    Step::Changed(changed) => Some(*changed),
                    Step::Skip | Step::End => None,
                })
            });
        Ok(RemoteChanges(Box::pin(changes)))
    }

    async fn activate(
        &self,
        node: &NodeId,
        request: ActivationRequest,
    ) -> Result<ActivationResult, LinkError> {
        let result: dto::ActivationResult = self
            .exchange(node, ACTIVATE, &dto::ActivationRequest::from(&request))
            .await?;
        result
            .try_into()
            .map_err(|invalid| unreadable(node, &invalid))
    }
}

enum Step {
    Changed(Box<Changed>),
    Skip,
    End,
}

fn decode(data: &str) -> Result<Changed, String> {
    let changed: dto::Changed = serde_json::from_str(data).map_err(|error| error.to_string())?;
    changed
        .try_into()
        .map_err(|invalid: dto::Invalid| invalid.to_string())
}

/// The changes a subscription to another node streams, until the node ends it or the
/// connection is cut.
pub struct RemoteChanges(Pin<Box<dyn Stream<Item = Changed> + Send>>);

impl fmt::Debug for RemoteChanges {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RemoteChanges")
    }
}

impl Stream for RemoteChanges {
    type Item = Changed;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Changed>> {
        self.0.as_mut().poll_next(cx)
    }
}
