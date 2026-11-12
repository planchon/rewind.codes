// TypeScript source split into lines of highlighted tokens, with Lezer's TypeScript grammar.
import { classHighlighter, highlightCode } from "@lezer/highlight";
import { parser } from "@lezer/javascript";

export interface Token {
  text: string;
  /** Space-separated `tok-…` classes; empty for plain text. */
  cls: string;
}

const typescript = parser.configure({ dialect: "ts" });

export function highlightLines(source: string): Token[][] {
  const text = source.replace(/\r\n?/g, "\n");
  const lines: Token[][] = [[]];
  try {
    highlightCode(
      text,
      typescript.parse(text),
      classHighlighter,
      (chunk, cls) => lines[lines.length - 1]?.push({ text: chunk, cls }),
      () => lines.push([]),
    );
  } catch {
    return text.split("\n").map((line) => [{ text: line, cls: "" }]);
  }
  // A trailing newline does not make a line of its own.
  if (lines.length > 1 && lines[lines.length - 1]?.length === 0 && text.endsWith("\n")) lines.pop();
  return lines;
}
