import { describe, expect, it } from "vitest";
import { highlightLines } from "./highlight";

describe("highlightLines", () => {
  it("keeps one array of tokens per source line, text intact", () => {
    const source =
      'import { sleep } from "rewind";\n\nexport default async function main() {\n  await sleep("10d");\n}\n';
    const lines = highlightLines(source);
    expect(lines).toHaveLength(5);
    expect(lines.map((l) => l.map((t) => t.text).join(""))).toEqual(source.split("\n").slice(0, 5));
    expect(lines[1]).toEqual([]);
  });

  it("classifies keywords, strings and definitions", () => {
    const [line] = highlightLines('const answer: string = "yes";');
    const cls = (text: string) => line?.find((t) => t.text === text)?.cls ?? "";
    expect(cls("const")).toContain("tok-keyword");
    expect(cls('"yes"')).toContain("tok-string");
    expect(cls("answer")).toContain("tok-definition");
    expect(cls("string")).toContain("tok-typeName");
  });
});
