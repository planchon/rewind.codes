import { execFileSync, spawnSync } from "node:child_process";
import { chmodSync, existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { request } from "node:http";
import { join } from "node:path";
import { eventually } from "./processes.js";

/** The label of every container the suite starts: `run.sh` removes leftovers by it. */
export const LABEL = "rewind-acceptance";

export const IMAGES = {
  postgres: "postgres:17",
  minio: "cgr.dev/chainguard/minio:latest",
  mc: "cgr.dev/chainguard/minio-client:latest",
  collector: "otel/opentelemetry-collector:0.165.0",
};

function docker(args: string[]): string {
  const result = spawnSync("docker", args, { encoding: "utf8" });
  if (result.status !== 0) {
    throw new Error(`docker ${args.join(" ")}: ${result.stderr || result.error}`);
  }
  return result.stdout;
}

/** The containers of a test, removed at the end. */
export class Containers {
  private readonly names: string[] = [];
  private static count = 0;

  /** Starts a detached container; returns its name. */
  run(role: string, args: string[]): string {
    Containers.count += 1;
    const name = `rewind-acceptance-${role}-${process.pid}-${Date.now()}-${Containers.count}`;
    docker(["run", "--detach", "--rm", "--label", LABEL, "--name", name, ...args]);
    this.names.push(name);
    return name;
  }

  /** `127.0.0.1:<port>`: where a container's port is published. */
  port(name: string, port: string): string {
    const line = docker(["port", name, port]).split("\n")[0];
    if (!line) throw new Error(`${name} publishes no ${port}`);
    return line.trim();
  }

  logs(name: string): string {
    const result = spawnSync("docker", ["logs", name], { encoding: "utf8" });
    return `${result.stdout}${result.stderr}`;
  }

  removeAll(): void {
    for (const name of this.names.splice(0)) {
      spawnSync("docker", ["rm", "--force", name], { encoding: "utf8" });
    }
  }
}

/** Postgres 17 on a tmpfs. */
export async function startPostgres(containers: Containers): Promise<{ url: string; name: string }> {
  const name = containers.run("postgres", [
    "--publish",
    "127.0.0.1::5432",
    "--tmpfs",
    "/var/lib/postgresql/data",
    "--env",
    "POSTGRES_USER=rewind",
    "--env",
    "POSTGRES_PASSWORD=rewind-secret",
    IMAGES.postgres,
  ]);
  const address = containers.port(name, "5432/tcp");
  await eventually(
    "Postgres accepts connections",
    () => {
      const ready = spawnSync(
        "docker",
        ["exec", name, "psql", "-h", "127.0.0.1", "-U", "rewind", "-d", "postgres", "-tAc", "SELECT 1"],
        { encoding: "utf8" },
      );
      return ready.status === 0 ? true : undefined;
    },
    60_000,
    300,
  );
  return { url: `postgres://rewind:rewind-secret@${address}/postgres`, name };
}

export interface Minio {
  endpoint: string;
  user: string;
  password: string;
  bucket: string;
  /** The objects of the bucket, recursively. */
  objects(): string[];
}

/** MinIO on a tmpfs, with one bucket. */
export async function startMinio(containers: Containers, bucket: string): Promise<Minio> {
  const user = "rewind-acceptance";
  const password = "rewind-acceptance-secret";
  const name = containers.run("minio", [
    "--publish",
    "127.0.0.1::9000",
    "--tmpfs",
    "/data",
    "--env",
    `MINIO_ROOT_USER=${user}`,
    "--env",
    `MINIO_ROOT_PASSWORD=${password}`,
    IMAGES.minio,
    "server",
    "/data",
  ]);
  const address = containers.port(name, "9000/tcp");
  await eventually("MinIO is live", () => httpStatus(`http://${address}/minio/health/live`).then((s) => (s === 200 ? true : undefined)));
  const mc = (args: string[]) =>
    docker([
      "run",
      "--rm",
      "--label",
      LABEL,
      "--network",
      `container:${name}`,
      "--env",
      `MC_HOST_rewind=http://${user}:${password}@127.0.0.1:9000`,
      IMAGES.mc,
      ...args,
    ]);
  mc(["mb", `rewind/${bucket}`]);
  return {
    endpoint: `http://${address}`,
    user,
    password,
    bucket,
    objects: () =>
      mc(["ls", "--recursive", `rewind/${bucket}`])
        .split("\n")
        .map((line) => line.trim().split(/\s+/).pop() ?? "")
        .filter((key) => key !== ""),
  };
}

const COLLECTOR_CONFIG = `receivers:
  otlp:
    protocols:
      grpc:
        endpoint: 0.0.0.0:4317
      http:
        endpoint: 0.0.0.0:4318
exporters:
  file/traces:
    path: /out/traces.jsonl
    flush_interval: 100ms
  file/logs:
    path: /out/logs.jsonl
    flush_interval: 100ms
  file/metrics:
    path: /out/metrics.jsonl
    flush_interval: 100ms
service:
  pipelines:
    traces:
      receivers: [otlp]
      exporters: [file/traces]
    logs:
      receivers: [otlp]
      exporters: [file/logs]
    metrics:
      receivers: [otlp]
      exporters: [file/metrics]
`;

export interface Collector {
  /** OTLP over HTTP, `http://127.0.0.1:<port>`. */
  http: string;
  /** The JSON lines the collector wrote, one export request per line. */
  traces(): unknown[];
}

/** An OpenTelemetry Collector that writes what it receives to files in `dir/out`. */
export async function startCollector(containers: Containers, dir: string): Promise<Collector> {
  const out = join(dir, "out");
  mkdirSync(out, { recursive: true });
  // The collector writes as its own user.
  chmodSync(out, 0o777);
  const config = join(dir, "collector.yaml");
  writeFileSync(config, COLLECTOR_CONFIG);
  const name = containers.run("collector", [
    "--publish",
    "127.0.0.1::4318",
    "--volume",
    `${config}:/etc/otelcol/config.yaml:ro`,
    "--volume",
    `${out}:/out`,
    IMAGES.collector,
    "--config",
    "/etc/otelcol/config.yaml",
  ]);
  await eventually("the collector is ready", () => (containers.logs(name).includes("Everything is ready") ? true : undefined));
  const file = join(out, "traces.jsonl");
  return {
    http: `http://${containers.port(name, "4318/tcp")}`,
    traces: () =>
      existsSync(file)
        ? readFileSync(file, "utf8")
            .split("\n")
            .filter((line) => line.trim() !== "")
            .map((line) => JSON.parse(line) as unknown)
        : [],
  };
}

function httpStatus(url: string): Promise<number> {
  return new Promise((resolve) => {
    const req = request(url, { method: "GET", timeout: 2000 }, (res) => {
      res.resume();
      resolve(res.statusCode ?? 0);
    });
    req.on("error", () => resolve(0));
    req.on("timeout", () => {
      req.destroy();
      resolve(0);
    });
    req.end();
  });
}

/** Whether Docker answers. */
export function dockerAvailable(): boolean {
  try {
    execFileSync("docker", ["version", "--format", "{{.Server.Version}}"], { stdio: "ignore" });
    return true;
  } catch {
    return false;
  }
}
