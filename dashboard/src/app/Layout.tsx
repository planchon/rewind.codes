// The frame of the app (archi/05, "One sidebar" and "Namespaces"): a sidebar with the organization
// picker, the namespace shown (in a namespace's scope) and Find at its top, the views or the page's
// own sidebar below, the account at its bottom; the page on the right, the namespace picker at the
// left of its header. The query (`q`) follows the reader from the Overview to Runs.
import {
  Activity,
  ChevronLeft,
  ChevronRight,
  Gauge,
  LayoutGrid,
  LockKeyhole,
  Search,
  Users,
  Workflow,
  Zap,
} from "lucide-react";
import { type ReactNode, useCallback, useState } from "react";
import { Link, Outlet, useLocation, useNavigate } from "react-router";
import { NamespaceMark } from "../components/NamespaceMark";
import { PageHeaderStart } from "../components/PageHeader";
import { queryLink } from "../model/query";
import { useAccess } from "../state/access";
import { useNamespaceNames } from "../state/namespaces";
import { scopedPath, splitScope } from "../state/scope";
import { useSession } from "../state/session";
import { AccountMenu, OrganizationSwitcher, useServerLine } from "./AccountMenus";
import { FindDialog, useFindShortcut } from "./Find";
import { NamespacePicker } from "./NamespacePicker";
import { isRunsView } from "./runLinks";
import { SidebarResizer, useFoldShortcut } from "./SidebarResizer";
import { ObjectScopeProvider, unscopedPath, useShownNamespace } from "./scope";
import { SidebarProvider, useSidebar } from "./sidebar";

/**
 * The query (`q`) and the window (`window`) of the views that take them: they follow the reader
 * from the Overview to Runs and back.
 */
function carried(pathname: string, search: string): { q: string; extra: Record<string, string> } {
  if (pathname !== "/overview" && !isRunsView(pathname)) return { q: "", extra: {} };
  const params = new URLSearchParams(search);
  const win = params.get("window");
  return { q: params.get("q") ?? "", extra: win ? { window: win } : {} };
}

function NavItem({
  to,
  icon,
  label,
  current,
  opensSidebar = false,
  onReopen,
}: {
  to: string;
  icon: ReactNode;
  label: string;
  current: boolean;
  /** The view has its own sidebar: a "›" says so. */
  opensSidebar?: boolean;
  /** Shows the view's own sidebar again, when it is the current page. */
  onReopen?: () => void;
}) {
  const { folded } = useSidebar();
  return (
    <Link
      to={to}
      className="nav-item"
      title={folded ? label : undefined}
      aria-current={current ? "page" : undefined}
      onClick={(e) => {
        if (current && onReopen) {
          e.preventDefault();
          onReopen();
        }
      }}
    >
      <span className="nav-icon" aria-hidden="true">
        {icon}
      </span>
      <span className="nav-label">{label}</span>
      {opensSidebar && <ChevronRight size={14} className="nav-end" aria-hidden="true" />}
    </Link>
  );
}

function MainNav() {
  const access = useAccess();
  const location = useLocation();
  const { pane, setShowViews } = useSidebar();
  const shown = useShownNamespace();
  const at = (path: string) => scopedPath(path, shown);
  const reads = access.can("runs:read");
  // The views are the same in every scope; the path after the scope says which one is open.
  const pathname = splitScope(location.pathname).path;
  const { q, extra } = carried(pathname, location.search);
  const inRuns = pathname === "/runs" || pathname.startsWith("/runs/");
  const reopen = pane ? () => setShowViews(false) : undefined;
  return (
    <div className="nav">
      {reads && (
        <>
          {access.statistics && (
            <NavItem
              to={queryLink(at("/overview"), q, extra)}
              icon={<LayoutGrid size={16} />}
              label="Overview"
              current={pathname === "/overview"}
            />
          )}
          <NavItem
            to={queryLink(at("/runs"), q, extra)}
            icon={<Activity size={16} />}
            label="Runs"
            current={inRuns}
            opensSidebar
            {...(reopen ? { onReopen: reopen } : {})}
          />
          <NavItem
            to={at("/workflows")}
            icon={<Workflow size={16} />}
            label="Workflows"
            current={pathname.startsWith("/workflows")}
            {...(reopen && pathname.startsWith("/workflows/") ? { onReopen: reopen } : {})}
          />
        </>
      )}
      {/* Statistics need `runs:read`, and a grant not limited to labels. */}
      {access.statistics && (
        <NavItem
          to={at("/actions")}
          icon={<Zap size={16} />}
          label="Actions"
          current={pathname.startsWith("/actions")}
        />
      )}
      {access.can("secrets:read") && (
        <NavItem
          to={at("/secrets")}
          icon={<LockKeyhole size={16} />}
          label="Secrets"
          current={pathname.startsWith("/secrets")}
        />
      )}
      {/* The last entry is the scope's: every namespace's use, or the one namespace's. */}
      {access.can("namespaces:read") &&
        (shown === null ? (
          <NavItem
            to="/namespaces"
            icon={<Users size={16} />}
            label="Namespaces"
            current={pathname.startsWith("/namespaces")}
          />
        ) : (
          <NavItem
            to={at("/usage")}
            icon={<Gauge size={16} />}
            label="Usage"
            current={pathname.startsWith("/usage")}
          />
        ))}
    </div>
  );
}

