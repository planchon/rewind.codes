// The mocked workflows and tests of `/v1` (archi/07, "HTTP API /v1"): `GET /v1/workflows` (each
// workflow the caller sees with its latest version and its runs of a window, paged by id with
// `after`), `GET /v1/workflows/{id}` (by its id or its name, at a version: its versions, its suite,
// its code), `PATCH` (its name and description), `promote`, `fork` and `pull`, and
// `GET /v1/tests/{digest}`. Reading needs `runs:read`, writing `workflows:write`; a grant limited
// to labels sees the workflows with a version carrying them; a recorded test is read like its run
// (`runs:read` in its namespace). Each write is told on the change stream (`workflow`).
import { HttpResponse, http } from "msw";
import type { Labels, ListWorkflowsQuery, WorkflowVersion } from "../model/types";
import { type Caller, grantOf } from "./access";
import { guard, json } from "./access-handlers";
import { told } from "./changes";
import type { World } from "./fixtures";
import { headerNamespace } from "./namespace-handlers";
import { invalidQuery, problem } from "./problem";
import { QueryError } from "./query";
import { listWorkflows } from "./readmodel";
import {
  forkWorkflow,
  latestVersion,
  type MockWorkflow,
  pullSource,
  visibleVersions,
  workflowDto,
  workflowId,
  workflowLabelOf,
  workflowNameProblem,
} from "./workflows";

const DESCRIPTION_MAX = 2_000;

/** The labels of the caller's grant; none for a caller of another tenant. */
function labelsOf(world: World, caller: Caller): Labels {
  return grantOf(world.access, caller)?.allowedLabels ?? {};
}

/** The workflows the caller sees: its tenant's, with a version carrying its grant's labels. */
function visible(world: World, caller: Caller): MockWorkflow[] {
  if (caller.tenant !== world.access.tenant) return [];
  const labels = labelsOf(world, caller);
  return world.workflows.workflows.filter((w) => visibleVersions(w, labels).length > 0);
}

/**
 * The workflow and version a reference names (`wf_…`, or a name resolved in the namespace first,
 * then among the organization's; then `@v12`, `@<digest>` or `@` and its first 12 digits).
 */
function resolve(
  workflows: readonly MockWorkflow[],
  ref: string,
  namespace: string | null,
  labels: Labels,
  organizationOnly = false,
): { workflow: MockWorkflow; version: WorkflowVersion } | Response {
  const at = ref.lastIndexOf("@");
  const base = at < 0 ? ref : ref.slice(0, at);
  const pick = at < 0 ? null : ref.slice(at + 1);
  const scopes = organizationOnly ? [null] : namespace === null ? [null] : [namespace, null];
  const workflow = base.startsWith("wf_")
    ? workflows.find((w) => w.id === base && (!organizationOnly || w.namespace === null))
    : scopes
        .map((scope) => workflows.find((w) => w.name === base && w.namespace === scope))
        .find((w) => w !== undefined);
  if (!workflow) return problem(404, "not-found", `No workflow ${base} in this organization.`);
  const versions = visibleVersions(workflow, labels);
  if (pick === null) return { workflow, version: versions[0] ?? latestVersion(workflow) };
  const number = /^v(\d+)$/.exec(pick)?.[1];
  const version =
    number !== undefined
      ? versions.find((v) => v.version === Number(number))
      : pick.length >= 12
        ? versions.find((v) => v.digest.startsWith(pick))
        : undefined;
  return version
    ? { workflow, version }
    : problem(404, "not-found", `No version ${pick} of the workflow ${workflowLabelOf(workflow)}.`);
}

