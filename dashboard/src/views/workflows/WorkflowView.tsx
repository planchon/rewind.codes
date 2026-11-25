// A workflow's page (archi/05, "Workflows"), at `/workflows/<id>`, at one version: its latest
// unless `?version=` names another, picked in the breadcrumb. The page shows the version's code,
// its interface (its input, output and signal schemas) and its suite as a table: the tests it
// inherited from its base, added (recorded from runs among them), replaced and retired, with the
// reasons (archi/05, "Suites and the gate"). Its sidebar holds its versions, the suite and the
// version's details; each test has its own page. Whoever holds `workflows:write` names, describes,
// promotes, forks and pulls it from its header.
import { Navigate, useParams } from "react-router";
import { useObjectNamespace } from "../../app/scope";
import { Loading, ProblemView } from "../../components/bits";
import { CodeView } from "../../components/CodeView";
import { JsonPeek } from "../../components/JsonPeek";
import { PageHeader } from "../../components/PageHeader";
import type { WorkflowVersion } from "../../model/types";
import {
  shortIdOf,
  suiteCounts,
  suiteOf,
  versionLabel,
  workflowLabel,
  workflowPath,
} from "../../model/workflows";
import { useWorkflowLabels, useWorkflowList } from "../../state/workflows";
import { VersionSwitcher } from "./bits";
import { CrumbLink, Crumbs } from "./Crumbs";
import { SuiteTable } from "./SuiteTable";
import { useShownWorkflow } from "./shown";
import { WorkflowControls } from "./WorkflowControls";
import { WorkflowSidebar } from "./WorkflowSidebar";

function Interface({ version, label }: { version: WorkflowVersion; label: string }) {
  const signals = Object.entries(version.signals ?? {});
  const ref = versionLabel(label, version.version);
  return (
    <section className="panel" aria-label="Interface">
      <header className="panel-head">
        <h2>Interface</h2>
      </header>
      <div className="panel-body">
        <dl className="kv">
          <div>
            <dt>Input</dt>
            <dd>
              <JsonPeek value={version.input ?? null} title={`${ref}: input schema`} />
            </dd>
          </div>
          <div>
            <dt>Output</dt>
            <dd>
              <JsonPeek value={version.output ?? null} title={`${ref}: output schema`} />
            </dd>
          </div>
          <div>
            <dt>Signals</dt>
            <dd>
              {signals.length === 0 ? (
                <span className="muted">none</span>
              ) : (
                <ul className="signal-list">
                  {signals.map(([name, schema]) => (
                    <li key={name}>
                      <span className="mono">{name}</span>{" "}
                      <JsonPeek value={schema} title={`${ref}: the schema of the signal ${name}`} />
                    </li>
                  ))}
                </ul>
              )}
            </dd>
          </div>
        </dl>
      </div>
    </section>
  );
}

export function WorkflowView() {
  const { id = "" } = useParams();
  // Another workflow is another page: it never shows the one before while it loads.
  return <WorkflowPage key={id} reference={id} />;
}

