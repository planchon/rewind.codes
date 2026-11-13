#!/bin/sh
# Writes deploy/.env (or the file given) with fresh secrets: the master key, the first admin
# key, and the passwords of Postgres and MinIO. It never overwrites an existing file: losing the
# master key loses every secret stored with it.
set -eu
cd "$(dirname "$0")"

env_file="${1:-.env}"
if [ -e "$env_file" ]; then
    echo "init.sh: $env_file exists; remove it first to start a new deployment" >&2
    exit 1
fi

random() {
    LC_ALL=C tr -dc 'A-Za-z0-9' </dev/urandom | head -c "$1"
}

umask 077
cat >"$env_file" <<ENV
# Written by init.sh. Keep this file secret, and back up REWIND_MASTER_KEY.

# The master key of the secret store: 32 random bytes in base64.
REWIND_MASTER_KEY=$(head -c 32 /dev/urandom | base64)
# The first API key (role admin, tenant \$REWIND_ADMIN_TENANT): it creates the others.
REWIND_ADMIN_KEY=rewind_sk_$(random 43)
REWIND_ADMIN_TENANT=default

POSTGRES_PASSWORD=$(random 32)
MINIO_ROOT_USER=rewind
MINIO_ROOT_PASSWORD=$(random 32)

# The workers under runc. Use the profile gvisor instead on a host with runsc.
COMPOSE_PROFILES=runc
ENV
echo "init.sh: wrote $env_file"
