/**
 * The model's tests, read as the API reads them (`crates/rewind-api`'s `TestInput`, `TestReplacement` and
 * `TestRetirement`, through serde), before anything is sent: `update_workflow` must not rename a workflow and then
 * learn that its tests cannot be read. The messages are serde's, which the API would answer.
 */

/** Why a value is not what serde reads, or `undefined` when it is. */
type Check = (value: unknown) => string | undefined;

/** The first test, replacement or retirement the API could not read, as its message; `undefined` when all can be. */
export function unreadableBody(tests: unknown[], replace: unknown[], retire: unknown[]): string | undefined {
  for (const [items, check] of [
    [tests, testInput],
    [replace, testReplacement],
    [retire, testRetirement],
  ] as const) {
    for (const item of items) {
      const problem = check(item);
      if (problem !== undefined) {
        return problem;
      }
    }
  }
  return undefined;
}

/** What serde says it found instead (`Unexpected`). */
function unexpected(value: unknown): string {
  if (value === null) {
    return "null";
  }
  switch (typeof value) {
    case "boolean":
      return `boolean \`${value}\``;
    case "number":
      return Number.isInteger(value) ? `integer \`${value}\`` : `floating point \`${value}\``;
    case "string":
      return `string ${JSON.stringify(value)}`;
    default:
      return Array.isArray(value) ? "sequence" : "map";
  }
}

function invalidType(value: unknown, expected: string): string {
  return `invalid type: ${unexpected(value)}, expected ${expected}`;
}

/** serde's list of what it expected: `` `a` ``, `` `a` or `b` ``, `` one of `a`, `b`, `c` ``. */
function oneOf(names: readonly string[]): string {
  const quoted = names.map((name) => `\`${name}\``);
  if (quoted.length === 1) {
    return quoted[0]!;
  }
  if (quoted.length === 2) {
    return `${quoted[0]} or ${quoted[1]}`;
  }
  return `one of ${quoted.join(", ")}`;
}

const anything: Check = () => undefined;

const string: Check = (value) => (typeof value === "string" ? undefined : invalidType(value, "a string"));

/**
 * A time as jiff reads it: RFC 3339 or Temporal's ISO 8601, with an offset. The grammar only; the server says whether
 * the date exists.
 */
const TIMESTAMP =
  /^(?:[+-]\d{6}|\d{4})-?\d{2}-?\d{2}[Tt ]\d{2}(?::?\d{2}(?::?\d{2}(?:[.,]\d{1,9})?)?)?(?:[Zz]|[+-]\d{2}(?::?\d{2}(?::?\d{2}(?:[.,]\d{1,9})?)?)?)(?:\[[^\]]*\])*$/;

const timestamp: Check = (value) =>
  string(value) ?? (TIMESTAMP.test(value as string) ? undefined : `invalid timestamp ${JSON.stringify(value)}`);

const u32: Check = (value) => {
  if (typeof value !== "number" || !Number.isInteger(value)) {
    return invalidType(value, "u32");
  }
  return value < 0 || value > 0xffff_ffff ? `invalid value: integer \`${value}\`, expected u32` : undefined;
};

/** `Option<T>`: `null` is `None`. */
function optional(check: Check): Check {
  return (value) => (value === null ? undefined : check(value));
}

function sequence(check: Check): Check {
  return (value) => {
    if (!Array.isArray(value)) {
      return invalidType(value, "a sequence");
    }
    for (const item of value) {
      const problem = check(item);
      if (problem !== undefined) {
        return problem;
      }
    }
    return undefined;
  };
}

/** A string map: `BTreeMap<String, T>`. */
function map(check: Check): Check {
  return (value) => {
    if (typeof value !== "object" || value === null || Array.isArray(value)) {
      return invalidType(value, "a map");
    }
    for (const key of Object.keys(value).sort()) {
      const problem = check((value as Record<string, unknown>)[key]);
      if (problem !== undefined) {
        return problem;
      }
    }
    return undefined;
  };
}

