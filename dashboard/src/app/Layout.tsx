import { LogOut, Monitor, Moon, Sun } from "lucide-react";
import { Link, NavLink, Outlet } from "react-router";
import { useServer } from "./Gate";
import { maskKey, useSession } from "./session";
import { type ThemeChoice, useSettings } from "./settings";

const NEXT_THEME: Record<ThemeChoice, ThemeChoice> = {
  system: "light",
  light: "dark",
  dark: "system",
};
const THEME_ICON = { system: Monitor, light: Sun, dark: Moon } as const;

export function Layout() {
  const server = useServer();
  const { key, signOut } = useSession();
  const { theme, setTheme, zone, setZone } = useSettings();
  const ThemeIcon = THEME_ICON[theme];
  const epochs = server.epochs.join(", ");

  return (
    <div className="shell">
      <header className="topbar">
        <Link to="/runs" className="brand" aria-label="ai-ptc dashboard">
          <span className="brand-mark" aria-hidden="true">
            AP
          </span>
          <span className="brand-name">ai-ptc</span>
        </Link>
        <nav className="nav" aria-label="Views">
          <NavLink to="/runs">Runs</NavLink>
          <NavLink to="/scripts">Scripts</NavLink>
          <NavLink to="/tools">Tools</NavLink>
        </nav>
        <div className="topbar-end">
          <span className="server-meta" title={`Roles: ${server.roles.join(", ")}`}>
            ptc {server.version} · epoch{server.epochs.length > 1 ? "s" : ""} {epochs}
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
          {key && (
            <span className="copyable" title="The API key in use">
              <span className="mono">{maskKey(key)}</span>
              <button
                type="button"
                className="icon-btn"
                aria-label="Sign out"
                title="Sign out"
                onClick={signOut}
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
