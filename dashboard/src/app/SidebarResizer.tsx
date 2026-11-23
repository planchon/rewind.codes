// The edge of the sidebar: dragged, it resizes the sidebar; clicked, it folds the sidebar to its
// icons, and clicked again, unfolds it. The arrows move it by 16 px, Home and End to its bounds,
// Enter folds it. `[` folds or unfolds it from anywhere outside a field.
import { type KeyboardEvent, type PointerEvent, useEffect, useRef } from "react";
import { SIDEBAR_MAX, SIDEBAR_MIN, useSidebar } from "./sidebar";

const STEP = 16;
/** A pointer that moved less than this, in px, between down and up clicked the edge. */
const CLICK_SLOP = 3;

export function SidebarResizer() {
  const { width, setWidth, folded, setFolded } = useSidebar();
  const drag = useRef<{ x: number; width: number; moved: boolean } | null>(null);

  if (folded) {
    return (
      <button
        type="button"
        className="sidebar-resizer"
        aria-label="Expand the sidebar"
        title="Click to expand ([)"
        onClick={() => setFolded(false)}
      />
    );
  }

  const down = (e: PointerEvent<HTMLDivElement>) => {
    if (e.button !== 0) return;
    e.preventDefault();
    e.currentTarget.setPointerCapture?.(e.pointerId);
    drag.current = { x: e.clientX, width, moved: false };
  };
  const move = (e: PointerEvent<HTMLDivElement>) => {
    const d = drag.current;
    if (!d) return;
    if (!d.moved && Math.abs(e.clientX - d.x) < CLICK_SLOP) return;
    if (!d.moved) {
      d.moved = true;
      document.documentElement.dataset.resizing = "";
    }
    setWidth(d.width + e.clientX - d.x, false);
  };
  const up = (e: PointerEvent<HTMLDivElement>) => {
    const d = drag.current;
    if (!d) return;
    drag.current = null;
    delete document.documentElement.dataset.resizing;
    if (d.moved) setWidth(d.width + e.clientX - d.x);
    else if (e.type === "pointerup") setFolded(true);
  };
  const key = (e: KeyboardEvent<HTMLDivElement>) => {
    if (e.key === "Enter" || e.key === " ") {
      e.preventDefault();
      setFolded(true);
      return;
    }
    const next =
      e.key === "ArrowLeft"
        ? width - STEP
        : e.key === "ArrowRight"
          ? width + STEP
          : e.key === "Home"
            ? SIDEBAR_MIN
            : e.key === "End"
              ? SIDEBAR_MAX
              : null;
    if (next === null) return;
    e.preventDefault();
    setWidth(next);
  };

  return (
    // biome-ignore lint/a11y/useSemanticElements: a focusable separator is a splitter, which <hr> is not
    <div
      role="separator"
      aria-orientation="vertical"
      aria-label="Resize the sidebar"
      aria-valuemin={SIDEBAR_MIN}
      aria-valuemax={SIDEBAR_MAX}
      aria-valuenow={width}
      tabIndex={0}
      className="sidebar-resizer"
      title="Drag to resize · click to collapse ([)"
      onPointerDown={down}
      onPointerMove={move}
      onPointerUp={up}
      onPointerCancel={up}
      onKeyDown={key}
    />
  );
}

/** `[` folds or unfolds the sidebar, outside the fields where it is typed. */
export function useFoldShortcut() {
  const { folded, setFolded } = useSidebar();
  useEffect(() => {
    const onKey = (e: globalThis.KeyboardEvent) => {
      if (e.key !== "[" || e.metaKey || e.ctrlKey || e.altKey) return;
      const target = e.target as HTMLElement | null;
      if (
        target?.closest("input, textarea, select, [contenteditable=''], [contenteditable='true']")
      )
        return;
      e.preventDefault();
      setFolded(!folded);
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [folded, setFolded]);
}
