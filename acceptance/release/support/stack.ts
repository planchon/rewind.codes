/**
 * The Rewind setups of the release gate (`acceptance/release/README.md`):
 *
 * - **A, deployed**: Postgres 17, blobs on MinIO, an OpenTelemetry Collector, one
 *   `rewind server --role api` and two `--role worker` behind an edge, as production runs: the edge
 *   sends what runs tests (`POST /v1/test`, `POST /v1/workflows`, `POST /v1/runs/{id}/fork`) to
 *   the workers and everything else to the api server
 *   (`deploy/edge/Caddyfile`). It lets MCP servers on
 *   127.0.0.1 through (`--allow-internal-tools`), and webhooks to private addresses, so that the
 *   gate's fixtures are reachable; stdio MCP servers too.
 * - **B, strict**: one server with both roles on Turso and local files, with a deployed server's
 *   defaults: no internal MCP server, no private webhook, no stdio, no WorkOS. Egress attacks
 *   are proven there.
 * - **D, lifecycle**: a deployed stack of its own, with the options a test gives it (short
 *   retention), whose servers the test kills and restarts and whose Postgres it pauses.
 *
 * The servers run the binary under test (`REWIND_BIN`, else the workspace's debug build).
 */
import { type ChildProcess, spawnSync } from "node:child_process";
import { randomBytes } from "node:crypto";
import { mkdirSync } from "node:fs";
import { createServer, type IncomingMessage, type Server as HttpServer, request as httpRequest, type ServerResponse } from "node:http";
import { join } from "node:path";
import { type Collector, Containers, type Minio, startCollector, startMinio, startPostgres } from "../../support/docker.js";
import { rewindBinary } from "../../support/paths.js";
import { eventually, freePort, Processes } from "../../support/processes.js";

/** The tenant of the operator key of every setup. */
export const OPERATOR_TENANT = "gate-operator";

/** One `rewind server` process of a setup. */
export interface ServerProcess {
  name: string;
  role: "api" | "worker" | "both";
  url: string;
  args: string[];
  child: ChildProcess;
}

/** What a setup publishes to the specs (`stack.json`): plain data, no handles. */
export interface StackInfo {
  name: string;
  /**
   * Where clients reach the setup: the edge of a deployed setup (`POST /v1/test` to the workers,
   * the rest to the api server), or the one server of the strict setup.
   */
  url: string;
  /** The api server itself, behind the edge. */
  apiUrl: string;
  /** The workers' own URLs, behind the edge. */
  workerUrls: string[];
  /** The operator's key (`--admin-key`), tenant `gate-operator`. */
  operatorKey: string;
  /** Where the servers' logs are. */
  logDir: string;
  /** The workers' names: `service.instance.id` of their telemetry. */
  workers: string[];
  /** Postgres's container, for the setups on Postgres. */
  postgres?: { container: string; url: string };
  minio?: { container: string; endpoint: string; user: string; password: string; bucket: string };
  /** The JSON lines the Collector writes, one export request per line. */
  collector?: { http: string; traces: string; logs: string; metrics: string };
  /** The directory of the setup on Turso: its database and blobs. */
  dataDir?: string;
}

export interface Stack {
  info: StackInfo;
  servers: Map<string, ServerProcess>;
  minio?: Minio;
  collector?: Collector;
  /** `kill -<signal>` of one server; resolves once it is gone. */
  kill(name: string, signal?: NodeJS.Signals): Promise<void>;
  /** Starts a server again with its arguments, and waits until it answers. */
  restart(name: string): Promise<void>;
  /** `docker pause` / `docker unpause` of Postgres. */
  pausePostgres(): void;
  unpausePostgres(): void;
  stop(): Promise<void>;
}

export interface DeployedOptions {
  /** The setup's name, in its logs' directory and its containers' names. */
  name: string;
  /** Where its logs and the Collector's output go. */
  dir: string;
  workers?: number;
  /** Arguments every server of the setup gets. */
  extraArgs?: string[];
  /** Arguments the api server alone gets. */
  apiArgs?: string[];
}

