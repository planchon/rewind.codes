#!/bin/sh
# Checks the walls around a worker, from inside its network namespace: the holder of the
# namespace shares it, and has the tools the worker's image has not.
#
#   ./check-walls.sh [worker-1-net]
#
# Open: the core network (MinIO here). Closed: names outside PTC_EGRESS_ALLOW, addresses no
# allowed name resolved to, and the cloud metadata endpoint. Exits with 1 if a wall is missing.
set -eu
cd "$(dirname "$0")"

holder="${1:-worker-1-net}"
failed=0

check() {
    expected="$1"
    what="$2"
    command="$3"
    if docker compose exec -T "$holder" sh -c "$command" >/dev/null 2>&1; then
        actual=open
    else
        actual=closed
    fi
    if [ "$actual" = "$expected" ]; then
        echo "ok      $what: $actual"
    else
        echo "FAILED  $what: $actual, expected $expected"
        failed=1
    fi
}

echo "from $holder:"
docker compose exec -T "$holder" ip route | sed 's/^/  route: /'
check open "the core network (MinIO)" "wget -q -T 5 -O /dev/null http://minio:9000/minio/health/live"
check closed "a name outside the allow-list" "nslookup example.com"
check closed "an address no allowed name resolved to" "wget -q -T 5 -O /dev/null http://1.1.1.1/"
check closed "the cloud metadata endpoint" "wget -q -T 5 -O /dev/null http://169.254.169.254/latest/meta-data/"
exit "$failed"
