// A TypeScript source with line numbers and, beside each line, notes about it: on a run's page,
// the tool calls, timers, signal waits and logs that line caused. The source shows whole: no
// scroll of its own, long lines wrapped. Each line is a row of one grid (its number, its code,
// its notes), so a wrapped line keeps its number and its notes beside it.
// Parts of a line can be targets (a call, a variable): hovered, focused or clicked, they report
// their key, for the values a run's time cursor shows.
import {
  type CSSProperties,
  type FocusEvent,
  type KeyboardEvent,
  type MouseEvent,
  type ReactNode,
  useEffect,
  useMemo,
  useRef,
} from "react";
import { highlightLines, type Token } from "../model/highlight";

export type LineMark = "error" | "failed" | "open" | "activity";

/** A part of a line that can be hovered: columns from 0, `to` excluded. */
export interface LineTarget {
  from: number;
  to: number;
  key: string;
  /** A class for its kind: `call`, `binding`… */
  kind: string;
}

export interface TargetEvents {
  /** The pointer or the focus is on a target. */
  enter: (key: string, element: HTMLElement) => void;
  leave: () => void;
  /** A click, Enter or Space on a target. */
  pick: (key: string) => void;
}

export interface CodeViewProps {
  source: string;
  notes?: (line: number) => ReactNode;
  mark?: (line: number) => LineMark | null;
  /** Before the number of a line: a mark of the time cursor. */
  gutter?: (line: number) => ReactNode;
  /** The targets of a line, without overlaps, in order. */
  targets?: (line: number) => LineTarget[];
  /** Extra classes of a target: pinned, hovered. */
  targetClass?: (key: string) => string;
  targetEvents?: TargetEvents;
  selectedLine?: number | null;
  onLineClick?: (line: number) => void;
  /** Scrolls this line into view when it changes. */
  focusLine?: number | null;
  /** Header of the notes column. */
  notesTitle?: string;
}

/** A line's tokens cut at its targets' edges, each piece with its target, if any. */
function pieces(tokens: Token[], targets: LineTarget[]) {
  const out: { text: string; cls: string; target: LineTarget | null }[] = [];
  let at = 0;
  for (const token of tokens) {
    let start = 0;
    while (start < token.text.length) {
      const pos = at + start;
      const inside = targets.find((t) => t.from <= pos && pos < t.to) ?? null;
      const next = inside
        ? inside.to
        : Math.min(...targets.filter((t) => t.from > pos).map((t) => t.from), Infinity);
      const end = Math.min(token.text.length, next - at);
      out.push({ text: token.text.slice(start, end), cls: token.cls, target: inside });
      start = end;
    }
    at += token.text.length;
  }
  return out;
}

/** The spaces a line starts with: its wrapped parts go on further in than that. */
function indentOf(tokens: Token[]): number {
  const text = tokens.map((t) => t.text).join("");
  return text.length - text.trimStart().length;
}

function targetOf(e: { target: EventTarget }): HTMLElement | null {
  return (e.target as HTMLElement).closest?.<HTMLElement>("[data-target]") ?? null;
}

export function CodeView({
  source,
  notes,
  mark,
  gutter,
  targets,
  targetClass,
  targetEvents,
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
  const click = (n: number) => (e: MouseEvent) => {
    const target = targetOf(e);
    if (target && targetEvents) {
      targetEvents.pick(target.dataset.target ?? "");
      return;
    }
    onLineClick?.(n);
  };

  const over = (e: MouseEvent) => {
    const target = targetOf(e);
    if (target) targetEvents?.enter(target.dataset.target ?? "", target);
  };
  const out = (e: MouseEvent) => {
    const from = targetOf(e);
    const to =
      e.relatedTarget instanceof HTMLElement ? e.relatedTarget.closest("[data-target]") : null;
    if (from && from.dataset.target !== (to as HTMLElement | null)?.dataset.target)
      targetEvents?.leave();
  };
  const focus = (e: FocusEvent) => {
    const target = targetOf(e);
    if (target) targetEvents?.enter(target.dataset.target ?? "", target);
  };
  const key = (e: KeyboardEvent) => {
    const target = targetOf(e);
    if (target && (e.key === "Enter" || e.key === " ")) {
      e.preventDefault();
      targetEvents?.pick(target.dataset.target ?? "");
    }
  };

  return (
    <div className={`code-view${notes ? " with-notes" : ""}`} ref={container}>
      <div className="code-gutter" aria-hidden="true">
        {lines.map((_, i) => {
          const n = i + 1;
          return (
            // biome-ignore lint/a11y/useKeyWithClickEvents lint/a11y/noStaticElementInteractions: the notes column has the keyboard targets
            <div key={n} className={rowClass(n)} style={{ gridRow: n }} onClick={click(n)}>
              {gutter?.(n)}
              {n}
            </div>
          );
        })}
      </div>
      {/* biome-ignore lint/a11y/noStaticElementInteractions: hovers and keys of the targets, which are focusable */}
      <div
        className="code-text"
        onMouseOver={targetEvents ? over : undefined}
        onMouseOut={targetEvents ? out : undefined}
        onFocus={targetEvents ? focus : undefined}
        onBlur={targetEvents ? () => targetEvents.leave() : undefined}
        onKeyDown={targetEvents ? key : undefined}
      >
        <pre>
          {lines.map((tokens, i) => {
            const n = i + 1;
            const lineTargets = targets?.(n) ?? [];
            return (
              // biome-ignore lint/a11y/useKeyWithClickEvents lint/a11y/noStaticElementInteractions: the notes column has the keyboard targets
              <div
                key={n}
                className={rowClass(n)}
                style={{ gridRow: n, "--indent": indentOf(tokens) } as CSSProperties}
                data-line={n}
                onClick={click(n)}
              >
                {tokens.length === 0
                  ? " "
                  : lineTargets.length === 0
                    ? tokens.map((t, k) =>
                        t.cls ? (
                          // biome-ignore lint/suspicious/noArrayIndexKey: tokens of a line never move
                          <span key={k} className={t.cls}>
                            {t.text}
                          </span>
                        ) : (
                          t.text
                        ),
                      )
                    : pieces(tokens, lineTargets).map((p, k, all) => {
                        const textOf = (key: string) =>
                          all
                            .filter((x) => x.target?.key === key)
                            .map((x) => x.text)
                            .join("");
                        if (!p.target) {
                          return p.cls ? (
                            // biome-ignore lint/suspicious/noArrayIndexKey: pieces of a line never move
                            <span key={k} className={p.cls}>
                              {p.text}
                            </span>
                          ) : (
                            p.text
                          );
                        }
                        const first = all[k - 1]?.target?.key !== p.target.key;
                        const extra = targetClass?.(p.target.key) ?? "";
                        return (
                          <span
                            // biome-ignore lint/suspicious/noArrayIndexKey: pieces of a line never move
                            key={k}
                            className={`${p.cls} hv hv-${p.target.kind}${extra ? ` ${extra}` : ""}`}
                            data-target={p.target.key}
                            {...(first
                              ? {
                                  tabIndex: 0,
                                  role: "button",
                                  "aria-label": `${textOf(p.target.key)}, line ${n}: its value at the cursor`,
                                }
                              : {})}
                          >
                            {p.text}
                          </span>
                        );
                      })}
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
              <div key={n} className={rowClass(n)} style={{ gridRow: n }}>
                {notes(n)}
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}
