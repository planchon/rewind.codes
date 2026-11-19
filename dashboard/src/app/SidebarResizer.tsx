// The edge of the sidebar, dragged to resize it. A double-click gives the default width back;
// the arrows move it by 16 px, Home and End to its bounds. Folded, the sidebar has no edge.
import { PanelLeftClose, PanelLeftOpen } from "lucide-react";
import { type KeyboardEvent, type PointerEvent, useEffect, useRef } from "react";
import { SIDEBAR_DEFAULT, SIDEBAR_MAX, SIDEBAR_MIN, useSidebar } from "./sidebar";

const STEP = 16;

export function SidebarResizer() {
  const { width, setWidth, folded } = useSidebar();
  const drag = useRef<{ x: number; width: number } | null>(null);
  if (folded) return null;

  const down = (e: PointerEvent<HTMLDivElement>) => {
    if (e.button !== 0) return;
    e.preventDefault();
    e.currentTarget.setPointerCapture(e.pointerId);
    drag.current = { x: e.clientX, width };
    document.documentElement.dataset.resizing = "";
  };
  const move = (e: PointerEvent<HTMLDivElement>) => {
    if (!drag.current) return;
    setWidth(drag.current.width + e.clientX - drag.current.x, false);
  };
  const up = (e: PointerEvent<HTMLDivElement>) => {
    if (!drag.current) return;
    setWidth(drag.current.width + e.clientX - drag.current.x);
    drag.current = null;
    delete document.documentElement.dataset.resizing;
  };
  const key = (e: KeyboardEvent<HTMLDivElement>) => {
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
      title="Drag to resize · double-click for the default width"
      onPointerDown={down}
      onPointerMove={move}
      onPointerUp={up}
      onPointerCancel={up}
      onKeyDown={key}
      onDoubleClick={() => setWidth(SIDEBAR_DEFAULT)}
    />
  );
}

/** Folds the sidebar to its icons, or unfolds it; `[` does the same. */
export function SidebarFold() {
  const { folded, setFolded } = useSidebar();
  const label = folded ? "Expand the sidebar" : "Collapse the sidebar to its icons";
  return (
    <button
      type="button"
      className="icon-btn sidebar-fold"
      aria-label={label}
      aria-pressed={folded}
      title={`${label} ([)`}
      onClick={() => setFolded(!folded)}
    >
      {folded ? <PanelLeftOpen size={16} /> : <PanelLeftClose size={16} />}
    </button>
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
