/**
 * Criteria 3 and 5 (`archi/09-roadmap.md`, "0.1 is done when"):
 *
 * - 3: a script calls a real MCP server, with types checked beforehand. The server is the
 *   reference `@modelcontextprotocol/server-everything`, over stdio through `npx`. A wrong
 *   argument type is refused by `rewind check` (and at start) before any run exists; a correct
 *   script runs to completion.
 * - 5: `rewind test` replays that recorded real run as a scenario, from its cassette; `rewind fork`
 *   continues a failed run with a corrected script without repeating its recorded calls.
 *
 * The MCP server's call log: the toolset starts it behind `tee`, which appends every JSON-RPC
 * message the client sends to a file. A `tools/call` there is a call the server received.
 */
import { readFileSync } from "node:fs";
import { expect, test } from "@playwright/test";
import { Project } from "../support/project.js";

/** The reference server, pinned. */
export const EVERYTHING = "@modelcontextprotocol/server-everything@2.2.1";

const GOOD = `import { everything } from "rewind:tools";

export default async function main(input: { city: "New York" | "Chicago" | "Los Angeles" }) {
  const echoed = await everything.echo({ message: "hello" });
  const sum = await everything["get-sum"]({ a: 2, b: 40 });
  // A tool with an output schema: its result is typed.
  const weather = await everything["get-structured-content"]({ location: input.city });
  return { echoed: echoed.text(), sum: sum.text(), celsius: weather.temperature > -100 };
}
`;

const WRONG_TYPE = `import { everything } from "rewind:tools";

export default async function main() {
  const sum = await everything["get-sum"]({ a: "2", b: 40 });
  return sum.text();
}
`;

/** Fails after its two calls: the sum's text is not JSON. */
const FAILING = `import { everything } from "rewind:tools";

export default async function main() {
  const echoed = await everything.echo({ message: "step 1" });
  const sum = await everything["get-sum"]({ a: 2, b: 40 });
  const total = sum.json<{ total: number }>().total;
  return { echoed: echoed.text(), total };
}
`;

const FIXED = `import { everything } from "rewind:tools";

export default async function main() {
  const echoed = await everything.echo({ message: "step 1" });
  const sum = await everything["get-sum"]({ a: 2, b: 40 });
  const total = Number(/is (\\d+)/.exec(sum.text())?.[1]);
  return { echoed: echoed.text(), total };
}
`;

