// Run handles in the mocked API, as the server computes them (rewind-core's `handle.rs`):
// `<namespace>/<origin>/<suffix>`, whose suffix is the start of the run's UUIDv7 in Crockford
// base32, and whose origin is its workflow's name, else the workflow's short id (`wf-7k2m9q`). A
// handle never changes: those stored before workflows had ids keep the origin they had.
import type { RunFixture, World } from "./fixtures";

const ALPHABET = "0123456789abcdefghjkmnpqrstvwxyz";
const SUFFIX_LEN = 12;
const MAX_SUFFIX_LEN = 25;

/** The 122 bits of a UUIDv7 that are not its version nor its variant, padded by 3 to 125. */
function padded(id: string): bigint {
  const uuid = BigInt(`0x${id.replaceAll("-", "")}`);
  const ms = uuid >> 80n;
  const randA = (uuid >> 64n) & 0xfffn;
  const randB = uuid & ((1n << 62n) - 1n);
  return ((ms << 74n) | (randA << 62n) | randB) << 3n;
}

export function runSuffix(id: string, len = SUFFIX_LEN): string {
  const bits = padded(id);
  let out = "";
  for (let i = 0; i < len; i += 1) {
    out += ALPHABET[Number((bits >> BigInt(125 - 5 * (i + 1))) & 31n)];
  }
  return out;
}

/** The shortest suffix, at least 12 characters, `id` shares with none of `others`. */
export function suffixLenApart(id: string, others: readonly string[]): number {
  const mine = runSuffix(id, MAX_SUFFIX_LEN);
  let len = SUFFIX_LEN;
  for (const other of others) {
    if (other === id) continue;
    const theirs = runSuffix(other, MAX_SUFFIX_LEN);
    let common = 0;
    while (common < MAX_SUFFIX_LEN && mine[common] === theirs[common]) common += 1;
    len = Math.max(len, common + 1);
  }
  return Math.min(len, MAX_SUFFIX_LEN);
}

export function originSlug(title: string): string | null {
  const slug = title
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+/, "")
    .slice(0, 40)
    .replace(/-+$/, "");
  return slug === "" ? null : slug;
}

/** Gives every run of the world its origin and suffix length, once: they never change after. */
export function ensureHandles(world: World): void {
  const pending = world.runs.filter((r) => r.origin === undefined || r.suffixLen === undefined);
  if (pending.length === 0) return;
  const ids = world.runs.map((r) => r.id);
  for (const run of pending) {
    if (run.origin === undefined) {
      const id = run.workflow?.id;
      run.origin = run.workflow?.name ?? (id ? `wf-${id.slice(3, 9)}` : "untitled");
    }
    if (run.suffixLen === undefined) {
      const near = ids.filter((id) => id.slice(0, 18) === run.id.slice(0, 18));
      run.suffixLen = suffixLenApart(run.id, near);
    }
  }
}

export function handleOf(world: World, run: RunFixture): string {
  ensureHandles(world);
  return `${run.namespace}/${run.origin ?? "untitled"}/${runSuffix(run.id, run.suffixLen ?? SUFFIX_LEN)}`;
}

/** The run a path names: its UUID, or its handle (percent-encoded or not). */
export function findRun(world: World, id: string): RunFixture | undefined {
  const wanted = id.includes("%") ? decodeURIComponent(id) : id;
  if (!wanted.includes("/")) return world.runs.find((r) => r.id === wanted);
  return world.runs.find((r) => handleOf(world, r) === wanted.toLowerCase());
}
