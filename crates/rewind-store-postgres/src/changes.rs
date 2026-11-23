//! The changes of the control plane as `NOTIFY` payloads (`archi/06-storage-and-scale.md`, "The
//! control plane"; `archi/07-code-architecture.md`, "`Cluster` and `NodeLink`").
//!
//! Every write of the catalog, of access and of secrets sends what it changed with
//! [`notify`], in its own transaction: Postgres delivers a notification only once its
//! transaction commits, and in commit order. A move of partitions goes to the channel
//! [`PARTITION_MAP`], every other change to [`CONTROL_PLANE`]; a listener of both receives them
//! in one order, the order of their commits.
//!
//! A payload names what changed, never a value: no secret, document or setting leaves the
//! database through it. It is a JSON object whose `k` is the kind of the change:
//!
//! ```text
//! {"k":"namespace","t":"acme","n":"alice"}              {"k":"tenant","t":"acme"}
//! {"k":"key","t":"acme","h":"<sha-256 hex>"}            {"k":"token","t":"acme","h":"<hex>"}
//! {"k":"token_key","t":"acme","kid":"rk_…"}             {"k":"grant","t":"acme","p":"user:…"}
//! {"k":"role","t":"acme","n":"triager"}                 {"k":"connection","id":"cn_…"}
//! {"k":"secret","t":"acme","ns":"*","n":"github"}       {"k":"subscription","t":"acme","id":"s1"}
//! {"k":"workflow","t":"acme","n":"triage","v":"<hex>"}  {"k":"script","t":"acme","d":"<hex>"}
//! {"k":"toolset","t":"acme","d":"<hex>"}                {"k":"test","t":"acme","d":"<hex>"}
//! {"k":"partition_map","n":["node-a","node-b"],"p":[[0,0,3],[1,1,2],[2,null,0]]}
//! ```
//!
//! A partition map lists every partition as `[partition, node, epoch]`, the node an index into
//! `n` (`null`: no owner): the whole map as it committed, about 10 bytes a partition.
//!
//! Postgres refuses a payload of 8000 bytes or more. A payload longer than [`MAX_FRAME`] bytes
//! is sent as consecutive frames, `#<id>:<index>:<count>:<part>`, where `id` is 16 random hex
//! digits that tell this payload's frames apart from any other's (Postgres drops a notification
//! identical to one sent earlier in the same transaction). The frames of one transaction reach
//! every listener together and in order, so a [`Decoder`] joins them back.

use rewind_core::{Digest, PartitionId};
use rewind_engine::ports::{
    ControlPlaneChange, NamespaceName, NodeId, PartitionEpoch, PartitionMap, PartitionOwner,
    Principal, SecretId, SecretNamespace, TenantId,
};
use serde::{Deserialize, Serialize};
use sqlx::PgConnection;

use crate::Error;

/// The channel of every change but the moves of partitions.
pub(crate) const CONTROL_PLANE: &str = "control_plane";

/// The channel of the moves of partitions: the whole map, as it committed.
pub(crate) const PARTITION_MAP: &str = "partition_map";

/// The longest payload sent as one notification, in bytes; Postgres refuses 8000 and more.
pub(crate) const MAX_FRAME: usize = 7_900;

/// The bytes a frame's header takes at most: `#`, 16 hex digits, and two numbers below 2^32.
const FRAME_HEADER: usize = 1 + 16 + 1 + 10 + 1 + 10 + 1;

/// A payload, as JSON.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "k", rename_all = "snake_case", deny_unknown_fields)]
enum Payload {
    PartitionMap {
        n: Vec<String>,
        p: Vec<(u16, Option<usize>, u64)>,
    },
    Namespace {
        t: String,
        n: String,
    },
    Tenant {
        t: String,
    },
    Key {
        t: String,
        h: String,
    },
    Token {
        t: String,
        h: String,
    },
    TokenKey {
        t: String,
        kid: String,
    },
    Grant {
        t: String,
        p: String,
    },
    Role {
        t: String,
        n: String,
    },
    Connection {
        id: String,
    },
    Secret {
        t: String,
        ns: String,
        n: String,
    },
    Workflow {
        t: String,
        n: String,
        v: String,
    },
    Subscription {
        t: String,
        id: String,
    },
    Script {
        t: String,
        d: String,
    },
    Toolset {
        t: String,
        d: String,
    },
    Test {
        t: String,
        d: String,
    },
}

