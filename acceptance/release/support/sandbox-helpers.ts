/**
 * Helpers of the sandbox, CLI and lifecycle specs of the release gate: start a run and wait for
 * its end through the recorded `Api`, read its export, replay an export with the CLI, and read
 * the Collector's spans.
 */
import { spawnSync } from "node:child_process";
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { flattenSpans, type SpanRecord } from "../../support/otlp.js";
import { rewindBinary, shortTempDir } from "../../support/paths.js";
import { eventually } from "../../support/processes.js";
import { type Api, expect, expectOk } from "./gate.js";

/** A run as `GET /v1/runs/{id}` answers it. */
export type RunJson = Record<string, any> & { id: string; status: string };

const ENDED = new Set(["completed", "failed", "cancelled", "aborted"]);

/**
 * Starts a run of `script` in `namespace`, without the type-check by default (these specs test
 * the runtime, not tsgo), and waits until it ends or waits for the outside world.
 */
export async function runScript(
  api: Api,
  script: string,
  options: { namespace?: string; input?: unknown; actionSet?: string; check?: boolean; timeoutMs?: number; extra?: Record<string, unknown> } = {},
): Promise<RunJson> {
  const started = await api.ns(options.namespace ?? "sandbox").post("/v1/runs", {
    code: script,
    input: options.input,
    actionSet: options.actionSet,
    check: options.check ?? false,
    wait: "30s",
    ...options.extra,
  });
  expectOk(started);
  return settled(api, started.json, options.timeoutMs);
}

/** Waits until the run ends, or waits for a signal, a tool or a timer. */
export async function settled(api: Api, run: RunJson, timeoutMs = 120_000): Promise<RunJson> {
  if (ENDED.has(run.status) || (run.waitingFor?.length ?? 0) > 0) return run;
  return eventually(
    `run ${run.id} ends or waits`,
    async () => {
      const reply = await api.get(`/v1/runs/${run.id}`, { query: { wait: "10s" } });
      expectOk(reply);
      const state = reply.json as RunJson;
      return ENDED.has(state.status) || (state.waitingFor?.length ?? 0) > 0 ? state : undefined;
    },
    timeoutMs,
    100,
  );
}

/** Waits until the run has ended. */
export async function ended(api: Api, id: string, timeoutMs = 120_000): Promise<RunJson> {
  return eventually(
    `run ${id} ends`,
    async () => {
      const reply = await api.get(`/v1/runs/${id}`, { query: { wait: "10s" } });
      expectOk(reply);
      return ENDED.has(reply.json.status) ? (reply.json as RunJson) : undefined;
    },
    timeoutMs,
    100,
  );
}

/** Asserts the run was aborted for a limit whose message contains `message`. */
export function expectLimit(run: RunJson, message: string): void {
  const context = JSON.stringify({ status: run.status, abort: run.abort, error: run.error });
  expect(run.status, context).toBe("aborted");
  expect(run.abort?.kind, context).toBe("limit_exceeded");
  expect(String(run.abort?.message), context).toContain(message);
}

/** The run's export (`GET /v1/runs/{id}/export`). */
export async function journal(api: Api, id: string): Promise<{ text: string; doc: any }> {
  const reply = await api.get(`/v1/runs/${id}/export`);
  expectOk(reply);
  return { text: reply.text, doc: reply.json };
}

/** The `type` of each event of a run export. */
export function eventTypes(doc: any): string[] {
  return (doc.events as { event: { type: string } }[]).map((event) => event.event.type);
}

/** `rewind [--json] replay <file>` of a run export, in a directory without a server. */
export function replayDocument(document: string, args: string[] = []): { code: number; stdout: string; stderr: string } {
  const dir = shortTempDir("rewind-gate-replay");
  const file = join(dir, "journal.json");
  writeFileSync(file, document);
  const env = { ...process.env };
  delete env.REWIND_URL;
  delete env.REWIND_API_KEY;
  delete env.REWIND_DIR;
  const ran = spawnSync(rewindBinary(), [...args, "replay", file], { cwd: dir, env, encoding: "utf8", timeout: 120_000 });
  return { code: ran.status ?? -1, stdout: ran.stdout, stderr: ran.stderr };
}

/** The spans the Collector wrote to `file` (one export request per line). */
export function collectorSpans(file: string): SpanRecord[] {
  if (!existsSync(file)) return [];
  const lines = readFileSync(file, "utf8").split("\n");
  // The Collector may be writing its last line: only the lines it ended are read.
  lines.pop();
  const exports = lines.filter((line) => line.trim() !== "").map((line) => JSON.parse(line) as unknown);
  return flattenSpans(exports);
}
