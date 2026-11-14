import { MutationCache, QueryCache, QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { Lock } from "lucide-react";
import { type ReactNode, useRef, useState } from "react";
import {
  createHashRouter,
  Navigate,
  Outlet,
  type RouteObject,
  RouterProvider,
  useRouteError,
} from "react-router";
import { ApiError } from "../api/client";
import { WHOAMI } from "../api/organization";
import { ProblemView } from "../components/bits";
import { FailuresView } from "../views/FailuresView";
import { OverviewView } from "../views/OverviewView";
import { ApiKeysView } from "../views/org/ApiKeysView";
import { InvitationsView } from "../views/org/InvitationsView";
import { MembersView } from "../views/org/MembersView";
import { OrganizationIndex, OrganizationView } from "../views/org/OrganizationView";
import { RolesView } from "../views/org/RolesView";
import { SettingsView } from "../views/org/SettingsView";
import { RunsView } from "../views/RunsView";
import { RunView } from "../views/run/RunView";
import { ScriptsView } from "../views/ScriptsView";
import { ScriptView } from "../views/ScriptView";
import { ToolsView } from "../views/ToolsView";
import { WaitingView } from "../views/WaitingView";
import { useAccess } from "./access";
import { Gate } from "./Gate";
import { Layout } from "./Layout";
import { NoPermission } from "./NoPermission";
import { SessionProvider, type SessionProviderProps, useSession } from "./session";
import { SettingsProvider } from "./settings";

function RouteError() {
  return (
    <div className="page">
      <ProblemView error={useRouteError()} title="This page failed" />
    </div>
  );
}

function NotFound() {
  return (
    <div className="page">
      <ProblemView error={new Error("No view at this address.")} title="Not found" />
    </div>
  );
}

function CannotRead() {
  return (
    <div className="page">
      <NoPermission permission="runs:read" what="read runs" />
    </div>
  );
}

/** The run views need `runs:read`. */
function Reading() {
  const access = useAccess();
  if (!access.can("runs:read")) return <CannotRead />;
  return <Outlet />;
}

/** Tool statistics are refused to a grant limited to labels. */
function ToolStats() {
  const access = useAccess();
  if (!access.toolStats) {
    return (
      <div className="page">
        <div className="no-permission" role="alert">
          <h1>
            <Lock size={15} aria-hidden="true" /> Tool statistics span every run
          </h1>
          <p>
            Your grant only reaches the runs carrying its labels, and the statistics of the tools
            count every run of the organization.
          </p>
        </div>
      </div>
    );
  }
  return <ToolsView />;
}

/** The first view the caller may open. */
function Home() {
  const access = useAccess();
  if (access.can("runs:read")) return <Navigate to="/overview" replace />;
  if (access.sections.length > 0) return <Navigate to="/organization" replace />;
  return <CannotRead />;
}

export const routes: RouteObject[] = [
  {
    path: "/",
    element: <Layout />,
    errorElement: <RouteError />,
    children: [
      { index: true, element: <Home /> },
      {
        element: <Reading />,
        children: [
          { path: "overview", element: <OverviewView /> },
          { path: "runs", element: <RunsView /> },
          { path: "waiting", element: <WaitingView /> },
          { path: "failures", element: <FailuresView /> },
          { path: "runs/:id", element: <RunView /> },
          { path: "scripts", element: <ScriptsView /> },
          { path: "scripts/:digest", element: <ScriptView /> },
          { path: "tools", element: <ToolStats /> },
        ],
      },
      {
        path: "organization",
        element: <OrganizationView />,
        children: [
          { index: true, element: <OrganizationIndex /> },
          { path: "members", element: <MembersView /> },
          { path: "invitations", element: <InvitationsView /> },
          { path: "roles", element: <RolesView /> },
          { path: "api-keys", element: <ApiKeysView /> },
          { path: "settings", element: <SettingsView /> },
        ],
      },
      { path: "*", element: <NotFound /> },
    ],
  },
];

/** A client error will not go away by asking again. */
function retry(count: number, error: unknown): boolean {
  if (error instanceof ApiError && error.status >= 400 && error.status < 500) return false;
  return count < 2;
}

/**
 * A query cache per key, or per member and organization: what one of them read is never shown
 * to another, not even while the new one's first answers are on their way (another tenant's
 * runs, scripts or payloads).
 */
function Queries({ children }: { children: ReactNode }) {
  const { identity } = useSession();
  return <IdentityQueries key={identity}>{children}</IdentityQueries>;
}

function IdentityQueries({ children }: { children: ReactNode }) {
  const session = useSession();
  const sessionRef = useRef(session);
  sessionRef.current = session;
  const clientRef = useRef<QueryClient | null>(null);
  const [client] = useState(() => {
    const onError = (error: unknown, key: readonly unknown[] | undefined) => {
      if (!(error instanceof ApiError)) return;
      const { phase } = sessionRef.current;
      if (error.status === 401) {
        // The key form says itself that the key was refused.
        if (phase.kind === "key" && (key?.[0] === "server" || key?.[0] === WHOAMI[0])) return;
        // Refused even with a renewed token, or a key revoked while the dashboard is open.
        sessionRef.current.unauthorized();
        if (phase.kind === "key")
          void clientRef.current?.invalidateQueries({ queryKey: ["server"] });
      } else if (error.status === 403 && key?.[0] !== WHOAMI[0]) {
        // The caller's grant changed meanwhile: what it may do is read again.
        void clientRef.current?.invalidateQueries({ queryKey: WHOAMI });
      }
    };
    return new QueryClient({
      queryCache: new QueryCache({ onError: (error, query) => onError(error, query.queryKey) }),
      mutationCache: new MutationCache({ onError: (error) => onError(error, undefined) }),
      defaultOptions: { queries: { retry, refetchOnWindowFocus: false } },
    });
  });
  clientRef.current = client;
  return <QueryClientProvider client={client}>{children}</QueryClientProvider>;
}

export function Providers({
  children,
  ...session
}: { children: ReactNode } & Omit<SessionProviderProps, "children">) {
  return (
    <SettingsProvider>
      <SessionProvider {...session}>
        <Queries>
          <Gate>{children}</Gate>
        </Queries>
      </SessionProvider>
    </SettingsProvider>
  );
}

export function App() {
  const [router] = useState(() => createHashRouter(routes));
  return (
    <Providers>
      <RouterProvider router={router} />
    </Providers>
  );
}
