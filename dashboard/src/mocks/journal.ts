// Builds journal entries as the API serves them, for the mocked API and the tests.
import type {
  AbortReason,
  CancelReason,
  Entry,
  JournalEvent,
  LogLevel,
  Payload,
  RunMode,
  ToolFailureKind,
} from "../api/types";

/** Above this size the API leaves a payload's value out (`PAYLOAD_VALUE_LIMIT`). */
export const PAYLOAD_VALUE_LIMIT = 256 * 1024;
const PREVIEW_CHARS = 120;

/** JSON with sorted keys: close enough to RFC 8785 for mocked payloads. */
export function canonicalJson(value: unknown): string {
  if (value === null || typeof value !== "object") return JSON.stringify(value) ?? "null";
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(",")}]`;
  const keys = Object.keys(value).sort();
  const record = value as Record<string, unknown>;
  return `{${keys.map((k) => `${JSON.stringify(k)}:${canonicalJson(record[k])}`).join(",")}}`;
}

/** A fake SHA-256: 64 hex digits that depend on every character of the input. */
export function fakeDigest(text: string): string {
  let out = "";
  for (let round = 0; round < 8; round += 1) {
    let h = 0x811c9dc5 ^ round;
    for (let i = 0; i < text.length; i += 1) {
      h ^= text.charCodeAt(i);
      h = Math.imul(h, 0x01000193) >>> 0;
    }
    h ^= h >>> 13;
    h = Math.imul(h, 0x5bd1e995) >>> 0;
    out += (h >>> 0).toString(16).padStart(8, "0");
  }
  return out;
}

export function payload(value: unknown, limit = PAYLOAD_VALUE_LIMIT): Payload {
  const canonical = canonicalJson(value);
  const bytes = new TextEncoder().encode(canonical).length;
  const chars = [...canonical];
  const preview =
    chars.length > PREVIEW_CHARS ? `${chars.slice(0, PREVIEW_CHARS).join("")}…` : canonical;
  const p: Payload = { digest: fakeDigest(canonical), bytes, preview };
  if (bytes <= limit) p.value = value;
  return p;
}

export interface StartOptions {
  script: string;
  toolset: string;
  input?: unknown;
  mode?: RunMode;
  epoch?: number;
  ttlMs?: number;
}

/** Appends entries to a journal, with a clock that moves forward with `after`. */
export class JournalBuilder {
  readonly entries: Entry[] = [];
  /** Every payload written, by digest, for `GET /v1/payloads/{digest}`. */
  readonly payloads = new Map<string, unknown>();
  clock: number;

  constructor(start: number) {
    this.clock = start;
  }

  /** Moves the clock forward. */
  after(ms: number): this {
    this.clock += ms;
    return this;
  }

  push(event: JournalEvent): this {
    this.entries.push({
      idx: this.entries.length,
      recordedAt: new Date(this.clock).toISOString(),
      event,
    });
    return this;
  }

  value(v: unknown): Payload {
    const p = payload(v);
    this.payloads.set(p.digest, v);
    return p;
  }

  started(o: StartOptions): this {
    return this.push({
      type: "run_started",
      script: o.script,
      toolset: o.toolset,
      input: this.value(o.input ?? {}),
      mode: o.mode ?? "live",
      epoch: o.epoch ?? 1,
      expiresAt: new Date(this.clock + (o.ttlMs ?? 30 * 86_400_000)).toISOString(),
    });
  }

  call(seq: number, tool: string, args: unknown, line: number, column = 3): this {
    return this.push({
      type: "schedule_tool_call",
      seq,
      tool,
      args: this.value(args),
      callsite: { line, column },
    });
  }

  ok(seq: number, result: unknown): this {
    return this.push({ type: "tool_call_completed", seq, result: this.value(result) });
  }

  fail(seq: number, kind: ToolFailureKind, message: string, data?: unknown): this {
    return this.push({
      type: "tool_call_failed",
      seq,
      error: data === undefined ? { kind, message } : { kind, message, data },
    });
  }

  timer(seq: number, durationMs: number, line: number | null, column = 3): this {
    return this.push({
      type: "start_timer",
      seq,
      durationMs,
      fireAt: new Date(this.clock + durationMs).toISOString(),
      callsite: line === null ? null : { line, column },
    });
  }

  fired(seq: number): this {
    return this.push({ type: "timer_fired", seq });
  }

  cancelTimer(seq: number): this {
    return this.push({ type: "cancel_timer", seq });
  }

  awaitSignal(name: string, line: number, timeoutSeq?: number, column = 3): this {
    return this.push({
      type: "signal_awaited",
      name,
      callsite: { line, column },
      timeout: timeoutSeq ?? null,
    });
  }

  signal(name: string, value: unknown): this {
    return this.push({ type: "signal_received", name, payload: this.value(value) });
  }

  log(level: LogLevel, message: string, line: number | null, column = 3): this {
    return this.push({
      type: "log",
      level,
      message,
      callsite: line === null ? null : { line, column },
    });
  }

  complete(output: unknown): this {
    return this.push({ type: "complete_run", output: this.value(output) });
  }

  failRun(name: string, message: string, stack?: string): this {
    return this.push({
      type: "fail_run",
      error: stack === undefined ? { name, message } : { name, message, stack },
    });
  }

  abort(kind: AbortReason["kind"], message: string): this {
    return this.push({ type: "run_aborted", reason: { kind, message } });
  }

  cancelRequested(reason: CancelReason, message?: string): this {
    return this.push({ type: "cancel_requested", reason, message: message ?? null });
  }
}
