import { randomBytes } from "node:crypto";
import { Rewind } from "@rewind.codes/sdk";
import { type Collector, Containers, type Minio, startCollector, startMinio, startPostgres } from "./docker.js";
import { rewindBinary } from "./paths.js";
import { eventually, freePort, Processes } from "./processes.js";

/**
 * A deployment as 06 describes it: one Postgres, blobs on S3 (MinIO), an OpenTelemetry
 * Collector, a `rewind server --role api` and two `rewind server --role worker`.
 */
export interface Deployment {
  /** The api server's URL. */
  url: string;
  /** The admin key, `--admin-key`, of the tenant `acme`. */
  adminKey: string;
  admin: Rewind;
  minio: Minio;
  collector: Collector;
  /** The workers' names: `service.instance.id` of their telemetry. */
  workers: string[];
  stop(): Promise<void>;
}

/** One `rewind server` with both roles, on Turso and local files in `dir`, over TCP. */
export async function startServer(dir: string): Promise<{ url: string; adminKey: string; admin: Rewind; stop(): Promise<void> }> {
  const processes = new Processes(dir);
  const adminKey = `rewind_sk_${randomBytes(24).toString("hex")}`;
  const port = await freePort();
  const url = `http://127.0.0.1:${port}`;
  const child = processes.start("server", rewindBinary(), [
    "server",
    "--store",
    `turso:${dir}/journal.db`,
    "--blobs",
    `${dir}/blobs`,
    "--master-key",
    randomBytes(32).toString("base64"),
    "--admin-key",
    adminKey,
    "--admin-tenant",
    "acme",
    "--listen",
    `127.0.0.1:${port}`,
  ]);
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
  return { url, adminKey, admin: new Rewind({ url, apiKey: adminKey }), stop: () => processes.stopAll() };
}

export async function startDeployment(dir: string, options: { workers?: number } = {}): Promise<Deployment> {
  const containers = new Containers();
  const processes = new Processes(dir);
  const stop = async () => {
    await processes.stopAll();
    containers.removeAll();
  };
  try {
    const [postgres, minio, collector] = await Promise.all([
      startPostgres(containers),
      startMinio(containers, "rewind-blobs"),
      startCollector(containers, dir),
    ]);
    const adminKey = `rewind_sk_${randomBytes(24).toString("hex")}`;
    const common = [
      "--store",
      postgres.url,
      "--blobs",
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
      "--otlp-endpoint",
      collector.http,
      "--otlp-protocol",
      "http",
      "--admin-tenant",
      "acme",
    ];
    const binary = rewindBinary();
    const server = async (name: string, role: "api" | "worker", extra: string[] = []) => {
      const port = await freePort();
      const child = processes.start(name, binary, [
        "server",
        "--role",
        role,
        "--worker-name",
        name,
        "--listen",
        `127.0.0.1:${port}`,
        ...common,
        ...extra,
      ]);
      const url = `http://127.0.0.1:${port}`;
      await eventually(`${name} listens`, async () => {
        if (child.exitCode !== null) throw new Error(`${name} exited with ${child.exitCode}: see ${dir}/${name}.log`);
        const response = await fetch(`${url}/v1/server`, { headers: { authorization: `Bearer ${adminKey}` } });
        return response.ok ? true : undefined;
      });
      return url;
    };
    // The api server registers the admin key and runs the migrations first.
    const url = await server("api", "api", ["--admin-key", adminKey]);
    const workers = Array.from({ length: options.workers ?? 2 }, (_, n) => `worker-${n + 1}`);
    for (const name of workers) {
      await server(name, "worker", ["--admin-key", adminKey]);
    }
    return {
      url,
      adminKey,
      admin: new Rewind({ url, apiKey: adminKey }),
      minio,
      collector,
      workers,
      stop,
    };
  } catch (error) {
    await stop();
    throw error;
  }
}