export function workflowHandlers(world: World, now: () => number) {
  const workflows = world.workflows;
  const changed = (w: MockWorkflow, response: Response) =>
    told(world, { kind: "workflow", workflow: w.id }, response);
  const dto = (caller: Caller, w: MockWorkflow, version?: WorkflowVersion) =>
    workflowDto(workflows, w, version ?? latestVersion(w), labelsOf(world, caller));

  /** The workflow a write names, among those the caller sees, or the refusal. */
  const target = (request: Request, ref: string) => {
    const caller = guard(world, request, "workflows:write");
    if (caller instanceof Response) return caller;
    const namespace = headerNamespace(request);
    if (namespace instanceof Response) return namespace;
    const found = resolve(visible(world, caller), ref, namespace, labelsOf(world, caller));
    return found instanceof Response ? found : { caller, namespace, workflow: found.workflow };
  };

  return [
    http.get("*/v1/workflows", ({ request }) => {
      const caller = guard(world, request, "runs:read");
      if (caller instanceof Response) return caller;
      const namespace = headerNamespace(request);
      if (namespace instanceof Response) return namespace;
      if (caller.tenant !== world.access.tenant) return HttpResponse.json({ workflows: [] });
      const q = new URL(request.url).searchParams;
      const query: ListWorkflowsQuery = {};
      for (const key of ["q", "code", "since", "until", "after"] as const) {
        const value = q.get(key);
        if (value) query[key] = value;
      }
      const scope = q.get("scope");
      if (scope === "namespace" || scope === "organization") query.scope = scope;
      const limit = q.get("limit");
      if (limit !== null) query.limit = Number(limit) || 50;
      try {
        return HttpResponse.json(
          listWorkflows(world, { query, namespace, labels: labelsOf(world, caller) }, now()),
        );
      } catch (error) {
        if (error instanceof QueryError) return invalidQuery(error.message, error.start, error.end);
        throw error;
      }
    }),

    http.get("*/v1/workflows/:ref", ({ request, params }) => {
      const caller = guard(world, request, "runs:read");
      if (caller instanceof Response) return caller;
      const namespace = headerNamespace(request);
      if (namespace instanceof Response) return namespace;
      const found = resolve(
        visible(world, caller),
        decodeURIComponent(String(params.ref)),
        namespace,
        labelsOf(world, caller),
      );
      if (found instanceof Response) return found;
      return HttpResponse.json(dto(caller, found.workflow, found.version));
    }),

    http.patch("*/v1/workflows/:ref", async ({ request, params }) => {
      const found = target(request, decodeURIComponent(String(params.ref)));
      if (found instanceof Response) return found;
      const body = await json<{ name?: string | null; description?: string | null }>(request);
      if (body instanceof Response) return body;
      const w = found.workflow;
      if (body.name !== undefined && body.name !== null) {
        const why = workflowNameProblem(body.name);
        if (why) return problem(422, "invalid-workflow", why);
        const taken = workflows.workflows.find(
          (o) => o !== w && o.name === body.name && o.namespace === w.namespace,
        );
        if (taken)
          return problem(
            409,
            "name-taken",
            `${w.namespace === null ? "The organization" : `The namespace ${w.namespace}`} already has a workflow named ${body.name} (${taken.id}).`,
          );
      }
      const description = body.description?.trim();
      if ((description?.length ?? 0) > DESCRIPTION_MAX)
        return problem(422, "invalid-workflow", "A description is at most 2,000 characters.");
      if (body.name !== undefined) w.name = body.name;
      if (body.description !== undefined) w.description = description || null;
      return changed(w, HttpResponse.json(dto(found.caller, w)));
    }),

    http.post("*/v1/workflows/:ref/promote", ({ request, params }) => {
      const found = target(request, decodeURIComponent(String(params.ref)));
      if (found instanceof Response) return found;
      const w = found.workflow;
      if (w.namespace === null) return HttpResponse.json(dto(found.caller, w));
      const taken =
        w.name !== null &&
        workflows.workflows.find((o) => o.namespace === null && o.name === w.name);
      if (taken)
        return problem(
          409,
          "name-taken",
          `The organization already has a workflow named ${w.name} (${taken.id}): rename this one first.`,
        );
      w.namespace = null;
      w.promotedAt = now();
      return changed(w, HttpResponse.json(dto(found.caller, w)));
    }),

    http.post("*/v1/workflows/:ref/fork", ({ request, params }) => {
      const caller = guard(world, request, "workflows:write");
      if (caller instanceof Response) return caller;
      const namespace = headerNamespace(request);
      if (namespace instanceof Response) return namespace;
      if (namespace === null)
        return problem(
          400,
          "invalid-request",
          "No namespace named: a fork goes to the namespace Rewind-Namespace names.",
        );
      const ref = decodeURIComponent(String(params.ref));
      const seen = visible(world, caller);
      const found = resolve(seen, ref, null, labelsOf(world, caller), true);
      if (found instanceof Response) {
        const other = resolve(seen, ref, namespace, labelsOf(world, caller));
        return other instanceof Response
          ? found
          : problem(
              409,
              "not-forkable",
              `${workflowLabelOf(other.workflow)} is the namespace ${other.workflow.namespace}'s: only the organization's workflows are forked.`,
            );
      }
      const source = found.workflow;
      const name = source.name;
      const taken =
        name !== null &&
        workflows.workflows.find((o) => o.namespace === namespace && o.name === name);
      if (taken)
        return problem(
          409,
          "name-taken",
          `The namespace ${namespace} already has a workflow named ${name} (${taken.id}).`,
        );
      const count = workflows.workflows.filter((w) => w.source?.workflow === source.id).length;
      const fork = forkWorkflow(
        workflows,
        source,
        found.version,
        namespace,
        now(),
        workflowId(`${source.id}@${namespace}#${count}`),
      );
      return changed(fork, HttpResponse.json(dto(caller, fork), { status: 201 }));
    }),

    http.post("*/v1/workflows/:ref/pull", ({ request, params }) => {
      const found = target(request, decodeURIComponent(String(params.ref)));
      if (found instanceof Response) return found;
      const w = found.workflow;
      if (!w.source)
        return problem(
          409,
          "not-a-fork",
          `${workflowLabelOf(w)} is not a fork: it has no source to pull from.`,
        );
      // The fork's own suite passes against the source's version: the mocked gate passes.
      if (!pullSource(workflows, w, now())) return HttpResponse.json(dto(found.caller, w));
      return changed(w, HttpResponse.json(dto(found.caller, w)));
    }),

    http.get("*/v1/tests/:digest", ({ request, params }) => {
      const caller = guard(world, request, "runs:read");
      if (caller instanceof Response) return caller;
      const digest = String(params.digest);
      const found = caller.tenant === world.access.tenant && workflows.tests.get(digest);
      if (!found) return problem(404, "not-found", `No test ${digest} in this organization.`);
      const from = found.test.from;
      if (from && found.restricted)
        return problem(
          403,
          "forbidden",
          `This test was recorded from a run of the namespace ${from.namespace}, and holds its data: reading it needs runs:read in that namespace, which your grant does not reach.`,
        );
      return HttpResponse.json(found.test);
    }),
  ];
}
