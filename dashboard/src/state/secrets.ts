// Secrets (archi/04, "Secrets"): the organization's (`namespace` null) and each namespace's, never
// a value, and the keys Rewind signs calls with. A list is read again every 30 s while shown, for
// its reads; a write reads again every list and usage of the secrets, a rotation the signing keys.
import { useQuery } from "@tanstack/react-query";
import type { PutSecret, Secret, SecretUsage, SigningKey, SigningKeyList } from "../model/types";
import { type Action, useAction } from "./actions";
import { type Read, useQueryRead } from "./read";
import { useStore } from "./store";

const SECRETS = ["secrets"] as const;
const SIGNING_KEYS = ["signing-keys"] as const;

/** The scope of a list in the cache: `*` for the organization's. */
const scope = (namespace: string | null) => namespace ?? "*";

/** The secrets of the organization, or of a namespace; read again while shown, for their reads. */
export function useSecrets(namespace: string | null, enabled = true): Read<Secret[]> {
  const store = useStore();
  return useQueryRead(
    useQuery({
      queryKey: [...SECRETS, scope(namespace), "list"],
      queryFn: ({ signal }) => store.api.secrets(namespace, signal),
      refetchInterval: 30_000,
      enabled,
    }),
  );
}

/** What reads one secret: the active and waiting runs that attach it, the workflows. */
export function useSecretUsage(
  namespace: string | null,
  name: string,
  enabled = true,
): Read<SecretUsage> {
  const store = useStore();
  return useQueryRead(
    useQuery({
      queryKey: [...SECRETS, scope(namespace), "usage", name],
      queryFn: ({ signal }) => store.api.secretUsage(namespace, name, signal),
      staleTime: 15_000,
      enabled,
    }),
  );
}

export interface SecretWrite {
  namespace: string | null;
  name: string;
  body: PutSecret;
}

/** Creates a secret or gives it a new value; answers it once the lists are read again. */
export function usePutSecret(): Action<SecretWrite, Secret> {
  const store = useStore();
  return useAction(async ({ namespace, name, body }: SecretWrite) => {
    const secret = await store.api.putSecret(namespace, name, body);
    await store.queryClient.invalidateQueries({ queryKey: SECRETS });
    return secret;
  });
}

export interface SecretRef {
  namespace: string | null;
  name: string;
}

/** Deletes a secret; the lists and the other secrets' usage are read again. */
export function useDeleteSecret(): Action<SecretRef> {
  const store = useStore();
  return useAction(async ({ namespace, name }: SecretRef) => {
    await store.api.deleteSecret(namespace, name);
    // The deleted secret's usage is not asked again: it has none.
    const gone = [...SECRETS, scope(namespace), "usage", name];
    void store.queryClient.invalidateQueries({
      queryKey: SECRETS,
      predicate: (query) => !gone.every((part, i) => query.queryKey[i] === part),
    });
  });
}

export function useSigningKeys(enabled = true): Read<SigningKeyList> {
  const store = useStore();
  return useQueryRead(
    useQuery({
      queryKey: SIGNING_KEYS,
      queryFn: ({ signal }) => store.api.signingKeys(signal),
      enabled,
    }),
  );
}

/** A new key signs from now on; the current one stays published for 24 hours. */
export function useRotateSigningKey(): Action<void, SigningKey> {
  const store = useStore();
  return useAction(async () => {
    const key = await store.api.rotateSigningKey();
    await store.queryClient.invalidateQueries({ queryKey: SIGNING_KEYS });
    return key;
  });
}
