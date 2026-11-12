// Nothing is shown before the server has answered with the operator's key. A server that needs
// no key (the embedded one, behind a local proxy) lets the operator in at once.
import { useQuery } from "@tanstack/react-query";
import { KeyRound } from "lucide-react";
import { createContext, type FormEvent, type ReactNode, useContext, useState } from "react";
import { ApiError } from "../api/client";
import type { ServerInfo } from "../api/types";
import { Loading, ProblemView } from "../components/bits";
import { useSession } from "./session";

const ServerContext = createContext<ServerInfo | null>(null);

export function useServer(): ServerInfo {
  const server = useContext(ServerContext);
  if (!server) throw new Error("useServer is used outside Gate");
  return server;
}

export function Gate({ children }: { children: ReactNode }) {
  const { api, key } = useSession();
  const server = useQuery({
    queryKey: ["server", key],
    queryFn: () => api.server(),
    staleTime: 60_000,
  });

  if (server.isPending) {
    return (
      <div className="gate">
        <Loading>Connecting to the server…</Loading>
      </div>
    );
  }
  if (server.isError) {
    const error = server.error;
    if (error instanceof ApiError && error.status === 401) {
      return <KeyForm refused={key !== null ? error.problem.detail : null} />;
    }
    return (
      <div className="gate">
        <div className="gate-card">
          <ProblemView error={error} title="The server cannot be reached" />
          <div>
            <button type="button" className="btn" onClick={() => void server.refetch()}>
              Try again
            </button>
          </div>
        </div>
      </div>
    );
  }
  return <ServerContext.Provider value={server.data}>{children}</ServerContext.Provider>;
}

export function KeyForm({ refused }: { refused: string | null }) {
  const { signIn } = useSession();
  const [value, setValue] = useState("");
  const [remember, setRemember] = useState(false);

  const submit = (e: FormEvent) => {
    e.preventDefault();
    if (value.trim()) signIn(value, remember);
  };

  return (
    <div className="gate">
      <form className="gate-card" onSubmit={submit}>
        <div className="brand">
          <span className="brand-mark">AP</span>
          <span className="brand-name">ai-ptc</span>
        </div>
        <h1>Sign in with an API key</h1>
        <p>
          The dashboard reads the API <code>/v1</code> with your key, and shows what its tenant may
          read. A key with the <code>reader</code> role is enough: the dashboard never changes a
          run.
        </p>
        <label className="field" style={{ flexDirection: "column", alignItems: "stretch", gap: 4 }}>
          <span className="field-label">API key</span>
          <input
            className="input mono"
            type="password"
            autoComplete="off"
            spellCheck={false}
            placeholder="ptc_sk_…"
            value={value}
            onChange={(e) => setValue(e.target.value)}
            // biome-ignore lint/a11y/noAutofocus: the only field of the page
            autoFocus
          />
        </label>
        <label className="check">
          <input
            type="checkbox"
            checked={remember}
            onChange={(e) => setRemember(e.target.checked)}
          />
          Remember on this device
        </label>
        {refused && (
          <div className="gate-error" role="alert">
            The server refused this key. {refused}
          </div>
        )}
        <div>
          <button type="submit" className="btn primary" disabled={!value.trim()}>
            <KeyRound size={13} /> Continue
          </button>
        </div>
      </form>
    </div>
  );
}
