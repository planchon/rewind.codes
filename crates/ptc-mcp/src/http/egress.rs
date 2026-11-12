//! The egress rules of the `http` tool (`archi/04-tools.md`, "The http tool and egress"):
//!
//! 1. The host must be in the toolset's allow-list ([`AllowList`]).
//! 2. The name is resolved once ([`CheckedResolver`], the only resolver of the client).
//! 3. Private, loopback, link-local and cloud-metadata addresses are refused ([`forbidden`]);
//!    a name with any such address is refused.
//! 4. The connection goes to the addresses that were checked: the resolver hands exactly
//!    those to the connector, so a DNS answer that changes in between is never used.
//!
//! A URL whose host is an IP address is checked against the same rules before the request.
//! Redirects go through all of them again (`super`).
//!
//! [`Egress`] lends steps 2 to 4 to the other requests a deployment sends where a tenant says:
//! the server's lifecycle webhooks.

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;

use url::{Host, Url};

/// A refused request: the reason, written for the model.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub(crate) struct Refused(pub(crate) String);

/// What a [`NameResolver`] returns.
pub type Resolving = Pin<Box<dyn Future<Output = io::Result<Vec<IpAddr>>> + Send>>;

/// Resolves host names for the `http` tool. The dispatcher uses the system's resolver; a
/// deployment or a test may plug its own. Its answer is checked before any connection.
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

/// Says whether an address is refused, and why.
pub(crate) type Policy = fn(IpAddr) -> Option<&'static str>;

/// How the `http` tool ends the reason of a refusal.
pub(crate) const HTTP_TOOL: &str = "the http tool does not reach it";

/// The client's resolver: resolves once, refuses the name if any of its addresses is refused,
/// and hands the connector exactly the addresses it checked.
#[derive(Debug)]
pub(crate) struct CheckedResolver {
    pub(crate) resolver: Arc<dyn NameResolver>,
    pub(crate) policy: Policy,
    /// Ends the reason of a refusal: who does not go there.
    pub(crate) refusal: &'static str,
}

