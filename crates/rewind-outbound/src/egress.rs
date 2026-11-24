//! The egress checks (`archi/04-tools.md`, "The `http` action and egress" and "Where MCP servers
//! and APIs may be"; `archi/06-storage-and-scale.md`, "Security"):
//!
//! 1. The name is resolved once ([`CheckedResolver`], the only resolver of the client).
//! 2. Private, loopback, link-local and cloud-metadata addresses are refused ([`forbidden`]); a
//!    name with any such address is refused.
//! 3. The connection goes to the addresses that were checked: the resolver hands exactly those
//!    to the connector, so a DNS answer that changes in between is never used.
//!
//! A URL whose host is an IP address is checked against the same rules before the request.
//! Redirects go through them again ([`crate::client`]).
//!
//! [`Egress`] holds them, for every request a deployment sends where a tenant says: the `http`
//! action's (whose allow-list `rewind-mcp` checks first), the MCP servers' and the APIs', the
//! OpenAPI documents' and the lifecycle webhooks'. For MCP servers and APIs, an operator may let
//! some internal hosts and networks through ([`InternalAllowList`], `--allow-internal-addresses`).

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use ipnet::IpNet;
use url::{Host, Url};

/// A refused request: the reason, written for the model. It travels as the source of the
/// client's error; [`Egress::refusal_of`] finds it there.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct Refused(pub String);

/// What a [`NameResolver`] returns.
pub type Resolving = Pin<Box<dyn Future<Output = io::Result<Vec<IpAddr>>> + Send>>;

/// Resolves host names for the checked clients. A deployment uses the system's resolver; a test
/// may plug its own. Its answer is checked before any connection.
pub trait NameResolver: Send + Sync + fmt::Debug + 'static {
    fn resolve(&self, host: &str) -> Resolving;
}

/// The system's resolver (`getaddrinfo`).
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemResolver;

impl NameResolver for SystemResolver {
    fn resolve(&self, host: &str) -> Resolving {
        let host = host.to_owned();
        Box::pin(async move {
            let addresses = tokio::net::lookup_host((host.as_str(), 0)).await?;
            Ok(addresses.map(|address| address.ip()).collect())
        })
    }
}

/// Says whether an address is refused, and why: [`forbidden`] in production; a test may treat
/// a local address as public.
pub type AddressPolicy = fn(IpAddr) -> Option<&'static str>;

/// What may be reached: the addresses `policy` does not refuse, and those `internal` lets
/// through. `refusal` ends the reason of a refusal: who does not go there.
#[derive(Debug, Clone)]
pub(crate) struct Rules {
    pub(crate) policy: AddressPolicy,
    pub(crate) internal: Arc<InternalAllowList>,
    pub(crate) refusal: &'static str,
}

/// An address that may be reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// The policy does not refuse it.
    Public,
    /// The policy refuses it, and the operator's allow-list lets it through.
    Internal,
}

impl Rules {
    /// Whether `address`, an address of the name `host` or given as such, may be reached; the
    /// reason when it may not.
    fn judge(&self, host: Option<&str>, address: IpAddr) -> Result<Reach, String> {
        let Some(reason) = (self.policy)(address) else {
            return Ok(Reach::Public);
        };
        if host.is_some_and(|host| self.internal.allows_name(host))
            || self.internal.allows_address(address)
        {
            return Ok(Reach::Internal);
        }
        Err(match host {
            Some(host) => format!(
                "{host} resolves to {address}, a {reason} address: {}",
                self.refusal
            ),
            None => format!("{address} is a {reason} address: {}", self.refusal),
        })
    }

    /// Checks the host of `url` when it is an address. A name is checked when it is resolved.
    fn judge_url(&self, url: &Url) -> Result<Reach, String> {
        let address = match url.host() {
            Some(Host::Ipv4(address)) => IpAddr::V4(address),
            Some(Host::Ipv6(address)) => IpAddr::V6(address),
            Some(Host::Domain(_)) => return Ok(Reach::Public),
            None => return Err(format!("{url} has no host")),
        };
        self.judge(None, address)
    }
}

