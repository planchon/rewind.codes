// Data hooks over the organization and access endpoints. Every write invalidates what it changes;
// `whoami` too when it may change what the caller holds or sees (its grant, the organization).
import { useQuery } from "@tanstack/react-query";
import { useApi } from "../app/session";

export const WHOAMI = ["whoami"] as const;
export const MEMBERS = ["members"] as const;
export const ORG_INVITATIONS = ["org-invitations"] as const;
export const MY_INVITATIONS = ["my-invitations"] as const;
export const ROLES = ["roles"] as const;
export const PERMISSION_CATALOG = ["permissions"] as const;
export const API_KEYS = ["api-keys"] as const;

export function useWhoami() {
  const api = useApi();
  return useQuery({
    queryKey: WHOAMI,
    queryFn: ({ signal }) => api.whoami(signal),
    staleTime: 30_000,
  });
}

/** The catalog never changes while the server runs. */
export function usePermissionCatalog(enabled = true) {
  const api = useApi();
  return useQuery({
    queryKey: PERMISSION_CATALOG,
    queryFn: ({ signal }) => api.permissions(signal),
    staleTime: Number.POSITIVE_INFINITY,
    enabled,
  });
}

export function useRoles(enabled = true) {
  const api = useApi();
  return useQuery({ queryKey: ROLES, queryFn: ({ signal }) => api.roles(signal), enabled });
}

export function useMembers(enabled = true) {
  const api = useApi();
  return useQuery({ queryKey: MEMBERS, queryFn: ({ signal }) => api.members(signal), enabled });
}

export function useOrgInvitations(enabled = true) {
  const api = useApi();
  return useQuery({
    queryKey: ORG_INVITATIONS,
    queryFn: ({ signal }) => api.invitations(signal),
    enabled,
  });
}

/** The pending invitations sent to the member's email. */
export function useMyInvitations(enabled = true) {
  const api = useApi();
  return useQuery({
    queryKey: MY_INVITATIONS,
    queryFn: ({ signal }) => api.myInvitations(signal),
    enabled,
  });
}

export function useApiKeys(enabled = true) {
  const api = useApi();
  return useQuery({ queryKey: API_KEYS, queryFn: ({ signal }) => api.apiKeys(signal), enabled });
}