/**
 * The namespace shown, under the organization picker: its mark and its name; the row leads back to
 * the same view over every namespace. None over every namespace.
 */
function NamespaceRow() {
  const shown = useShownNamespace();
  const names = useNamespaceNames();
  const { pathname, search } = useLocation();
  const { folded } = useSidebar();
  if (shown === null) return null;
  const display = names.get(shown);
  const label = display ?? shown;
  return (
    <Link
      to={unscopedPath(pathname, search)}
      className="ns-row"
      aria-label={`Back to all namespaces (from ${label})`}
      title={folded ? `Back to all namespaces (from ${label})` : undefined}
    >
      <ChevronLeft size={16} aria-hidden="true" className="ns-row-back" />
      <NamespaceMark name={shown} />
      <span
        className={`ns-row-label${display ? "" : " mono"}`}
        title={display ? `${display} (namespace ${shown})` : shown}
      >
        {label}
      </span>
    </Link>
  );
}

function Sidebar({ onFind }: { onFind: () => void }) {
  const { pane, showViews, setShowViews, setSlot, folded } = useSidebar();
  const navigate = useNavigate();
  useFoldShortcut();
  const { phase } = useSession();
  const server = useServerLine();
  const paneShown = pane !== null && !showViews;
  const member = phase.kind === "member" ? phase.state.person : null;
  return (
    <aside className="sidebar" aria-label="Sidebar">
      <div className="sidebar-head">
        <div className="sidebar-top">
          <OrganizationSwitcher />
        </div>
        <NamespaceRow />
        <button
          type="button"
          className="find-button"
          onClick={onFind}
          title={folded ? "Find (F)" : undefined}
        >
          <Search size={14} aria-hidden="true" />
          <span>Find</span>
          <kbd className="kbd">F</kbd>
        </button>
      </div>
      <div className="sidebar-panes">
        <nav
          className="sidebar-pane"
          aria-label="Views"
          data-shown={paneShown ? undefined : ""}
          inert={paneShown}
        >
          <MainNav />
        </nav>
        <div
          className="sidebar-pane"
          {...(pane ? { role: "region", "aria-label": pane.title } : {})}
          data-shown={paneShown ? "" : undefined}
          inert={!paneShown}
          hidden={pane === null}
        >
          {pane && (
            <button
              type="button"
              className="pane-head"
              title={pane.backLabel}
              aria-label={`${pane.backLabel} (${pane.title})`}
              onClick={() => {
                if (pane.back !== null) void navigate(pane.back);
                else setShowViews(true);
              }}
            >
              <ChevronLeft size={16} aria-hidden="true" />
              <span>{pane.title}</span>
            </button>
          )}
          <div className="pane-body" ref={setSlot} />
        </div>
      </div>
      <div className="sidebar-foot">
        <AccountMenu
          user={member}
          server={server}
          onOrganization={() => void navigate("/organization")}
          onConnect={() => void navigate("/connect")}
          onConnections={() => void navigate("/account/connections")}
        />
      </div>
      <SidebarResizer />
    </aside>
  );
}

/** The pages outside the scopes: the organization's settings and the person's account. */
function unscoped(pathname: string): boolean {
  return ["/organization", "/connect", "/account"].some(
    (p) => pathname === p || pathname.startsWith(`${p}/`),
  );
}

export function Layout() {
  const [finding, setFinding] = useState(false);
  const openFind = useCallback(() => setFinding(true), []);
  const { pathname } = useLocation();
  useFindShortcut(openFind);
  return (
    <SidebarProvider>
      <ObjectScopeProvider>
        <div className="shell">
          <Sidebar onFind={openFind} />
          <main className="main">
            <PageHeaderStart.Provider value={unscoped(pathname) ? null : <NamespacePicker />}>
              <Outlet />
            </PageHeaderStart.Provider>
          </main>
        </div>
        <FindDialog open={finding} onOpenChange={setFinding} />
      </ObjectScopeProvider>
    </SidebarProvider>
  );
}
