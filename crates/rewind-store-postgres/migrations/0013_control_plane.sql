-- Version 13: what the control plane keeps of the run plane (`archi/06-storage-and-scale.md`,
-- "The control plane", "Partitions, owners and nodes").

-- The partition a namespace's runs live in, chosen when the catalog creates it and never
-- changed. Null for a namespace the journal store created: the catalog sets it at its next
-- `ensure_namespace` or `put_namespace`, or `PostgresCatalog::place_namespaces` sets every one.
ALTER TABLE namespaces ADD COLUMN partition integer CHECK (partition >= 0 AND partition < 65536);

-- The nodes that joined the cluster. A row is a member only while its node holds the session
-- advisory lock on its id (`crate::cluster`): the row of a node that died stays until the node
-- that watched it, or the next node to join, deletes it. `joined_at` is the database's clock.
CREATE TABLE nodes (
    id text COLLATE "C" PRIMARY KEY,
    address text NOT NULL,
    build text NOT NULL,
    epochs bigint[] NOT NULL,
    pool text COLLATE "C",
    executors_only boolean NOT NULL,
    joined_at bigint NOT NULL
);

-- The partition map: each partition's owner, and its epoch, raised by one at each move. Every
-- partition of the deployment has a row, from 0, created when the cluster is opened. `pool` is
-- the pool of nodes whose partitions they are; null: the shared pool.
CREATE TABLE partitions (
    id integer PRIMARY KEY CHECK (id >= 0 AND id < 65536),
    pool text COLLATE "C",
    node text COLLATE "C",
    epoch bigint NOT NULL DEFAULT 0 CHECK (epoch >= 0)
);

-- The runs `rewind server migrate-runs` moved into a partition, whose version 7 ids carry no
-- partition.
CREATE TABLE legacy_runs (
    id uuid PRIMARY KEY,
    tenant text COLLATE "C" NOT NULL,
    partition integer NOT NULL CHECK (partition >= 0 AND partition < 65536)
);