fn text(tenant: &TenantId) -> String {
    tenant.as_str().to_owned()
}

impl From<&ControlPlaneChange> for Payload {
    fn from(change: &ControlPlaneChange) -> Self {
        match change {
            ControlPlaneChange::PartitionMap(map) => {
                let mut nodes: Vec<String> = Vec::new();
                let p = map
                    .partitions
                    .iter()
                    .map(|(partition, owner)| {
                        let node = owner.node.as_ref().map(|node| {
                            match nodes.iter().position(|known| known == node.as_str()) {
                                Some(index) => index,
                                None => {
                                    nodes.push(node.as_str().to_owned());
                                    nodes.len() - 1
                                }
                            }
                        });
                        (partition.get(), node, owner.epoch.0)
                    })
                    .collect();
                Payload::PartitionMap { n: nodes, p }
            }
            ControlPlaneChange::Namespace { tenant, name } => Payload::Namespace {
                t: text(tenant),
                n: name.as_str().to_owned(),
            },
            ControlPlaneChange::Tenant { tenant } => Payload::Tenant { t: text(tenant) },
            ControlPlaneChange::Key { tenant, hash } => Payload::Key {
                t: text(tenant),
                h: hash.to_hex(),
            },
            ControlPlaneChange::Token { tenant, hash } => Payload::Token {
                t: text(tenant),
                h: hash.to_hex(),
            },
            ControlPlaneChange::TokenKey { tenant, kid } => Payload::TokenKey {
                t: text(tenant),
                kid: kid.clone(),
            },
            ControlPlaneChange::Grant { tenant, principal } => Payload::Grant {
                t: text(tenant),
                p: principal.to_string(),
            },
            ControlPlaneChange::Role { tenant, name } => Payload::Role {
                t: text(tenant),
                n: name.clone(),
            },
            ControlPlaneChange::Connection { id } => Payload::Connection { id: id.clone() },
            ControlPlaneChange::Secret { tenant, secret } => Payload::Secret {
                t: text(tenant),
                ns: secret.namespace.as_str().to_owned(),
                n: secret.name.as_str().to_owned(),
            },
            ControlPlaneChange::Workflow {
                tenant,
                name,
                version,
            } => Payload::Workflow {
                t: text(tenant),
                n: name.clone(),
                v: version.to_hex(),
            },
            ControlPlaneChange::Subscription { tenant, id } => Payload::Subscription {
                t: text(tenant),
                id: id.clone(),
            },
            ControlPlaneChange::Script { tenant, digest } => Payload::Script {
                t: text(tenant),
                d: digest.to_hex(),
            },
            ControlPlaneChange::Toolset { tenant, digest } => Payload::Toolset {
                t: text(tenant),
                d: digest.to_hex(),
            },
            ControlPlaneChange::Test { tenant, digest } => Payload::Test {
                t: text(tenant),
                d: digest.to_hex(),
            },
        }
    }
}

/// A payload this build cannot read: from a newer build, or damaged.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unreadable control-plane notification: {0}")]
pub(crate) struct UnreadableChange(String);

fn bad(what: impl std::fmt::Display) -> UnreadableChange {
    UnreadableChange(what.to_string())
}

fn tenant_of(t: String) -> Result<TenantId, UnreadableChange> {
    TenantId::new(t).map_err(bad)
}

fn digest_of(hex: &str) -> Result<Digest, UnreadableChange> {
    Digest::from_hex(hex).map_err(|_| bad(format!("invalid digest {hex:?}")))
}

fn namespace_of(name: String) -> Result<NamespaceName, UnreadableChange> {
    NamespaceName::new(name.clone()).map_err(|_| bad(format!("invalid namespace {name:?}")))
}

impl TryFrom<Payload> for ControlPlaneChange {
    type Error = UnreadableChange;