/// The client's resolver: resolves once, refuses the name if any of its addresses is refused,
/// and hands the connector exactly the addresses it checked.
#[derive(Debug)]
struct CheckedResolver {
    resolver: Arc<dyn NameResolver>,
    rules: Rules,
    /// Set once the resolver hands out an address only the operator's allow-list lets through.
    reached_internal: Option<Arc<AtomicBool>>,
}

impl reqwest::dns::Resolve for CheckedResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_owned();
        let resolver = self.resolver.clone();
        let rules = self.rules.clone();
        let reached_internal = self.reached_internal.clone();
        Box::pin(async move {
            let (addresses, reach) = resolve(resolver.as_ref(), &host, &rules)
                .await
                .map_err(Unresolved::into_boxed)?;
            if reach == Reach::Internal
                && let Some(reached) = &reached_internal
            {
                reached.store(true, Ordering::SeqCst);
            }
            let addresses: reqwest::dns::Addrs = Box::new(
                addresses
                    .into_iter()
                    .map(|address| SocketAddr::new(address, 0)),
            );
            Ok(addresses)
        })
    }
}

/// Why a name gives no address to connect to.
#[derive(Debug)]
enum Unresolved {
    /// The resolver failed, or found nothing.
    Failed(io::Error),
    /// An address of the name is refused.
    Refused(Refused),
}

impl Unresolved {
    fn into_boxed(self) -> Box<dyn Error + Send + Sync> {
        match self {
            Unresolved::Failed(error) => Box::new(error),
            Unresolved::Refused(refused) => Box::new(refused),
        }
    }
}

/// Resolves `host` once and checks every address it has: [`Reach::Internal`] when any of them
/// is reached only through the operator's allow-list.
async fn resolve(
    resolver: &dyn NameResolver,
    host: &str,
    rules: &Rules,
) -> Result<(Vec<IpAddr>, Reach), Unresolved> {
    let addresses = resolver.resolve(host).await.map_err(|error| {
        Unresolved::Failed(io::Error::new(
            error.kind(),
            format!("{host} could not be resolved: {error}"),
        ))
    })?;
    if addresses.is_empty() {
        return Err(Unresolved::Failed(io::Error::other(format!(
            "{host} has no address"
        ))));
    }
    let mut reach = Reach::Public;
    for address in &addresses {
        if rules
            .judge(Some(host), *address)
            .map_err(|reason| Unresolved::Refused(Refused(reason)))?
            == Reach::Internal
        {
            reach = Reach::Internal;
        }
    }
    Ok((addresses, reach))
}

/// The egress checks: a name is resolved once, a name with any private, loopback, link-local or
/// cloud-metadata address is refused, and the connection goes to the addresses that were
/// checked. A URL whose host is an address is checked before the request. The internal hosts
/// and networks of an [`InternalAllowList`] are let through, when one is given.
#[derive(Debug, Clone)]
pub struct Egress {
    resolver: Arc<dyn NameResolver>,
    rules: Rules,
}

impl Egress {
    /// The checks, resolving names with `resolver`. `refusal` ends the reason of a refusal,
    /// such as "webhooks are not sent there".
    pub fn new(resolver: Arc<dyn NameResolver>, refusal: &'static str) -> Self {
        Self::with_policy(resolver, forbidden, refusal)
    }

    /// The checks with another policy of addresses: for tests, which serve on 127.0.0.1 and
    /// treat it as a public address.
    pub fn with_policy(
        resolver: Arc<dyn NameResolver>,
        policy: AddressPolicy,
        refusal: &'static str,
    ) -> Self {
        Self {
            resolver,
            rules: Rules {
                policy,
                internal: Arc::new(InternalAllowList::default()),
                refusal,
            },
        }
    }

    /// The same checks, which let through the internal hosts and networks of `internal`.
    pub fn allowing(mut self, internal: InternalAllowList) -> Self {
        self.rules.internal = Arc::new(internal);
        self
    }

    /// `builder` with the checked resolver as its only resolver, and without proxies: a proxy
    /// would resolve the names itself.
    pub fn apply(&self, builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
        self.apply_noting(builder, None)
    }

    /// [`Egress::apply`], noting in `reached_internal` when a name resolves to an address only
    /// the allow-list lets through.
    pub(crate) fn apply_noting(
        &self,
        builder: reqwest::ClientBuilder,
        reached_internal: Option<Arc<AtomicBool>>,
    ) -> reqwest::ClientBuilder {
        builder.no_proxy().dns_resolver(Arc::new(CheckedResolver {
            resolver: self.resolver.clone(),
            rules: self.rules.clone(),
            reached_internal,
        }))
    }

