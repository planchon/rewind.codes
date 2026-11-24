// Answers of the mocked API that are not a success: `application/problem+json`.
import { HttpResponse } from "msw";
import type { Problem } from "../model/types";

const TITLES: Record<string, string> = {
  unauthorized: "Unauthorized",
  forbidden: "Forbidden",
  "not-found": "Not found",
  "invalid-request": "Invalid request",
  "invalid-query": "The query cannot be read",
  "last-owner": "The organization would have no owner",
  "role-in-use": "The role is in use",
  "unsupported-media-type": "Unsupported media type",
  "run-ended": "The run has ended",
  "name-taken": "The name is taken",
  "not-forkable": "The workflow cannot be forked",
  "not-a-fork": "The workflow is not a fork",
  "invalid-workflow": "The workflow was refused",
  "invalid-signal": "The run does not take this signal",
};

export function problem(status: number, slug: string, detail: string): HttpResponse<Problem> {
  return HttpResponse.json(
    { type: `urn:rewind:problem:${slug}`, title: TITLES[slug] ?? slug, status, detail },
    { status, headers: { "Content-Type": "application/problem+json" } },
  ) as HttpResponse<Problem>;
}

/** A query the server cannot read: `invalid-query`, with the position of the term at fault. */
export function invalidQuery(detail: string, start: number, end: number): HttpResponse<Problem> {
  return HttpResponse.json(
    {
      type: "urn:rewind:problem:invalid-query",
      title: TITLES["invalid-query"] ?? "invalid-query",
      status: 400,
      detail,
      query: { start, end },
    },
    { status: 400, headers: { "Content-Type": "application/problem+json" } },
  ) as HttpResponse<Problem>;
}