    fn try_from(payload: Payload) -> Result<Self, UnreadableChange> {
        Ok(match payload {
            Payload::PartitionMap { n, p } => {
                let nodes = n
                    .into_iter()
                    .map(|node| NodeId::new(node).map_err(bad))
                    .collect::<Result<Vec<_>, _>>()?;
                let mut map = PartitionMap::default();
                for (partition, node, epoch) in p {
                    let node = node
                        .map(|index| {
                            nodes
                                .get(index)
                                .cloned()
                                .ok_or_else(|| bad(format!("no node {index}")))
                        })
                        .transpose()?;
                    map.partitions.insert(
                        PartitionId::new(partition),
                        PartitionOwner {
                            node,
                            epoch: PartitionEpoch(epoch),
                        },
                    );
                }
                ControlPlaneChange::PartitionMap(map)
            }
            Payload::Namespace { t, n } => ControlPlaneChange::Namespace {
                tenant: tenant_of(t)?,
                name: namespace_of(n)?,
            },
            Payload::Tenant { t } => ControlPlaneChange::Tenant {
                tenant: tenant_of(t)?,
            },
            Payload::Key { t, h } => ControlPlaneChange::Key {
                tenant: tenant_of(t)?,
                hash: digest_of(&h)?,
            },
            Payload::Token { t, h } => ControlPlaneChange::Token {
                tenant: tenant_of(t)?,
                hash: digest_of(&h)?,
            },
            Payload::TokenKey { t, kid } => ControlPlaneChange::TokenKey {
                tenant: tenant_of(t)?,
                kid,
            },
            Payload::Grant { t, p } => ControlPlaneChange::Grant {
                tenant: tenant_of(t)?,
                principal: Principal::parse(&p).map_err(bad)?,
            },
            Payload::Role { t, n } => ControlPlaneChange::Role {
                tenant: tenant_of(t)?,
                name: n,
            },
            Payload::Connection { id } => ControlPlaneChange::Connection { id },
            Payload::Secret { t, ns, n } => ControlPlaneChange::Secret {
                tenant: tenant_of(t)?,
                secret: SecretId {
                    namespace: if ns == SecretNamespace::ORGANIZATION {
                        SecretNamespace::Organization
                    } else {
                        SecretNamespace::Namespace(namespace_of(ns)?)
                    },
                    name: rewind_core::SecretName::new(n).map_err(bad)?,
                },
            },
            Payload::Workflow { t, n, v } => ControlPlaneChange::Workflow {
                tenant: tenant_of(t)?,
                name: n,
                version: digest_of(&v)?,
            },
            Payload::Subscription { t, id } => ControlPlaneChange::Subscription {
                tenant: tenant_of(t)?,
                id,
            },
            Payload::Script { t, d } => ControlPlaneChange::Script {
                tenant: tenant_of(t)?,
                digest: digest_of(&d)?,
            },
            Payload::Toolset { t, d } => ControlPlaneChange::Toolset {
                tenant: tenant_of(t)?,
                digest: digest_of(&d)?,
            },
            Payload::Test { t, d } => ControlPlaneChange::Test {
                tenant: tenant_of(t)?,
                digest: digest_of(&d)?,
            },
        })
    }
}

/// The channel a change is sent on.
pub(crate) fn channel(change: &ControlPlaneChange) -> &'static str {
    match change {
        ControlPlaneChange::PartitionMap(_) => PARTITION_MAP,
        _ => CONTROL_PLANE,
    }
}

/// The notifications that carry a change: one, or the frames of a payload above
/// [`MAX_FRAME`] bytes.
pub(crate) fn encode(change: &ControlPlaneChange) -> Result<Vec<String>, Error> {
    let json = serde_json::to_string(&Payload::from(change))
        .map_err(|err| Error::Corrupt(format!("a change that cannot be encoded: {err}")))?;
    if json.len() <= MAX_FRAME {
        return Ok(vec![json]);
    }
    let mut id = [0u8; 8];
    getrandom::fill(&mut id).map_err(|err| Error::Random(err.to_string()))?;
    let id: String = id.iter().map(|byte| format!("{byte:02x}")).collect();
    let mut parts = Vec::new();
    let mut rest = json.as_str();
    while !rest.is_empty() {
        let mut end = rest.len().min(MAX_FRAME - FRAME_HEADER);
        while !rest.is_char_boundary(end) {
            end -= 1;
        }
        let (part, tail) = rest.split_at(end);
        parts.push(part);
        rest = tail;
    }
    let count = parts.len();
    Ok(parts
        .into_iter()
        .enumerate()
        .map(|(index, part)| format!("#{id}:{index}:{count}:{part}"))
        .collect())
}

