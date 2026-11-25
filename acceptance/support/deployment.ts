import { randomBytes } from "node:crypto";
import { Rewind } from "@rewind.codes/sdk";
import { type Collector, Containers, type Minio, startCollector, startMinio, startPostgres } from "./docker.js";
import { rewindBinary } from "./paths.js";
import type { StagingCredentials } from "./workos.js";
import { eventually, freePort, Processes } from "./processes.js";

/**
 * A deployment as 06 describes it: one Postgres for the control plane, a bucket on S3 (MinIO)
 * for the partitions, the archive and the blobs, an OpenTelemetry Collector, and two nodes
 * (`rewind server`, each an HTTP edge and a node) linked by the node link.
 */
export interface Deployment {
  /** The first node's URL. */
  url: string;
  /** Every node's URL, in the order of `nodes`: each answers every route. */
  urls: string[];
  /** The admin key, `--admin-key`, of the organization `acme`. */
  adminKey: string;
  admin: Rewind;
  minio: Minio;
  collector: Collector;
  /** The nodes' ids (`--node-id`): `service.instance.id` of their telemetry. */
  nodes: string[];
  stop(): Promise<void>;
}

/** A `rewind server` on Turso and local files, started by `startServer`. */
export interface Server {
  /** Its URL, for the API. */
  url: string;
  /**
   * The dashboard's origin as a browser opens it: `localhost`, where browsers keep the sign-in's
   * `Secure` cookies over `http://`.
   */
  origin: string;
  adminKey: string;
  admin: Rewind;
  stop(): Promise<void>;
}

/**
 * One `rewind server`, a single node on Turso and local files in `dir`, over TCP. With `workos`,
 * the dashboard's sign-in on that WorkOS environment: the four `WORKOS_*`, with a cookie password
 * made for this server and the redirect URI of its own origin (only a sign-in with GitHub goes
 * there).
 */
export async function startServer(dir: string, options: { workos?: StagingCredentials } = {}): Promise<Server> {
  const processes = new Processes(dir);
  const adminKey = `rewind_sk_${randomBytes(24).toString("hex")}`;
  const port = await freePort();
  const url = `http://127.0.0.1:${port}`;
  const origin = `http://localhost:${port}`;
  const env = options.workos
    ? {
        WORKOS_CLIENT_ID: options.workos.clientId,
        WORKOS_API_KEY: options.workos.apiKey,
        WORKOS_COOKIE_PASSWORD: randomBytes(32).toString("hex"),
        WORKOS_REDIRECT_URI: `${origin}/auth/callback`,
      }
    : {};
  const child = processes.start("server", rewindBinary(), [
    "server",
    "--store",
    `turso:${dir}/control.db`,
    "--bucket",
    `${dir}/bucket`,
    "--master-key",
    randomBytes(32).toString("base64"),
    "--admin-key",
    adminKey,
    "--admin-organization",
    "acme",
    "--listen",
    `127.0.0.1:${port}`,
  ], { env });
  try {
    await eventually("the server listens", async () => {
      if (child.exitCode !== null) throw new Error(`the server exited with ${child.exitCode}: see ${dir}/server.log`);
      const response = await fetch(`${url}/v1/server`, { headers: { authorization: `Bearer ${adminKey}` } });
      return response.ok ? true : undefined;
    });
  } catch (error) {
    await processes.stopAll();
    throw error;
  }
  return { url, origin, adminKey, admin: new Rewind({ url, apiKey: adminKey }), stop: () => processes.stopAll() };
}

export async function startDeployment(dir: string, options: { nodes?: number } = {}): Promise<Deployment> {
  const containers = new Containers();
  const processes = new Processes(dir);
  const stop = async () => {
    await processes.stopAll();
    containers.removeAll();
  };
  try {
    const [postgres, minio, collector] = await Promise.all([
      startPostgres(containers),
      startMinio(containers, "rewind"),
      startCollector(containers, dir),
    ]);
    const adminKey = `rewind_sk_${randomBytes(24).toString("hex")}`;
    const common = [
      "--store",
      postgres.url,
      "--bucket",
      `s3://${minio.bucket}`,
      "--s3-endpoint",
      minio.endpoint,
      "--s3-region",
      "us-east-1",
      "--s3-allow-http",
      "--s3-access-key-id",
      minio.user,
      "--s3-secret-access-key",
      minio.password,
      "--master-key",
      randomBytes(32).toString("base64"),
      "--cluster-secret",
      randomBytes(32).toString("hex"),
      "--otlp-endpoint",
      collector.http,
      "--otlp-protocol",
      "http",
      "--admin-key",
      adminKey,
      "--admin-organization",
      "acme",
    ];
    const binary = rewindBinary();
    const nodes = Array.from({ length: options.nodes ?? 2 }, (_, n) => `node-${n + 1}`);
    const urls: string[] = [];
    // One at a time: the first opens the cluster and its partitions, each next one takes its share.
    for (const name of nodes) {
      const port = await freePort();
      const link = await freePort();
      const child = processes.start(name, binary, [
        "server",
        "--node-id",
        name,
        "--listen",
        `127.0.0.1:${port}`,
        "--link-listen",
        `127.0.0.1:${link}`,
        "--link-address",
        `127.0.0.1:${link}`,
        ...common,
      ]);
      const url = `http://127.0.0.1:${port}`;
      await eventually(`${name} listens`, async () => {
        if (child.exitCode !== null) throw new Error(`${name} exited with ${child.exitCode}: see ${dir}/${name}.log`);
        const response = await fetch(`${url}/v1/server`, { headers: { authorization: `Bearer ${adminKey}` } });
        return response.ok ? true : undefined;
      });
      urls.push(url);
    }
    // Every node counts the others: the partitions are shared out.
    for (const [n, url] of urls.entries()) {
      await eventually(`${nodes[n]} sees the ${nodes.length} nodes`, async () => {
        const response = await fetch(`${url}/v1/server`, { headers: { authorization: `Bearer ${adminKey}` } });
        const info = (await response.json()) as { node?: string; nodes?: number };
        return info.node === nodes[n] && info.nodes === nodes.length ? true : undefined;
      });
    }
    return {
      url: urls[0]!,
      urls,
      adminKey,
      admin: new Rewind({ url: urls[0]!, apiKey: adminKey }),
      minio,
      collector,
      nodes,
      stop,
    };
  } catch (error) {
    await stop();
    throw error;
  }
}