function WorkflowPage({ reference }: { reference: string }) {
  const shown = useShownWorkflow(reference);
  const { read, workflow, versions, version, asked, latest } = shown;
  // A namespace's workflow: the picker and the sidebar show its namespace.
  useObjectNamespace(workflow?.namespace);
  const labelOf = useWorkflowLabels();
  const label = workflow
    ? workflowLabel({ ...workflow, comment: versions[0]?.comment })
    : reference.startsWith("wf_")
      ? shortIdOf(reference)
      : reference;
  const source = workflow?.source ?? null;
  const sourceLabel = source ? (labelOf(source.workflow) ?? shortIdOf(source.workflow)) : null;
  const suite = version ? suiteOf(version, versions) : null;
  const counts = suite ? suiteCounts(suite) : null;
  const summary = counts
    ? [
        counts.inherited && `${counts.inherited} inherited`,
        counts.added && `${counts.added} added`,
        counts.replaced && `${counts.replaced} replaced`,
        counts.kept && `${counts.kept} kept`,
        counts.retired && `${counts.retired} retired`,
      ].filter(Boolean)
    : [];
  const description = workflow
    ? (workflow.description ?? (workflow.name ? null : versions[0]?.comment?.description) ?? null)
    : null;

  return (
    <div className="page">
      <WorkflowSidebar workflow={workflow} version={version} versions={versions} latest={latest} />
      <PageHeader
        crumbs={
          <Crumbs>
            <CrumbLink to="/workflows">Workflows</CrumbLink>
            <CrumbLink to={workflow ? workflowPath(workflow.id) : workflowPath(reference)}>
              <span
                className={workflow?.name || workflow?.versions[0]?.comment ? undefined : "mono"}
              >
                {label}
              </span>
            </CrumbLink>
          </Crumbs>
        }
        title={
          workflow && version ? (
            <VersionSwitcher
              versions={versions}
              current={version}
              to={(v, isLatest) => workflowPath(workflow.id, isLatest ? null : v.version)}
            />
          ) : (
            (asked ?? "")
          )
        }
        info={
          workflow && (
            <>
              {description && <p className="info-card-head">{description}</p>}
              <p>
                {source
                  ? `A fork of ${sourceLabel} v${source.version}, in its namespace. `
                  : workflow.namespace
                    ? "A namespace's workflow: its runs are that end user's. "
                    : "An organization's workflow: any namespace's runs execute it. "}
                Every test of a suite passed when it joined it; a test changed on purpose says why.
              </p>
            </>
          )
        }
        actions={
          workflow && (
            <WorkflowControls workflow={workflow} label={label} sourceLabel={sourceLabel} />
          )
        }
      />
      {read.isPending ? (
        <Loading>Loading the workflow…</Loading>
      ) : read.isError ? (
        <ProblemView
          error={read.error}
          title={
            asked ? `Version ${asked} of ${label} could not be read` : `${label} could not be read`
          }
        />
      ) : workflow && version === null ? (
        <p className="page-intro">
          No version {asked} among the {versions.length} versions of {label}.{" "}
          <CrumbLink to={workflowPath(workflow.id)}>Its latest version</CrumbLink>
        </p>
      ) : workflow && version ? (
        <div className="stack">
          <section className="panel" aria-label="Code">
            <header className="panel-head">
              <h2>Code</h2>
              <span className="muted">
                workflow.ts · {workflow.code.trimEnd().split("\n").length} lines
              </span>
            </header>
            <CodeView source={workflow.code} />
          </section>
          {(version.input || version.output || version.signals) && (
            <Interface version={version} label={label} />
          )}
          <section
            className="suite"
            aria-label={`Suite of ${versionLabel(label, version.version)}`}
          >
            <div className="section-head">
              <h2>
                Suite <span className="count">{version.tests?.length ?? 0}</span>
              </h2>
              {summary.length > 0 && <span className="dim">{summary.join(" · ")}</span>}
            </div>
            <SuiteTable
              workflow={workflow.id}
              label={label}
              version={version}
              versions={versions}
              latest={latest}
            />
          </section>
        </div>
      ) : null}
    </div>
  );
}

/**
 * `/scripts/<digest>`, a link of an earlier version: the workflow one of whose versions has that
 * code, at its page.
 */
export function WorkflowOfCode() {
  const { digest = "" } = useParams();
  const found = useWorkflowList({ query: { code: digest, limit: 1 }, namespace: null });
  if (found.status === "pending") return <Loading>Finding its workflow…</Loading>;
  if (found.status === "error")
    return (
      <div className="page">
        <ProblemView error={found.error} title="Its workflow could not be read" />
      </div>
    );
  const first = found.rows[0];
  if (!first)
    return (
      <div className="page">
        <ProblemView
          error={new Error(`No workflow has the code ${digest.slice(0, 12)}.`)}
          title="Not found"
        />
      </div>
    );
  return <Navigate to={workflowPath(first.id)} replace />;
}
