// One sidebar (archi/05, "One sidebar"). It shows the views; a page that needs more than a
// toolbar (Observability, Run, Organization) turns it into its own with `PageSidebar`, whose
// content is drawn into the sidebar, under a "back" that shows the views again or leads up a
// level. The organization picker and Find stay above both.
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
}

const SidebarContext = createContext<SidebarContextValue | null>(null);

export function SidebarProvider({ children }: { children: ReactNode }) {
  const [slot, setSlot] = useState<HTMLElement | null>(null);
  const [pane, setPaneState] = useState<PagePane | null>(null);
  const [showViews, setShowViews] = useState(false);
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
    () => ({ slot, setSlot, pane, setPane, showViews, setShowViews }),
    [slot, pane, setPane, showViews],
  );
  return <SidebarContext.Provider value={value}>{children}</SidebarContext.Provider>;
}

export function useSidebar(): SidebarContextValue {
  const value = useContext(SidebarContext);
  if (!value) throw new Error("useSidebar is used outside SidebarProvider");
  return value;
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
