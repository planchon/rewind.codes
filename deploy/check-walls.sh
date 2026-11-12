#!/bin/sh
# Checks the walls around a worker, from inside its network settings:
#
#   ./check-walls.sh [worker-1-net]
#
# Under runc, from the holder of the worker's namespace, which shares it and has the tools the
# worker's image has not. Under gVisor, the worker's sandbox has taken over that namespace, and
# nothing else can run in it: from the service `walls`, a gVisor sandbox with the network
# settings of the workers under gVisor, in a holder of its own (compose.yaml), started for the
# check and removed after it.
#
# Open: the core network (MinIO here). Closed: names outside PTC_EGRESS_ALLOW, addresses no
# allowed name resolved to, and the cloud metadata endpoint. Exits with 1 if a wall is missing.
set -eu
cd "$(dirname "$0")"

holder="${1:-worker-1-net}"
failed=0

holder_id="$(docker compose ps -q "$holder")"
if [ -z "$holder_id" ]; then
    echo "check-walls.sh: $holder is not running" >&2
    exit 1
fi

# The runtime of the worker that joined the holder's namespace.
project="$(docker inspect -f '{{index .Config.Labels "com.docker.compose.project"}}' "$holder_id")"
runtime="$(docker ps -q --filter "label=com.docker.compose.project=$project" |
    xargs docker inspect -f '{{.HostConfig.NetworkMode}} {{.HostConfig.Runtime}}' |
    awk -v mode="container:$holder_id" '$1 == mode { print $2; exit }')"

# Starts a service of the check, again: a new holder gives a new namespace, which the sandbox
# of a previous check took over.
start() {
    if ! output="$(docker compose up -d --no-deps --force-recreate --wait "$1" 2>&1)"; then
        echo "$output" >&2
        exit 1
    fi
}

if [ "$runtime" = runsc ]; then
    probe=walls
    trap 'docker compose rm -fs walls walls-net >/dev/null 2>&1' EXIT
    start walls-net
    start walls
    echo "from a gVisor sandbox with the network settings of the worker in $holder:"
else
    probe="$holder"
    echo "from $holder:"
fi

check() {
    expected="$1"
    what="$2"
    command="$3"
    if docker compose exec -T "$probe" sh -c "$command" >/dev/null 2>&1; then
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

docker compose exec -T "$probe" ip route | sed 's/^/  route: /'
docker compose exec -T "$probe" cat /etc/resolv.conf | sed -n 's/^nameserver /  dns: /p'
check open "the core network (MinIO)" "wget -q -T 5 -O /dev/null http://minio:9000/minio/health/live"
check closed "a name outside the allow-list" "nslookup example.com"
check closed "an address no allowed name resolved to" "wget -q -T 5 -O /dev/null http://1.1.1.1/"
check closed "the cloud metadata endpoint" "wget -q -T 5 -O /dev/null http://169.254.169.254/latest/meta-data/"
exit "$failed"
