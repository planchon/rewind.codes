// Testing: each workflow, by name, with its latest version: what it does, the size of its suite,
// when it was saved (archi/05, "Dashboard"). A workflow opens on its latest version.
import type { ReactNode } from "react";
import { Link, useNavigate } from "react-router";
import { useWorkflows } from "../../api/queries";
import type { Workflow } from "../../api/types";
import { useAccess } from "../../app/access";
import { NoPermission } from "../../app/NoPermission";
import { Ago, Digest, Empty, LabelList, Loading, ProblemView } from "../../components/bits";
import { PageHeader } from "../../components/PageHeader";
import { formatCount } from "../../model/format";
import { workflowPath } from "../../model/workflows";
import { TestingSidebar } from "./TestingSidebar";

/** Testing needs `toolsets:read`: workflows are read with the toolsets. */
export function TestingGate({ children }: { children: ReactNode }) {
  const access = useAccess();
  if (access.can("toolsets:read")) return <>{children}</>;
  return (
    <div className="page">
      <NoPermission permission="toolsets:read" what="see the workflows and their tests" />
    </div>
  );
}

function WorkflowRow({ workflow }: { workflow: Workflow }) {
  const navigate = useNavigate();
  const tests = workflow.tests?.length ?? 0;
  const to = workflowPath(workflow.name);
  return (
    <tr
      className="link"
      onClick={(e) => {
        if ((e.target as HTMLElement).closest("a, button")) return;
        navigate(to);
      }}
    >
      <td>
        <Link to={to} className="mono">
          {workflow.name}
        </Link>
      </td>
      <td className="wrap workflow-description">
        {workflow.description || <span className="muted">–</span>}
      </td>
      <td>
        <Digest digest={workflow.version} />
      </td>
      <td className="right num">{formatCount(tests)}</td>
      <td className="wrap">
        <LabelList labels={workflow.labels} />
      </td>
      <td className="dim">
        <Ago ms={Date.parse(workflow.createdAt)} />
      </td>
    </tr>
  );
}

export function TestingView() {
  return (
    <TestingGate>
      <WorkflowList />
    </TestingGate>
  );
}

function WorkflowList() {
  const workflows = useWorkflows();
  const list = workflows.data?.pages.flatMap((p) => p.workflows) ?? [];
  return (
    <div className="page">
      <TestingSidebar />
      <PageHeader title="Testing" />
      <p className="page-intro">
        Each workflow and its suite: the tests every version passed when it was saved, and that the
        next version must pass too.
      </p>
      {workflows.isPending ? (
        <Loading>Loading workflows…</Loading>
      ) : workflows.isError ? (
        <ProblemView error={workflows.error} title="The workflows could not be read" />
      ) : (
        <div className="table-wrap">
          <table className="grid">
            <thead>
              <tr>
                <th>Workflow</th>
                <th>Description</th>
                <th>Latest version</th>
                <th className="right">Tests</th>
                <th>Labels</th>
                <th>Saved</th>
              </tr>
            </thead>
            <tbody>
              {list.map((w) => (
                <WorkflowRow key={w.name} workflow={w} />
              ))}
            </tbody>
          </table>
          {list.length === 0 && (
            <Empty>No workflow yet: the agent tool save_workflow saves one with its tests.</Empty>
          )}
          {workflows.hasNextPage && (
            <div className="list-foot">
              <span className="num">{formatCount(list.length)} workflows shown</span>
              <button
                type="button"
                className="btn small"
                disabled={workflows.isFetchingNextPage}
                onClick={() => void workflows.fetchNextPage()}
              >
                {workflows.isFetchingNextPage ? "Loading…" : "Load more"}
              </button>
            </div>
          )}
        </div>
      )}
    </div>
  );
}
