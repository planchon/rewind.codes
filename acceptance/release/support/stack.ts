/**
 * The Rewind setups of the release gate (`acceptance/release/README.md`):
 *
 * - **A, deployed**: Postgres 17 for the control plane, a bucket on MinIO for the partitions, the
 *   archive and the blobs, an OpenTelemetry Collector, and two nodes (`rewind server`, each an
 *   HTTP edge and a node) linked by the node link, behind an edge that spreads the requests over
 *   them, as `deploy/edge/Caddyfile` does: every node answers every route, and reaches the owner
 *   of a run's partition through the link. It lets MCP servers on 127.0.0.1 through
 *   (`--allow-internal-addresses`), and webhooks to private addresses, so that the gate's fixtures
 *   are reachable; stdio MCP servers too.
 * - **B, strict**: one node on Turso and local files, with a deployed server's defaults: no
 *   internal MCP server, no private webhook, no stdio, no WorkOS. Egress attacks are proven there.
 * - **D, lifecycle**: a deployed stack of its own, with the options a test gives it (a short
 *   retention), whose nodes the test kills and restarts and whose Postgres it pauses.
 *
 * The servers run the binary under test (`REWIND_BIN`, else the workspace's debug build).
 */
import { type ChildProcess, spawnSync } from "node:child_process";
import { randomBytes } from "node:crypto";
import { mkdirSync } from "node:fs";
import { createServer, type IncomingMessage, type Server as HttpServer, request as httpRequest, type ServerResponse } from "node:http";
import { join } from "node:path";
import { CONTROL_DATABASE, type Collector, Containers, type Minio, startCollector, startMinio, startPostgres } from "../../support/docker.js";
import { rewindBinary } from "../../support/paths.js";
import { eventually, freePort, Processes } from "../../support/processes.js";

/** The tenant of the operator key of every setup. */
export const OPERATOR_TENANT = "gate-operator";

/** One `rewind server` process of a setup: a node, with its HTTP edge. */
export interface ServerProcess {
  /** Its node id (`--node-id`), and the name of its log. */
  name: string;
  /** Its own HTTP edge. */
  url: string;
  args: string[];
  child: ChildProcess;
}

/** What a setup publishes to the specs (`stack.json`): plain data, no handles. */
export interface StackInfo {
  name: string;
  /**
   * Where clients reach the setup: the edge in front of the nodes of a deployed setup, or the one
   * node of the strict setup.
   */
  url: string;
  /** Each node's own HTTP edge, behind the setup's edge, in the order of `nodes`. */
  nodeUrls: string[];
  /** The operator's key (`--admin-key`), tenant `gate-operator`. */
  operatorKey: string;
  /** Where the servers' logs are. */
  logDir: string;
  /** The nodes' ids (`--node-id`): `service.instance.id` of their telemetry. */
  nodes: string[];
  /** Postgres's container and the control plane's database, for the setups on Postgres. */
  postgres?: { container: string; url: string; database: string };
  minio?: { container: string; endpoint: string; user: string; password: string; bucket: string };
  /** The JSON lines the Collector writes, one export request per line. */
  collector?: { http: string; traces: string; logs: string; metrics: string };
  /** The directory of the setup on Turso: its control plane and its bucket. */
  dataDir?: string;
}

export interface Stack {
  info: StackInfo;
  servers: Map<string, ServerProcess>;
  minio?: Minio;
  collector?: Collector;
  /** `kill -<signal>` of one node; resolves once it is gone. */
  kill(name: string, signal?: NodeJS.Signals): Promise<void>;
  /** Starts a node again with its arguments, and waits until it answers. */
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
  /** The nodes of the setup: 2 by default. */
  nodes?: number;
  /** Arguments every node of the setup gets. */
  extraArgs?: string[];
}

