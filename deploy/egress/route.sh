#!/bin/sh
# Holds a node's network namespace: the node joins it (`network_mode: service:…`), so the
# node itself needs no capability. Its only routes are the core network and the default
# route through the egress gateway, which is also its DNS server (`dns:` in compose.yaml).
#
# The holder is healthy once its route is set (/run/routed, on a tmpfs). A node under gVisor
# waits for it: its sandbox copies the namespace's addresses and routes when it starts, and
# takes them from the namespace.
set -eu

gateway="${REWIND_EGRESS_ADDRESS:?REWIND_EGRESS_ADDRESS: the egress gateway's address}"
ip route replace default via "$gateway"
: >/run/routed
echo "route: default via $gateway"
trap 'exit 0' TERM INT
sleep 2147483647 &
wait