/** A deployed setup: Postgres, MinIO, a Collector, an api server and its workers. */
export async function startDeployed(options: DeployedOptions): Promise<Stack> {
  const { dir } = options;
  mkdirSync(dir, { recursive: true });
  const containers = new Containers();
  const processes = new Processes(dir);
  const servers = new Map<string, ServerProcess>();
  let postgresContainer = "";
  let edge: HttpServer | undefined;
  const stop = async () => {
    if (postgresContainer !== "") spawnSync("docker", ["unpause", postgresContainer], { encoding: "utf8" });
    if (edge) {
      edge.closeAllConnections();
      await new Promise<void>((resolve) => edge!.close(() => resolve()));
    }
    await processes.stopAll();
    containers.removeAll();
  };
  try {
    const [postgres, minio, collector] = await Promise.all([
      startPostgres(containers),
      startMinio(containers, "rewind-blobs"),
      startCollector(containers, dir),
    ]);
    postgresContainer = postgres.name;
    const operatorKey = `rewind_sk_${randomBytes(24).toString("hex")}`;
    const apiPort = await freePort();
    const edgePort = await freePort();
    // Clients and the webhooks scripts hand out reach the setup at its edge.
    const url = `http://127.0.0.1:${edgePort}`;
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
      OPERATOR_TENANT,
      "--public-url",
      url,
      ...(options.extraArgs ?? []),
    ];
    const start = async (name: string, role: "api" | "worker", port: number, extra: string[]) => {
      const args = [
        "server",
        "--role",
        role,
        "--worker-name",
        name,
        "--listen",
        `127.0.0.1:${port}`,
        ...common,
        "--admin-key",
        operatorKey,
        ...extra,
      ];
      const server: ServerProcess = { name, role, url: `http://127.0.0.1:${port}`, args, child: processes.start(name, rewindBinary(), args) };
      servers.set(name, server);
      await answers(server, operatorKey, dir);
    };
    // The api server runs the migrations and registers the operator key first.
    await start("api", "api", apiPort, options.apiArgs ?? []);
    const workers = Array.from({ length: options.workers ?? 2 }, (_, n) => `worker-${n + 1}`);
    for (const name of workers) await start(name, "worker", await freePort(), []);
    edge = await startEdge(edgePort, servers);
    const info: StackInfo = {
      name: options.name,
      url,
      apiUrl: servers.get("api")!.url,
      workerUrls: workers.map((name) => servers.get(name)!.url),
      operatorKey,
      logDir: dir,
      workers,
      postgres: { container: postgres.name, url: postgres.url },
      minio: {
        container: minioContainer(minio),
        endpoint: minio.endpoint,
        user: minio.user,
        password: minio.password,
        bucket: minio.bucket,
      },
      collector: {
        http: collector.http,
        traces: join(dir, "out", "traces.jsonl"),
        logs: join(dir, "out", "logs.jsonl"),
        metrics: join(dir, "out", "metrics.jsonl"),
      },
    };
    return handle(info, servers, processes, stop, operatorKey, dir, postgresContainer, minio, collector);
  } catch (error) {
    await stop();
    throw error;
  }
}

/** The strict setup: one server, both roles, Turso, a deployed server's defaults. */
export async function startStrict(options: { name: string; dir: string; extraArgs?: string[] }): Promise<Stack> {
  const { dir } = options;
  mkdirSync(dir, { recursive: true });
  const processes = new Processes(dir);
  const servers = new Map<string, ServerProcess>();
  const operatorKey = `rewind_sk_${randomBytes(24).toString("hex")}`;
  const port = await freePort();
  const url = `http://127.0.0.1:${port}`;
  const dataDir = join(dir, "data");
  mkdirSync(dataDir, { recursive: true });
  const args = [
    "server",
    "--store",
    `turso:${dataDir}/journal.db`,
    "--blobs",
    `${dataDir}/blobs`,
    "--master-key",
    randomBytes(32).toString("base64"),
    "--admin-key",
    operatorKey,
    "--admin-tenant",
    OPERATOR_TENANT,
    "--public-url",
    url,
    "--worker-name",
    "strict",
    "--listen",
    `127.0.0.1:${port}`,
    ...(options.extraArgs ?? []),
  ];
  const server: ServerProcess = { name: "strict", role: "both", url, args, child: processes.start("strict", rewindBinary(), args) };
  servers.set("strict", server);
  const stop = () => processes.stopAll();
  try {
    await answers(server, operatorKey, dir);
  } catch (error) {
    await stop();
    throw error;
  }
  const info: StackInfo = { name: options.name, url, apiUrl: url, workerUrls: [url], operatorKey, logDir: dir, workers: ["strict"], dataDir };
  return handle(info, servers, processes, stop, operatorKey, dir, "");
}

