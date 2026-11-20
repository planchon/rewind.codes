// Data hooks over the secrets (the organization's, `namespace` null, and each namespace's) and the
// signing keys. A write invalidates everything under `SECRETS`: a list, the usage of each secret.
import { useQuery } from "@tanstack/react-query";
import { useApi } from "../app/session";

export const SECRETS = ["secrets"] as const;
export const SIGNING_KEYS = ["signing-keys"] as const;

/** The scope of a list in the cache: `*` for the organization's. */
const scope = (namespace: string | null) => namespace ?? "*";

/** The secrets of the organization, or of a namespace; polled while shown, for their reads. */
export function useSecrets(namespace: string | null, enabled = true) {
  const api = useApi();
  return useQuery({
    queryKey: [...SECRETS, scope(namespace), "list"],
    queryFn: ({ signal }) => api.secrets(namespace, signal),
    refetchInterval: 30_000,
    enabled,
  });
}

/** What reads one secret: the active and waiting runs that attach it, the workflows. */
export function useSecretUsage(namespace: string | null, name: string, enabled = true) {
  const api = useApi();
  return useQuery({
    queryKey: [...SECRETS, scope(namespace), "usage", name],
    queryFn: ({ signal }) => api.secretUsage(namespace, name, signal),
    staleTime: 15_000,
    enabled,
  });
}

export function useSigningKeys(enabled = true) {
  const api = useApi();
  return useQuery({
    queryKey: SIGNING_KEYS,
    queryFn: ({ signal }) => api.signingKeys(signal),
    enabled,
  });
}
