// The operator's session: the API key and the client that sends it. The key designates the
// tenant; everything the dashboard shows is what that key may read. It is kept for the browser
// tab (sessionStorage), or on this device when the operator asks (localStorage).
import {
  createContext,
  type ReactNode,
  useCallback,
  useContext,
  useMemo,
  useRef,
  useState,
} from "react";
import { Api } from "../api/client";
import { read, write } from "./storage";

const KEY = "rewind.key";

export interface Session {
  api: Api;
  key: string | null;
  /** Uses a key; `remember` keeps it on this device. */
  signIn(key: string, remember: boolean): void;
  /** Forgets the key. */
  signOut(): void;
}

const SessionContext = createContext<Session | null>(null);

function storedKey(): string | null {
  return read("session", KEY) ?? read("local", KEY);
}

export interface SessionProviderProps {
  children: ReactNode;
  /** The origin of the API; the page's own by default. */
  baseUrl?: string;
  fetch?: (request: Request) => Promise<Response>;
}

export function SessionProvider({ children, baseUrl, fetch }: SessionProviderProps) {
  const [key, setKey] = useState<string | null>(storedKey);
  const keyRef = useRef(key);
  keyRef.current = key;

  const api = useMemo(
    () =>
      new Api({
        baseUrl: baseUrl ?? window.location.origin,
        key: () => keyRef.current,
        ...(fetch ? { fetch } : {}),
      }),
    [baseUrl, fetch],
  );

  const signIn = useCallback((next: string, remember: boolean) => {
    const trimmed = next.trim();
    write("session", KEY, trimmed);
    write("local", KEY, remember ? trimmed : null);
    keyRef.current = trimmed;
    setKey(trimmed);
  }, []);

  const signOut = useCallback(() => {
    write("session", KEY, null);
    write("local", KEY, null);
    keyRef.current = null;
    setKey(null);
  }, []);

  const value = useMemo(() => ({ api, key, signIn, signOut }), [api, key, signIn, signOut]);
  return <SessionContext.Provider value={value}>{children}</SessionContext.Provider>;
}

export function useSession(): Session {
  const session = useContext(SessionContext);
  if (!session) throw new Error("useSession is used outside SessionProvider");
  return session;
}

export function useApi(): Api {
  return useSession().api;
}

/** `rewind_sk_…a1b2`: enough to tell keys apart, not enough to use one. */
export function maskKey(key: string): string {
  const prefix = key.startsWith("rewind_sk_") ? "rewind_sk_" : "";
  return `${prefix}…${key.slice(-4)}`;
}
