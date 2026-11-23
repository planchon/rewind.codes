// The box of a table: a card whose header stays under the page's while the page scrolls. No cell
// is cut: each column is at least its width and holds its content whole, and a table wider than
// its page scrolls sideways, a thin shadow on an edge while columns hide past it, so it reads as
// scrolling rather than cut (macOS hides scrollbars until one scrolls). Its header follows the
// page in both cases (app.css, "The header follows the page"): this box measures how far it may
// go, from the table's top to its last row.
import { type ReactNode, useCallback, useEffect, useRef, useState } from "react";

interface Overflow {
  /** The table is wider than its box: the box scrolls. */
  wide: boolean;
  /** Columns hide on its left. */
  less: boolean;
  /** Columns hide on its right. */
  more: boolean;
}

const FITS: Overflow = { wide: false, less: false, more: false };

/**
 * Whether a box's table is wider than it, and hides columns on a side; and how far its header
 * may follow the page, set on the frame as `--head-travel`. Never wide, in tests.
 */
function useOverflow(): [(box: HTMLElement | null) => void, Overflow] {
  const [overflow, setOverflow] = useState<Overflow>(FITS);
  const cleanup = useRef<(() => void) | null>(null);
  useEffect(() => () => cleanup.current?.(), []);
  const ref = useCallback((box: HTMLElement | null) => {
    cleanup.current?.();
    cleanup.current = null;
    if (!box || typeof ResizeObserver === "undefined") return;
    const frame = box.parentElement;
    const measure = () => {
      const table = box.querySelector("table");
      const wide = table !== null && table.offsetWidth > box.clientWidth + 1;
      const less = wide && box.scrollLeft > 1;
      const more = wide && box.scrollLeft + box.clientWidth < box.scrollWidth - 1;
      setOverflow((o) =>
        o.wide === wide && o.less === less && o.more === more ? o : { wide, less, more },
      );
      const head = table?.tHead;
      const travel = table && head ? Math.max(0, table.offsetHeight - head.offsetHeight) : 0;
      frame?.style.setProperty("--head-travel", `${travel}px`);
    };
    const observer = new ResizeObserver(measure);
    observer.observe(box);
    const table = box.querySelector("table");
    if (table) observer.observe(table);
    box.addEventListener("scroll", measure, { passive: true });
    measure();
    cleanup.current = () => {
      observer.disconnect();
      box.removeEventListener("scroll", measure);
    };
  }, []);
  return [ref, overflow];
}

export function TableScroll({
  stale = false,
  children,
}: {
  /** A new search is loading: the rows shown are the previous search's, dimmed. */
  stale?: boolean;
  children: ReactNode;
}) {
  const [ref, { wide, less, more }] = useOverflow();
  return (
    <div
      className="table-frame"
      data-less={less ? "" : undefined}
      data-more={more ? "" : undefined}
    >
      <div
        ref={ref}
        className={`table-wrap${stale ? " is-stale" : ""}`}
        data-wide={wide ? "" : undefined}
        aria-busy={stale || undefined}
      >
        {children}
      </div>
      {/* The card's top corners around its header while it follows the page. */}
      <span className="table-corner start" aria-hidden="true" />
      <span className="table-corner end" aria-hidden="true" />
    </div>
  );
}
