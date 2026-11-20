// Secrets as the views read them (archi/04, "Secrets"): their names, their reads over 30 days, and
// what a deletion would stop, told from `GET …/secrets/{name}/usage`.
import type { DailyReads, Labels, SecretUsage } from "../api/types";

const DAY = 86_400_000;
/** The reads the API answers: the last 30 days. */
export const READ_DAYS = 30;
/** The largest value the API takes, in bytes of UTF-8. */
export const MAX_VALUE_BYTES = 16 * 1024;

/** Why a name cannot be a secret's, or `null` when it can: the API's rule. */
export function secretNameProblem(name: string): string | null {
  if (name.length === 0) return "Give the secret a name.";
  if (name.length > 64) return "At most 64 characters.";
  if (!/^[a-z]/.test(name)) return "Start with a lowercase letter.";
  if (!/^[a-z][a-z0-9_-]*$/.test(name)) return "Only lowercase letters, digits, - and _.";
  return null;
}

/** Why a value cannot be written, or `null`: empty, or above 16 KiB. */
export function secretValueProblem(value: string): string | null {
  if (value.length === 0) return "Type the value.";
  if (new TextEncoder().encode(value).length > MAX_VALUE_BYTES) return "At most 16 KiB.";
  return null;
}

/** The reads of each of the last 30 UTC days, the oldest first, a day without a read at 0. */
export function readSeries(reads: readonly DailyReads[], now: number): number[] {
  const byDay = new Map(reads.map((r) => [r.day, r.reads]));
  const out: number[] = [];
  for (let d = READ_DAYS - 1; d >= 0; d--) {
    out.push(byDay.get(new Date(now - d * DAY).toISOString().slice(0, 10)) ?? 0);
  }
  return out;
}

export function totalReads(reads: readonly DailyReads[]): number {
  return reads.reduce((sum, r) => sum + r.reads, 0);
}

/** The labels a secret written under this grant takes: those typed, and the grant's own. */
export function labelsTaken(typed: Labels, grant: Labels): Labels {
  return { ...typed, ...grant };
}

/** Whether a deletion stops nothing: no active run nor workflow attaches the secret. */
export function stopsNothing(usage: SecretUsage): boolean {
  return usage.activeTotal === 0 && usage.workflows.length === 0;
}

export function plural(n: number, one: string, many = `${one}s`): string {
  return `${n} ${n === 1 ? one : many}`;
}
