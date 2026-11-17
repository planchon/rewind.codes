// On a script's page: the suites of the workflow versions whose script it is, the newest version
// of each workflow first. A script that no version names has no stored tests: the tests of an
// ad hoc script are not kept (archi/05, "Suites and the gate").
import { Link } from "react-router";
import { useWorkflowsWithScript, useWorkflowVersions } from "../../api/queries";
import type { Workflow } from "../../api/types";
import { useAccess } from "../../app/access";
import { Loading, ProblemView } from "../../components/bits";
import { workflowPath, workflowRef } from "../../model/workflows";
import { SuiteTable } from "./SuiteTable";

function WorkflowSuite({ newest, older }: { newest: Workflow; older: number }) {
  const versions = useWorkflowVersions(newest.name);
  const list = versions.data ?? [];
  const latest = list[0]?.version === newest.version;
  return (
    <section className="suite" aria-label={`Suite of ${workflowRef(newest)}`}>
      <div className="section-head">
        <h3>
          <Link to={workflowPath(newest.name, latest ? null : newest.version)} className="mono">
            {workflowRef(newest)}
          </Link>{" "}
          {latest && <span className="tag info">latest</span>}
        </h3>
        <span className="dim">
          {newest.tests?.length ?? 0} test{newest.tests?.length === 1 ? "" : "s"}
          {older > 0 && ` · also the script of ${older} older version${older === 1 ? "" : "s"}`}
        </span>
      </div>
      {versions.isPending ? (
        <Loading>Loading its suite…</Loading>
      ) : (
        <SuiteTable version={newest} versions={list.length > 0 ? list : [newest]} />
      )}
    </section>
  );
}

export function ScriptSuites({ digest }: { digest: string }) {
  const access = useAccess();
  const readable = access.can("toolsets:read");
  const found = useWorkflowsWithScript(digest, readable);
  if (!readable) return null;
  const byName = new Map<string, Workflow[]>();
  for (const v of found.data ?? []) byName.set(v.name, [...(byName.get(v.name) ?? []), v]);
  return (
    <section className="stack" aria-label="Tests of this script">
      <div className="section-head">
        <h2>Tests of this script</h2>
      </div>
      {found.isPending ? (
        <Loading>Looking for the workflows it is the script of…</Loading>
      ) : found.isError ? (
        <ProblemView error={found.error} title="Its workflows could not be read" />
      ) : byName.size === 0 ? (
        <p className="muted">
          No workflow version has this script: its tests are not stored. The tests of a script
          become a suite when it is saved as a workflow.
        </p>
      ) : (
        [...byName.values()].map((versions) => (
          <WorkflowSuite
            key={versions[0]?.name}
            newest={versions[0] as Workflow}
            older={versions.length - 1}
          />
        ))
      )}
    </section>
  );
}
