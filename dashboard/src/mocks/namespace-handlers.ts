// The mocked namespaces endpoints of `/v1` (archi/07, "HTTP API /v1"), and the header
// `Rewind-Namespace`, which narrows a key's or a member's reads to one namespace.
import { HttpResponse, http } from "msw";
import { durationMs, isMonth, monthOf, namespaceNameProblem } from "../model/quotas";
import type {
  NamespaceOrder,
  NamespacePatch,
  PutNamespace,
  PutNamespaceDefaults,
  Quotas,
} from "../model/types";
import type { Caller } from "./access";
import { guard, json } from "./access-handlers";
import { told } from "./changes";
import type { World } from "./fixtures";
import {
  alertsOf,
  defaultsDto,
  detailDto,
  emptyNamespaceWorld,
  highest,
  type NamespaceWorld,
  namespaceDto,
  newNamespace,
  usageOf,
} from "./namespaces";
import { problem } from "./problem";

export const NAMESPACE_HEADER = "Rewind-Namespace";

const NAME_RULE = "1 to 128 letters, digits, -, _, ., : and @";

/**
 * The namespace a request names with `Rewind-Namespace`: `null` without the header, the
 * server's refusal (`invalid-request`) for a name that is not one.
 */
export function headerNamespace(request: Request): string | null | Response {
  const name = request.headers.get(NAMESPACE_HEADER);
  if (name === null) return null;
  if (namespaceNameProblem(name) !== null)
    return problem(
      400,
      "invalid-request",
      `${NAMESPACE_HEADER}: ${JSON.stringify(name)} is not a namespace: ${NAME_RULE}.`,
    );
  return name;
}

/** The namespaces of the caller's tenant; a tenant without any has an empty world. */
export function namespacesOf(world: World, caller: Caller): NamespaceWorld {
  const tenant = caller.tenant ?? "";
  let found = world.namespaces.get(tenant);
  if (!found) {
    found = emptyNamespaceWorld();
    world.namespaces.set(tenant, found);
  }
  return found;
}

const COUNTS = ["activeRuns", "runsPerMonth", "scripts", "maxScriptLines"] as const;
const DURATIONS = ["cpuPerMonth", "maxTtl"] as const;

/** Why limits are refused, as the server reads `Quotas` (unknown fields refused), or `null`. */
function quotasProblem(value: unknown): string | null {
  if (value === undefined) return null;
  if (value === null || typeof value !== "object" || Array.isArray(value))
    return 'the limits are an object, as { "cpuPerMonth": "2h", "activeRuns": 10 }';
  for (const [key, limit] of Object.entries(value)) {
    if (limit === null) continue;
    if ((COUNTS as readonly string[]).includes(key)) {
      if (typeof limit !== "number" || !Number.isInteger(limit) || limit < 0)
        return `${key}: expected a whole number, 0 or more`;
    } else if ((DURATIONS as readonly string[]).includes(key)) {
      if (
        (typeof limit !== "string" && typeof limit !== "number") ||
        durationMs(limit as string | number) === null
      )
        return `${key}: expected a duration, as "2h" or "30m", or a number of milliseconds`;
    } else {
      return `unknown field ${JSON.stringify(key)}: the limits are ${[...COUNTS, ...DURATIONS].join(", ")}`;
    }
  }
  return null;
}

/** Limits without the quotas set to `null`, which follow the default. */
function cleanQuotas(value: Quotas | undefined): Quotas {
  return Object.fromEntries(
    Object.entries(value ?? {}).filter(([, limit]) => limit !== null && limit !== undefined),
  ) as Quotas;
}

const ORDERS: readonly NamespaceOrder[] = ["cpu", "runs", "scripts", "name"];

