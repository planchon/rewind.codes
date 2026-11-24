// The frame of the app (archi/05, "One sidebar"): a sidebar with the organization picker and Find
// at its top, the views or the page's own sidebar below, the account at its bottom; the page on
// the right. The query (`q`) follows the reader from the Overview to Runs.
import {
  Activity,
  ChevronLeft,
  ChevronRight,
  LayoutGrid,
  LockKeyhole,
  Search,
  Users,
  Workflow,
  Zap,
} from "lucide-react";
import { type ReactNode, useCallback, useState } from "react";
import { Link, Outlet, useLocation, useNavigate } from "react-router";
import { queryLink } from "../model/query";
import { useAccess } from "../state/access";
import { useSession } from "../state/session";
import { AccountMenu, OrganizationSwitcher, useServerLine } from "./AccountMenus";
import { FindDialog, useFindShortcut } from "./Find";
import { isRunsView } from "./runLinks";
import { SidebarResizer, useFoldShortcut } from "./SidebarResizer";
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
  const { pathname, search } = useLocation();
  const { pane, setShowViews } = useSidebar();
  const reads = access.can("runs:read");
  const { q, extra } = carried(pathname, search);
  const inRuns = pathname === "/runs" || pathname.startsWith("/runs/");
  const reopen = pane ? () => setShowViews(false) : undefined;
  return (
    <div className="nav">
      {reads && (
        <>
          {access.statistics && (
            <NavItem
              to={queryLink("/overview", q, extra)}
              icon={<LayoutGrid size={16} />}
              label="Overview"
              current={pathname === "/overview"}
            />
          )}
          <NavItem
            to={queryLink("/runs", q, extra)}
            icon={<Activity size={16} />}
            label="Runs"
            current={inRuns}
            opensSidebar
            {...(reopen ? { onReopen: reopen } : {})}
          />
          <NavItem
            to="/workflows"
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
          to="/actions"
          icon={<Zap size={16} />}
          label="Actions"
          current={pathname.startsWith("/actions")}
        />
      )}
      {access.can("namespaces:read") && (
        <NavItem
          to="/namespaces"
          icon={<Users size={16} />}
          label="Namespaces"
          current={pathname.startsWith("/namespaces")}
        />
      )}
      {access.can("secrets:read") && (
        <NavItem
          to="/secrets"
          icon={<LockKeyhole size={16} />}
          label="Secrets"
          current={pathname.startsWith("/secrets")}
        />
      )}
    </div>
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

export function Layout() {
  const [finding, setFinding] = useState(false);
  const openFind = useCallback(() => setFinding(true), []);
  useFindShortcut(openFind);
  return (
    <SidebarProvider>
      <div className="shell">
        <Sidebar onFind={openFind} />
        <main className="main">
          <Outlet />
        </main>
      </div>
      <FindDialog open={finding} onOpenChange={setFinding} />
    </SidebarProvider>
  );
}
