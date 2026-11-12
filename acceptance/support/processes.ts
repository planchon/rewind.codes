import { type ChildProcess, spawn } from "node:child_process";
import { createWriteStream } from "node:fs";
import { createServer } from "node:net";
import { join } from "node:path";
import { setTimeout as delay } from "node:timers/promises";

/** Long-running processes of a test (servers), their output in log files, stopped at the end. */
export class Processes {
  private readonly children: { name: string; child: ChildProcess }[] = [];

  constructor(private readonly logDir: string) {}

  /** Starts `command`; its standard output and error go to `<logDir>/<name>.log`. */
  start(name: string, command: string, args: string[], options: { env?: NodeJS.ProcessEnv; cwd?: string } = {}) {
    const log = createWriteStream(join(this.logDir, `${name}.log`), { flags: "a" });
    const child = spawn(command, args, {
      cwd: options.cwd,
      env: { ...process.env, ...options.env },
      stdio: ["ignore", "pipe", "pipe"],
    });
    child.stdout?.pipe(log, { end: false });
    child.stderr?.pipe(log, { end: false });
    child.on("exit", (code, signal) => log.end(`\n[exited: code ${code}, signal ${signal}]\n`));
    this.children.push({ name, child });
    return child;
  }

  /** Stops one process: SIGTERM, then SIGKILL after `graceMs`. */
  async stop(child: ChildProcess, graceMs = 10_000): Promise<void> {
    if (child.exitCode !== null || child.signalCode !== null) return;
    const exited = new Promise<void>((resolve) => child.once("exit", () => resolve()));
    child.kill("SIGTERM");
    const timer = setTimeout(() => child.kill("SIGKILL"), graceMs);
    await exited;
    clearTimeout(timer);
  }

  /** Stops every process, the last started first. */
  async stopAll(): Promise<void> {
    for (const { child } of [...this.children].reverse()) {
      await this.stop(child);
    }
  }
}

/** A TCP port free on 127.0.0.1 right now. */
export async function freePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const server = createServer();
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const address = server.address();
      const port = typeof address === "object" && address !== null ? address.port : 0;
      server.close(() => resolve(port));
    });
  });
}

/** Calls `check` until it returns a value other than `undefined`, for up to `timeoutMs`. */
export async function eventually<T>(
  what: string,
  check: () => Promise<T | undefined> | T | undefined,
  timeoutMs = 60_000,
  intervalMs = 250,
): Promise<T> {
  const deadline = Date.now() + timeoutMs;
  let last: unknown;
  for (;;) {
    try {
      const value = await check();
      if (value !== undefined) return value;
    } catch (error) {
      last = error;
    }
    if (Date.now() > deadline) {
      throw new Error(`timed out after ${timeoutMs} ms waiting until ${what}${last ? `: ${String(last)}` : ""}`);
    }
    await delay(intervalMs);
  }
}
