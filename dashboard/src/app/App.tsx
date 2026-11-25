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
import { useAccess } from "../state/access";
import { isNamespaceName, scopedPath, splitScope } from "../state/scope";
import { SessionProvider, type SessionProviderProps, SessionState } from "../state/session";
import { ActionsView } from "../views/ActionsView";
import { OverviewView } from "../views/OverviewView";
import { RunsView } from "../views/runs/RunsView";
import { WorkflowsView } from "../views/workflows/WorkflowsView";
import { Gate } from "./Gate";
import { Layout } from "./Layout";
import { NoPermission } from "./NoPermission";
import { adoptHashRoute, restoreRoute } from "./pendingRoute";
import { NamespaceScope, useAt } from "./scope";
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
 * their query, Scripts, Testing and Tools open Workflows and Actions, in the scope of the
 * namespace they named (`?ns=bob`), else in the scope of the route.
 */
function Moved({ to }: { to: string }) {
  const { search } = useLocation();
  const at = useAt();
  const params = new URLSearchParams(search);
  const ns = params.get("ns");
  params.delete("ns");
  const path = ns && isNamespaceName(ns) ? scopedPath(to, ns) : at(to);
  const rest = params.toString();
  return <Navigate to={rest ? `${path}?${rest}` : path} replace />;
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

/** Runs; at a link of an earlier version that named its namespace (`/runs?ns=bob`), its scope. */
function RunsAt() {
  const { pathname, search } = useLocation();
  if (new URLSearchParams(search).has("ns")) return <Moved to={splitScope(pathname).path} />;
  return <RunsView />;
}

/**
 * The page of one object, a run or a workflow, asked for in a scope: it has one address, out of
 * the scope, and shows its namespace there.
 */
function OutOfScope() {
  const { pathname, search } = useLocation();
  return <Navigate to={`${splitScope(pathname).path}${search}`} replace />;
}

/** A namespace's page of an earlier version (`/namespaces/bob`): its scope, at its Overview. */
function NamespaceMoved() {
  const { name = "" } = useParams();
  const access = useAccess();
  if (!isNamespaceName(name)) return <NotFound />;
  const view = access.can("runs:read") && access.statistics ? "/overview" : "/usage";
  return <Navigate to={scopedPath(view, name)} replace />;
}

/** The first view the caller may open, in the scope of the route. */
function Home() {
  const access = useAccess();
  const at = useAt();
  const scoped = at("/") !== "/";
  if (access.can("runs:read"))
    return <Navigate to={at(access.statistics ? "/overview" : "/runs")} replace />;
  if (access.can("namespaces:read"))
    return <Navigate to={scoped ? at("/usage") : "/namespaces"} replace />;
  if (access.can("secrets:read")) return <Navigate to={at("/secrets")} replace />;
  if (access.sections.length > 0) return <Navigate to="/organization" replace />;
  return <CannotRead />;
}

/** A run's page, loaded when one is opened: the debugger, its parser and its source view. */
const runView = async () => ({ Component: (await import("../views/run/RunView")).RunPage });

/**
 * The views of a scope (archi/05, "Namespaces"): the same over every namespace (at `/`) and in one
 * namespace (under `/ns/:namespace`), which their reads follow.
 */
function scopeViews(): RouteObject[] {
  return [
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
        { path: "runs", element: <RunsAt /> },
        { path: "runs/running", element: <RunsAt /> },
        { path: "runs/waiting", element: <RunsAt /> },
        { path: "runs/failures", element: <RunsAt /> },
        { path: "workflows", element: <WorkflowsView /> },
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
        { path: "scripts", element: <Moved to="/workflows" /> },
        { path: "testing", element: <Moved to="/workflows" /> },
        { path: "tools", element: <Moved to="/actions" /> },
      ],
    },
    // The organization's secrets, or a namespace's then the organization's. The view checks
    // `secrets:read`.
    {
      path: "secrets",
      lazy: async () => ({
        Component: (await import("../views/secrets/SecretsView")).SecretsView,
      }),
    },
  ];
}

export const routes: RouteObject[] = [
  {
    path: "/",
    element: <Layout />,
    errorElement: <RouteError />,
    children: [
      { index: true, element: <Home /> },
      ...scopeViews(),
      // The pages of one object: one address each, out of any scope.
      {
        element: <Reading />,
        children: [
          // One route for every section: the page stays mounted, and its event stream open.
          { path: "runs/*", lazy: runView },
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
          // The pages of earlier versions.
          { path: "waiting", element: <Moved to="/runs/waiting" /> },
          { path: "failures", element: <Moved to="/runs/failures" /> },
          {
            path: "scripts/:digest",
            lazy: async () => ({
              Component: (await import("../views/workflows/WorkflowView")).WorkflowOfCode,
            }),
          },
          { path: "testing/:name", element: <MovedTesting /> },
          { path: "testing/:name/tests/:digest", element: <MovedTesting /> },
        ],
      },
      {
        path: "namespaces",
        lazy: async () => ({
          Component: (await import("../views/namespaces/NamespacesView")).NamespacesView,
        }),
      },
      { path: "namespaces/:name", element: <NamespaceMoved /> },
      // One namespace's scope: the same views, and its Usage in place of Namespaces.
      {
        path: "ns/:namespace",
        element: <NamespaceScope />,
        children: [
          { index: true, element: <Home /> },
          ...scopeViews(),
          {
            path: "usage",
            lazy: async () => ({
              Component: (await import("../views/namespaces/NamespaceView")).NamespaceView,
            }),
          },
          { path: "runs/*", element: <OutOfScope /> },
          { path: "workflows/*", element: <OutOfScope /> },
          { path: "*", element: <NotFound /> },
        ],
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
