// The organization (archi/06, "Tenancy, namespaces, identity and access"): its members and their
// grants, its invitations, its roles and the catalog of permissions, its API keys and settings,
// and the invitations waiting for the member. Each list is a collection of rows by key, read
// when a view shows it and again after each write. A write whose outcome is known beforehand
// shows at once (a grant changed, a member removed, an invitation or a key revoked, a role
// written or deleted), and the server's refusal takes it back; a write whose answer the server
// makes (an invitation sent, a key and its secret) shows with that answer. Who calls is read
// again when a write may change what the caller holds or sees.
import { useQuery } from "@tanstack/react-query";
import type {
  ApiKey,
  CreateApiKey,
  CreateInvitation,
  Invitation,
  Member,
  PermissionInfo,
  PutRole,
  Role,
  SetGrant,
  UpdateOrganization,
} from "../model/types";
import { type Action, useAction } from "./actions";
import { heldList, keyedList, type ListInit, useList } from "./list";
import { type Read, useQueryRead } from "./read";
import { refreshWhoami, useSession } from "./session";
import { type Store, useStore } from "./store";

const MEMBERS = (store: Store): ListInit<Member> => ({
  load: (signal) => store.api.members(signal),
  key: (m) => m.user.id,
});
const INVITATIONS = (store: Store): ListInit<Invitation> => ({
  load: (signal) => store.api.invitations(signal),
  key: (i) => i.id,
});
const MY_INVITATIONS = (store: Store): ListInit<Invitation> => ({
  load: (signal) => store.api.myInvitations(signal),
  key: (i) => i.id,
});
const ROLES = (store: Store): ListInit<Role> => ({
  load: (signal) => store.api.roles(signal),
  key: (r) => r.name,
});
const API_KEYS = (store: Store): ListInit<ApiKey> => ({
  load: (signal) => store.api.apiKeys(signal),
  key: (k) => k.id,
});

const members = (store: Store) => keyedList(store, "members", MEMBERS);
const invitations = (store: Store) => keyedList(store, "invitations", INVITATIONS);
const roles = (store: Store) => keyedList(store, "roles", ROLES);
const apiKeys = (store: Store) => keyedList(store, "api-keys", API_KEYS);

// Reads

/** The organization's members, each with their grant. */
export function useMembers(enabled = true): Read<readonly Member[]> {
  return useList(enabled ? "members" : null, MEMBERS);
}

/** The invitations the organization sent. */
export function useOrgInvitations(enabled = true): Read<readonly Invitation[]> {
  return useList(enabled ? "invitations" : null, INVITATIONS);
}

/** The pending invitations sent to the member's email. */
export function useMyInvitations(enabled = true): Read<readonly Invitation[]> {
  return useList(enabled ? "my-invitations" : null, MY_INVITATIONS);
}

/** The built-in roles, then the organization's own. */
export function useRoles(enabled = true): Read<readonly Role[]> {
  return useList(enabled ? "roles" : null, ROLES);
}

export function useApiKeys(enabled = true): Read<readonly ApiKey[]> {
  return useList(enabled ? "api-keys" : null, API_KEYS);
}

/** The catalog of permissions: it never changes while the server runs. */
export function usePermissionCatalog(enabled = true): Read<PermissionInfo[]> {
  const { api } = useStore();
  return useQueryRead(
    useQuery({
      queryKey: ["permissions"],
      queryFn: ({ signal }) => api.permissions(signal),
      staleTime: Number.POSITIVE_INFINITY,
      enabled,
    }),
  );
}

// Members

export interface GrantChange {
  user: string;
  grant: SetGrant;
  /** The caller's own grant: what it may do is read again. */
  self: boolean;
}

/** Gives a member a role and allowed labels; shown at once. */
export function useSetGrant(): Action<GrantChange> {
  const store = useStore();
  return useAction(async ({ user, grant, self }: GrantChange) => {
    await members(store).change(
      () => store.api.setGrant(user, grant),
      (rows) => {
        if (!rows.has(user)) return;
        rows.update(user, (m) => {
          m.grant = { ...m.grant, role: grant.role, allowedLabels: grant.allowedLabels ?? {} };
        });
      },
    );
    if (self) await refreshWhoami(store);
  });
}