test.describe.serial("criteria 3 and 5: a real MCP server, rewind check, rewind test, rewind fork", () => {
  let project: Project;
  let log: string;
  let toolset: string;
  let recorded: string;

  /** The `tools/call` requests the MCP server received, by tool name. */
  const toolCalls = () =>
    readFileSync(log, "utf8")
      .split("\n")
      .filter((line) => line.trim() !== "")
      .map((line) => JSON.parse(line) as { method?: string; params?: { name: string; _meta?: Record<string, unknown> } })
      .filter((message) => message.method === "tools/call")
      .map((message) => message.params!);

  test.beforeAll(async () => {
    project = new Project("rewind-c35");
    log = project.write("mcp-calls.jsonl", "");
    project.write(
      "toolset.json",
      JSON.stringify({
        mcp: [
          {
            name: "everything",
            command: "sh",
            args: ["-c", `tee -a "$MCP_CALL_LOG" | npx -y ${EVERYTHING} stdio`],
            env: { MCP_CALL_LOG: log },
          },
        ],
      }),
    );
  });

  test.afterAll(async () => {
    await project?.dispose();
  });

  test("criterion 3: a wrong argument type is refused before any run exists; a correct script completes", async () => {
    const added = await project.json(["toolsets", "add", "toolset.json"]);
    expect(added.code, added.ran.stderr).toBe(0);
    toolset = added.value.id;
    const declarations = await project.rewind(["toolsets", "declarations", toolset]);
    expect(declarations.stdout).toContain(`"get-sum"(args: {`);

    project.write("wrong.ts", WRONG_TYPE);
    project.write("good.ts", GOOD);
    // Checked against the registered toolset (the server), and against the file (in the CLI).
    for (const against of [toolset, "toolset.json"]) {
      const checked = await project.rewind(["check", "wrong.ts", "--toolset", against]);
      expect(checked.code, checked.stdout + checked.stderr).toBe(65);
      expect(checked.stdout + checked.stderr).toContain("TS2322: Type 'string' is not assignable to type 'number'");
      expect(checked.stdout + checked.stderr).toContain("wrong.ts:4:");
    }
    // Starting it is refused the same way.
    const refused = await project.rewind(["run", "wrong.ts", "--toolset", toolset]);
    expect(refused.code).toBe(65);
    expect(refused.stderr).toContain("TS2322");
    expect((await project.json(["runs"])).value.runs).toEqual([]);
    expect(toolCalls()).toEqual([]);

    const ok = await project.rewind(["check", "good.ts", "--toolset", toolset]);
    expect(ok.code, ok.stdout + ok.stderr).toBe(0);
    const ran = await project.json(["run", "good.ts", "--toolset", toolset, "--input", `{"city":"Chicago"}`, "--wait", "120s"]);
    expect(ran.code, JSON.stringify(ran.value)).toBe(0);
    expect(ran.value.status).toBe("completed");
    expect(ran.value.output.value).toEqual({ echoed: "Echo: hello", sum: "The sum of 2 and 40 is 42.", celsius: true });
    recorded = ran.value.id;

    // The server received the three calls, each with its idempotency key and trace context.
    const calls = toolCalls();
    expect(calls.map((call) => call.name)).toEqual(["echo", "get-sum", "get-structured-content"]);
    for (const call of calls) {
      expect(JSON.stringify(call._meta)).toContain(recorded);
      expect(JSON.stringify(call._meta)).toContain("traceparent");
    }
  });

  test("criterion 5: rewind test replays the recorded run as a scenario, without calling the server", async () => {
    const exported = await project.rewind(["export", recorded, "--as", "cassette", "--output", "recorded.cassette.json"]);
    expect(exported.code, exported.stderr).toBe(0);
    project.write(
      "scenario.json",
      JSON.stringify({
        script: "good.ts",
        toolset: "toolset.json",
        cassette: "recorded.cassette.json",
        input: { city: "Chicago" },
        expect: {
          status: "completed",
          output: { echoed: "Echo: hello", sum: "The sum of 2 and 40 is 42.", celsius: true },
          calls: { "everything.echo": 1, "everything.get-sum": 1, "everything.get-structured-content": 1 },
        },
      }),
    );
    const before = toolCalls().length;
    const tested = await project.rewind(["test", "scenario.json"]);
    expect(tested.code, tested.stdout + tested.stderr).toBe(0);
    expect(tested.stdout.startsWith("PASS"), tested.stdout).toBe(true);
    expect(toolCalls()).toHaveLength(before);

    // The same scenario with another expectation fails: the test is not vacuous.
    project.write(
      "wrong-scenario.json",
      readFileSync(project.path("scenario.json"), "utf8").replace('"celsius":true', '"celsius":false'),
    );
    const failed = await project.rewind(["test", "wrong-scenario.json"]);
    expect(failed.code).toBe(1);
  });

  test("criterion 5: rewind fork continues a failed run without repeating its recorded calls", async () => {
    project.write("failing.ts", FAILING);
    project.write("fixed.ts", FIXED);
    const before = toolCalls().length;
    const failed = await project.json(["run", "failing.ts", "--toolset", toolset, "--wait", "120s"]);
    expect(failed.code, JSON.stringify(failed.value)).toBe(1);
    expect(failed.value.error.name).toBe("SyntaxError");
    const parent: string = failed.value.id;
    expect(toolCalls().slice(before).map((call) => call.name)).toEqual(["echo", "get-sum"]);

    // What a fork reuses.
    const replay = await project.rewind(["replay", parent, "--script", "fixed.ts"]);
    expect(replay.stdout).toContain("a fork reuses: #1 everything.echo");

    const afterFailure = toolCalls().length;
    const fork = await project.json(["fork", parent, "--script", "fixed.ts", "--wait", "120s"]);
    expect(fork.code, JSON.stringify(fork.value)).toBe(0);
    expect(fork.value.status).toBe("completed");
    expect(fork.value.output.value).toEqual({ echoed: "Echo: step 1", total: 42 });
    expect(fork.value.parent.run).toBe(parent);
    // The MCP server received no call: both outcomes came from the parent's journal.
    expect(toolCalls()).toHaveLength(afterFailure);

    const forks = await project.json(["runs", "--parent", parent]);
    expect(forks.value.runs.map((run: { id: string }) => run.id)).toEqual([fork.value.id]);
  });
});
