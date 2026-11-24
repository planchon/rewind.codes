// The dashboard's session, and the state of each identity it signs in.
//
// A server with WorkOS (`GET /auth/config` answers) signs members in on the dashboard's pages
// through `/auth`: the session lives in a cookie only `/auth` reads, and the access token sent to
// `/v1` lives in memory only. It is asked for at load, again a minute before it expires, and once
// when `/v1` refuses it. A server without WorkOS (404) takes an API key, typed by the operator and
// kept for the tab (sessionStorage), or on this device when asked (localStorage).
//
// Each identity (a key, or a member in an organization) gets a state of its own (`SessionState`):
// what one of them read is never shown to another. Who calls (`GET /v1/whoami`) is read again
// when a write may have changed it, and when the server refuses a permission the caller thought
// it held.
import { useQuery } from "@tanstack/react-query";
import {
  createContext,
  type ReactNode,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { read, write } from "../model/storage";
import type { AuthConfig, Authenticated, AuthState, Identity, ServerInfo } from "../model/types";
import { Api, ApiError, type AuthAnswer } from "../sources/client";
import { type Action, useAction } from "./actions";
import { type Read, useQueryRead } from "./read";
import { type Failure, StateProvider, type Store, useStore } from "./store";

const KEY = "rewind.key";
/** A fresh access token is asked for this long before the current one expires. */
export const REFRESH_MARGIN_MS = 60_000;
/** The least wait before asking again when the server kept the same token. */
const SAME_TOKEN_WAIT_MS = 10_000;
/** The wait before asking again when the server could not be reached. */
const RETRY_WAIT_MS = 10_000;
/** The query parameter of an invitation link: `https://app.rewind.codes/?invitation_token=…`. */
const INVITATION_PARAM = "invitation_token";

/** A step of a sign-in: anything but signed in. */
export type Step = Exclude<AuthState, { status: "authenticated" }>;

export type AuthPhase =
  /** `GET /auth/config` and the first `POST /auth/session` are on their way. */
  | { kind: "starting" }
  | { kind: "unreachable"; error: unknown }
  /** No WorkOS on this server: an API key. */
  | { kind: "key" }
  | { kind: "signed_out"; config: AuthConfig; notice: string | null }
  | { kind: "step"; config: AuthConfig; state: Step }
  | { kind: "waitlisted"; config: AuthConfig; email: string }
  | {
      kind: "member";
      config: AuthConfig;
      state: Authenticated;
      /** The server's clock minus the browser's. */
      skewMs: number;
      /** The last answer kept the token it already had. */
      unchanged: boolean;
    };

export interface Session {
  phase: AuthPhase;
  /** The API key, without WorkOS. */
  key: string | null;
  /** What the state belongs to: a key, or a user in an organization. */
  identity: string;
  /** The token of the invitation link the page was opened with, kept through the sign-in. */
  invitationToken: string | null;
  /** Uses a key; `remember` keeps it on this device. */
  signIn(key: string, remember: boolean): void;
  /** Forgets the key, or signs the member out (`POST /auth/logout`). */
  signOut(): Promise<void>;
  /** Switches the session to another organization of the member. */
  switchOrganization(organizationId: string): Promise<void>;
  /** Back to the first page of the sign-in. */
  backToSignIn(): void;
  /** Leaves for GitHub, through `/auth/github`. */
  signInWithGithub(): void;
  /** Leaves the dashboard for another address (an MCP client's redirect after the consent). */
  assign(url: string): void;
  /** Asks the server again after it could not be reached. */
  retry(): void;
}

/** The session as `state/` uses it. */
interface SessionWorks extends Session {
  api: Api;
  /** `/v1` refused the credentials, even renewed: back to signing in. */
  unauthorized(): void;
  /** Applies the answer of a sign-in step. */
  advance(answer: AuthAnswer): void;
  /** The email went on the waitlist. */
  waitlisted(email: string): void;
}

const SessionContext = createContext<SessionWorks | null>(null);

function storedKey(): string | null {
  return read("session", KEY) ?? read("local", KEY);
}

function readInvitationToken(): string | null {
  try {
    return new URLSearchParams(window.location.search).get(INVITATION_PARAM);
  } catch {
    return null;
  }
}

/**
 * The reason a sign-in with GitHub failed, which `/auth/callback` puts in the address
 * (`/?auth_error=…`), as a sentence for the sign-in page. Read once, then taken out of the
 * address.
 */
const AUTH_ERROR_PARAM = "auth_error";

function takeAuthError(): string | null {
  try {
    const url = new URL(window.location.href);
    const code = url.searchParams.get(AUTH_ERROR_PARAM);
    if (code === null) return null;
    url.searchParams.delete(AUTH_ERROR_PARAM);
    window.history.replaceState(window.history.state, "", url.toString());
    switch (code) {
      case "state_mismatch":
        return "The sign-in did not start on this page, or took too long: sign in again.";
      case "provider_error":
        return "GitHub or WorkOS refused the sign-in: try again, or sign in with a code by email. New to Rewind? Join the waitlist below.";
      default:
        return "The sign-in with GitHub did not go through: try again, or sign in with a code by email. New to Rewind? Join the waitlist below.";
    }
  } catch {
    return null;
  }
}

/** Takes the invitation's token out of the address, once it has served. */
function dropInvitationToken(): void {
  try {
    const url = new URL(window.location.href);
    if (!url.searchParams.has(INVITATION_PARAM)) return;
    url.searchParams.delete(INVITATION_PARAM);
    window.history.replaceState(window.history.state, "", url.toString());
  } catch {
    // The address stays as it is.
  }
}

function identityOf(phase: AuthPhase, key: string | null): string {
  if (phase.kind === "key") return `key:${key ?? ""}`;
  if (phase.kind === "member")
    return `member:${phase.state.person.id}@${phase.state.organizationId ?? ""}`;
  return "";
}

/** When to ask for a fresh token, in ms from now. */
export function refreshDelay(
  expiresAt: string,
  skewMs: number,
  unchanged: boolean,
  now = Date.now(),
): number {
  let delay = Date.parse(expiresAt) - REFRESH_MARGIN_MS - (now + skewMs);
  if (unchanged) delay = Math.max(delay, SAME_TOKEN_WAIT_MS);
  return Math.max(delay, 1_000);
}

export interface SessionProviderProps {
  children: ReactNode;
  /** The origin of the API; the page's own by default. */
  baseUrl?: string;
  fetch?: (request: Request) => Promise<Response>;
  /** Leaves the page for another address (GitHub's sign-in); `location.assign` by default. */
  assign?: (url: string) => void;
}

export function SessionProvider({ children, baseUrl, fetch, assign }: SessionProviderProps) {
  const [key, setKey] = useState<string | null>(storedKey);
  const [phase, setPhaseState] = useState<AuthPhase>({ kind: "starting" });
  const [invitationToken, setInvitationToken] = useState(readInvitationToken);
  const [authError] = useState(takeAuthError);
  const [attempt, setAttempt] = useState(0);

  const phaseRef = useRef(phase);
  const keyRef = useRef(key);
  /** The Bearer token `/v1` gets: the key, or the access token. */
  const tokenRef = useRef<string | null>(null);
  const configRef = useRef<AuthConfig | null>(null);
  const renewRef = useRef<() => Promise<boolean>>(async () => false);
  /** Changes with every step the member takes: an older refresh's answer is then dropped. */
  const generation = useRef(0);
  const inflight = useRef<Promise<boolean> | null>(null);
  const retryTimer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);

  const setPhase = useCallback((next: AuthPhase) => {
    phaseRef.current = next;
    if (next.kind === "member") tokenRef.current = next.state.accessToken;
    else if (next.kind === "key") tokenRef.current = keyRef.current;
    else tokenRef.current = null;
    setPhaseState(next);
  }, []);

  const api = useMemo(
    () =>
      new Api({
        baseUrl: baseUrl ?? window.location.origin,
        token: () => tokenRef.current,
        renew: () => renewRef.current(),
        ...(fetch ? { fetch } : {}),
      }),
    [baseUrl, fetch],
  );

  const signedOut = useCallback(
    (notice: string | null) => {
      const config = configRef.current;
      if (config) setPhase({ kind: "signed_out", config, notice });
    },
    [setPhase],
  );

  const apply = useCallback(
    ({ state, skewMs }: AuthAnswer) => {
      const config = configRef.current;
      if (!config) return;
      if (state.status === "authenticated") {
        const before = phaseRef.current;
        const unchanged =
          before.kind === "member" && before.state.accessToken === state.accessToken;
        setPhase({ kind: "member", config, state, skewMs, unchanged });
      } else {
        setPhase({ kind: "step", config, state });
      }
    },
    [setPhase],
  );

  /** Asks `POST /auth/session` for the state, the token refreshed when it is about to expire. */
  const refresh = useCallback((): Promise<boolean> => {
    if (inflight.current) return inflight.current;
    const started = generation.current;
    clearTimeout(retryTimer.current);
    const asked = api
      .session()
      .then(
        (answer) => {
          if (generation.current !== started) return false;
          apply(answer);
          return answer.state.status === "authenticated";
        },
        (error: unknown) => {
          if (generation.current !== started) return false;
          if (error instanceof ApiError && error.status === 401) {
            generation.current += 1;
            signedOut("Your session has ended: sign in again.");
            return false;
          }
          // Not reached: the token holds to its expiry; ask again soon.
          retryTimer.current = setTimeout(() => {
            if (phaseRef.current.kind === "member") void refresh();
          }, RETRY_WAIT_MS);
          return false;
        },
      )
      .finally(() => {
        inflight.current = null;
      });
    inflight.current = asked;
    return asked;
  }, [api, apply, signedOut]);

  renewRef.current = () =>
    phaseRef.current.kind === "member" ? refresh() : Promise.resolve(false);

  // At load: WorkOS or a key; then the session the cookie holds, if any.
  // biome-ignore lint/correctness/useExhaustiveDependencies: `attempt` asks again
  useEffect(() => {
    let cancelled = false;
    setPhase({ kind: "starting" });
    (async () => {
      let config: AuthConfig | null;
      try {
        config = await api.authConfig();
      } catch (error) {
        if (!cancelled) setPhase({ kind: "unreachable", error });
        return;
      }
      if (cancelled) return;
      if (config === null) {
        setPhase({ kind: "key" });
        return;
      }
      configRef.current = config;
      try {
        const answer = await api.session();
        if (!cancelled) apply(answer);
      } catch (error) {
        if (cancelled) return;
        if (error instanceof ApiError && error.status === 401) signedOut(authError);
        else setPhase({ kind: "unreachable", error });
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [api, apply, setPhase, signedOut, attempt, authError]);

  // A minute before the access token expires, a fresh one.
  useEffect(() => {
    if (phase.kind !== "member") return;
    const delay = refreshDelay(phase.state.expiresAt, phase.skewMs, phase.unchanged);
    const timer = setTimeout(() => void refresh(), delay);
    return () => clearTimeout(timer);
  }, [phase, refresh]);

  // Signed in: the invitation's token has served.
  useEffect(() => {
    if (phase.kind !== "member") return;
    dropInvitationToken();
    setInvitationToken(null);
  }, [phase.kind]);

  useEffect(() => () => clearTimeout(retryTimer.current), []);

  const signIn = useCallback((next: string, remember: boolean) => {
    const trimmed = next.trim();
    write("session", KEY, trimmed);
    write("local", KEY, remember ? trimmed : null);
    keyRef.current = trimmed;
    setKey(trimmed);
    if (phaseRef.current.kind === "key") tokenRef.current = trimmed;
  }, []);

  const forgetKey = useCallback(() => {
    write("session", KEY, null);
    write("local", KEY, null);
    keyRef.current = null;
    setKey(null);
    if (phaseRef.current.kind === "key") tokenRef.current = null;
  }, []);

  const signOut = useCallback(async () => {
    if (phaseRef.current.kind === "key") {
      forgetKey();
      return;
    }
    generation.current += 1;
    await api.signOut();
    signedOut(null);
  }, [api, forgetKey, signedOut]);

  const unauthorized = useCallback(() => {
    if (phaseRef.current.kind === "key") {
      forgetKey();
      return;
    }
    generation.current += 1;
    signedOut("Your session has ended: sign in again.");
  }, [forgetKey, signedOut]);

  const advance = useCallback(
    (answer: AuthAnswer) => {
      generation.current += 1;
      apply(answer);
    },
    [apply],
  );

  const switchOrganization = useCallback(
    async (organizationId: string) => {
      generation.current += 1;
      const answer = await api.session(organizationId);
      // A refresh asked while the switch was on its way may answer for the previous organization.
      generation.current += 1;
      apply(answer);
    },
    [api, apply],
  );

  const waitlisted = useCallback(
    (email: string) => {
      const config = configRef.current;
      if (config) setPhase({ kind: "waitlisted", config, email });
    },
    [setPhase],
  );

  const backToSignIn = useCallback(() => signedOut(null), [signedOut]);

  const leave = useCallback(
    (url: string) => (assign ?? ((to: string) => window.location.assign(to)))(url),
    [assign],
  );

  const signInWithGithub = useCallback(
    () => leave(api.githubUrl(invitationToken)),
    [api, leave, invitationToken],
  );

  const retry = useCallback(() => setAttempt((n) => n + 1), []);

  keyRef.current = key;
  const identity = identityOf(phase, key);
  const value = useMemo<SessionWorks>(
    () => ({
      api,
      phase,
      key,
      identity,
      invitationToken,
      signIn,
      signOut,
      unauthorized,
      advance,
      switchOrganization,
      waitlisted,
      backToSignIn,
      signInWithGithub,
      assign: leave,
      retry,
    }),
    [
      api,
      phase,
      key,
      identity,
      invitationToken,
      signIn,
      signOut,
      unauthorized,
      advance,
      switchOrganization,
      waitlisted,
      backToSignIn,
      signInWithGithub,
      leave,
      retry,
    ],
  );
  return <SessionContext.Provider value={value}>{children}</SessionContext.Provider>;
}

function useWorks(): SessionWorks {
  const session = useContext(SessionContext);
  if (!session) throw new Error("useSession is used outside SessionProvider");
  return session;
}

export function useSession(): Session {
  return useWorks();
}

/** `rewind_sk_…a1b2`: enough to tell keys apart, not enough to use one. */
export function maskKey(key: string): string {
  const prefix = key.startsWith("rewind_sk_") ? "rewind_sk_" : "";
  return `${prefix}…${key.slice(-4)}`;
}

// Who calls

const SERVER = ["server"] as const;
const WHOAMI = ["whoami"] as const;

/** The server's version and roles, and what it says of `/mcp`. */
export function useServerInfo(): Read<ServerInfo> {
  const { api } = useStore();
  return useQueryRead(
    useQuery({ queryKey: SERVER, queryFn: () => api.server(), staleTime: 60_000 }),
  );
}

/** Who calls, with what it may do: read before anything else is shown. */
export function useWhoami(): Read<Identity> {
  const { api } = useStore();
  return useQueryRead(
    useQuery({
      queryKey: WHOAMI,
      queryFn: ({ signal }) => api.whoami(signal),
      staleTime: 30_000,
    }),
  );
}

/** Reads who calls again, after a write that may change its grant or its organization. */
export function refreshWhoami(store: Store): Promise<void> {
  return store.queryClient.invalidateQueries({ queryKey: WHOAMI });
}

/**
 * The state of the identity signed in: a new one for each key, or member and organization, so
 * that nothing one of them read shows to another, not even while the next one's first answers
 * are on their way (another tenant's runs, scripts or payloads).
 */
export function SessionState({ children }: { children: ReactNode }) {
  const session = useWorks();
  const sessionRef = useRef(session);
  sessionRef.current = session;
  const { api, phase, key, identity } = session;
  const token =
    phase.kind === "member" ? phase.state.accessToken : phase.kind === "key" ? key : null;
  const [onError] = useState(() => (error: unknown, failure: Failure) => {
    if (!(error instanceof ApiError)) return;
    const read = failure.key?.[0];
    if (error.status === 401) {
      // The key form says itself that the key was refused.
      if (sessionRef.current.phase.kind === "key" && (read === SERVER[0] || read === WHOAMI[0]))
        return;
      // Refused even with a renewed token, or a key revoked while the dashboard is open.
      sessionRef.current.unauthorized();
    } else if (error.status === 403 && read !== WHOAMI[0]) {
      // The caller's grant changed meanwhile: what it may do is read again.
      void refreshWhoami(failure.store);
    }
  });
  return (
    <StateProvider key={identity} api={api} token={token} onError={onError}>
      {children}
    </StateProvider>
  );
}

// The sign-in's steps

/**
 * Sends a 6-digit code to the email; answers whether it was sent. While sign-ups are closed, an
 * email without an account gets none: the session goes to the waitlist's step.
 */
export function useSendCode(): Action<string, boolean> {
  const { api, invitationToken, advance } = useWorks();
  return useAction(async (email: string) => {
    const answer = await api.sendMagicCode(email, invitationToken);
    if (answer === null) return true;
    advance(answer);
    return false;
  });
}

export function useVerifyCode(): Action<{ email: string; code: string }> {
  const { api, invitationToken, advance } = useWorks();
  return useAction(async ({ email, code }: { email: string; code: string }) => {
    advance(await api.verifyMagicCode(email, code, invitationToken));
  });
}

export function useVerifyEmail(): Action<string> {
  const { api, advance } = useWorks();
  return useAction(async (code: string) => {
    advance(await api.verifyEmail(code));
  });
}

/** Signs in to one of the organizations the member belongs to. */
export function useChooseOrganization(): Action<string> {
  const { api, advance } = useWorks();
  return useAction(async (organizationId: string) => {
    advance(await api.selectOrganization(organizationId));
  });
}

export function useJoinWaitlist(): Action<string> {
  const { api, waitlisted } = useWorks();
  return useAction(async (email: string) => {
    await api.joinWaitlist(email);
    waitlisted(email);
  });
}

export function useSignOut(): Action<void> {
  const { signOut } = useWorks();
  return useAction(() => signOut());
}

/**
 * Switches the session to another organization of the member. `then` runs right after the
 * switch, before the views of the new organization replace the ones that asked.
 */
export function useSwitchOrganization(then: () => void = () => {}): Action<string> {
  const { switchOrganization } = useWorks();
  return useAction(async (organizationId: string) => {
    await switchOrganization(organizationId);
    then();
  });
}
