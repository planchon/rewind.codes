import { describe, expect, it } from "vitest";
import { readNamespaceSearch, readScriptSearch, readToolSearch, scriptQueryOf } from "./searches";

const TOOLS = ["github.close_issue", "github.list_issues", "slack.post_message", "ask_user"];

describe("the Tools search", () => {
  const kept = (q: string) => TOOLS.filter(readToolSearch(q).keeps);

  it("keeps the tools whose name holds each word, or that a tool: term names", () => {
    expect(kept("")).toEqual(TOOLS);
    expect(kept("issue")).toEqual(["github.close_issue", "github.list_issues"]);
    expect(kept("GitHub list")).toEqual(["github.list_issues"]);
    expect(kept("tool:github.*")).toEqual(["github.close_issue", "github.list_issues"]);
    expect(kept("tool:ask_user,slack.post_message")).toEqual(["slack.post_message", "ask_user"]);
    expect(kept("-tool:github.* -ask")).toEqual(["slack.post_message"]);
    expect(kept('"post_message"')).toEqual(["slack.post_message"]);
  });

  it("asks the API for the calls of one toolset", () => {
    expect(readToolSearch("toolset:5f3a9c issue").toolset).toBe("5f3a9c");
    expect(readToolSearch("issue").toolset).toBeNull();
  });

  it("says which term it cannot read, and why", () => {
    expect(readToolSearch("status:failed").fault).toEqual({
      message:
        "status: not a field of the tools; search the tools by tool:, toolset: or a part of their name",
      start: 0,
      end: 13,
    });
    expect(readToolSearch("x @team:infra").fault?.start).toBe(2);
    expect(readToolSearch("toolset:nope").fault?.message).toMatch(/hexadecimal/);
    expect(readToolSearch("toolset:aaaaaa toolset:bbbbbb").fault?.message).toMatch(/one toolset/);
    expect(readToolSearch("-toolset:aaaaaa").fault?.message).toMatch(/not excluded/);
    // Characters, as the server counts them, not UTF-16 units.
    expect(readToolSearch("é😀 status:x").fault).toMatchObject({ start: 3, end: 11 });
  });
});

describe("the Scripts search", () => {
  it("reads a tool and labels for the API", () => {
    expect(readScriptSearch("tool:github.close_issue @team:infra @env:prod")).toEqual({
      labels: ["team:infra", "env:prod"],
      tool: "github.close_issue",
      fault: null,
    });
    expect(readScriptSearch('@note:"two words"').labels).toEqual(["note:two words"]);
  });

  it("refuses what the API cannot answer", () => {
    expect(readScriptSearch("triage").fault?.message).toMatch(/not searched by their text/);
    expect(readScriptSearch("-tool:x").fault?.message).toMatch(/cannot exclude/);
    expect(readScriptSearch("tool:github.*").fault?.message).toMatch(/full name/);
    expect(readScriptSearch("tool:a tool:b").fault).toMatchObject({ start: 7, end: 13 });
    expect(readScriptSearch("@team:a,b").fault?.message).toMatch(/one value per label/);
    expect(readScriptSearch("status:failed").fault?.message).toMatch(/not a field of the scripts/);
  });

  it("reads an older link's filters as its search", () => {
    expect(scriptQueryOf(new URLSearchParams(""))).toBeNull();
    expect(
      scriptQueryOf(new URLSearchParams("label=team:infra&label=note:a b&tool=crm.provision")),
    ).toBe('@team:infra @note:"a b" tool:crm.provision');
  });
});

describe("the Namespaces search", () => {
  it("is the start of a name", () => {
    expect(readNamespaceSearch(" user_7K ")).toEqual({ prefix: "user_7K", fault: null });
    expect(readNamespaceSearch("org:acme@eu")).toEqual({ prefix: "org:acme@eu", fault: null });
  });

  it("points at the first character a name cannot hold", () => {
    expect(readNamespaceSearch("  da na")).toEqual({
      prefix: "",
      fault: {
        message: "A namespace is searched by the start of its name: letters, digits and - _ . : @",
        start: 4,
        end: 5,
      },
    });
  });
});
