// The labels of a row of a table (a run's, a secret's, a grant's): whole when they are few and
// short (`labelsFit`); otherwise one chip says how many there are, and its card holds them all and
// nothing else, so a row never grows with its labels. An id is never shown: its label is its key
// and a link to its runs (`IdLabel`).
import { type ReactNode, useLayoutEffect, useRef } from "react";
import { labelsFit } from "../model/format";
import type { Labels } from "../model/types";
import { LabelList } from "./bits";
import { HoverCard } from "./HoverCard";

export function RowLabels({
  labels,
  onPick,
  pickTitle,
  owner = "",
  empty = false,
}: {
  labels: Labels;
  /** Makes each label a button: what it filters by. */
  onPick?: ((pair: string) => void) | undefined;
  pickTitle?: (pair: string) => string;
  /** Whose labels they are, for the card's name: "The run's". */
  owner?: string;
  /** "–" when there is none, rather than nothing. */
  empty?: boolean;
}) {
  const pairs = Object.entries(labels).sort(([a], [b]) => a.localeCompare(b));
  if (pairs.length === 0) return empty ? <span className="muted">–</span> : null;
  const pick = { onPick, ...(pickTitle ? { pickTitle } : {}) };
  if (labelsFit(pairs))
    return (
      <span className="row-labels">
        <LabelList labels={labels} {...pick} />
      </span>
    );
  const count = `${pairs.length} label${pairs.length === 1 ? "" : "s"}`;
  return (
    <span className="row-labels">
      <HoverCard
        label={owner ? `${owner} ${count}` : count}
        className="labels-card"
        trigger={
          <button type="button" className="label more">
            {count}
          </button>
        }
      >
        <ShrunkToLines>
          <LabelList labels={labels} {...pick} />
        </ShrunkToLines>
      </HoverCard>
    </span>
  );
}

/**
 * Chips wrapped in a card as wide as its longest line: a box sized to wrapped content keeps its
 * greatest width, and would leave the end of each line empty, the card's right padding wider than
 * its left. Measured once laid out, from the layout's offsets, which the card's opening scale does
 * not change; the lines stay the same, none of them being wider than the longest.
 */
function ShrunkToLines({ children }: { children: ReactNode }) {
  const ref = useRef<HTMLSpanElement>(null);
  useLayoutEffect(() => {
    const box = ref.current?.firstElementChild;
    if (!(box instanceof HTMLElement)) return;
    box.style.width = "";
    const chips = [...box.children].filter((c): c is HTMLElement => c instanceof HTMLElement);
    const right = Math.max(0, ...chips.map((c) => c.offsetLeft + c.offsetWidth)) - box.offsetLeft;
    // Offsets are rounded to the pixel: one more keeps the last chip of the line on it.
    if (right > 0) box.style.width = `${right + 1}px`;
  });
  return (
    <span ref={ref} className="shrunk-lines">
      {children}
    </span>
  );
}
