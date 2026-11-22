// The top of a live table (archi/05, "Nothing moves on its own"): its toolbar, whose slot
// for an answer held back ("N new · Show") is always there, so that its arrival moves no row, and
// which stays under the page's header while the table scrolls; and the mark that tells the state
// whether the table is scrolled to its top, where answers apply at once.
import { type ReactNode, useCallback, useEffect, useRef } from "react";
import { formatCount } from "../model/format";
import type { PendingAnswer } from "../state/runs";

/** The height of the page's sticky header: a table whose top is under it is scrolled. */
const HEADER_PX = 56;

/**
 * Tells `onChange` whether the element is at or below the top of the view: false once the page is
 * scrolled past it. Without IntersectionObserver (tests), always at the top.
 */
export function useAtTop(
  onChange: (atTop: boolean) => void,
): (element: HTMLElement | null) => void {
  const onChangeRef = useRef(onChange);
  onChangeRef.current = onChange;
  const observer = useRef<IntersectionObserver | null>(null);
  useEffect(() => () => observer.current?.disconnect(), []);
  return useCallback((element: HTMLElement | null) => {
    observer.current?.disconnect();
    observer.current = null;
    if (!element || typeof IntersectionObserver === "undefined") return;
    const watch = new IntersectionObserver(
      (entries) => {
        const entry = entries[entries.length - 1];
        if (!entry) return;
        const top = entry.rootBounds?.top ?? HEADER_PX;
        onChangeRef.current(entry.isIntersecting || entry.boundingClientRect.top >= top);
      },
      { rootMargin: `-${HEADER_PX}px 0px 0px 0px`, threshold: 0 },
    );
    watch.observe(element);
    observer.current = watch;
  }, []);
}

/** "N new · Show": an answer that adds, drops or reorders the rows, held back. */
export function PendingButton({
  pending,
  onShow,
}: {
  pending: PendingAnswer | null;
  onShow: () => void;
}) {
  if (!pending) return null;
  const what = pending.added > 0 ? `${formatCount(pending.added)} new` : "Updated";
  return (
    <button
      type="button"
      className="pending-btn"
      onClick={onShow}
      title="Rows were added, dropped or reordered since the table was scrolled: show them"
    >
      {what} · Show
    </button>
  );
}

/**
 * The toolbar above a live table: what it says on the left, the slot of an answer held back on
 * the right; then the mark of the table's top.
 */
export function TableToolbar({
  pending,
  onShow,
  setAtTop,
  children,
}: {
  pending: PendingAnswer | null;
  onShow: () => void;
  setAtTop: (atTop: boolean) => void;
  children?: ReactNode;
}) {
  const top = useAtTop(setAtTop);
  return (
    <>
      <div className="table-toolbar">
        <div className="table-toolbar-start">{children}</div>
        <div className="table-toolbar-slot">
          <PendingButton pending={pending} onShow={onShow} />
        </div>
      </div>
      <div ref={top} className="table-top" aria-hidden="true" />
    </>
  );
}