    /// Checks the host of `url` when it is an address: the reason when it is refused. A name is
    /// checked when it is resolved.
    pub fn check_url(&self, url: &Url) -> Result<(), String> {
        self.rules.judge_url(url).map(drop)
    }

    /// [`Egress::check_url`], saying whether the address is reached only through the allow-list.
    pub fn judge_url(&self, url: &Url) -> Result<Reach, String> {
        self.rules.judge_url(url)
    }

    /// Checks `url` as a request to it would be: its host when it is an address, otherwise
    /// every address its name resolves to now. The reason when it is refused or cannot be
    /// resolved.
    pub async fn check(&self, url: &Url) -> Result<(), String> {
        self.check_url(url)?;
        match url.host() {
            Some(Host::Domain(name)) => resolve(self.resolver.as_ref(), name, &self.rules)
                .await
                .map(drop)
                .map_err(|error| match error {
                    Unresolved::Failed(error) => error.to_string(),
                    Unresolved::Refused(Refused(reason)) => reason,
                }),
            _ => Ok(()),
        }
    }

    /// The reason, when a request failed because these checks refused its address, or a
    /// redirect the client does not follow ([`Refused`] among the error's sources).
    pub fn refusal_of(error: &(dyn Error + 'static)) -> Option<String> {
        let mut source = Some(error);
        while let Some(current) = source {
            if let Some(Refused(reason)) = current.downcast_ref::<Refused>() {
                return Some(reason.clone());
            }
            source = current.source();
        }
        None
    }
}

/// The internal hosts and networks an operator lets a deployment's MCP servers and APIs be
/// reached at, despite the egress checks (`--allow-internal-addresses`): names
/// (`mcp.internal.example`), the subdomains of a name (`*.svc.cluster.local`, not the name
/// itself), addresses (`10.0.0.7`, `fd00::7`) and networks in CIDR notation (`10.20.0.0/16`,
/// `fd00::/8`). A name lets through every address it resolves to; an address or a network lets
/// through any name that resolves into it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InternalAllowList {
    names: Vec<String>,
    subdomains: Vec<String>,
    networks: Vec<IpNet>,
}

impl InternalAllowList {
    /// Reads the events of the list; the first one that is neither a name, a `*.` wildcard,
    /// an address nor a network is refused.
    pub fn parse<S: AsRef<str>>(events: &[S]) -> Result<Self, String> {
        let mut list = Self::default();
        for entry in events {
            let entry = entry.as_ref().trim();
            let invalid = || {
                format!(
                    "{entry:?} is not a host name, a *.domain wildcard, an IP address or a CIDR \
                     network"
                )
            };
            if let Ok(network) = IpNet::from_str(entry) {
                list.networks.push(network.trunc());
                continue;
            }
            let bare = entry.trim_start_matches('[').trim_end_matches(']');
            if let Ok(address) = bare.parse::<IpAddr>() {
                list.networks.push(IpNet::from(address));
                continue;
            }
            let name = entry.trim_end_matches('.');
            if let Some(parent) = name.strip_prefix("*.") {
                list.subdomains.push(domain(parent).ok_or_else(invalid)?);
                continue;
            }
            if name.contains(['/', '*', ':']) {
                return Err(invalid());
            }
            list.names.push(domain(name).ok_or_else(invalid)?);
        }
        Ok(list)
    }

    /// Whether the list has no event.
    pub fn is_empty(&self) -> bool {
        self.names.is_empty() && self.subdomains.is_empty() && self.networks.is_empty()
    }

    /// Whether the operator lets `host` through: a name on the list or a subdomain of one, or
    /// an address (`127.0.0.1`, `[::1]`) within one of its networks.
    pub fn allows_host(&self, host: &str) -> bool {
        let bare = host.trim_start_matches('[').trim_end_matches(']');
        match bare.parse::<IpAddr>() {
            Ok(address) => self.allows_address(address),
            Err(_) => self.allows_name(bare),
        }
    }

