// The mocked secrets of a tenant: the organization's and each namespace's, their reads and last
// read; and the signing keys. A value is kept only to be written over: no answer holds it.
import type { Labels, Secret, SigningKey } from "../api/types";
import { fakeDigest } from "./journal";

const HOUR = 3_600_000;
const DAY = 24 * HOUR;

export interface MockSecret {
  /** `null` for the organization's. */
  namespace: string | null;
  name: string;
  value: string;
  labels: Labels;
  createdBy: string;
  createdAt: number;
  updatedBy: string;
  updatedAt: number;
  /** Reads by UTC day (`2026-11-20`). */
  reads: Map<string, number>;
  lastRead: { run: string; namespace: string; at: number } | null;
}

export interface MockSigningKey {
  kid: string;
  createdAt: number;
  retiredAt: number | null;
}

export interface SecretWorld {
  secrets: MockSecret[];
  /** Newest first: the current one, then those retired. */
  signingKeys: MockSigningKey[];
}

/** A run the fixtures hold, as far as its attachments go. */
export interface AttachingRun {
  id: string;
  namespace: string;
  start: number;
  secrets: Record<string, string>;
}

export function emptySecretWorld(): SecretWorld {
  return { secrets: [], signingKeys: [] };
}

export const dayOf = (ms: number) => new Date(ms).toISOString().slice(0, 10);

/** A signing key's id: the first 8 bytes of a SHA-256, in hex. */
export function newKid(seed: string): string {
  return fakeDigest(`signing-key:${seed}`).slice(0, 16);
}

/** Whether the run attaches the secret: `org/<name>`, or `ns/<name>` of the run's own namespace. */
export function attaches(
  run: { namespace: string; secrets?: Record<string, string> },
  s: MockSecret,
) {
  return Object.values(run.secrets ?? {}).some((ref) => refersTo(ref, s, run.namespace));
}

/** Whether an attachment, read in a namespace (or any, `null`), names the secret. */
export function refersTo(ref: string, s: MockSecret, namespace: string | null): boolean {
  if (s.namespace === null) return ref === `org/${s.name}`;
  return ref === `ns/${s.name}` && (namespace === null || namespace === s.namespace);
}

export function createSecretWorld({
  now,
  runs,
}: {
  now: number;
  runs: readonly AttachingRun[];
}): SecretWorld {
  const secret = (
    namespace: string | null,
    name: string,
    labels: Labels,
    by: string,
    ageDays: number,
    perDay: number,
    rewrittenDaysAgo: number | null = null,
  ): MockSecret => {
    const s: MockSecret = {
      namespace,
      name,
      value: `${name}-${fakeDigest(name).slice(0, 12)}`,
      labels,
      createdBy: by,
      createdAt: now - ageDays * DAY,
      updatedBy: rewrittenDaysAgo === null ? by : "key:ci",
      updatedAt: now - (rewrittenDaysAgo ?? ageDays) * DAY,
      reads: new Map(),
      lastRead: null,
    };
    // Reads on most days since it was written, a quiet day now and then.
    for (let d = Math.min(29, ageDays); perDay > 0 && d >= 0; d--) {
      const n = d % 7 === 3 ? 0 : perDay + ((d * 13) % 5);
      if (n > 0) s.reads.set(dayOf(now - d * DAY), n);
    }
    // Its last read is the newest run that attaches it.
    const last = runs.filter((r) => attaches(r, s)).sort((a, b) => b.start - a.start)[0];
    if (last && perDay > 0)
      s.lastRead = { run: last.id, namespace: last.namespace, at: last.start };
    return s;
  };
  return {
    secrets: [
      secret(null, "github", {}, "user:user_01ADA", 40, 12),
      secret(null, "slack", {}, "user:user_01ADA", 40, 20, 6),
      secret(null, "crm", { env: "prod" }, "user:user_01GRACE", 18, 4),
      secret(null, "zendesk", { env: "prod" }, "user:user_01GRACE", 2, 0),
      secret("alice", "github", {}, "token:alice", 25, 6),
      secret("bob", "github", {}, "user:user_01GRACE", 12, 2),
      secret("chen", "linear", {}, "token:chen", 5, 0),
    ],
    // Rotated two hours ago: the previous key is still published.
    signingKeys: [
      { kid: newKid("current"), createdAt: now - 2 * HOUR, retiredAt: null },
      { kid: newKid("previous"), createdAt: now - 60 * DAY, retiredAt: now - 2 * HOUR },
      { kid: newKid("first"), createdAt: now - 200 * DAY, retiredAt: now - 60 * DAY },
    ],
  };
}

export function secretDto(s: MockSecret, now: number): Secret {
  const since = dayOf(now - 29 * DAY);
  const dto: Secret = {
    name: s.name,
    scope: s.namespace === null ? "org" : "ns",
    reference: `${s.namespace === null ? "org" : "ns"}/${s.name}`,
    createdBy: s.createdBy,
    createdAt: new Date(s.createdAt).toISOString(),
    updatedBy: s.updatedBy,
    updatedAt: new Date(s.updatedAt).toISOString(),
    reads: [...s.reads]
      .filter(([day]) => day >= since)
      .sort(([a], [b]) => a.localeCompare(b))
      .map(([day, reads]) => ({ day, reads })),
    lastRead: s.lastRead
      ? {
          run: s.lastRead.run,
          namespace: s.lastRead.namespace,
          at: new Date(s.lastRead.at).toISOString(),
        }
      : null,
  };
  if (s.namespace === null) dto.labels = s.labels;
  else dto.namespace = s.namespace;
  return dto;
}

export function signingKeyDto(k: MockSigningKey): SigningKey {
  return {
    kid: k.kid,
    createdAt: new Date(k.createdAt).toISOString(),
    retiredAt: k.retiredAt === null ? null : new Date(k.retiredAt).toISOString(),
  };
}
