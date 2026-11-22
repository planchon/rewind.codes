// Connections (archi/06, "Connections"): the MCP clients a person let act for them, Claude above
// all, each opening the organizations they ticked; the connections of the organization's members
// that open it; and the consent of an MCP client (archi/07, "MCP and OAuth"). A connection
// changed, revoked, or taken out of the organization shows at once, and comes back if the server
// refuses; both lists are read again after each write. Connect Claude asks every 2 s, while it is
// open, whether a connection arrived.
import { useQuery } from "@tanstack/react-query";
import { useEffect, useState, useSyncExternalStore } from "react";
import { arrivedSince, type Snapshot, snapshotOf } from "../model/connections";
import type {
  Connection,
  ConsentAnswer,
  ConsentDecision,
  ConsentRequest,
  UpdateConnection,
} from "../model/types";
import { type Action, useAction } from "./actions";
import { heldList, keyedList, type ListInit, useKeyedList, useList } from "./list";
import { type Read, useQueryRead } from "./read";
import { type Store, useStore } from "./store";

/** How often Connect Claude asks whether Claude has connected, while it is open. */
const WATCH_MS = 2_000;

const CONNECTIONS = (store: Store): ListInit<Connection> => ({
  load: (signal) => store.api.connections(signal),
  key: (c) => c.id,
});
const ORG_CONNECTIONS = (store: Store): ListInit<Connection> => ({
  load: (signal) => store.api.organizationConnections(signal),
  key: (c) => c.id,
});

const connections = (store: Store) => keyedList(store, "connections", CONNECTIONS);
const orgConnections = (store: Store) => keyedList(store, "org-connections", ORG_CONNECTIONS);

/** The caller's own connections, in every organization. */
export function useConnections(enabled = true): Read<readonly Connection[]> {
  return useList(enabled ? "connections" : null, CONNECTIONS);
}

/** The connections that open the caller's organization: `organizations` holds it only. */
export function useOrganizationConnections(enabled = true): Read<readonly Connection[]> {
  return useList(enabled ? "org-connections" : null, ORG_CONNECTIONS);
}

export interface ArrivingConnections {
  connections: Read<readonly Connection[]>;
  /** What the connections were when the view opened is known, from an answer asked since. */
  ready: boolean;
  /** A connection made, or used, since the view opened; `null` until one is. */
  arrived: Connection | null;
}

const noSubscribe = () => () => {};

/**
 * The caller's connections, asked for every 2 s while `watching`, and the one that arrived since
 * the view opened: compared with an answer asked after it opened, not one another view read.
 */
export function useArrivingConnections(enabled: boolean, watching: boolean): ArrivingConnections {
  const { list, read } = useKeyedList(
    enabled ? "connections" : null,
    CONNECTIONS,
    watching ? WATCH_MS : null,
  );
  const [openedAt] = useState(() => Date.now());
  const answeredAt = useSyncExternalStore(
    list?.subscribe ?? noSubscribe,
    () => list?.answeredAt ?? 0,
    () => list?.answeredAt ?? 0,
  );
  const [snapshot, setSnapshot] = useState<Snapshot | null>(null);
  const rows = read.data;
  useEffect(() => {
    if (snapshot === null && rows && answeredAt >= openedAt) setSnapshot(snapshotOf(rows));
  }, [snapshot, rows, answeredAt, openedAt]);
  return {
    connections: read,
    ready: snapshot !== null,
    arrived: snapshot && rows ? arrivedSince(snapshot, rows) : null,
  };
}

export interface ConnectionChange {
  id: string;
  body: UpdateConnection;
  /** The organizations' names, by id, to show the change before the server answers. */
  names: ReadonlyMap<string, string>;
}

/** Replaces the organizations a connection opens, with their access and labels; shown at once. */
export function useUpdateConnection(): Action<ConnectionChange> {
  const store = useStore();
  return useAction(async ({ id, body, names }: ConnectionChange) => {
    await connections(store).change(
      () => store.api.updateConnection(id, body),
      (rows) => {
        if (!rows.has(id)) return;
        rows.update(id, (c) => {
          c.organizations = body.organizations.map((o) => ({
            id: o.id,
            name: names.get(o.id) ?? null,
            access: o.access,
            allowedLabels: o.allowedLabels ?? {},
          }));
        });
      },
      without(heldList(store, "org-connections")),
    );
  });
}

/** Revokes one of the caller's connections: the client loses it at once; gone at once. */
export function useRevokeConnection(): Action<string> {
  const store = useStore();
  return useAction(async (id: string) => {
    await connections(store).change(
      () => store.api.revokeConnection(id),
      (rows) => {
        if (rows.has(id)) rows.delete(id);
      },
      without(heldList(store, "org-connections")),
    );
  });
}

/** Takes the caller's organization out of a connection; gone from its list at once. */
export function useRemoveOrganizationConnection(): Action<string> {
  const store = useStore();
  return useAction(async (id: string) => {
    await orgConnections(store).change(
      () => store.api.removeOrganizationConnection(id),
      (rows) => {
        if (rows.has(id)) rows.delete(id);
      },
      without(heldList(store, "connections")),
    );
  });
}

function without<T>(value: T | undefined): T[] {
  return value === undefined ? [] : [value];
}

/** What an MCP client's authorization request asks: read once, never again. */
export function useConsent(request: string | null): Read<ConsentRequest> {
  const { api } = useStore();
  return useQueryRead(
    useQuery({
      queryKey: ["consent", request],
      queryFn: ({ signal }) => api.consent(request ?? "", signal),
      enabled: request !== null && request !== "",
      retry: false,
      staleTime: Number.POSITIVE_INFINITY,
    }),
  );
}

/** Allows or denies the request; answers where to send the browser back. */
export function useAnswerConsent(): Action<ConsentDecision, ConsentAnswer> {
  const { api } = useStore();
  return useAction((decision: ConsentDecision) => api.answerConsent(decision));
}
