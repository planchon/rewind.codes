// A refused action, explained where it was tried: what the rule is, then what the server said.
import { TriangleExclamationPoint } from "lucide-react";
import { ApiError } from "../state/errors";

export interface Explanation {
  title: string;
  /** What to do about it. */
  hint: string | null;
  /** The server's own words. */
  detail: string;
}

export function explain(error: unknown): Explanation {
  if (!(error instanceof ApiError)) {
    return {
      title: "Something went wrong",
      hint: null,
      detail: error instanceof Error ? error.message : String(error),
    };
  }
  const detail = error.problem.detail;
  switch (error.slug) {
    case "last-owner":
      return {
        title: "The organization must keep an owner",
        hint: "Give the owner role to another member first.",
        detail,
      };
    case "role-in-use":
      return {
        title: "This role is still held",
        hint: "Members, API keys or invitations hold it: give them another role, then delete it.",
        detail,
      };
    case "forbidden":
      return {
        title: "Your role does not allow this",
        hint: "No one gives more than they hold: an owner can do it, or give you a role that can.",
        detail,
      };
    case "unauthorized":
      return { title: "You are signed out", hint: "Sign in again.", detail };
    case "identity-unavailable":
      return {
        title: "The identity provider cannot be reached",
        hint: "Try again in a moment.",
        detail,
      };
    case "not-found":
      return { title: "Not found", hint: "Someone may have changed it meanwhile.", detail };
    default:
      return { title: error.problem.title, hint: null, detail };
  }
}

export function ErrorNote({ error, title }: { error: unknown; title?: string }) {
  if (!error) return null;
  const e = explain(error);
  const slug = error instanceof ApiError ? `${error.status} · ${error.slug}` : null;
  return (
    <div className="error-note" role="alert">
      <div className="error-note-head">
        <TriangleExclamationPoint size={13} aria-hidden="true" className="tone-critical" />
        <strong>{title ?? e.title}</strong>
        {slug && <span className="slug">{slug}</span>}
      </div>
      {e.hint && <p>{e.hint}</p>}
      {e.detail && <p className="muted">{e.detail}</p>}
    </div>
  );
}