/** Removes a member from the organization; gone at once. */
export function useRemoveMember(): Action<string> {
  const store = useStore();
  return useAction(async (user: string) => {
    await members(store).change(
      () => store.api.removeMember(user),
      (rows) => {
        if (rows.has(user)) rows.delete(user);
      },
    );
  });
}

// Invitations

/** Invites an email with a grant; answers the invitation sent. */
export function useInvite(): Action<CreateInvitation, Invitation> {
  const store = useStore();
  return useAction((body: CreateInvitation) =>
    invitations(store).change(() => store.api.invite(body)),
  );
}

/** Sends an invitation again; answers it as sent. */
export function useResendInvitation(): Action<string, Invitation> {
  const store = useStore();
  return useAction((id: string) => invitations(store).change(() => store.api.resendInvitation(id)));
}

/** Revokes an invitation: its link stops working; shown revoked at once. */
export function useRevokeInvitation(): Action<string> {
  const store = useStore();
  return useAction(async (id: string) => {
    await invitations(store).change(
      () => store.api.revokeInvitation(id),
      (rows) => {
        if (!rows.has(id)) return;
        rows.update(id, (inv) => {
          inv.state = "revoked";
        });
      },
    );
  });
}

// Roles

export interface RoleWrite {
  name: string;
  body: PutRole;
}

/**
 * Creates or changes a role; shown at once. A change applies at once to whoever holds the role,
 * the caller included: what it may do is read again.
 */
export function useSaveRole(): Action<RoleWrite> {
  const store = useStore();
  return useAction(async ({ name, body }: RoleWrite) => {
    await roles(store).change(
      () => store.api.putRole(name, body),
      (rows) => {
        const permissions = [...body.permissions];
        const description = body.description ?? "";
        if (rows.has(name)) {
          rows.update(name, (r) => {
            r.description = description;
            r.permissions = permissions;
          });
        } else {
          const now = new Date().toISOString();
          rows.insert({
            name,
            description,
            permissions,
            builtIn: false,
            createdAt: now,
            updatedAt: now,
          });
        }
      },
    );
    await refreshWhoami(store);
  });
}

/** Deletes a role of the organization; gone at once, back if a grant still holds it. */
export function useDeleteRole(): Action<string> {
  const store = useStore();
  return useAction(async (name: string) => {
    await roles(store).change(
      () => store.api.deleteRole(name),
      (rows) => {
        if (rows.has(name)) rows.delete(name);
      },
    );
  });
}

// API keys

/** Creates a key; answers it with its secret, the only time anyone sees it. */
export function useCreateApiKey(): Action<CreateApiKey, ApiKey> {
  const store = useStore();
  return useAction((body: CreateApiKey) =>
    apiKeys(store).change(() => store.api.createApiKey(body)),
  );
}

/** Revokes a key: every client sending it is refused from its next request; shown at once. */
export function useRevokeApiKey(): Action<string> {
  const store = useStore();
  return useAction(async (id: string) => {
    await apiKeys(store).change(
      () => store.api.revokeApiKey(id),
      (rows) => {
        if (!rows.has(id)) return;
        rows.update(id, (k) => {
          k.revokedAt = new Date().toISOString();
        });
      },
    );
  });
}

// The organization

/** Renames the organization, or changes the role a member gets at their first session. */
export function useUpdateOrganization(): Action<UpdateOrganization> {
  const store = useStore();
  return useAction(async (body: UpdateOrganization) => {
    await store.api.updateOrganization(body);
    // The name shows in the switcher, the default role in the settings: both come with whoami.
    await refreshWhoami(store);
  });
}

/**
 * Joins an invitation's organization, or creates one, then switches the session to it. `then`
 * runs right after the switch, before the views of the new organization replace these ones.
 */
export function useJoinOrCreate(then: () => void = () => {}): {
  join: Action<Invitation>;
  create: Action<string>;
} {
  const store = useStore();
  const { switchOrganization } = useSession();
  const join = useAction(async (invitation: Invitation) => {
    await store.api.acceptInvitation(invitation.id);
    const organization = invitation.organization?.id;
    if (!organization) {
      await heldList(store, "my-invitations")?.refreshNow();
      return;
    }
    await switchOrganization(organization);
    then();
  });
  const create = useAction(async (name: string) => {
    const organization = await store.api.createOrganization(name);
    await switchOrganization(organization.id);
    then();
  });
  return { join, create };
}
