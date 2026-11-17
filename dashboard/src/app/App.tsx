import { MutationCache, QueryCache, QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { Lock } from "lucide-react";
import { type ReactNode, useRef, useState } from "react";
import {
  createHashRouter,
  Navigate,
  Outlet,
  type RouteObject,
  RouterProvider,
  useLocation,
  useRouteError,
} from "react-router";
import { ApiError } from "../api/client";
import { WHOAMI } from "../api/organization";
import { ProblemView } from "../components/bits";
import { fieldTerm, joinQuery } from "../model/query";
import { OverviewView } from "../views/OverviewView";
import { ObservabilityView } from "../views/observability/ObservabilityView";
import { ScriptsView } from "../views/ScriptsView";
import { ToolsView } from "../views/ToolsView";
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

/**
 * A link of an earlier version: `#/runs`, `#/waiting` and `#/failures` open Observability with their
 * query, and the namespace filter they named (`?ns=bob`) becomes the term `namespace:bob`.
 */
function Moved({ to }: { to: string }) {
  const { search } = useLocation();
  const params = new URLSearchParams(search);
  const ns = params.get("ns");
  params.delete("ns");
  if (ns) params.set("q", joinQuery(params.get("q"), fieldTerm("namespace", ns)));
  const rest = params.toString();
  return <Navigate to={rest ? `${to}?${rest}` : to} replace />;
}

/** The first view the caller may open. */
function Home() {
  const access = useAccess();
  if (access.can("runs:read")) return <Navigate to="/overview" replace />;
  if (access.can("namespaces:read")) return <Navigate to="/namespaces" replace />;
  if (access.sections.length > 0) return <Navigate to="/organization" replace />;
  return <CannotRead />;
}

/** A run's page, loaded when one is opened: the debugger, its parser and its source view. */
const runView = async () => ({ Component: (await import("../views/run/RunView")).RunView });

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
          { path: "observability", element: <ObservabilityView /> },
          { path: "observability/:view", element: <ObservabilityView /> },
          { path: "runs", element: <Moved to="/observability" /> },
          { path: "waiting", element: <Moved to="/observability/waiting" /> },
          { path: "failures", element: <Moved to="/observability/failures" /> },
          // One route for every section: the page stays mounted, and its event stream open.
          { path: "runs/:id/:section?", lazy: runView },
          { path: "scripts", element: <ScriptsView /> },
          {
            path: "scripts/:digest",
            lazy: async () => ({ Component: (await import("../views/ScriptView")).ScriptView }),
          },
          { path: "tools", element: <ToolStats /> },
        ],
      },
      // Testing reads the workflows with the toolsets (`toolsets:read`): its views check it.
      {
        path: "testing",
        lazy: async () => ({
          Component: (await import("../views/testing/TestingView")).TestingView,
        }),
      },
      {
        path: "testing/:name",
        lazy: async () => ({
          Component: (await import("../views/testing/WorkflowView")).WorkflowView,
        }),
      },
      {
        path: "testing/:name/tests/:digest",
        lazy: async () => ({
          Component: (await import("../views/testing/TestView")).TestView,
        }),
      },
      {
        path: "namespaces",
        lazy: async () => ({
          Component: (await import("../views/namespaces/NamespacesView")).NamespacesView,
        }),
      },
      {
        path: "namespaces/:name",
        lazy: async () => ({
          Component: (await import("../views/namespaces/NamespaceView")).NamespaceView,
        }),
      },
      {
        path: "organization",
        lazy: async () => ({
          Component: (await import("../views/org/OrganizationView")).OrganizationView,
        }),
        children: [
          {
            index: true,
            lazy: async () => ({
              Component: (await import("../views/org/OrganizationView")).OrganizationIndex,
            }),
          },
          {
            path: "members",
            lazy: async () => ({
              Component: (await import("../views/org/MembersView")).MembersView,
            }),
          },
          {
            path: "invitations",
            lazy: async () => ({
              Component: (await import("../views/org/InvitationsView")).InvitationsView,
            }),
          },
          {
            path: "roles",
            lazy: async () => ({ Component: (await import("../views/org/RolesView")).RolesView }),
          },
          {
            path: "api-keys",
            lazy: async () => ({
              Component: (await import("../views/org/ApiKeysView")).ApiKeysView,
            }),
          },
          {
            path: "settings",
            lazy: async () => ({
              Component: (await import("../views/org/SettingsView")).SettingsView,
            }),
          },
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
