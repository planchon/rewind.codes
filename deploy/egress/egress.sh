#!/bin/sh
# The workers' egress gateway: their only way out of the core network.
#
# Workers have no route but the core network and a default route through this container
# (route.sh). It forwards a worker's connection only to an address that one of the allowed
# names resolved to, through this container's own DNS server:
#
#   1. dnsmasq answers only the names of PTC_EGRESS_ALLOW (and their subdomains), from Docker's
#      DNS; any other name is refused. IPv6 answers are dropped: workers have no IPv6 route.
#   2. Each address it answers joins the ipset `allowed`.
#   3. iptables forwards TCP from the core network to the addresses of `allowed` only, and
#      never to 169.254.0.0/16 (the cloud metadata endpoint), whatever a name resolves to.
#
# PTC_EGRESS_ADDRESS is this container's address on the core network, the workers' gateway and
# DNS server. PTC_EGRESS_ALLOW lists the allowed names, separated by spaces or commas.
set -eu

address="${PTC_EGRESS_ADDRESS:?PTC_EGRESS_ADDRESS: this gateway's address on the core network}"
allow=$(printf '%s' "${PTC_EGRESS_ALLOW:-}" | tr ',' ' ')

inside=$(ip -o -4 addr show | awk -v ip="$address" '{ split($4, a, "/"); if (a[1] == ip) print $2 }')
if [ -z "$inside" ]; then
    echo "egress: no interface holds $address" >&2
    exit 1
fi

ipset create allowed hash:ip -exist
ipset flush allowed

# Forwarded traffic: replies, then new TCP connections to allowed addresses only.
iptables -P FORWARD DROP
iptables -F FORWARD
iptables -A FORWARD -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT
iptables -A FORWARD -i "$inside" -d 169.254.0.0/16 -j DROP
iptables -A FORWARD -i "$inside" ! -o "$inside" -p tcp -m set --match-set allowed dst -j ACCEPT
iptables -t nat -C POSTROUTING ! -o "$inside" -j MASQUERADE 2>/dev/null \
    || iptables -t nat -A POSTROUTING ! -o "$inside" -j MASQUERADE

# This container itself: from the core network, only DNS.
iptables -F INPUT
iptables -A INPUT -i lo -j ACCEPT
iptables -A INPUT -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT
iptables -A INPUT -i "$inside" -p udp --dport 53 -j ACCEPT
iptables -A INPUT -i "$inside" -p tcp --dport 53 -j ACCEPT
iptables -A INPUT -i "$inside" -j DROP

set -- --keep-in-foreground --pid-file= --no-resolv --no-hosts --no-poll --filter-AAAA \
    --listen-address="$address" --bind-interfaces --cache-size=1000 --log-facility=-
if [ -n "${PTC_EGRESS_LOG_QUERIES:-}" ]; then
    set -- "$@" --log-queries
fi
for name in $allow; do
    set -- "$@" --server="/$name/127.0.0.11" --ipset="/$name/allowed"
done
echo "egress: forwarding to the addresses of: ${allow:-nothing}"
exec dnsmasq "$@"
