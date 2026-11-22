// The frame of the app (archi/05, "One sidebar"): a sidebar with the organization picker and Find
// at its top, the views or the page's own sidebar below, the account at its bottom; the page on
// the right. The query (`q`) follows the reader from the Overview to Observability.
import {
  ChevronLeft,
  ChevronRight,
  Eye,
  FileCode,
  FlaskConical,
  LayoutGrid,
  LockKeyhole,
  Search,
  Users,
  Wrench,
} from "lucide-react";
import { type ReactNode, useCallback, useState } from "react";
import { Link, Outlet, useLocation, useNavigate } from "react-router";
import { queryLink } from "../model/query";
import { useAccess } from "../state/access";
import { type ConnectionState, useConnection } from "../state/connection";
import { useSession } from "../state/session";
import { AccountMenu, OrganizationSwitcher, useServerLine } from "./AccountMenus";
import { FindDialog, useFindShortcut } from "./Find";
import { SidebarFold, SidebarResizer, useFoldShortcut } from "./SidebarResizer";
import { SidebarProvider, useSidebar } from "./sidebar";

/**
 * The query (`q`) and the window (`window`) of the views that take them: they follow the reader
 * from the Overview to Observability and back.
 */
function carried(pathname: string, search: string): { q: string; extra: Record<string, string> } {
  if (pathname !== "/overview" && !pathname.startsWith("/observability"))
    return { q: "", extra: {} };
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
  const inObservability = pathname.startsWith("/observability") || pathname.startsWith("/runs/");
  const reopen = pane ? () => setShowViews(false) : undefined;
  return (
    <div className="nav">
      {reads && (
        <>
          <NavItem
            to={queryLink("/overview", q, extra)}
            icon={<LayoutGrid size={16} />}
            label="Overview"
            current={pathname === "/overview"}
          />
          <NavItem
            to={queryLink("/observability", q, extra)}
            icon={<Eye size={16} />}
            label="Observability"
            current={inObservability}
            opensSidebar
            {...(reopen ? { onReopen: reopen } : {})}
          />
          <NavItem
            to="/scripts"
            icon={<FileCode size={16} />}
            label="Scripts"
            current={pathname.startsWith("/scripts")}
          />
        </>
      )}
      {access.can("toolsets:read") && (
        <NavItem
          to="/testing"
          icon={<FlaskConical size={16} />}
          label="Testing"
          current={pathname.startsWith("/testing")}
        />
      )}
      {/* Tool statistics need `runs:read`, and a grant not limited to labels. */}
      {access.toolStats && (
        <NavItem
          to="/tools"
          icon={<Wrench size={16} />}
          label="Tools"
          current={pathname.startsWith("/tools")}
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

const CONNECTION_WORDS: Record<ConnectionState, string> = {
  idle: "Not following the runs: no view of runs is open",
  connecting: "Connecting to the runs' changes…",
  live: "Live: the runs' changes show as they happen",
  reconnecting: "Reconnecting to the runs' changes: the views are read every 10 to 30 s meanwhile",
};

/**
 * The change stream's state: a dot at a fixed place of the top bar, of one size whatever its
 * state, so that a change of state moves nothing.
 */
function ConnectionDot() {
  const state = useConnection();
  return (
    <span
      className={`connection-dot state-${state}`}
      role="img"
      aria-label={CONNECTION_WORDS[state]}
      title={CONNECTION_WORDS[state]}
    >
      <span className="connection-dot-mark" aria-hidden="true" />
    </span>
  );
}

function Sidebar({ onFind }: { onFind: () => void }) {
  const { pane, showViews, setShowViews, setSlot, folded } = useSidebar();
  const navigate = useNavigate();
  useFoldShortcut();
  const { phase } = useSession();
  const server = useServerLine();
  const paneShown = pane !== null && !showViews;
  const member = phase.kind === "member" ? phase.state.user : null;
  return (
    <aside className="sidebar" aria-label="Sidebar">
      <div className="sidebar-head">
        <div className="sidebar-top">
          <OrganizationSwitcher />
          <ConnectionDot />
          <SidebarFold />
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
