//! The first step of the `http` tool's egress rules (`archi/04-tools.md`, "The http tool and
//! egress"): the host must be in the toolset's allow-list. The other steps, a name resolved
//! once, private and metadata addresses refused, the connection to the checked address, are
//! `rewind-outbound`'s [`Egress`](rewind_outbound::Egress), the client's only resolver.

use std::net::IpAddr;

use rewind_outbound::{AddressPolicy, Refused, domain, is_subdomain};
use url::{Host, Url};

/// How the `http` tool ends the reason of a refusal.
pub(crate) const HTTP_TOOL: &str = "the http tool does not reach it";

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
                    Entry::Subdomains(parent) => is_subdomain(&name, parent),
                    Entry::Address(_) => false,
                })
            }
            Host::Ipv4(address) => self.entries.contains(&Entry::Address(IpAddr::V4(*address))),
            Host::Ipv6(address) => self.entries.contains(&Entry::Address(IpAddr::V6(*address))),
        }
    }
}

/// Checks a URL before a request is sent to it: an http or https URL, a host in the allow-list,
/// and for a host given as an address, an address the tool may reach. Hosts given as names are
/// checked by the client's resolver.
pub(crate) fn check(url: &Url, allowed: &AllowList, policy: AddressPolicy) -> Result<(), Refused> {
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

#[cfg(test)]
mod tests {
    use rewind_outbound::forbidden;

    use super::*;

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
}
