import { Lock } from "lucide-react";
import { type ReactNode, useState } from "react";
import {
  createBrowserRouter,
  Navigate,
  Outlet,
  type RouteObject,
  RouterProvider,
  useLocation,
  useParams,
  useRouteError,
} from "react-router";
import { ProblemView } from "../components/bits";
import { fieldTerm, joinQuery } from "../model/query";
import { useAccess } from "../state/access";
import { SessionProvider, type SessionProviderProps, SessionState } from "../state/session";
import { ActionsView } from "../views/ActionsView";
import { OverviewView } from "../views/OverviewView";
import { RunsView } from "../views/runs/RunsView";
import { WorkflowsView } from "../views/workflows/WorkflowsView";
import { Gate } from "./Gate";
import { Layout } from "./Layout";
import { NoPermission } from "./NoPermission";
import { adoptHashRoute, restoreRoute } from "./pendingRoute";
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

/**
 * The views drawn from the statistics, which come from counters that know no labels: refused to
 * a grant limited to labels (archi/05, "Statistics").
 */
function Statistics({ children }: { children: ReactNode }) {
  const access = useAccess();
  if (!access.statistics) {
    return (
      <div className="page">
        <div className="no-permission" role="alert">
          <h1>
            <Lock size={15} aria-hidden="true" /> Statistics span every run
          </h1>
          <p>
            Your grant only reaches the runs carrying its labels, and the statistics count every run
            of the organization. Search your runs in Runs.
          </p>
        </div>
      </div>
    );
  }
  return children;
}

/**
 * A link of an earlier version: `/waiting`, `/failures` and Observability's views open Runs with
 * their query, the namespace filter they named (`?ns=bob`) becoming the term `namespace:bob`;
 * Scripts, Testing and Tools open Workflows and Actions.
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

/** Observability's views, by their names then: every run, the active ones, waiting, failures. */
const OBSERVABILITY_VIEWS: Record<string, string> = {
  active: "/runs/running",
  waiting: "/runs/waiting",
  failures: "/runs/failures",
};

function MovedObservability() {
  const { view } = useParams();
  return <Moved to={OBSERVABILITY_VIEWS[view ?? ""] ?? "/runs"} />;
}

/** Testing's pages of a workflow and of a test, by the workflow's name: its page takes a name. */
function MovedTesting() {
  const { name, digest } = useParams();
  const path = `/workflows/${encodeURIComponent(name ?? "")}${digest ? `/tests/${digest}` : ""}`;
  return <Moved to={path} />;
}

/** The first view the caller may open. */
function Home() {
  const access = useAccess();
  if (access.can("runs:read"))
    return <Navigate to={access.statistics ? "/overview" : "/runs"} replace />;
  if (access.can("namespaces:read")) return <Navigate to="/namespaces" replace />;
  if (access.can("secrets:read")) return <Navigate to="/secrets" replace />;
  if (access.sections.length > 0) return <Navigate to="/organization" replace />;
  return <CannotRead />;
}

/** A run's page, loaded when one is opened: the debugger, its parser and its source view. */
const runView = async () => ({ Component: (await import("../views/run/RunView")).RunPage });

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
          {
            path: "overview",
            element: (
              <Statistics>
                <OverviewView />
              </Statistics>
            ),
          },
          // Runs: every run, the running ones, the waiting ones, the failures. A run's page is
          // `/runs/` and its handle (`namespace/origin/suffix`) or its UUID (`runPath`), which
          // never reads as these three words alone.
          { path: "runs", element: <RunsView /> },
          { path: "runs/running", element: <RunsView /> },
          { path: "runs/waiting", element: <RunsView /> },
          { path: "runs/failures", element: <RunsView /> },
          // One route for every section: the page stays mounted, and its event stream open.
          { path: "runs/*", lazy: runView },
          { path: "workflows", element: <WorkflowsView /> },
          {
            path: "workflows/:id",
            lazy: async () => ({
              Component: (await import("../views/workflows/WorkflowView")).WorkflowView,
            }),
          },
          {
            path: "workflows/:id/tests/:digest",
            lazy: async () => ({
              Component: (await import("../views/workflows/TestView")).TestView,
            }),
          },
          {
            path: "actions",
            element: (
              <Statistics>
                <ActionsView />
              </Statistics>
            ),
          },
          // The views of earlier versions.
          { path: "observability", element: <Moved to="/runs" /> },
          { path: "observability/:view", element: <MovedObservability /> },
          { path: "waiting", element: <Moved to="/runs/waiting" /> },
          { path: "failures", element: <Moved to="/runs/failures" /> },
          { path: "scripts", element: <Moved to="/workflows" /> },
          {
            path: "scripts/:digest",
            lazy: async () => ({
              Component: (await import("../views/workflows/WorkflowView")).WorkflowOfCode,
            }),
          },
          { path: "testing", element: <Moved to="/workflows" /> },
          { path: "testing/:name", element: <MovedTesting /> },
          { path: "testing/:name/tests/:digest", element: <MovedTesting /> },
          { path: "tools", element: <Moved to="/actions" /> },
        ],
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
      // The organization's secrets; a namespace's are on its page. The view checks `secrets:read`.
      {
        path: "secrets",
        lazy: async () => ({
          Component: (await import("../views/secrets/SecretsView")).SecretsView,
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
            path: "signing-keys",
            lazy: async () => ({
              Component: (await import("../views/org/SigningKeysView")).SigningKeysView,
            }),
          },
          {
            path: "connections",
            lazy: async () => ({
              Component: (await import("../views/org/ConnectionsView")).ConnectionsView,
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
      // Claude: Connect Claude, and the person's connections.
      {
        path: "connect",
        lazy: async () => ({
          Component: (await import("../views/claude/ConnectView")).ConnectView,
        }),
      },
      {
        path: "account/connections",
        lazy: async () => ({
          Component: (await import("../views/claude/AccountConnectionsView"))
            .AccountConnectionsView,
        }),
      },
      { path: "*", element: <NotFound /> },
    ],
  },
  // The consent of an MCP client, reached from `/oauth/authorize`: a page of its own, as a
  // sign-in is, outside the frame.
  {
    path: "/connect/authorize",
    errorElement: <RouteError />,
    lazy: async () => ({
      Component: (await import("../views/claude/ConsentView")).ConsentView,
    }),
  },
];

export function Providers({
  children,
  ...session
}: { children: ReactNode } & Omit<SessionProviderProps, "children">) {
  return (
    <SettingsProvider>
      <SessionProvider {...session}>
        <SessionState>
          <Gate>{children}</Gate>
        </SessionState>
      </SessionProvider>
    </SettingsProvider>
  );
}

export function App() {
  const [router] = useState(() => {
    // A link of an earlier version, its route in the fragment; then, back from GitHub on `/`,
    // the address the sign-in started from.
    adoptHashRoute();
    restoreRoute();
    return createBrowserRouter(routes);
  });
  return (
    <Providers>
      <RouterProvider router={router} />
    </Providers>
  );
}