/** A unit-only enum, `snake_case`, as serde_json reads it from a value: a string, or a map of one key. */
function variants(...names: string[]): Check {
  return (value) => {
    let variant: unknown = value;
    if (typeof value === "object" && value !== null && !Array.isArray(value)) {
      const entries = Object.entries(value);
      if (entries.length !== 1) {
        return "invalid value: map, expected map with a single key";
      }
      const [key, content] = entries[0]!;
      if (!names.includes(key)) {
        return `unknown variant \`${key}\`, expected ${oneOf(names)}`;
      }
      return content === null ? undefined : invalidType(content, "unit");
    }
    if (typeof variant !== "string") {
      return invalidType(variant, "string or map");
    }
    return names.includes(variant) ? undefined : `unknown variant \`${variant}\`, expected ${oneOf(names)}`;
  };
}

/**
 * A struct with `deny_unknown_fields`: its fields read in the map's order (serde_json's maps are sorted), then the
 * first required one missing, in the order of the declaration. serde also reads a struct from an array, by position:
 * that is left to the server.
 */
function struct(name: string, fields: Record<string, { check: Check; required?: boolean }>): Check {
  const declared = Object.keys(fields);
  return (value) => {
    if (Array.isArray(value)) {
      return undefined;
    }
    if (typeof value !== "object" || value === null) {
      return invalidType(value, `struct ${name}`);
    }
    const object = value as Record<string, unknown>;
    for (const key of Object.keys(object).sort()) {
      const field = fields[key];
      if (field === undefined) {
        return `unknown field \`${key}\`, expected ${oneOf(declared)}`;
      }
      const problem = field.check(object[key]);
      if (problem !== undefined) {
        return problem;
      }
    }
    const missing = declared.find((key) => fields[key]!.required && !(key in object));
    return missing === undefined ? undefined : `missing field \`${missing}\``;
  };
}

/** `#[serde(untagged)]`: the first variant that reads it. */
function untagged(name: string, ...checks: Check[]): Check {
  return (value) =>
    checks.some((check) => check(value) === undefined)
      ? undefined
      : `data did not match any variant of untagged enum ${name}`;
}

const STATUS = variants("queued", "running", "waiting", "completed", "failed", "cancelled", "aborted");

const expectations = struct("Expectations", {
  status: { check: optional(STATUS) },
  output: { check: anything },
  error: {
    check: optional(
      struct("ExpectedError", { name: { check: string, required: true }, message: { check: optional(string) } }),
    ),
  },
  calls: {
    check: sequence(
      struct("ExpectedCall", {
        action: { check: string, required: true },
        args: { check: anything },
        times: { check: optional(u32) },
      }),
    ),
  },
  order: { check: optional(variants("any", "listed")) },
});

const actionRule = struct("ActionRule", {
  args: { check: anything },
  returns: { check: anything },
  error: {
    check: optional(
      struct("RuleError", {
        message: { check: string, required: true },
        kind: { check: optional(variants("error", "unavailable", "timeout", "outcome_unknown")) },
        data: { check: anything },
      }),
    ),
  },
  times: { check: optional(u32) },
});

const test = struct("Test", {
  name: { check: string, required: true },
  input: { check: anything },
  start: { check: optional(timestamp) },
  seed: { check: optional(string) },
  world: {
    check: struct("World", {
      actions: { check: map(untagged("ActionRules", sequence(actionRule), actionRule)) },
      handlers: { check: optional(string) },
      cassette: { check: anything },
    }),
  },
  signals: {
    check: sequence(
      struct("TestSignal", {
        name: { check: string, required: true },
        payload: { check: anything },
        when: { check: optional(string) },
      }),
    ),
  },
  expect: { check: expectations },
  from: {
    check: optional(
      struct("TestFromRun", {
        run: { check: string, required: true },
        version: { check: string, required: true },
        namespace: { check: string, required: true },
      }),
    ),
  },
});

const testInput = untagged(
  "TestInput",
  string,
  struct("TestFrom", { test: { check: string, required: true }, expect: { check: expectations, required: true } }),
  test,
);

const testReplacement = struct("TestReplacement", {
  name: { check: string, required: true },
  reason: { check: string, required: true },
  new: { check: optional(testInput) },
  expect: { check: optional(expectations) },
});

const testRetirement = struct("TestRetirement", {
  names: { check: sequence(string), required: true },
  reason: { check: string, required: true },
});
