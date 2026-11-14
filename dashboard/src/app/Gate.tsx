// Nothing of the app is shown before the server has said who calls. With WorkOS, a member signs
// in on the sign-in pages, and one without an organization goes through the onboarding. Without
// WorkOS, the operator types a key; a server that needs none (the embedded one, behind a local
// proxy) lets the operator in at once.
import { useQuery } from "@tanstack/react-query";
import { KeyRound } from "lucide-react";
import {
  createContext,
  type FormEvent,
  type ReactNode,
  useContext,
  useMemo,
  useState,
} from "react";
import { ApiError } from "../api/client";
import { useWhoami } from "../api/organization";
import type { ServerInfo } from "../api/types";
import { Loading, ProblemView } from "../components/bits";
import { AccessContext, accessOf } from "./access";
import { Brand } from "./Brand";
import { Onboarding } from "./Onboarding";
import { SignIn } from "./SignIn";
import { useSession } from "./session";

const ServerContext = createContext<ServerInfo | null>(null);

export function useServer(): ServerInfo {
  const server = useContext(ServerContext);
  if (!server) throw new Error("useServer is used outside Gate");
  return server;
}

function Connecting() {
  return (
    <div className="gate">
      <Loading>Connecting to the server…</Loading>
    </div>
  );
}

function Unreachable({ error, retry }: { error: unknown; retry: () => void }) {
  const { phase, signOut } = useSession();
  return (
    <div className="gate">
      <div className="gate-card">
        <ProblemView error={error} title="The server cannot be reached" />
        <div className="gate-actions">
          <button type="button" className="btn" onClick={retry}>
            Try again
          </button>
          {phase.kind === "member" && (
            <button type="button" className="btn ghost" onClick={() => void signOut()}>
              Sign out
            </button>
          )}
        </div>
      </div>
    </div>
  );
}

export function Gate({ children }: { children: ReactNode }) {
  const { phase, retry } = useSession();
  switch (phase.kind) {
    case "starting":
      return <Connecting />;
    case "unreachable":
      return <Unreachable error={phase.error} retry={retry} />;
    case "key":
      return <Connected>{children}</Connected>;
    case "signed_out":
    case "step":
    case "waitlisted":
      return <SignIn />;
    case "member":
      return phase.state.organizationId ? <Connected>{children}</Connected> : <Onboarding />;
  }
}

/** The server's version and who calls, read before anything else is shown. */
function Connected({ children }: { children: ReactNode }) {
  const { api, phase, key } = useSession();
  const server = useQuery({
    queryKey: ["server"],
    queryFn: () => api.server(),
    staleTime: 60_000,
  });
  const whoami = useWhoami();
  const access = useMemo(() => (whoami.data ? accessOf(whoami.data) : null), [whoami.data]);

  const error = server.error ?? whoami.error;
  if (error) {
    if (phase.kind === "key" && error instanceof ApiError && error.status === 401) {
      return <KeyForm refused={key !== null ? error.problem.detail : null} />;
    }
    return (
      <Unreachable
        error={error}
        retry={() => {
          void server.refetch();
          void whoami.refetch();
        }}
      />
    );
  }
  if (!server.data || !access) return <Connecting />;
  return (
    <ServerContext.Provider value={server.data}>
      <AccessContext.Provider value={access}>{children}</AccessContext.Provider>
    </ServerContext.Provider>
  );
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
        <Brand />
        <h1>Sign in with an API key</h1>
        <p>
          The dashboard calls the API <code>/v1</code> with your key, and shows what its role
          allows. A key with the <code>reader</code> role is enough to read runs.
        </p>
        <label className="field col">
          <span className="field-label">API key</span>
          <input
            className="input mono"
            type="password"
            autoComplete="off"
            spellCheck={false}
            placeholder="rewind_sk_…"
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
