import type { Duration } from "./types.js";

const UNITS: ReadonlyArray<readonly [string, number]> = [
  ["w", 604_800_000],
  ["d", 86_400_000],
  ["h", 3_600_000],
  ["m", 60_000],
  ["s", 1_000],
  ["ms", 1],
];

const UNIT_MS: ReadonlyMap<string, number> = new Map(UNITS);

/**
 * The number of milliseconds of a duration, read as the server reads it: a number of
 * milliseconds, a string of digits (milliseconds too), or numbers with units (`"500ms"`,
 * `"30s"`, `"1h30m"`, `"1.5d"`, `"2w"`), possibly separated by spaces.
 *
 * @throws {RangeError} when the duration is malformed or negative.
 */
export function toMillis(duration: Duration): number {
  if (typeof duration === "number") {
    if (!Number.isFinite(duration) || duration < 0) {
      throw new RangeError(`invalid duration ${duration}: expected a number of milliseconds, 0 or more`);
    }
    return Math.round(duration);
  }
  const text = duration.trim();
  if (text === "") {
    throw new RangeError("invalid duration: it is empty");
  }
  if (/^\d+$/.test(text)) {
    return Number(text);
  }
  let total = 0;
  let rest = text;
  while (rest !== "") {
    const match = /^(\d+(?:\.\d*)?|\.\d+)([a-z]*)\s*/i.exec(rest);
    if (match === null) {
      throw new RangeError(`invalid duration ${JSON.stringify(duration)}: invalid number at ${JSON.stringify(rest)}`);
    }
    const [whole, number = "", unit = ""] = match;
    if (unit === "") {
      throw new RangeError(
        `invalid duration ${JSON.stringify(duration)}: missing unit after ${number} (expected ms, s, m, h, d or w)`,
      );
    }
    const unitMs = UNIT_MS.get(unit);
    if (unitMs === undefined) {
      throw new RangeError(
        `invalid duration ${JSON.stringify(duration)}: unknown unit ${JSON.stringify(unit)} (expected ms, s, m, h, d or w)`,
      );
    }
    total += Number(number) * unitMs;
    rest = rest.slice(whole.length);
  }
  return Math.round(total);
}

/** The shortest form of a number of milliseconds: `"1h30m"`, `"500ms"`, `"0ms"`. */
export function formatDuration(ms: number): string {
  let rest = Math.max(0, Math.round(ms));
  if (rest === 0) {
    return "0ms";
  }
  let text = "";
  for (const [unit, unitMs] of UNITS) {
    const count = Math.floor(rest / unitMs);
    if (count > 0) {
      text += `${count}${unit}`;
      rest -= count * unitMs;
    }
  }
  return text;
}

/** A duration as the API's query strings take it: always with a unit. */
export function durationParam(duration: Duration): string {
  return formatDuration(toMillis(duration));
}
