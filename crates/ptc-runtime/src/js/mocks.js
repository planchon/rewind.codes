// The glue of TypeScript mock handlers (archi/05-testing-and-observability.md, "Testing and
// mocks"). It belongs to no epoch: nothing a mock does reaches a script but its result, which is
// journaled.
//
// Evaluates to setup(handlers), which takes the module's default export and returns
// { tools, call }: the names of the tools with a handler, sorted, and call(tool, argsJson,
// callJson), a promise of the JSON of { result } or { failure: { kind, message, data? } }.

((globalThis) => {
  "use strict";

  const { Error, JSON, Map, Object, Promise, String, TypeError } = globalThis;
  const { parse, stringify } = JSON;
  const { entries } = Object;
  const KINDS = ["error", "unavailable", "timeout", "outcome_unknown"];
  // As in the script's glue: a lone surrogate cannot leave the isolate.
  const LONE_SURROGATE = /\\\\|\\ud[89a-f][0-9a-f]{2}/gi;
  const wellFormed = (json) =>
    json.replace(LONE_SURROGATE, (match) => match === "\\\\" ? match : "\\ufffd");

  function failure(error) {
    const kind = KINDS.includes(error?.kind) ? error.kind : "error";
    const message = error instanceof Error
      ? String(error.message)
      : typeof error === "string"
      ? error
      : String(stringify(error) ?? error);
    let data;
    try {
      data = error?.data === undefined ? undefined : parse(stringify(error.data));
    } catch {
      data = undefined;
    }
    return data === undefined ? { kind, message } : { kind, message, data };
  }

  return function setup(handlers) {
    if (handlers === null || typeof handlers !== "object") {
      throw new TypeError(
        "a module of mock handlers must `export default` an object of handlers, by tool name or by namespace",
      );
    }
    const table = new Map();
    for (const [key, value] of entries(handlers)) {
      if (typeof value === "function") {
        table.set(key, value);
      } else if (value !== null && typeof value === "object") {
        for (const [name, handler] of entries(value)) {
          if (typeof handler !== "function") {
            throw new TypeError(`the mock handler ${key}.${name} is not a function`);
          }
          table.set(`${key}.${name}`, handler);
        }
      } else {
        throw new TypeError(`the mock handler ${key} is not a function`);
      }
    }

    async function run(tool, argsJson, callJson) {
      const value = await table.get(tool)(parse(argsJson), parse(callJson));
      const json = stringify(value === undefined ? null : value);
      if (json === undefined) {
        throw new TypeError(`the mock handler of ${tool} returned a value that is not JSON`);
      }
      return json;
    }

    return {
      tools: [...table.keys()].sort(),
      call(tool, argsJson, callJson) {
        return Promise.resolve().then(() => run(tool, argsJson, callJson)).then(
          (json) => `{"result":${wellFormed(json)}}`,
          (error) => wellFormed(stringify({ failure: failure(error) })),
        );
      },
    };
  };
})(globalThis);