/// Sends the changes, in this order, with the transaction `conn` is in: listeners receive them
/// once it commits, and never if it rolls back.
pub(crate) async fn notify(
    conn: &mut PgConnection,
    changes: &[ControlPlaneChange],
) -> Result<(), Error> {
    if changes.is_empty() {
        return Ok(());
    }
    let mut channels = Vec::new();
    let mut payloads = Vec::new();
    for change in changes {
        for frame in encode(change)? {
            channels.push(channel(change));
            payloads.push(frame);
        }
    }
    sqlx::query(
        "SELECT pg_notify(c, p) FROM unnest($1::text[], $2::text[]) WITH ORDINALITY AS f(c, p, n) \
         ORDER BY n",
    )
    .bind(channels)
    .bind(payloads)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Joins frames back into changes, in the order they arrive.
#[derive(Debug, Default)]
pub(crate) struct Decoder {
    /// The frames received of a payload sent in parts: its id, the parts expected, the text so
    /// far.
    partial: Option<(String, usize, usize, String)>,
}

impl Decoder {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// The change a notification completes; `None` while a payload sent in parts is missing
    /// some.
    pub(crate) fn feed(
        &mut self,
        payload: &str,
    ) -> Result<Option<ControlPlaneChange>, UnreadableChange> {
        let Some(frame) = payload.strip_prefix('#') else {
            if self.partial.is_some() {
                return Err(bad("a payload's frames were interrupted"));
            }
            return decode(payload).map(Some);
        };
        let mut fields = frame.splitn(4, ':');
        let (Some(id), Some(index), Some(count), Some(part)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            return Err(bad("a frame without its header"));
        };
        let index: usize = index.parse().map_err(bad)?;
        let count: usize = count.parse().map_err(bad)?;
        let (mut text, expected) = match self.partial.take() {
            None if index == 0 => (String::new(), 0),
            Some((known, next, of, text)) if known == id && next == index && of == count => {
                (text, next)
            }
            _ => return Err(bad("a payload's frames arrived out of order")),
        };
        text.push_str(part);
        if expected + 1 < count {
            self.partial = Some((id.to_owned(), expected + 1, count, text));
            return Ok(None);
        }
        decode(&text).map(Some)
    }
}

fn decode(json: &str) -> Result<ControlPlaneChange, UnreadableChange> {
    let payload: Payload = serde_json::from_str(json).map_err(bad)?;
    ControlPlaneChange::try_from(payload)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn tenant(id: &str) -> TenantId {
        TenantId::new(id).unwrap()
    }

    fn round_trip(change: &ControlPlaneChange) -> ControlPlaneChange {
        let mut decoder = Decoder::new();
        let frames = encode(change).unwrap();
        let (last, first) = frames.split_last().unwrap();
        for frame in first {
            assert!(frame.len() <= MAX_FRAME, "{}", frame.len());
            assert_eq!(decoder.feed(frame).unwrap(), None);
        }
        assert!(last.len() <= MAX_FRAME);
        decoder
            .feed(last)
            .unwrap()
            .expect("the last frame completes it")
    }

    fn every_kind() -> Vec<ControlPlaneChange> {
        let acme = tenant("acme");
        let digest = Digest::of(b"x");
        let alice = NamespaceName::new("alice").unwrap();
        let mut map = PartitionMap::unowned(3);
        map.partitions.insert(
            PartitionId::new(1),
            PartitionOwner {
                node: Some(NodeId::new("node-a").unwrap()),
                epoch: PartitionEpoch(7),
            },
        );
        vec![
            ControlPlaneChange::PartitionMap(map),
            ControlPlaneChange::Namespace {
                tenant: acme.clone(),
                name: alice.clone(),
            },
            ControlPlaneChange::Tenant {
                tenant: acme.clone(),
            },
            ControlPlaneChange::Key {
                tenant: acme.clone(),
                hash: digest,
            },
            ControlPlaneChange::Token {
                tenant: acme.clone(),
                hash: digest,
            },
            ControlPlaneChange::TokenKey {
                tenant: acme.clone(),
                kid: "rk_1".into(),
            },
            ControlPlaneChange::Grant {
                tenant: acme.clone(),
                principal: Principal::Key(digest),
            },
            ControlPlaneChange::Role {
                tenant: acme.clone(),
                name: "triager".into(),
            },
            ControlPlaneChange::Connection { id: "cn_1".into() },
            ControlPlaneChange::Secret {
                tenant: acme.clone(),
                secret: SecretId::organization(rewind_core::SecretName::new("github").unwrap()),
            },
            ControlPlaneChange::Secret {
                tenant: acme.clone(),
                secret: SecretId::of_namespace(
                    alice,
                    rewind_core::SecretName::new("github").unwrap(),
                ),
            },
            ControlPlaneChange::Workflow {
                tenant: acme.clone(),
                name: "triage".into(),
                version: digest,
            },
            ControlPlaneChange::Subscription {
                tenant: acme.clone(),
                id: "s1".into(),
            },
            ControlPlaneChange::Script {
                tenant: acme.clone(),
                digest,
            },
            ControlPlaneChange::Toolset {
                tenant: acme.clone(),
                digest,
            },
            ControlPlaneChange::Test {
                tenant: acme,
                digest,
            },
        ]
    }

    #[test]
    fn every_kind_of_change_round_trips() {
        for change in every_kind() {
            assert_eq!(encode(&change).unwrap().len(), 1, "{change:?}");
            assert_eq!(round_trip(&change), change);
        }
    }

    #[test]
    fn a_payload_names_what_changed() {
        let frames = encode(&ControlPlaneChange::Namespace {
            tenant: tenant("acme"),
            name: NamespaceName::new("alice").unwrap(),
        })
        .unwrap();
        assert_eq!(frames, [r#"{"k":"namespace","t":"acme","n":"alice"}"#]);
    }

    #[test]
    fn a_map_of_a_thousand_partitions_goes_in_frames() {
        let nodes: Vec<NodeId> = (0..40)
            .map(|n| NodeId::new(format!("{}-{n}", "x".repeat(60))).unwrap())
            .collect();
        let map = PartitionMap {
            partitions: (0..1_000u16)
                .map(|id| {
                    (
                        PartitionId::new(id),
                        PartitionOwner {
                            node: Some(nodes[usize::from(id) % nodes.len()].clone()),
                            epoch: PartitionEpoch(u64::MAX - u64::from(id)),
                        },
                    )
                })
                .collect::<BTreeMap<_, _>>(),
        };
        let change = ControlPlaneChange::PartitionMap(map);
        let frames = encode(&change).unwrap();
        assert!(frames.len() > 2, "{}", frames.len());
        assert_eq!(round_trip(&change), change);
    }

    #[test]
    fn a_map_of_64_partitions_fits_one_notification() {
        let node = NodeId::new("x".repeat(64)).unwrap();
        let map = PartitionMap {
            partitions: (0..64u16)
                .map(|id| {
                    (
                        PartitionId::new(id),
                        PartitionOwner {
                            node: Some(node.clone()),
                            epoch: PartitionEpoch(1_000_000),
                        },
                    )
                })
                .collect(),
        };
        assert_eq!(
            encode(&ControlPlaneChange::PartitionMap(map))
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn interrupted_or_foreign_payloads_are_refused() {
        let role = ControlPlaneChange::Role {
            tenant: tenant("acme"),
            name: "é".repeat(5_000),
        };
        let frames = encode(&role).unwrap();
        assert_eq!(frames.len(), 2);
        let mut decoder = Decoder::new();
        assert_eq!(decoder.feed(&frames[0]).unwrap(), None);
        assert!(decoder.feed(r#"{"k":"tenant","t":"acme"}"#).is_err());
        assert!(Decoder::new().feed(&frames[1]).is_err(), "a frame alone");
        assert!(Decoder::new().feed(r#"{"k":"node","id":"a"}"#).is_err());
        assert!(Decoder::new().feed("not json").is_err());
    }
}