export function namespaceHandlers(world: World, now: () => number) {
  /** The month of a request (`month`), the current one by default. */
  const monthParam = (request: Request): string | Response => {
    const month = new URL(request.url).searchParams.get("month");
    if (month === null) return monthOf(now());
    return isMonth(month)
      ? month
      : problem(
          400,
          "invalid-request",
          `month: ${JSON.stringify(month)} is not a month, as 2026-11.`,
        );
  };

  /** The caller, its namespaces and the namespace the path names, as the header allows it. */
  const reach = (request: Request, name: string, permission: Parameters<typeof guard>[2]) => {
    const caller = guard(world, request, permission);
    if (caller instanceof Response) return caller;
    const scope = headerNamespace(request);
    if (scope instanceof Response) return scope;
    const namespaces = namespacesOf(world, caller);
    const ns = scope === null || scope === name ? namespaces.namespaces.get(name) : undefined;
    return { caller, namespaces, ns };
  };
  const noNamespace = (name: string) =>
    problem(404, "not-found", `No namespace ${JSON.stringify(name)} in this organization.`);

  return [
    http.get("*/v1/namespaces", ({ request }) => {
      const caller = guard(world, request, "namespaces:read");
      if (caller instanceof Response) return caller;
      const scope = headerNamespace(request);
      if (scope instanceof Response) return scope;
      const month = monthParam(request);
      if (month instanceof Response) return month;
      const q = new URL(request.url).searchParams;
      const order = (q.get("order") ?? "cpu") as NamespaceOrder;
      if (!ORDERS.includes(order))
        return problem(400, "invalid-request", `order: expected one of ${ORDERS.join(", ")}.`);
      const above = q.get("above") === null ? null : Number(q.get("above"));
      if (above !== null && !(Number.isFinite(above) && above >= 0))
        return problem(400, "invalid-request", "above: expected a share of a limit, as 0.8.");
      const limit = Math.min(500, Number(q.get("limit") ?? 50));
      if (!(Number.isInteger(limit) && limit > 0))
        return problem(400, "invalid-request", "limit: expected a whole number from 1 to 500.");
      const prefix = q.get("prefix") ?? "";
      const folded = prefix.toLowerCase();
      const namespaces = namespacesOf(world, caller);
      const key = (name: string): number => {
        const ns = namespaces.namespaces.get(name);
        if (!ns || order === "name") return 0;
        const usage = usageOf(ns, month);
        if (order === "cpu") return durationMs(usage.cpu) ?? 0;
        return order === "runs" ? usage.runsStarted : usage.scripts;
      };
      const listed = [...namespaces.namespaces.values()]
        .filter(
          (ns) =>
            (ns.name.startsWith(prefix) ||
              (ns.displayName ?? "").toLowerCase().startsWith(folded)) &&
            (scope === null || ns.name === scope) &&
            (above === null || highest(namespaces, ns, month) >= above),
        )
        .map((ns) => ({ ns, key: key(ns.name) }))
        .sort((a, b) => b.key - a.key || (a.ns.name < b.ns.name ? -1 : 1));
      const offset = Number(q.get("after") ?? 0) || 0;
      const page = listed.slice(offset, offset + limit);
      const next = offset + limit < listed.length ? String(offset + limit) : null;
      return HttpResponse.json({
        month,
        namespaces: page.map(({ ns }) => namespaceDto(namespaces, ns, month)),
        total: listed.length,
        ...(next ? { next } : {}),
      });
    }),

    http.get("*/v1/namespaces/:name", ({ request, params }) => {
      const name = String(params.name);
      const found = reach(request, name, "namespaces:read");
      if (found instanceof Response) return found;
      if (!found.ns) return noNamespace(name);
      const month = monthParam(request);
      if (month instanceof Response) return month;
      return HttpResponse.json(detailDto(found.namespaces, found.ns, month));
    }),

    http.put("*/v1/namespaces/:name", async ({ request, params }) => {
      const name = String(params.name);
      const found = reach(request, name, "namespaces:write");
      if (found instanceof Response) return found;
      const why = namespaceNameProblem(name);
      if (why) return problem(400, "invalid-request", `${JSON.stringify(name)}: ${NAME_RULE}.`);
      const body = await json<PutNamespace>(request);
      if (body instanceof Response) return body;
      const refused = quotasProblem(body.exceptions);
      if (refused) return problem(400, "invalid-request", `exceptions: ${refused}.`);
      let ns = found.ns;
      if (ns?.deletingSince != null)
        return problem(
          409,
          "namespace-deleting",
          `The namespace ${name} is being erased: it takes no change.`,
        );
      if (!ns) {
        ns = newNamespace(name, now());
        found.namespaces.namespaces.set(name, ns);
      }
      ns.exceptions = cleanQuotas(body.exceptions);
      return told(
        world,
        { kind: "namespace", name },
        HttpResponse.json(detailDto(found.namespaces, ns, monthOf(now()))),
      );
    }),

    http.patch("*/v1/namespaces/:name", async ({ request, params }) => {
      const name = String(params.name);
      const found = reach(request, name, "namespaces:write");
      if (found instanceof Response) return found;
      if (!found.ns) return noNamespace(name);
      const body = await json<NamespacePatch>(request);
      if (body instanceof Response) return body;
      const unknown = Object.keys(body).filter((k) => k !== "displayName");
      if (unknown.length > 0)
        return problem(400, "invalid-request", `${unknown[0]}: not a field of a namespace patch.`);
      if (found.ns.deletingSince != null)
        return problem(
          409,
          "namespace-deleting",
          `The namespace ${name} is being erased: it takes no change.`,
        );
      if ("displayName" in body) {
        const value = body.displayName;
        if (value !== null && value !== undefined && typeof value !== "string")
          return problem(400, "invalid-request", "displayName: expected a string or null.");
        const trimmed = (value ?? "").trim();
        if ([...trimmed].length > 128 || /\p{Cc}/u.test(trimmed))
          return problem(
            400,
            "invalid-request",
            "displayName: at most 128 characters, without control characters.",
          );
        found.ns.displayName = trimmed === "" ? null : trimmed;
      }
      return told(
        world,
        { kind: "namespace", name },
        HttpResponse.json(detailDto(found.namespaces, found.ns, monthOf(now()))),
      );
    }),

    http.get("*/v1/organization/namespace-names", ({ request }) => {
      const caller = guard(world, request, "namespaces:read");
      if (caller instanceof Response) return caller;
      const names = [...namespacesOf(world, caller).namespaces.values()]
        .flatMap((ns) => (ns.displayName ? [{ name: ns.name, displayName: ns.displayName }] : []))
        .sort((a, b) => (a.name < b.name ? -1 : 1));
      return HttpResponse.json({ namespaces: names });
    }),

    http.delete("*/v1/namespaces/:name", ({ request, params }) => {
      const name = String(params.name);
      const found = reach(request, name, "namespaces:write");
      if (found instanceof Response) return found;
      const ns = found.ns;
      if (!ns) return noNamespace(name);
      if (ns.deletingSince === null) {
        ns.deletingSince = now();
        ns.tokens = 0;
        ns.tokensRevokedBefore = now();
        ns.activeRuns = 0;
      }
      return told(world, { kind: "namespace", name }, new HttpResponse(null, { status: 202 }));
    }),

    http.delete("*/v1/namespaces/:name/tokens", ({ request, params }) => {
      const name = String(params.name);
      const found = reach(request, name, "tokens:write");
      if (found instanceof Response) return found;
      const ns = found.ns;
      if (!ns) return noNamespace(name);
      const revoked = ns.tokens;
      ns.tokens = 0;
      ns.tokensRevokedBefore = now();
      return told(
        world,
        { kind: "namespace", name },
        HttpResponse.json({
          revoked,
          revokedBefore: new Date(ns.tokensRevokedBefore).toISOString(),
        }),
      );
    }),

    http.get("*/v1/organization/namespace-defaults", ({ request }) => {
      const caller = guard(world, request, "namespaces:read");
      if (caller instanceof Response) return caller;
      return HttpResponse.json(defaultsDto(namespacesOf(world, caller), monthOf(now())));
    }),

    http.put("*/v1/organization/namespace-defaults", async ({ request }) => {
      const caller = guard(world, request, "namespaces:write");
      if (caller instanceof Response) return caller;
      const body = await json<PutNamespaceDefaults>(request);
      if (body instanceof Response) return body;
      if (body.defaults === undefined)
        return problem(400, "invalid-request", "defaults: the field is required.");
      const refused = quotasProblem(body.defaults);
      if (refused) return problem(400, "invalid-request", `defaults: ${refused}.`);
      const namespaces = namespacesOf(world, caller);
      namespaces.defaults = cleanQuotas(body.defaults);
      return told(
        world,
        { kind: "namespace" },
        HttpResponse.json(defaultsDto(namespaces, monthOf(now()))),
      );
    }),

    http.get("*/v1/organization/quota-alerts", ({ request }) => {
      const caller = guard(world, request, "namespaces:read");
      if (caller instanceof Response) return caller;
      const scope = headerNamespace(request);
      if (scope instanceof Response) return scope;
      const month = monthParam(request);
      if (month instanceof Response) return month;
      const named = new URL(request.url).searchParams.get("namespace") ?? scope ?? undefined;
      if (scope !== null && named !== scope) return HttpResponse.json({ month, alerts: [] });
      return HttpResponse.json({
        month,
        alerts: alertsOf(namespacesOf(world, caller), month, named),
      });
    }),
  ];
}