    fn allows_name(&self, host: &str) -> bool {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        self.names.contains(&host)
            || self
                .subdomains
                .iter()
                .any(|parent| is_subdomain(&host, parent))
    }

    fn allows_address(&self, address: IpAddr) -> bool {
        // An IPv4 address mapped into IPv6 is the IPv4 address.
        let address = match address {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(address, IpAddr::V4),
            IpAddr::V4(_) => address,
        };
        self.networks
            .iter()
            .any(|network| network.contains(&address))
    }
}

/// Whether `name` is a subdomain of `parent`, both lowercase: `a.example.org` of `example.org`,
/// not `example.org` itself nor `evilexample.org`.
pub fn is_subdomain(name: &str, parent: &str) -> bool {
    name.strip_suffix(parent)
        .is_some_and(|prefix| prefix.len() > 1 && prefix.ends_with('.'))
}

/// A name in its ASCII, lowercase form.
pub fn domain(name: &str) -> Option<String> {
    match Host::parse(name) {
        Ok(Host::Domain(name)) => Some(name),
        _ => None,
    }
}

/// Why the egress checks refuse an address, if they do: everything but global unicast
/// addresses. The ranges are those of the IANA special-purpose address registries, with the
/// cloud metadata endpoints they contain (`169.254.169.254`, `fd00:ec2::254`,
/// `100.100.100.200`). An IPv6 address that embeds an IPv4 one (mapped, NAT64, 6to4) is judged
/// by its IPv4 address.
pub fn forbidden(address: IpAddr) -> Option<&'static str> {
    match address {
        IpAddr::V4(address) => forbidden_v4(address),
        IpAddr::V6(address) => forbidden_v6(address),
    }
}

fn forbidden_v4(address: Ipv4Addr) -> Option<&'static str> {
    let [a, b, c, _] = address.octets();
    let reason = match (a, b, c) {
        (0, _, _) => "\"this network\"",
        (10, _, _) | (172, 16..=31, _) | (192, 168, _) => "private",
        (100, 64..=127, _) => "shared (carrier-grade NAT)",
        (127, _, _) => "loopback",
        (169, 254, _) => "link-local (cloud metadata)",
        (192, 0, 0) => "IETF protocol",
        (192, 0, 2) | (198, 51, 100) | (203, 0, 113) => "documentation",
        (192, 88, 99) => "6to4 relay",
        (198, 18..=19, _) => "benchmarking",
        (224..=239, _, _) => "multicast",
        (240..=255, _, _) => "reserved",
        _ => return None,
    };
    Some(reason)
}