function handle(
  info: StackInfo,
  servers: Map<string, ServerProcess>,
  processes: Processes,
  stop: () => Promise<void>,
  operatorKey: string,
  dir: string,
  postgresContainer: string,
  minio?: Minio,
  collector?: Collector,
): Stack {
  const get = (name: string) => {
    const server = servers.get(name);
    if (!server) throw new Error(`no server ${name} in ${info.name}`);
    return server;
  };
  return {
    info,
    servers,
    minio,
    collector,
    async kill(name, signal = "SIGKILL") {
      const server = get(name);
      if (server.child.exitCode !== null || server.child.signalCode !== null) return;
      const gone = new Promise<void>((resolve) => server.child.once("exit", () => resolve()));
      server.child.kill(signal);
      await gone;
    },
    async restart(name) {
      const server = get(name);
      if (server.child.exitCode === null && server.child.signalCode === null) {
        await processes.stop(server.child);
      }
      server.child = processes.start(name, rewindBinary(), server.args);
      await answers(server, operatorKey, dir);
    },
    pausePostgres() {
      if (postgresContainer === "") throw new Error(`${info.name} has no Postgres`);
      const paused = spawnSync("docker", ["pause", postgresContainer], { encoding: "utf8" });
      if (paused.status !== 0) throw new Error(`docker pause: ${paused.stderr}`);
    },
    unpausePostgres() {
      if (postgresContainer === "") throw new Error(`${info.name} has no Postgres`);
      const unpaused = spawnSync("docker", ["unpause", postgresContainer], { encoding: "utf8" });
      if (unpaused.status !== 0) throw new Error(`docker unpause: ${unpaused.stderr}`);
    },
    stop,
  };
}

/**
 * The edge of a deployed setup, as `deploy/edge/Caddyfile` routes: what runs tests (`POST /v1/test`,
 * `POST /v1/workflows`, `POST /v1/runs/{id}/fork`) goes to the workers in turn; everything else
 * goes to the api server, unbuffered (event streams, long polls). A server that is down answers
 * 502, as Caddy does.
 */
async function startEdge(port: number, servers: Map<string, ServerProcess>): Promise<HttpServer> {
  let turn = 0;
  const forward = (req: IncomingMessage, res: ServerResponse, expectContinue: boolean) => {
    const path = (req.url ?? "").split("?")[0] ?? "";
    const toWorkers = req.method === "POST" && /^\/v1\/(test|workflows|runs\/[^/]+\/fork)$/.test(path);
    const targets = toWorkers ? [...servers.values()].filter((s) => s.role === "worker") : [servers.get("api")!];
    const target = new URL(targets[turn++ % targets.length]!.url);
    const upstream = httpRequest(
      {
        host: target.hostname,
        port: target.port,
        method: req.method,
        path: req.url,
        headers: { ...req.headers, host: target.host },
        agent: false,
      },
      (answer) => {
        res.writeHead(answer.statusCode ?? 502, answer.headers);
        answer.pipe(res);
      },
    );
    upstream.on("error", () => {
      if (!res.headersSent) res.writeHead(502, { "content-type": "text/plain" });
      res.end("the edge cannot reach the server");
    });
    res.on("close", () => upstream.destroy());
    if (expectContinue) {
      // `Expect: 100-continue` goes to the server, which answers it: `100 Continue` is relayed and
      // the body follows; a final answer (a 413 from Content-Length) is relayed without the body.
      upstream.on("continue", () => {
        res.writeContinue();
        req.pipe(upstream);
      });
    } else {
      req.pipe(upstream);
    }
  };
  const edge = createServer((req, res) => forward(req, res, false));
  edge.on("checkContinue", (req, res) => forward(req, res, true));
  await new Promise<void>((resolve) => edge.listen(port, "127.0.0.1", resolve));
  return edge;
}

/** Waits until a server answers `GET /v1/server`, or fails if it exits first. */
async function answers(server: ServerProcess, operatorKey: string, dir: string): Promise<void> {
  await eventually(
    `${server.name} answers`,
    async () => {
      if (server.child.exitCode !== null) {
        throw new Error(`${server.name} exited with ${server.child.exitCode}: see ${dir}/${server.name}.log`);
      }
      const response = await fetch(`${server.url}/v1/server`, { headers: { authorization: `Bearer ${operatorKey}` } });
      return response.ok ? true : undefined;
    },
    120_000,
    200,
  );
}

/** The MinIO container's name, from the `docker port` lookup `startMinio` made. */
function minioContainer(minio: Minio): string {
  const port = minio.endpoint.split(":").pop();
  const found = spawnSync("docker", ["ps", "--filter", "label=rewind-acceptance", "--filter", `publish=${port}`, "--format", "{{.Names}}"], {
    encoding: "utf8",
  });
  return found.stdout.trim().split("\n")[0] ?? "";
}
