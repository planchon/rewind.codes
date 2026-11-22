// One sidebar (archi/05, "One sidebar"). It shows the views; a page that needs more than a
// toolbar (Observability, Run, Organization) turns it into its own with `PageSidebar`, whose
// content is drawn into the sidebar, under a "back" that shows the views again or leads up a
// level. The organization picker and Find stay above both. Its width is the reader's: dragged
// at its edge, or folded to a rail of icons; both are kept in browser storage.
import {
  createContext,
  type ReactNode,
  useCallback,
  useContext,
  useLayoutEffect,
  useMemo,
  useState,
} from "react";
import { createPortal } from "react-dom";
import { read, write } from "../model/storage";

export const SIDEBAR_DEFAULT = 256;
export const SIDEBAR_MIN = 200;
export const SIDEBAR_MAX = 480;
/** The folded sidebar: a rail of icons. */
export const SIDEBAR_RAIL = 52;
const WIDTH_KEY = "rewind.sidebar";
const FOLDED_KEY = "rewind.sidebar.folded";

export const clampWidth = (px: number) =>
  Math.round(Math.min(SIDEBAR_MAX, Math.max(SIDEBAR_MIN, px)));

function storedWidth(): number {
  const px = Number(read("local", WIDTH_KEY));
  return Number.isFinite(px) && px >= SIDEBAR_MIN && px <= SIDEBAR_MAX ? px : SIDEBAR_DEFAULT;
}

/** What a page says of its sidebar. */
export interface PagePane {
  /** The page's name, at the head of its sidebar. */
  title: string;
  /**
   * Where "back" leads, as a path of the app; `null` shows the views without leaving the page.
   */
  back: string | null;
  /** What "back" says it does, for its label. */
  backLabel: string;
}

interface SidebarContextValue {
  /** Where a page's sidebar is drawn. */
  slot: HTMLElement | null;
  setSlot(element: HTMLElement | null): void;
  pane: PagePane | null;
  setPane(pane: PagePane | null): void;
  /** The views are shown over the page's sidebar ("back" from a page that has no level up). */
  showViews: boolean;
  setShowViews(show: boolean): void;
  /** Its width when unfolded, in px. `keep` stores it (at the end of a drag). */
  width: number;
  setWidth(px: number, keep?: boolean): void;
  /** Folded to a rail of icons. */
  folded: boolean;
  setFolded(folded: boolean): void;
}

const SidebarContext = createContext<SidebarContextValue | null>(null);

export function SidebarProvider({ children }: { children: ReactNode }) {
  const [slot, setSlot] = useState<HTMLElement | null>(null);
  const [pane, setPaneState] = useState<PagePane | null>(null);
  const [showViews, setShowViews] = useState(false);
  const [width, setWidthState] = useState(storedWidth);
  const [folded, setFoldedState] = useState(() => read("local", FOLDED_KEY) === "1");
  const setWidth = useCallback((px: number, keep = true) => {
    const next = clampWidth(px);
    setWidthState(next);
    if (keep) write("local", WIDTH_KEY, next === SIDEBAR_DEFAULT ? null : String(next));
  }, []);
  const setFolded = useCallback((next: boolean) => {
    setFoldedState(next);
    write("local", FOLDED_KEY, next ? "1" : null);
  }, []);
  // The width lives on the root, so the menus sized after the sidebar follow it.
  useLayoutEffect(() => {
    const root = document.documentElement;
    const px = folded ? SIDEBAR_RAIL : width;
    if (px === SIDEBAR_DEFAULT) root.style.removeProperty("--sidebar-width");
    else root.style.setProperty("--sidebar-width", `${px}px`);
    if (folded) root.dataset.sidebar = "folded";
    else delete root.dataset.sidebar;
  }, [width, folded]);
  const setPane = useCallback((next: PagePane | null) => {
    setPaneState((current) =>
      current?.title === next?.title &&
      current?.back === next?.back &&
      current?.backLabel === next?.backLabel
        ? current
        : next,
    );
  }, []);
  // Another page's sidebar opens on itself, not on the views.
  const title = pane?.title ?? null;
  useLayoutEffect(() => {
    if (title !== null) setShowViews(false);
  }, [title]);
  const value = useMemo(
    () => ({
      slot,
      setSlot,
      pane,
      setPane,
      showViews,
      setShowViews,
      width,
      setWidth,
      folded,
      setFolded,
    }),
    [slot, pane, setPane, showViews, width, setWidth, folded, setFolded],
  );
  return <SidebarContext.Provider value={value}>{children}</SidebarContext.Provider>;
}

export function useSidebar(): SidebarContextValue {
  const value = useContext(SidebarContext);
  if (!value) throw new Error("useSidebar is used outside SidebarProvider");
  return value;
}

/** Whether the sidebar is folded to its icons; `false` outside the shell (a test of one part). */
export function useSidebarFolded(): boolean {
  return useContext(SidebarContext)?.folded ?? false;
}

/**
 * The page's own sidebar: its content is drawn into the sidebar, under a head with the page's
 * title and a "back". Outside the shell (a test of one part), nothing is drawn.
 */
export function PageSidebar({
  title,
  back = null,
  backLabel,
  children,
}: {
  title: string;
  back?: string | null;
  backLabel?: string;
  children: ReactNode;
}) {
  const sidebar = useContext(SidebarContext);
  const setPane = sidebar?.setPane;
  const label = backLabel ?? (back === null ? "Show the views" : "Back");
  useLayoutEffect(() => {
    if (!setPane) return;
    setPane({ title, back, backLabel: label });
    return () => setPane(null);
  }, [setPane, title, back, label]);
  if (!sidebar?.slot) return null;
  return createPortal(children, sidebar.slot);
}
