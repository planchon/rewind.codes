import { type ChildProcess, spawn, spawnSync } from "node:child_process";
import { rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { Rewind } from "@rewind.codes/sdk";
import { Agent, fetch as undiciFetch } from "undici";
import { rewindBinary, shortTempDir } from "./paths.js";
import { eventually } from "./processes.js";

/** What a `rewind` command did. */
export interface Ran {
  code: number;
  stdout: string;
  stderr: string;
}

/**
 * A project directory, as a developer has one: scripts, action set files, and the `.rewind/` of the
 * embedded server the CLI starts on demand. Its servers are killed and the directory removed at
 * the end.
 */
export class Project {
  readonly root: string;
  readonly state: string;
  private readonly env: NodeJS.ProcessEnv;

  constructor(prefix: string) {
    this.root = shortTempDir(prefix);
    this.state = join(this.root, ".rewind");
    const env: NodeJS.ProcessEnv = { ...process.env };
    delete env.REWIND_URL;
    delete env.REWIND_DIR;
    delete env.REWIND_API_KEY;
    this.env = env;
  }

  path(name: string): string {
    return join(this.root, name);
  }

  write(name: string, text: string): string {
    const path = this.path(name);
    writeFileSync(path, text);
    return path;
  }

  /** `rewind <args…>` started in the project directory. */
  spawn(args: string[]): ChildProcess {
    return spawn(rewindBinary(), args, { cwd: this.root, env: this.env, stdio: ["ignore", "pipe", "pipe"] });
  }

  /** `rewind <args…>` to its end. */
  rewind(args: string[], timeoutMs = 180_000): Promise<Ran> {
    return finish(this.spawn(args), timeoutMs, args);
  }

  /** `rewind --json <args…>`, its standard output parsed. */
  async json(args: string[]): Promise<{ code: number; value: any; ran: Ran }> {
    const ran = await this.rewind(["--json", ...args]);
    try {
      return { code: ran.code, value: JSON.parse(ran.stdout), ran };
    } catch {
      throw new Error(`rewind --json ${args.join(" ")}: not JSON (exit ${ran.code})\nstdout: ${ran.stdout}\nstderr: ${ran.stderr}`);
    }
  }

  /** The pids of this project's embedded servers. */
  servers(): number[] {
    const found = spawnSync("pgrep", ["-f", `server --embedded --dir ${this.state}`], { encoding: "utf8" });
    return found.stdout
      .split("\n")
      .map((line) => Number.parseInt(line.trim(), 10))
      .filter((pid) => Number.isInteger(pid));
  }

  /** `kill -9` of the embedded servers; returns their pids once they are gone. */
  async killServers(): Promise<number[]> {
    const pids = this.servers();
    for (const pid of pids) {
      try {
        process.kill(pid, "SIGKILL");
      } catch {
        // Already gone.
      }
    }
    await eventually("the killed servers are gone", () => (pids.every((pid) => !alive(pid)) ? true : undefined), 20_000, 50);
    return pids;
  }

  /**
   * The SDK's client over the embedded server's unix socket (no key: the socket is the
   * credential). The server must be running.
   */
  client(): Rewind {
    const dispatcher = new Agent({ connect: { socketPath: join(this.state, "server.sock") } });
    const viaSocket = async (request: Request): Promise<Response> => {
      const body = request.body === null ? undefined : await request.arrayBuffer();
      const response = await undiciFetch(request.url, {
        method: request.method,
        headers: [...request.headers],
        body,
        signal: request.signal,
        dispatcher,
      });
      return response as unknown as Response;
    };
    return new Rewind({ url: "http://localhost", apiKey: "", fetch: viaSocket as typeof fetch });
  }

  async dispose(): Promise<void> {
    await this.killServers();
    rmSync(this.root, { recursive: true, force: true });
  }
}

function alive(pid: number): boolean {
  const ps = spawnSync("ps", ["-o", "stat=", "-p", String(pid)], { encoding: "utf8" });
  const stat = ps.stdout.trim();
  return stat !== "" && !stat.startsWith("Z");
}

/** Collects a command's output until it exits. */
export function finish(child: ChildProcess, timeoutMs: number, args: string[] = []): Promise<Ran> {
  return new Promise((resolve, reject) => {
    let stdout = "";
    let stderr = "";
    child.stdout?.setEncoding("utf8").on("data", (chunk: string) => (stdout += chunk));
    child.stderr?.setEncoding("utf8").on("data", (chunk: string) => (stderr += chunk));
    const timer = setTimeout(() => {
      child.kill("SIGKILL");
      reject(new Error(`rewind ${args.join(" ")} did not end in ${timeoutMs} ms\nstdout: ${stdout}\nstderr: ${stderr}`));
    }, timeoutMs);
    child.on("error", reject);
    child.on("close", (code) => {
      clearTimeout(timer);
      resolve({ code: code ?? -1, stdout, stderr });
    });
  });
}
