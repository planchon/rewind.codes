import { QueryCache, QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { type ReactNode, useRef, useState } from "react";
import {
  createHashRouter,
  Navigate,
  type RouteObject,
  RouterProvider,
  useRouteError,
} from "react-router";
import { ApiError } from "../api/client";
import { ProblemView } from "../components/bits";
import { FailuresView } from "../views/FailuresView";
import { OverviewView } from "../views/OverviewView";
import { RunsView } from "../views/RunsView";
import { RunView } from "../views/run/RunView";
import { ScriptsView } from "../views/ScriptsView";
import { ScriptView } from "../views/ScriptView";
import { ToolsView } from "../views/ToolsView";
import { WaitingView } from "../views/WaitingView";
import { Gate } from "./Gate";
import { Layout } from "./Layout";
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

export const routes: RouteObject[] = [
  {
    path: "/",
    element: <Layout />,
    errorElement: <RouteError />,
    children: [
      { index: true, element: <Navigate to="/overview" replace /> },
      { path: "overview", element: <OverviewView /> },
      { path: "runs", element: <RunsView /> },
      { path: "waiting", element: <WaitingView /> },
      { path: "failures", element: <FailuresView /> },
      { path: "runs/:id", element: <RunView /> },
      { path: "scripts", element: <ScriptsView /> },
      { path: "scripts/:digest", element: <ScriptView /> },
      { path: "tools", element: <ToolsView /> },
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
 * A query cache per key: what one key read is never shown under another, not even while the new
 * key's first answers are on their way (another tenant's runs, scripts or payloads).
 */
function Queries({ children }: { children: ReactNode }) {
  const { key } = useSession();
  return <KeyQueries key={key ?? ""}>{children}</KeyQueries>;
}

function KeyQueries({ children }: { children: ReactNode }) {
  const { signOut } = useSession();
  const signOutRef = useRef(signOut);
  signOutRef.current = signOut;
  const clientRef = useRef<QueryClient | null>(null);
  const [client] = useState(
    () =>
      new QueryClient({
        queryCache: new QueryCache({
          // A key revoked while the dashboard is open: back to the key form.
          onError: (error, query) => {
            if (
              error instanceof ApiError &&
              error.status === 401 &&
              query.queryKey[0] !== "server"
            ) {
              signOutRef.current();
              void clientRef.current?.invalidateQueries({ queryKey: ["server"] });
            }
          },
        }),
        defaultOptions: { queries: { retry, refetchOnWindowFocus: false } },
      }),
  );
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
