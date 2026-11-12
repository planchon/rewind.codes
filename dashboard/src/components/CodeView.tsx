// A TypeScript source with line numbers and, beside each line, notes about it: on a run's page,
// the tool calls, timers, signal waits and logs that line caused. Three stacks of equal-height
// rows (numbers, code, notes) keep the code scrollable sideways while the notes stay in place.
import { type ReactNode, useEffect, useMemo, useRef } from "react";
import { highlightLines } from "../model/highlight";

export type LineMark = "error" | "failed" | "open" | "activity";

export interface CodeViewProps {
  source: string;
  notes?: (line: number) => ReactNode;
  mark?: (line: number) => LineMark | null;
  selectedLine?: number | null;
  onLineClick?: (line: number) => void;
  /** Scrolls this line into view when it changes. */
  focusLine?: number | null;
  /** Header of the notes column. */
  notesTitle?: string;
}

export function CodeView({
  source,
  notes,
  mark,
  selectedLine = null,
  onLineClick,
  focusLine = null,
  notesTitle,
}: CodeViewProps) {
  const lines = useMemo(() => highlightLines(source), [source]);
  const container = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (focusLine === null) return;
    const row = container.current?.querySelector<HTMLElement>(`[data-line="${focusLine}"]`);
    row?.scrollIntoView?.({ block: "nearest" });
  }, [focusLine]);

  const rowClass = (n: number) => {
    const m = mark?.(n) ?? null;
    return `code-row${m ? ` mark-${m}` : ""}${n === selectedLine ? " selected" : ""}${onLineClick ? " clickable" : ""}`;
  };
  const click = onLineClick ? (n: number) => () => onLineClick(n) : () => undefined;

  return (
    <div className={`code-view${notes ? " with-notes" : ""}`} ref={container}>
      <div className="code-gutter" aria-hidden="true">
        {lines.map((_, i) => {
          const n = i + 1;
          return (
            // biome-ignore lint/a11y/useKeyWithClickEvents lint/a11y/noStaticElementInteractions: the notes column has the keyboard targets
            <div key={n} className={rowClass(n)} onClick={click(n)}>
              {n}
            </div>
          );
        })}
      </div>
      <div className="code-text">
        <pre>
          {lines.map((tokens, i) => {
            const n = i + 1;
            return (
              // biome-ignore lint/a11y/useKeyWithClickEvents lint/a11y/noStaticElementInteractions: the notes column has the keyboard targets
              <div key={n} className={rowClass(n)} data-line={n} onClick={click(n)}>
                {tokens.length === 0
                  ? " "
                  : tokens.map((t, k) =>
                      t.cls ? (
                        // biome-ignore lint/suspicious/noArrayIndexKey: tokens of a line never move
                        <span key={k} className={t.cls}>
                          {t.text}
                        </span>
                      ) : (
                        t.text
                      ),
                    )}
              </div>
            );
          })}
        </pre>
      </div>
      {notes && (
        <div className="code-notes" title={notesTitle}>
          {lines.map((_, i) => {
            const n = i + 1;
            return (
              <div key={n} className={rowClass(n)}>
                {notes(n)}
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}
