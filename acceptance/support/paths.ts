import { existsSync, mkdirSync, mkdtempSync, realpathSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

/** The `acceptance/` directory. */
export const acceptance = resolve(dirname(fileURLToPath(import.meta.url)), "..");
/** The repository's root. */
export const root = resolve(acceptance, "..");
/** Logs, collector output and reports of the last run. Not committed. */
export const results = join(acceptance, "results");

/** The `rewind` binary under test: `REWIND_BIN`, or the workspace's debug build. */
export function rewindBinary(): string {
  const binary = process.env.REWIND_BIN ?? join(root, "target", "debug", "rewind");
  if (!existsSync(binary)) {
    throw new Error(`no rewind binary at ${binary}: run \`cargo build -p rewind\`, or set REWIND_BIN`);
  }
  return binary;
}

/** A directory for one test's logs and files, under `results/`. */
export function resultDir(name: string): string {
  const dir = join(results, name);
  mkdirSync(dir, { recursive: true });
  return dir;
}

/**
 * A new temporary directory with a short path, without symbolic links: a unix socket's path is
 * limited to 103 bytes, and the embedded server's socket lives in `<dir>/.rewind/`.
 */
export function shortTempDir(prefix: string): string {
  const base = existsSync("/tmp") ? "/tmp" : tmpdir();
  return realpathSync(mkdtempSync(join(base, `${prefix}-`)));
}
