// The mocked workflows and tests of `/v1`: `GET /v1/workflows` (the latest version of each, by
// name, paged with `after`; with `script`, every version whose script it is, newest first), `GET /v1/workflows/{name}` (its versions, newest first) and
// `GET /v1/tests/{digest}`. They need `toolsets:read`; a grant limited to labels sees the
// workflows carrying them; a recorded test is read like its run (`runs:read` in its namespace).
import { HttpResponse, http } from "msw";
import type { Workflow } from "../api/types";
import { type Caller, grantOf } from "./access";
import { guard } from "./access-handlers";
import type { World } from "./fixtures";
import { problem } from "./problem";

/** The versions the caller sees: its tenant's, carrying its grant's allowed labels. */
function visibleVersions(world: World, caller: Caller): Workflow[] {
  if (caller.tenant !== world.access.tenant) return [];
  const pairs = Object.entries(grantOf(world.access, caller)?.allowedLabels ?? {});
  return world.workflows.versions.filter((v) => pairs.every(([k, val]) => v.labels[k] === val));
}

export function workflowHandlers(world: World) {
  return [
    http.get("*/v1/workflows", ({ request }) => {
      const caller = guard(world, request, "toolsets:read");
      if (caller instanceof Response) return caller;
      const q = new URL(request.url).searchParams;
      const script = q.get("script");
      if (script !== null) {
        const versions = visibleVersions(world, caller)
          .filter((v) => v.script === script)
          .sort((a, b) => b.createdAt.localeCompare(a.createdAt));
        return HttpResponse.json({ workflows: versions });
      }
      const after = q.get("after");
      const limit = Math.min(500, Math.max(1, Number(q.get("limit") ?? 50) || 50));
      const latest = new Map<string, Workflow>();
      for (const v of visibleVersions(world, caller)) {
        const seen = latest.get(v.name);
        if (!seen || v.createdAt > seen.createdAt) latest.set(v.name, v);
      }
      const names = [...latest.keys()].sort().filter((name) => after === null || name > after);
      const page = names.slice(0, limit);
      const last = page[page.length - 1];
      return HttpResponse.json({
        workflows: page.map((name) => latest.get(name) as Workflow),
        ...(names.length > limit && last ? { next: last } : {}),
      });
    }),

    http.get("*/v1/workflows/:name", ({ request, params }) => {
      const caller = guard(world, request, "toolsets:read");
      if (caller instanceof Response) return caller;
      const name = String(params.name);
      const versions = visibleVersions(world, caller)
        .filter((v) => v.name === name)
        .sort((a, b) => b.createdAt.localeCompare(a.createdAt));
      return versions.length > 0
        ? HttpResponse.json({ name, versions })
        : problem(404, "not-found", `No workflow ${name} for this tenant.`);
    }),

    http.get("*/v1/tests/:digest", ({ request, params }) => {
      const caller = guard(world, request, "toolsets:read");
      if (caller instanceof Response) return caller;
      const digest = String(params.digest);
      const found = caller.tenant === world.access.tenant && world.workflows.tests.get(digest);
      if (!found) return problem(404, "not-found", `No test ${digest} for this tenant.`);
      const origin = found.test.origin;
      if (origin) {
        const reads = guard(world, request, "runs:read");
        if (reads instanceof Response) return reads;
        if (found.restricted)
          return problem(
            403,
            "forbidden",
            `This test was recorded from a run of the namespace ${origin.namespace}, and holds its data: reading it needs runs:read in that namespace, which your grant does not reach.`,
          );
      }
      return HttpResponse.json(found.test);
    }),
  ];
}
