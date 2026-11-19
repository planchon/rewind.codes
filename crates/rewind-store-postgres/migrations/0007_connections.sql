-- Version 7: people's connections (`archi/06-storage-and-scale.md`, "Tables", "Connections"),
-- outside every tenant: the OAuth clients (metadata documents cached under their URL, clients
-- registered as `rc_…`), the authorization codes, the connections and their tokens, all hashed.
-- A connection's grants are rows of `grants` (principal `connection:cn_…`), found by principal
-- across tenants. A connection's `expires_at` is when its latest refresh token lapses: the
-- workers delete it 30 days later.

CREATE INDEX grants_by_principal ON grants (principal);

CREATE TABLE oauth_clients (
    id text COLLATE "C" PRIMARY KEY,
    kind text NOT NULL,
    name text NOT NULL,
    uri text,
    logo_uri text,
    redirect_uris text[] NOT NULL,
    auth_method text NOT NULL,
    secret_hash bytea,
    fetched_at bigint,
    expires_at bigint,
    created_at bigint NOT NULL,
    last_used_at bigint
);
CREATE INDEX oauth_clients_by_kind ON oauth_clients (kind);

-- The organizations a code opens: a JSON object from tenant to access (`read`, `run`).
CREATE TABLE oauth_codes (
    hash bytea PRIMARY KEY,
    client_id text COLLATE "C" NOT NULL,
    user_id text COLLATE "C" NOT NULL,
    redirect_uri text NOT NULL,
    code_challenge text NOT NULL,
    resource text NOT NULL,
    organizations jsonb NOT NULL,
    labels jsonb NOT NULL,
    expires_at bigint NOT NULL,
    used_at bigint,
    connection_id text COLLATE "C"
);
CREATE INDEX oauth_codes_by_expiry ON oauth_codes (expires_at);

CREATE TABLE connections (
    id text COLLATE "C" PRIMARY KEY,
    user_id text COLLATE "C" NOT NULL,
    client_id text COLLATE "C" NOT NULL,
    client_name text NOT NULL,
    created_at bigint NOT NULL,
    last_used_at bigint,
    revoked_at bigint,
    expires_at bigint NOT NULL
);
CREATE INDEX connections_by_user ON connections (user_id, created_at, id);
CREATE INDEX connections_by_client ON connections (client_id);
-- One live connection per person and client.
CREATE UNIQUE INDEX connections_live ON connections (user_id, client_id) WHERE revoked_at IS NULL;
CREATE INDEX connections_by_expiry ON connections (expires_at);

CREATE TABLE connection_tokens (
    hash bytea PRIMARY KEY,
    connection_id text COLLATE "C" NOT NULL,
    kind text NOT NULL,
    expires_at bigint NOT NULL,
    created_at bigint NOT NULL,
    used_at bigint
);
CREATE INDEX connection_tokens_by_connection ON connection_tokens (connection_id);
CREATE INDEX connection_tokens_by_expiry ON connection_tokens (expires_at);
