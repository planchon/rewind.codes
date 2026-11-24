// The workflow a page shows, and its version: the one `?version=` names (its number, or a digest
// from a link of an earlier version), else its latest. The page's address is the workflow's id and
// the version's number: a name or a digest it was reached by is replaced once read, the rest of
// the address kept.
import { useEffect } from "react";
import { useLocation, useNavigate, useSearchParams } from "react-router";
import type { Workflow, WorkflowVersion } from "../../model/types";
import { findVersion } from "../../model/workflows";
import type { Read } from "../../state/read";
import { useWorkflow } from "../../state/workflows";

export interface ShownWorkflow {
  read: Read<Workflow>;
  workflow: Workflow | undefined;
  /** Its versions, newest first. */
  versions: readonly WorkflowVersion[];
  /** The version shown; `null` while it loads, or when the one named is not among them. */
  version: WorkflowVersion | null;
  /** What `?version=` says, as written. */
  asked: string | null;
  /** The version shown is its latest. */
  latest: boolean;
}

/** `12` and `v12` name a version by its number. */
function versionNumber(asked: string | null): number | null {
  const n = asked ? /^v?(\d+)$/.exec(asked)?.[1] : undefined;
  return n === undefined ? null : Number(n);
}

export function useShownWorkflow(ref: string): ShownWorkflow {
  const [params] = useSearchParams();
  const location = useLocation();
  const navigate = useNavigate();
  const asked = params.get("version");
  const number = versionNumber(asked);
  const read = useWorkflow(ref, number);
  const workflow = read.data;
  const versions = workflow?.versions ?? [];
  let version: WorkflowVersion | null = null;
  if (workflow) {
    if (asked === null) version = workflow.version;
    else if (number !== null && workflow.version.version === number) version = workflow.version;
    else version = findVersion(versions, asked);
  }
  // The version as its list holds it (with its suite), the code being the workflow's.
  const listed = version ? (versions.find((v) => v.version === version?.version) ?? version) : null;
  const latest = listed !== null && versions[0]?.version === listed.version;

  // The address names the workflow by its id, and the version by its number.
  const id = workflow?.id;
  const canonical = listed && asked !== null && number === null ? String(listed.version) : asked;
  useEffect(() => {
    if (!id) return;
    if (ref === id && canonical === asked) return;
    const next = new URLSearchParams(location.search);
    if (canonical === null) next.delete("version");
    else next.set("version", canonical);
    const pathname = location.pathname.replace(
      /^\/workflows\/[^/]+/,
      `/workflows/${encodeURIComponent(id)}`,
    );
    const search = next.toString();
    void navigate(`${pathname}${search ? `?${search}` : ""}`, { replace: true });
  }, [id, ref, canonical, asked, location.pathname, location.search, navigate]);

  return { read, workflow, versions, version: listed, asked, latest };
}
