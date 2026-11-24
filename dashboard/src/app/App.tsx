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
import { isNamespaceName, scopedPath } from "../state/scope";
import { SessionProvider, type SessionProviderProps, SessionState } from "../state/session";
import { OverviewView } from "../views/OverviewView";
import { ObservabilityView } from "../views/observability/ObservabilityView";
import { ScriptsView } from "../views/ScriptsView";
import { ToolsView } from "../views/ToolsView";
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
            of the organization. Search your runs in Observability.
          </p>
        </div>
      </div>
    );
  }
  return children;
}

/**
 * A link of an earlier version: `/runs`, `/waiting` and `/failures` open Observability with their
 * query, in the scope of the namespace they named (`?ns=bob`).
 */
function Moved({ to }: { to: string }) {
  const { search } = useLocation();
  const params = new URLSearchParams(search);
  const ns = params.get("ns");
  params.delete("ns");
  const path = scopedPath(to, ns && isNamespaceName(ns) ? ns : null);
  const rest = params.toString();
  return <Navigate to={rest ? `${path}?${rest}` : path} replace />;
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
    return <Navigate to={at(access.statistics ? "/overview" : "/observability")} replace />;
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
        { path: "observability", element: <ObservabilityView /> },
        { path: "observability/:view", element: <ObservabilityView /> },
        {
          path: "scripts",
          element: (
            <Statistics>
              <ScriptsView />
            </Statistics>
          ),
        },
        {
          path: "tools",
          element: (
            <Statistics>
              <ToolsView />
            </Statistics>
          ),
        },
      ],
    },
    // Testing reads the workflows with the toolsets (`toolsets:read`): its views check it.
    {
      path: "testing",
      lazy: async () => ({
        Component: (await import("../views/testing/TestingView")).TestingView,
      }),
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
      {
        element: <Reading />,
        children: [
          { path: "runs", element: <Moved to="/observability" /> },
          { path: "waiting", element: <Moved to="/observability/waiting" /> },
          { path: "failures", element: <Moved to="/observability/failures" /> },
          // One route for every section: the page stays mounted, and its event stream open. A run
          // is named by its UUID or by its handle, `namespace/origin/suffix` (`runPath`).
          { path: "runs/*", lazy: runView },
          {
            path: "scripts/:digest",
            lazy: async () => ({ Component: (await import("../views/ScriptView")).ScriptView }),
          },
        ],
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
