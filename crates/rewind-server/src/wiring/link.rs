//! The node link (`archi/07-code-architecture.md`, "`Cluster` and `NodeLink`"): HTTP/2 between
//! the nodes of a deployment on Postgres ([`crate::link`]), and inside the process for the
//! embedded server and a single node on Turso, where no node is remote.

use futures_util::StreamExt as _;
use futures_util::stream::BoxStream;
use rewind_engine::adapters::LocalLink;
use rewind_engine::ports::link::{
    ActivationRequest, ActivationResult, Changed, NodeReply, NodeRequest, SubscriptionKey,
};
use rewind_engine::ports::{LinkError, NodeId, NodeLink};

use crate::app::ServerPlane;
use crate::link::{HttpLink, MembersOf};
use crate::wiring::control::ServerMembership;

/// The node link of this server.
#[derive(Debug)]
pub(crate) enum ServerLink {
    /// The nodes of this process.
    Local(LocalLink<ServerPlane>),
    /// The other nodes of the deployment, over HTTP/2.
    Http(HttpLink<MembersOf<ServerMembership>>),
}

impl NodeLink for ServerLink {
    type Changes = BoxStream<'static, Changed>;

    async fn call(&self, node: &NodeId, request: NodeRequest) -> Result<NodeReply, LinkError> {
        match self {
            ServerLink::Local(link) => link.call(node, request).await,
            ServerLink::Http(link) => link.call(node, request).await,
        }
    }

    async fn subscribe(
        &self,
        node: &NodeId,
        key: SubscriptionKey,
        from: Option<u64>,
    ) -> Result<Self::Changes, LinkError> {
        match self {
            ServerLink::Local(link) => Ok(link.subscribe(node, key, from).await?.boxed()),
            ServerLink::Http(link) => Ok(link.subscribe(node, key, from).await?.boxed()),
        }
    }

    async fn activate(
        &self,
        node: &NodeId,
        request: ActivationRequest,
    ) -> Result<ActivationResult, LinkError> {
        match self {
            ServerLink::Local(link) => link.activate(node, request).await,
            ServerLink::Http(link) => link.activate(node, request).await,
        }
    }
}