fn forbidden_v6(address: Ipv6Addr) -> Option<&'static str> {
    let segments = address.segments();
    let embedded = |high: u16, low: u16| Ipv4Addr::from((u32::from(high) << 16) | u32::from(low));
    if let Some(v4) = address.to_ipv4_mapped() {
        return forbidden_v4(v4);
    }
    let reason = match segments {
        [0, 0, 0, 0, 0, 0, 0, 0] => "unspecified",
        [0, 0, 0, 0, 0, 0, 0, 1] => "loopback",
        // IPv4-compatible addresses, deprecated.
        [0, 0, 0, 0, 0, 0, _, _] => "reserved",
        // NAT64: judged by the IPv4 address it reaches.
        [0x64, 0xff9b, 0, 0, 0, 0, high, low] => return forbidden_v4(embedded(high, low)),
        [0x64, 0xff9b, 1, ..] => "local NAT64",
        [0x100, 0, 0, 0, ..] => "discard",
        [0x2001, 0xdb8, ..] => "documentation",
        [0x2001, second, ..] if second < 0x200 => "IETF protocol (Teredo, benchmarking)",
        // 6to4: judged by the IPv4 address it embeds.
        [0x2002, high, low, ..] => return forbidden_v4(embedded(high, low)),
        [0x3fff, second, ..] if second < 0x1000 => "documentation",
        [0x5f00, ..] => "segment routing",
        [first, ..] if first & 0xfe00 == 0xfc00 => "unique local (private, cloud metadata)",
        [first, ..] if first & 0xffc0 == 0xfe80 => "link-local",
        [first, ..] if first & 0xffc0 == 0xfec0 => "site-local",
        [first, ..] if first & 0xff00 == 0xff00 => "multicast",
        // Everything else outside 2000::/3, the global unicast range.
        [first, ..] if first & 0xe000 != 0x2000 => "reserved",
        _ => return None,
    };
    Some(reason)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused(address: &str) -> bool {
        forbidden(address.parse().unwrap()).is_some()
    }

    #[test]
    fn private_loopback_link_local_and_metadata_addresses_are_refused() {
        for address in [
            "0.0.0.0",
            "10.1.2.3",
            "100.100.100.200",
            "127.0.0.1",
            "127.255.255.254",
            "169.254.169.254",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "192.0.2.1",
            "198.18.0.1",
            "224.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            "::127.0.0.1",
            "64:ff9b::a9fe:a9fe",
            "2002:7f00:1::",
            "2001:db8::1",
            "2001::1",
            "fc00::1",
            "fd00:ec2::254",
            "fe80::1",
            "ff02::1",
            // Outside the global unicast range: reserved by the IETF.
            "::ffff:0:7f00:1",
            "100:0:0:1::1",
            "4000::1",
            "e000::1",
        ] {
            assert!(refused(address), "{address} must be refused");
        }
    }

    #[test]
    fn global_addresses_are_reached() {
        for address in [
            "1.1.1.1",
            "8.8.8.8",
            "172.32.0.1",
            "100.128.0.1",
            "93.184.216.34",
            "2606:4700:4700::1111",
            "2a00:1450:4007:80c::200e",
            "::ffff:8.8.8.8",
            "64:ff9b::808:808",
            "2002:808:808::",
        ] {
            assert!(!refused(address), "{address} must be reached");
        }
    }

    /// Answers every name with the same addresses.
    #[derive(Debug)]
    struct Fixed(Vec<IpAddr>);

    impl NameResolver for Fixed {
        fn resolve(&self, _host: &str) -> Resolving {
            let addresses = self.0.clone();
            Box::pin(async move { Ok(addresses) })
        }
    }

    fn egress(addresses: &[&str]) -> Egress {
        let addresses = addresses.iter().map(|a| a.parse().unwrap()).collect();
        Egress::new(Arc::new(Fixed(addresses)), "webhooks are not sent there")
    }

    #[tokio::test]
    async fn every_request_gets_the_same_checks() {
        let public = egress(&["93.184.215.14"]);
        let url = |text: &str| Url::parse(text).unwrap();
        assert_eq!(
            public.check(&url("https://hooks.example.com/x")).await,
            Ok(())
        );
        assert_eq!(public.check(&url("https://93.184.215.14/x")).await, Ok(()));
        assert_eq!(
            public.check(&url("https://[::1]/x")).await,
            Err("::1 is a loopback address: webhooks are not sent there".into())
        );
        assert_eq!(
            public.check_url(&url("https://169.254.169.254/latest")),
            Err(
                "169.254.169.254 is a link-local (cloud metadata) address: webhooks are not \
                 sent there"
                    .into()
            )
        );
        // The WHATWG parser reads every spelling of an address as that address.
        assert!(public.check_url(&url("http://2130706433/")).is_err());
        let internal = egress(&["93.184.215.14", "10.0.0.7"]);
        assert_eq!(
            internal.check(&url("https://hooks.example.com/x")).await,
            Err(
                "hooks.example.com resolves to 10.0.0.7, a private address: webhooks are not \
                 sent there"
                    .into()
            )
        );
        let nothing = egress(&[]);
        assert_eq!(
            nothing.check(&url("https://gone.example.com/x")).await,
            Err("gone.example.com has no address".into())
        );
        let system = Egress::new(Arc::new(SystemResolver), "webhooks are not sent there");
        let refused = system.check(&url("https://localhost/x")).await.unwrap_err();
        assert!(refused.starts_with("localhost resolves to"), "{refused}");
    }

    #[test]
    fn the_internal_allow_list_reads_names_wildcards_addresses_and_networks() {
        let list = InternalAllowList::parse(&[
            "MCP.Internal.example.",
            "*.svc.cluster.local",
            "10.20.0.0/16",
            "10.9.9.9",
            "[fd00::7]",
            "fc00::/7",
        ])
        .unwrap();
        assert!(list.allows_name("mcp.internal.example"));
        assert!(list.allows_name("MCP.INTERNAL.EXAMPLE."));
        assert!(!list.allows_name("other.internal.example"));
        assert!(list.allows_name("crm.tools.svc.cluster.local"));
        assert!(!list.allows_name("svc.cluster.local"));
        assert!(!list.allows_name("evilsvc.cluster.local"));
        let address = |text: &str| text.parse::<IpAddr>().unwrap();
        assert!(list.allows_address(address("10.20.3.4")));
        assert!(!list.allows_address(address("10.21.0.1")));
        assert!(list.allows_address(address("10.9.9.9")));
        assert!(!list.allows_address(address("10.9.9.8")));
        assert!(list.allows_address(address("::ffff:10.20.0.1")));
        assert!(list.allows_address(address("fd00::7")));
        assert!(list.allows_address(address("fdff::1")));
        assert!(!list.allows_address(address("169.254.169.254")));
        assert!(list.allows_host("mcp.internal.example"));
        assert!(list.allows_host("10.20.0.1"));
        assert!(list.allows_host("[fd00::7]"));
        assert!(!list.allows_host("127.0.0.1"));
        assert!(!list.allows_host("example.com"));
        assert!(InternalAllowList::default().is_empty());
        assert!(!list.is_empty());

        for invalid in [
            "",
            "10.0.0.0/33",
            "mcp.internal:8080",
            "*.",
            "http://x",
            "a b",
        ] {
            let error = InternalAllowList::parse(&[invalid]).unwrap_err();
            assert!(error.contains("is not a host name"), "{invalid}: {error}");
        }
    }

    #[tokio::test]
    async fn the_internal_allow_list_lets_its_hosts_through_and_nothing_else() {
        let url = |text: &str| Url::parse(text).unwrap();
        let list = InternalAllowList::parse(&["mcp.internal", "10.20.0.0/16"]).unwrap();
        let rules = Rules {
            policy: forbidden,
            internal: Arc::new(list.clone()),
            refusal: "MCP servers are not reached there",
        };
        assert_eq!(
            rules.judge(Some("mcp.internal"), IpAddr::from([10, 0, 0, 1])),
            Ok(Reach::Internal)
        );
        assert_eq!(
            rules.judge(Some("crm.example"), IpAddr::from([10, 20, 0, 1])),
            Ok(Reach::Internal)
        );
        assert_eq!(
            rules.judge(Some("crm.example"), IpAddr::from([93, 184, 215, 14])),
            Ok(Reach::Public)
        );
        assert_eq!(
            rules.judge(Some("crm.example"), IpAddr::from([10, 0, 0, 1])),
            Err("crm.example resolves to 10.0.0.1, a private address: MCP servers are not reached there".into())
        );
        assert_eq!(
            rules.judge_url(&url("http://10.20.1.1:8080/mcp")),
            Ok(Reach::Internal)
        );
        assert_eq!(
            rules.judge_url(&url("http://169.254.169.254/latest")),
            Err("169.254.169.254 is a link-local (cloud metadata) address: MCP servers are not reached there".into())
        );

        // A name with an address the list lets through and one it does not is refused.
        let mixed = egress(&["10.20.0.1", "10.0.0.1"]).allowing(list.clone());
        assert!(mixed.check(&url("https://crm.example/mcp")).await.is_err());
        let inside = egress(&["10.20.0.1"]).allowing(list);
        assert_eq!(inside.check(&url("https://crm.example/mcp")).await, Ok(()));
        // Without the list, the same name is refused.
        assert!(
            egress(&["10.20.0.1"])
                .check(&url("https://crm.example/mcp"))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_client_built_with_the_checks_refuses_internal_addresses() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let client = egress(&["127.0.0.1"])
            .apply(reqwest::Client::builder())
            .build()
            .unwrap();
        let error = client
            .post(format!("http://rebound.test:{port}/x"))
            .send()
            .await
            .unwrap_err();
        assert_eq!(
            Egress::refusal_of(&error),
            Some(
                "rebound.test resolves to 127.0.0.1, a loopback address: webhooks are not sent \
                 there"
                    .into()
            )
        );
        let accepted =
            tokio::time::timeout(std::time::Duration::from_millis(100), listener.accept()).await;
        assert!(accepted.is_err(), "nothing connected");
    }
}
