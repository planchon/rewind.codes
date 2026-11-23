#!/bin/sh
# The DNS server of the nodes under gVisor (resolv.conf): a relay to Docker's DNS server.
#
# Docker's DNS server, at 127.0.0.11, is reached through the host network stack of each
# container's namespace. A gVisor sandbox runs its own network stack, where 127.0.0.11 is its
# own loopback: a node under gVisor cannot reach Docker's DNS server.
#
# This relay stands where a node's holder stands: on the core network only, with the egress
# gateway as its DNS server (`dns:` in compose.yaml). It asks Docker's DNS server every question,
# so it answers what a node under runc is answered: the names of the core network, and every
# other name through the egress gateway, which answers the allowed names only. It caches
# nothing, as Docker's DNS server does not.
#
# REWIND_NODE_DNS_ADDRESS is this relay's address on the core network.
set -eu

address="${REWIND_NODE_DNS_ADDRESS:?REWIND_NODE_DNS_ADDRESS: this relay's address on the core network}"

echo "dns: relaying to Docker's DNS server on $address"
exec dnsmasq --keep-in-foreground --pid-file= --no-resolv --no-hosts --no-poll \
    --server=127.0.0.11 --listen-address="$address" --bind-interfaces --cache-size=0 \
    --log-facility=-