/** A deployed setup: Postgres, MinIO, a Collector, and its nodes behind an edge. */
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
      startMinio(containers, "rewind"),
      startCollector(containers, dir),
    ]);
    postgresContainer = postgres.name;
    const operatorKey = `rewind_sk_${randomBytes(24).toString("hex")}`;
    const edgePort = await freePort();
    // Clients and the webhooks scripts hand out reach the setup at its edge.
    const url = `http://127.0.0.1:${edgePort}`;
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
      operatorKey,
      "--admin-organization",
      OPERATOR_TENANT,
      "--public-url",
      url,
      ...(options.extraArgs ?? []),
    ];
    const nodes = Array.from({ length: options.nodes ?? 2 }, (_, n) => `node-${n + 1}`);
    // One at a time: the first opens the cluster and its partitions, each next one takes its share.
    for (const name of nodes) {
      const port = await freePort();
      const link = await freePort();
      const args = [
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
      ];
      const server: ServerProcess = { name, url: `http://127.0.0.1:${port}`, args, child: processes.start(name, rewindBinary(), args) };
      servers.set(name, server);
      await answers(server, operatorKey, dir);
    }
    await clustered([...servers.values()], operatorKey);
    for (const server of servers.values()) await routable(server, operatorKey);
    edge = await startEdge(edgePort, servers);
    const info: StackInfo = {
      name: options.name,
      url,
      nodeUrls: nodes.map((name) => servers.get(name)!.url),
      operatorKey,
      logDir: dir,
      nodes,
      postgres: { container: postgres.name, url: postgres.url, database: CONTROL_DATABASE },
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

/** The strict setup: one node on Turso and local files, a deployed server's defaults. */
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
    `turso:${dataDir}/control.db`,
    "--bucket",
    `${dataDir}/bucket`,
    "--master-key",
    randomBytes(32).toString("base64"),
    "--admin-key",
    operatorKey,
    "--admin-organization",
    OPERATOR_TENANT,
    "--public-url",
    url,
    "--node-id",
    "strict",
    "--listen",
    `127.0.0.1:${port}`,
    ...(options.extraArgs ?? []),
  ];
  const server: ServerProcess = { name: "strict", url, args, child: processes.start("strict", rewindBinary(), args) };
  servers.set("strict", server);
  const stop = () => processes.stopAll();
  try {
    await answers(server, operatorKey, dir);
  } catch (error) {
    await stop();
    throw error;
  }
  const info: StackInfo = { name: options.name, url, nodeUrls: [url], operatorKey, logDir: dir, nodes: ["strict"], dataDir };
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
    if (!server) throw new Error(`no node ${name} in ${info.name}`);
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
      await routable(server, operatorKey);
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
 * The edge of a deployed setup, as `deploy/edge/Caddyfile` routes: each request goes to the next
 * node in turn, unbuffered (event streams). A node that refuses the connection (killed, or
 * starting again) leaves the request to the next one; when every node refuses it, the edge
 * answers 502, as Caddy does. A body is read whole first, so that it can go to another node; a
 * request with `Expect: 100-continue` goes to one node, which answers the expectation.
 */
async function startEdge(port: number, servers: Map<string, ServerProcess>): Promise<HttpServer> {
  let turn = 0;
  const send = (req: IncomingMessage, res: ServerResponse, body: Buffer | undefined, tried: number) => {
    const nodes = [...servers.values()];
    const target = new URL(nodes[turn++ % nodes.length]!.url);
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
        // A node that dies in the middle of an answer (an event stream, a held wait) cuts the
        // client's too, as Caddy does: a piped answer that never ends would hold its client.
        answer.on("close", () => {
          if (!answer.complete) res.destroy();
        });
      },
    );
    upstream.on("error", (error: NodeJS.ErrnoException) => {
      if (body !== undefined && error.code === "ECONNREFUSED" && tried + 1 < nodes.length && !res.headersSent) {
        send(req, res, body, tried + 1);
        return;
      }
      if (!res.headersSent) res.writeHead(502, { "content-type": "text/plain" });
      res.end("the edge cannot reach a node");
    });
    res.on("close", () => upstream.destroy());
    if (body === undefined) {
      // `Expect: 100-continue` goes to the node, which answers it: `100 Continue` is relayed and
      // the body follows; a final answer (a 413 from Content-Length) is relayed without the body.
      upstream.on("continue", () => {
        res.writeContinue();
        req.pipe(upstream);
      });
    } else {
      upstream.end(body);
    }
  };
  const edge = createServer((req, res) => {
    const chunks: Buffer[] = [];
    req.on("data", (chunk: Buffer) => chunks.push(chunk));
    req.on("end", () => send(req, res, Buffer.concat(chunks), 0));
  });
  edge.on("checkContinue", (req, res) => send(req, res, undefined, 0));
  await new Promise<void>((resolve) => edge.listen(port, "127.0.0.1", resolve));
  return edge;
}

/** Waits until a node answers `GET /v1/server`, or fails if it exits first. */
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

/**
 * Waits until a node reaches every partition: a list of runs asks each one, and is answered 503
 * while the node knows no owner of one (it has just started and not read the partition map yet).
 */
async function routable(server: ServerProcess, operatorKey: string): Promise<void> {
  await eventually(
    `${server.name} reaches every partition`,
    async () => {
      const response = await fetch(`${server.url}/v1/runs?limit=1`, { headers: { authorization: `Bearer ${operatorKey}` } });
      return response.ok ? true : undefined;
    },
    60_000,
    100,
  );
}

/** Waits until every node counts every other one in the deployment (`GET /v1/server`'s `nodes`). */
async function clustered(servers: ServerProcess[], operatorKey: string): Promise<void> {
  for (const server of servers) {
    await eventually(
      `${server.name} sees the ${servers.length} nodes`,
      async () => {
        const response = await fetch(`${server.url}/v1/server`, { headers: { authorization: `Bearer ${operatorKey}` } });
        const info = (await response.json()) as { node?: string; nodes?: number };
        return info.node === server.name && info.nodes === servers.length ? true : undefined;
      },
      60_000,
      200,
    );
  }
}

/** The MinIO container's name, from the `docker port` lookup `startMinio` made. */
function minioContainer(minio: Minio): string {
  const port = minio.endpoint.split(":").pop();
  const found = spawnSync("docker", ["ps", "--filter", "label=rewind-acceptance", "--filter", `publish=${port}`, "--format", "{{.Names}}"], {
    encoding: "utf8",
  });
  return found.stdout.trim().split("\n")[0] ?? "";
}
