import { LogOut, Monitor, Moon, Star, Sun, X } from "lucide-react";
import { useRef } from "react";
import { Link, NavLink, Outlet, useLocation } from "react-router";
import { queryLink } from "../model/query";
import { OrganizationSwitcher, UserMenu } from "./AccountMenus";
import { useAccess } from "./access";
import { useServer } from "./Gate";
import { useSavedSearches } from "./searches";
import { maskKey, useSession } from "./session";
import { type ThemeChoice, useSettings } from "./settings";

const NEXT_THEME: Record<ThemeChoice, ThemeChoice> = {
  system: "light",
  light: "dark",
  dark: "system",
};
const THEME_ICON = { system: Monitor, light: Sun, dark: Moon } as const;

/** The views that take a query: it follows the operator from one to the other. */
const QUERY_VIEWS = new Set(["/overview", "/runs", "/waiting", "/failures"]);
/** The views that take a window. */
const WINDOW_VIEWS = new Set(["/overview", "/failures"]);

function SavedMenu() {
  const { searches, remove } = useSavedSearches();
  const menu = useRef<HTMLDetailsElement>(null);
  if (searches.length === 0) return null;
  return (
    <details className="saved-menu" ref={menu}>
      <summary title="Saved searches, kept in this browser">
        <Star size={12} aria-hidden="true" /> Saved{" "}
        <span className="num muted">{searches.length}</span>
      </summary>
      <ul aria-label="Saved searches">
        {searches.map((s) => (
          <li key={s.name}>
            <Link
              to={queryLink(`/${s.view}`, s.q)}
              title={`${s.view}: ${s.q || "every run"}`}
              onClick={() => menu.current?.removeAttribute("open")}
            >
              <span className="saved-name">{s.name}</span>
              <span className="mono muted saved-q">{s.q || "every run"}</span>
            </Link>
            <button
              type="button"
              className="icon-btn"
              aria-label={`Forget the saved search ${s.name}`}
              onClick={() => remove(s.name)}
            >
              <X size={12} />
            </button>
          </li>
        ))}
      </ul>
    </details>
  );
}

export function Layout() {
  const location = useLocation();
  const here = new URLSearchParams(location.search);
  const carried = QUERY_VIEWS.has(location.pathname) ? (here.get("q") ?? "") : "";
  const win = WINDOW_VIEWS.has(location.pathname) ? here.get("window") : null;
  const link = (path: string) =>
    queryLink(path, carried, win && WINDOW_VIEWS.has(path) ? { window: win } : {});
  const server = useServer();
  const access = useAccess();
  const { key, signOut, phase } = useSession();
  const { theme, setTheme, zone, setZone } = useSettings();
  const ThemeIcon = THEME_ICON[theme];
  const epochs = server.epochs.join(", ");
  const reads = access.can("runs:read");
  const member = phase.kind === "member" ? phase.state.user : null;

  return (
    <div className="shell">
      <header className="topbar">
        <Link to="/" className="brand" aria-label="Rewind dashboard">
          <span className="brand-mark" aria-hidden="true">
            AP
          </span>
          <span className="brand-name">Rewind</span>
        </Link>
        <nav className="nav" aria-label="Views">
          {reads && (
            <>
              <NavLink to={link("/overview")}>Overview</NavLink>
              <NavLink to={link("/runs")}>Runs</NavLink>
              <NavLink to={link("/waiting")}>Waiting</NavLink>
              <NavLink to={link("/failures")}>Failures</NavLink>
              <NavLink to="/scripts">Scripts</NavLink>
              {access.toolStats && <NavLink to="/tools">Tools</NavLink>}
            </>
          )}
          {access.sections.length > 0 && <NavLink to="/organization">Organization</NavLink>}
          {reads && <SavedMenu />}
        </nav>
        <div className="topbar-end">
          <span className="server-meta" title={`Roles: ${server.roles.join(", ")}`}>
            rewind {server.version} · epoch{server.epochs.length > 1 ? "s" : ""} {epochs}
            {server.embedded ? " · embedded" : ""}
            {server.virtualTime ? " · virtual time" : ""}
          </span>
          <fieldset className="seg">
            <legend className="sr-only">Time zone</legend>
            <button type="button" aria-pressed={zone === "utc"} onClick={() => setZone("utc")}>
              UTC
            </button>
            <button type="button" aria-pressed={zone === "local"} onClick={() => setZone("local")}>
              Local
            </button>
          </fieldset>
          <button
            type="button"
            className="icon-btn"
            aria-label={`Theme: ${theme}. Switch to ${NEXT_THEME[theme]}`}
            title={`Theme: ${theme}`}
            onClick={() => setTheme(NEXT_THEME[theme])}
          >
            <ThemeIcon size={15} />
          </button>
          {member && <OrganizationSwitcher />}
          {member && <UserMenu user={member} />}
          {phase.kind === "key" && key && (
            <span className="copyable" title="The API key in use">
              <span className="mono">{maskKey(key)}</span>
              <button
                type="button"
                className="icon-btn"
                aria-label="Sign out"
                title="Sign out"
                onClick={() => void signOut()}
              >
                <LogOut size={14} />
              </button>
            </span>
          )}
        </div>
      </header>
      <main className="main">
        <Outlet />
      </main>
    </div>
  );
}