impl reqwest::dns::Resolve for CheckedResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_owned();
        let resolver = self.resolver.clone();
        let (policy, refusal) = (self.policy, self.refusal);
        Box::pin(async move {
            let addresses = resolve(resolver.as_ref(), &host, policy, refusal)
                .await
                .map_err(|error| error.into_boxed())?;
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

/// Resolves `host` once and checks every address it has.
async fn resolve(
    resolver: &dyn NameResolver,
    host: &str,
    policy: Policy,
    refusal: &str,
) -> Result<Vec<IpAddr>, Unresolved> {
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
    for address in &addresses {
        if let Some(reason) = policy(*address) {
            return Err(Unresolved::Refused(Refused(format!(
                "{host} resolves to {address}, a {reason} address: {refusal}"
            ))));
        }
    }
    Ok(addresses)
}

/// The egress checks of the `http` tool, without its allow-list, for the other requests a
/// deployment sends to a place a tenant chose: the server's lifecycle webhooks. A name is
/// resolved once, a name with any private, loopback, link-local or cloud-metadata address is
/// refused, and the connection goes to the addresses that were checked. A URL whose host is an
/// address is checked before the request.
#[derive(Debug, Clone)]
pub struct Egress {
    resolver: Arc<dyn NameResolver>,
    policy: Policy,
    refusal: &'static str,
}

impl Egress {
    /// The checks, resolving names with `resolver`. `refusal` ends the reason of a refusal,
    /// such as "webhooks are not sent there".
    pub fn new(resolver: Arc<dyn NameResolver>, refusal: &'static str) -> Self {
        Self {
            resolver,
            policy: forbidden,
            refusal,
        }
    }

    /// `builder` with the checked resolver as its only resolver, and without proxies: a proxy
    /// would resolve the names itself.
    pub fn apply(&self, builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
        builder.no_proxy().dns_resolver(Arc::new(CheckedResolver {
            resolver: self.resolver.clone(),
            policy: self.policy,
            refusal: self.refusal,
        }))
    }

    /// Checks the host of `url` when it is an address: the reason when it is refused. A name is
    /// checked when it is resolved.
    pub fn check_url(&self, url: &Url) -> Result<(), String> {
        let address = match url.host() {
            Some(Host::Ipv4(address)) => IpAddr::V4(address),
            Some(Host::Ipv6(address)) => IpAddr::V6(address),
            Some(Host::Domain(_)) => return Ok(()),
            None => return Err(format!("{url} has no host")),
        };
        match (self.policy)(address) {
            Some(reason) => Err(format!("{address} is a {reason} address: {}", self.refusal)),
            None => Ok(()),
        }
    }

    /// Checks `url` as a request to it would be: its host when it is an address, otherwise
    /// every address its name resolves to now. The reason when it is refused or cannot be
    /// resolved.
    pub async fn check(&self, url: &Url) -> Result<(), String> {
        self.check_url(url)?;
        match url.host() {
            Some(Host::Domain(name)) => {
                resolve(self.resolver.as_ref(), name, self.policy, self.refusal)
                    .await
                    .map(drop)
                    .map_err(|error| match error {
                        Unresolved::Failed(error) => error.to_string(),
                        Unresolved::Refused(Refused(reason)) => reason,
                    })
            }
            _ => Ok(()),
        }
    }

    /// The reason, when a request failed because these checks refused its address.
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

/// The hosts of a toolset's allow-list: names (`api.example.com`), wildcards for the subdomains
/// of a name (`*.example.com`, not `example.com` itself), or IP addresses.
#[derive(Debug, Clone)]
pub(crate) struct AllowList {
    entries: Vec<Entry>,
    listed: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Entry {
    Name(String),
    Subdomains(String),
    Address(IpAddr),
}

impl AllowList {
    pub(crate) fn new(hosts: &[String]) -> Self {
        let entries = hosts
            .iter()
            .filter_map(|host| {
                let host = host.trim().trim_end_matches('.');
                if let Some(parent) = host.strip_prefix("*.") {
                    return domain(parent).map(Entry::Subdomains);
                }
                let bare = host.trim_start_matches('[').trim_end_matches(']');
                if let Ok(address) = bare.parse::<IpAddr>() {
                    return Some(Entry::Address(address));
                }
                match Host::parse(host) {
                    Ok(Host::Domain(name)) => Some(Entry::Name(name)),
                    Ok(Host::Ipv4(address)) => Some(Entry::Address(IpAddr::V4(address))),
                    Ok(Host::Ipv6(address)) => Some(Entry::Address(IpAddr::V6(address))),
                    Err(_) => None,
                }
            })
            .collect();
        Self {
            entries,
            listed: hosts.join(", "),
        }
    }

    fn allows(&self, host: &Host<&str>) -> bool {
        match host {
            Host::Domain(name) => {
                let name = name.trim_end_matches('.').to_ascii_lowercase();
                self.entries.iter().any(|entry| match entry {
                    Entry::Name(allowed) => *allowed == name,
                    Entry::Subdomains(parent) => name
                        .strip_suffix(parent.as_str())
                        .is_some_and(|prefix| prefix.len() > 1 && prefix.ends_with('.')),
                    Entry::Address(_) => false,
                })
            }
            Host::Ipv4(address) => self.entries.contains(&Entry::Address(IpAddr::V4(*address))),
            Host::Ipv6(address) => self.entries.contains(&Entry::Address(IpAddr::V6(*address))),
        }
    }
}

/// A name in its ASCII, lowercase form.
fn domain(name: &str) -> Option<String> {
    match Host::parse(name) {
        Ok(Host::Domain(name)) => Some(name),
        _ => None,
    }
}

/// Checks a URL before a request is sent to it: an http or https URL, a host in the allow-list,
/// and for a host given as an address, an address the tool may reach. Hosts given as names are
/// checked by [`CheckedResolver`].
pub(crate) fn check(url: &Url, allowed: &AllowList, policy: Policy) -> Result<(), Refused> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(Refused(format!(
            "the http tool only sends http and https requests, not {}:",
            url.scheme()
        )));
    }
    let Some(host) = url.host() else {
        return Err(Refused(format!("{url} has no host")));
    };
    if !allowed.allows(&host) {
        return Err(Refused(format!(
            "{host} is not in the toolset's allow-list of the http tool ({})",
            allowed.listed
        )));
    }
    let address = match host {
        Host::Ipv4(address) => Some(IpAddr::V4(address)),
        Host::Ipv6(address) => Some(IpAddr::V6(address)),
        Host::Domain(_) => None,
    };
    if let Some(address) = address
        && let Some(reason) = policy(address)
    {
        return Err(Refused(format!(
            "{address} is a {reason} address: {HTTP_TOOL}"
        )));
    }
    Ok(())
}

/// Why the `http` tool refuses an address, if it does: everything but global unicast
/// addresses. The ranges are those of the IANA special-purpose address registries, with the
/// cloud metadata endpoints they contain (`169.254.169.254`, `fd00:ec2::254`,
/// `100.100.100.200`). An IPv6 address that embeds an IPv4 one (mapped, NAT64, 6to4) is judged
/// by its IPv4 address.
pub(crate) fn forbidden(address: IpAddr) -> Option<&'static str> {
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

    #[test]
    fn the_allow_list_matches_names_subdomains_and_addresses() {
        let allowed = AllowList::new(&[
            "API.example.com".into(),
            "*.example.org".into(),
            "93.184.216.34".into(),
            "[2606:4700::1]".into(),
            "bücher.example".into(),
        ]);
        let ok = |url: &str| check(&Url::parse(url).unwrap(), &allowed, forbidden).is_ok();
        assert!(ok("https://api.example.com/x"));
        assert!(ok("https://API.EXAMPLE.COM./x"));
        assert!(ok("http://a.example.org:8080/"));
        assert!(ok("https://a.b.example.org/"));
        assert!(!ok("https://example.org/"));
        assert!(!ok("https://evilexample.org/"));
        assert!(!ok("https://api.example.com.evil.net/"));
        assert!(ok("http://93.184.216.34/"));
        assert!(ok("http://[2606:4700::1]/"));
        assert!(ok("https://xn--bcher-kva.example/"));
        assert!(!ok("ftp://api.example.com/"));
    }

    #[test]
    fn addresses_in_urls_are_checked_even_when_allowed() {
        let allowed = AllowList::new(&["127.0.0.1".into(), "localhost".into()]);
        let error = check(
            &Url::parse("http://127.0.0.1/").unwrap(),
            &allowed,
            forbidden,
        )
        .unwrap_err();
        assert_eq!(
            error.0,
            "127.0.0.1 is a loopback address: the http tool does not reach it"
        );
        // The WHATWG parser reads every spelling of an address as that address.
        let decimal = check(
            &Url::parse("http://2130706433/").unwrap(),
            &allowed,
            forbidden,
        );
        assert!(decimal.is_err());
        let error = check(
            &Url::parse("http://internal.example/").unwrap(),
            &allowed,
            forbidden,
        )
        .unwrap_err();
        assert_eq!(
            error.0,
            "internal.example is not in the toolset's allow-list of the http tool (127.0.0.1, localhost)"
        );
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
    async fn other_requests_get_the_same_checks_without_an_allow_list() {
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
