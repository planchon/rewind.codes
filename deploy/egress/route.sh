#!/bin/sh
# Holds a worker's network namespace: the worker joins it (`network_mode: service:…`), so the
# worker itself needs no capability. Its only routes are the core network and the default
# route through the egress gateway, which is also its DNS server (`dns:` in compose.yaml).
set -eu

gateway="${PTC_EGRESS_ADDRESS:?PTC_EGRESS_ADDRESS: the egress gateway's address}"
ip route replace default via "$gateway"
echo "route: default via $gateway"
trap 'exit 0' TERM INT
sleep 2147483647 &
wait
