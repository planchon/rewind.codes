// A workflow in Testing, at one version: the latest unless `?version=` names another, picked in
// the breadcrumb. The page shows the version's interface (its input, output and signal schemas)
// and its suite as a table: the tests it inherited from its parent, added (recorded from runs
// among them), updated and retired, with the reasons (archi/05, "Suites and the gate"). The
// sidebar lists the workflows, the suite and the version's details; each test has its own page.
import { useParams, useSearchParams } from "react-router";
import { useWorkflowVersions } from "../../api/queries";
import type { Workflow } from "../../api/types";
import { Loading, ProblemView } from "../../components/bits";
import { JsonPeek } from "../../components/JsonPeek";
import { PageHeader } from "../../components/PageHeader";
import {
  findVersion,
  type SuiteKind,
  suiteCounts,
  suiteOf,
  workflowPath,
  workflowRef,
} from "../../model/workflows";
import { CrumbLink, Crumbs, Switcher, type SwitchGroup } from "./Crumbs";
import { SuiteTable } from "./SuiteTable";
import { TestingSidebar } from "./TestingSidebar";
import { TestingGate } from "./TestingView";

export const KIND_TAG: Record<
  SuiteKind | "retired",
  { label: string; className: string; title: string }
> = {
  inherited: { label: "inherited", className: "tag", title: "Kept from its parent as it was" },
  added: { label: "added", className: "tag info", title: "New in this version" },
  updated: {
    label: "updated",
    className: "tag warning",
    title: "Replaces the parent's test of the same name",
  },
  kept: {
    label: "in its suite",
    className: "tag",
    title: "Its parent is not among this workflow's versions: inherited or added",
  },
  retired: {
    label: "retired",
    className: "tag critical",
    title: "A test of the parent this version dropped",
  },
};

/**
 * The version a page shows: the one `?version=` names, or the latest. `null` while the versions
 * load, or when the version named is not among them.
 */
export function useVersion(versions: readonly Workflow[]): {
  version: Workflow | null;
  asked: string | null;
} {
  const [params] = useSearchParams();
  const asked = params.get("version");
  return { version: asked ? findVersion(versions, asked) : (versions[0] ?? null), asked };
}

/** The versions to switch to, newest first, each leading where `to` says. */
export function versionGroups(
  versions: readonly Workflow[],
  current: Workflow,
  to: (version: Workflow, latest: boolean) => string,
): SwitchGroup[] {
  return [
    {
      label: "Versions, newest first",
      items: versions.map((v, i) => {
        const tests = v.tests?.length ?? 0;
        return {
          key: v.version,
          label: <span className="mono">{v.version.slice(0, 12)}</span>,
          end: `${i === 0 ? "latest · " : ""}${tests} test${tests === 1 ? "" : "s"}`,
          to: to(v, i === 0),
          current: v.version === current.version,
        };
      }),
    },
  ];
}

/** The version's segment of the breadcrumb: its digest, and whether it is the latest. */
export function VersionSwitcher({
  versions,
  current,
  to,
}: {
  versions: readonly Workflow[];
  current: Workflow;
  to: (version: Workflow, latest: boolean) => string;
}) {
  const latest = versions[0]?.version === current.version;
  return (
    <Switcher
      title="Version"
      label={
        <>
          <span className="mono">{current.version.slice(0, 12)}</span>
          {latest && <span className="tag info">latest</span>}
        </>
      }
      groups={versionGroups(versions, current, to)}
    />
  );
}

function Interface({ version }: { version: Workflow }) {
  const signals = Object.entries(version.signals);
  return (
    <section className="panel" aria-label="Interface">
      <header className="panel-head">
        <h2>Interface</h2>
        <span className="end muted">
          the JSON Schemas its runs and its tests are checked against
        </span>
      </header>
      <div className="panel-body">
        <dl className="kv">
          <div>
            <dt>Input</dt>
            <dd>
              <JsonPeek value={version.input} title={`${workflowRef(version)}: input schema`} />
            </dd>
          </div>
          <div>
            <dt>Output</dt>
            <dd>
              <JsonPeek value={version.output} title={`${workflowRef(version)}: output schema`} />
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
                      <JsonPeek
                        value={schema}
                        title={`${workflowRef(version)}: the schema of the signal ${name}`}
                      />
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
  return (
    <TestingGate>
      <WorkflowPage />
    </TestingGate>
  );
}

function WorkflowPage() {
  const { name = "" } = useParams();
  const versions = useWorkflowVersions(name);
  const list = versions.data ?? [];
  const { version, asked } = useVersion(list);
  const latest = list[0];
  const suite = version ? suiteOf(version, list) : null;
  const counts = suite ? suiteCounts(suite) : null;
  const summary = counts
    ? [
        counts.inherited && `${counts.inherited} inherited`,
        counts.added && `${counts.added} added`,
        counts.updated && `${counts.updated} updated`,
        counts.kept && `${counts.kept} kept`,
        counts.retired && `${counts.retired} retired`,
      ].filter(Boolean)
    : [];

  return (
    <div className="page">
      <TestingSidebar workflow={name} version={version} versions={list} />
      <PageHeader
        crumbs={
          <Crumbs>
            <CrumbLink to="/testing">Testing</CrumbLink>
            <CrumbLink to={workflowPath(name)}>{name}</CrumbLink>
          </Crumbs>
        }
        title={
          version ? (
            <VersionSwitcher
              versions={list}
              current={version}
              to={(v, isLatest) => workflowPath(name, isLatest ? null : v.version)}
            />
          ) : (
            (asked?.slice(0, 12) ?? "")
          )
        }
      />
      {versions.isPending ? (
        <Loading>Loading its versions…</Loading>
      ) : versions.isError ? (
        <ProblemView error={versions.error} title={`The workflow ${name} could not be read`} />
      ) : version === null ? (
        <p className="page-intro">
          No version {asked} among the {list.length} versions of {name}.{" "}
          <CrumbLink to={workflowPath(name)}>Its latest version</CrumbLink>
        </p>
      ) : (
        <div className="stack">
          <p className="page-intro">
            {latest?.description ? `${latest.description} ` : ""}
            {list.length} version{list.length === 1 ? "" : "s"}. Each passed its suite when it was
            saved; a test changed on purpose says why.
          </p>
          <Interface version={version} />
          <section className="suite" aria-label={`Suite of ${workflowRef(version)}`}>
            <div className="section-head">
              <h2>
                Suite <span className="count">{version.tests?.length ?? 0}</span>
              </h2>
              {summary.length > 0 && <span className="dim">{summary.join(" · ")}</span>}
            </div>
            <SuiteTable version={version} versions={list} />
          </section>
        </div>
      )}
    </div>
  );
}
