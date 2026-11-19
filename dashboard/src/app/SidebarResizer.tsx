// The edge of the sidebar, dragged to resize it: the width lives in `--sidebar-width` on the root,
// so the menus sized after the sidebar follow, and in browser storage, so it is kept. A
// double-click gives the default width back; the arrows move it by 16 px, Home and End to its
// bounds.
import {
  type KeyboardEvent,
  type PointerEvent,
  useCallback,
  useLayoutEffect,
  useRef,
  useState,
} from "react";
import { read, write } from "./storage";

export const SIDEBAR_DEFAULT = 256;
export const SIDEBAR_MIN = 200;
export const SIDEBAR_MAX = 480;
const KEY = "rewind.sidebar";
const STEP = 16;

const clamp = (px: number) => Math.round(Math.min(SIDEBAR_MAX, Math.max(SIDEBAR_MIN, px)));

function stored(): number {
  const px = Number(read("local", KEY));
  return Number.isFinite(px) && px >= SIDEBAR_MIN && px <= SIDEBAR_MAX ? px : SIDEBAR_DEFAULT;
}

function save(px: number) {
  write("local", KEY, px === SIDEBAR_DEFAULT ? null : String(px));
}

export function SidebarResizer() {
  const [width, setWidth] = useState(stored);
  const drag = useRef<{ x: number; width: number } | null>(null);

  useLayoutEffect(() => {
    const root = document.documentElement.style;
    if (width === SIDEBAR_DEFAULT) root.removeProperty("--sidebar-width");
    else root.setProperty("--sidebar-width", `${width}px`);
  }, [width]);

  const set = useCallback((px: number) => {
    const next = clamp(px);
    setWidth(next);
    save(next);
  }, []);

  const down = (e: PointerEvent<HTMLDivElement>) => {
    if (e.button !== 0) return;
    e.preventDefault();
    e.currentTarget.setPointerCapture(e.pointerId);
    drag.current = { x: e.clientX, width };
    document.documentElement.dataset.resizing = "";
  };
  const move = (e: PointerEvent<HTMLDivElement>) => {
    if (!drag.current) return;
    setWidth(clamp(drag.current.width + e.clientX - drag.current.x));
  };
  const up = (e: PointerEvent<HTMLDivElement>) => {
    if (!drag.current) return;
    set(drag.current.width + e.clientX - drag.current.x);
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
    set(next);
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
      onDoubleClick={() => set(SIDEBAR_DEFAULT)}
    />
  );
}
