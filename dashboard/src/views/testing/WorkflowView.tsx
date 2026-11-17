// A workflow in Testing, at one version: the latest unless `?version=` names another, picked in
// the header. The version shows its lineage (the version it derives from), its script, toolset and
// schemas, and its suite: the tests it inherited from its parent, added (recorded from runs among
// them), updated and retired, with the reasons (archi/05, "Suites and the gate"). Each test opens
// on its own page.
import { ArrowRight, GitBranch } from "lucide-react";
import { Link, useNavigate, useParams, useSearchParams } from "react-router";
import { useWorkflowVersions } from "../../api/queries";
import type { Workflow } from "../../api/types";
import { Ago, Digest, LabelList, Loading, ProblemView } from "../../components/bits";
import { PageHeader } from "../../components/PageHeader";
import { JsonValue } from "../../components/Payload";
import {
  findVersion,
  type SuiteKind,
  suiteCounts,
  suiteOf,
  suitePlaces,
  type TestPlace,
  testPath,
  workflowPath,
  workflowRef,
} from "../../model/workflows";
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

/** The versions of a workflow, newest first, to pick one: in the header of its pages. */
export function VersionPicker({
  versions,
  current,
  onPick,
}: {
  versions: readonly Workflow[];
  current: Workflow;
  onPick: (version: string) => void;
}) {
  return (
    <select
      className="select mono version-picker"
      aria-label="Version"
      value={current.version}
      onChange={(e) => onPick(e.target.value)}
    >
      {versions.map((v, i) => {
        const tests = v.tests?.length ?? 0;
        return (
          <option key={v.version} value={v.version}>
            {workflowRef(v)}
            {i === 0 ? " · latest" : ""} · {tests} test{tests === 1 ? "" : "s"}
          </option>
        );
      })}
    </select>
  );
}

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

/** A JSON Schema, in short: its properties and their types, or its type. */
function schemaLine(schema: unknown): string {
  if (typeof schema !== "object" || schema === null) return JSON.stringify(schema);
  const s = schema as {
    type?: unknown;
    properties?: Record<string, { type?: unknown }>;
    required?: string[];
  };
  if (s.properties && typeof s.properties === "object") {
    const required = new Set(s.required ?? []);
    const fields = Object.entries(s.properties).map(([key, value]) => {
      const type = typeof value?.type === "string" ? value.type : "…";
      return `${key}${required.has(key) ? "" : "?"}: ${type}`;
    });
    return `{ ${fields.join(", ")} }`;
  }
  return typeof s.type === "string" ? s.type : Object.keys(s).length === 0 ? "any" : "…";
}

function Schema({ name, schema }: { name: string; schema: unknown }) {
  return (
    <details className="schema">
      <summary title={`${name}: its JSON Schema`}>
        <code>{schemaLine(schema)}</code>
      </summary>
      <div className="json-box">
        <JsonValue value={schema} />
      </div>
    </details>
  );
}

function TestRow({
  entry,
  workflow,
  parent,
}: {
  entry: TestPlace;
  workflow: Workflow;
  parent: Workflow | null;
}) {
  const tag = KIND_TAG[entry.kind];
  return (
    <li className="suite-test">
      <span className={tag.className} title={tag.title}>
        {tag.label}
      </span>
      <Link className="suite-name" to={testPath(workflow.name, entry.digest, workflow.version)}>
        {entry.name}
      </Link>
      {entry.recordedFrom && (
        <Link
          to={`/runs/${entry.recordedFrom}`}
          className="nowrap"
          title={`The run it was recorded from: ${entry.recordedFrom}`}
        >
          its run
        </Link>
      )}
      {entry.kind === "updated" && entry.from ? (
        <span className="suite-change">
          <Link
            to={testPath(workflow.name, entry.from, parent?.version)}
            title="The parent's test it replaced"
          >
            <span className="digest">{entry.from.slice(0, 12)}</span>
          </Link>
          <ArrowRight size={12} aria-hidden="true" />
          <span className="digest" title={entry.digest}>
            {entry.digest.slice(0, 12)}
          </span>
        </span>
      ) : (
        <span className="digest end" title={entry.digest}>
          {entry.digest.slice(0, 12)}
        </span>
      )}
      {(entry.kind === "updated" || entry.kind === "retired") && (
        <p className="suite-reason">
          {entry.reason ? (
            <>Why: {entry.reason}</>
          ) : (
            <span className="muted">No reason recorded.</span>
          )}
        </p>
      )}
    </li>
  );
}

function VersionPanel({
  version,
  versions,
  onPick,
}: {
  version: Workflow;
  versions: readonly Workflow[];
  onPick: (version: string) => void;
}) {
  const suite = suiteOf(version, versions);
  const counts = suiteCounts(suite);
  const latest = versions[0]?.version === version.version;
  const summary = [
    counts.inherited && `${counts.inherited} inherited`,
    counts.added && `${counts.added} added`,
    counts.updated && `${counts.updated} updated`,
    counts.kept && `${counts.kept} kept`,
    counts.retired && `${counts.retired} retired`,
  ].filter(Boolean);
  const retired = suitePlaces(suite).filter((place) => place.kind === "retired");

  return (
    <section className="panel version" aria-label={`Version ${workflowRef(version)}`}>
      <header className="panel-head">
        <h2>
          <GitBranch size={14} aria-hidden="true" />
          <span className="mono">{workflowRef(version)}</span>
        </h2>
        {latest && <span className="tag info">latest</span>}
        <span className="end muted">
          saved <Ago ms={Date.parse(version.createdAt)} />
        </span>
      </header>
      <div className="panel-body stack">
        <dl className="kv">
          <div>
            <dt>Derives from</dt>
            <dd>
              {!version.parent ? (
                <span className="dim">nothing: the first version</span>
              ) : suite.parent ? (
                <button
                  type="button"
                  className="link-btn mono"
                  onClick={() => onPick(suite.parent?.version ?? "")}
                >
                  {workflowRef(suite.parent)}
                </button>
              ) : (
                <>
                  <Digest digest={version.parent} />{" "}
                  <span className="muted">(not among the versions of {version.name})</span>
                </>
              )}
            </dd>
          </div>
          <div>
            <dt>Script</dt>
            <dd>
              <Link to={`/scripts/${version.script}`} title="Its source">
                <span className="digest">{version.script.slice(0, 12)}</span>
              </Link>
            </dd>
          </div>
          <div>
            <dt>Toolset</dt>
            <dd>
              <Digest digest={version.toolset} />
            </dd>
          </div>
          <div>
            <dt>Input</dt>
            <dd>
              <Schema name="Input" schema={version.input} />
            </dd>
          </div>
          <div>
            <dt>Output</dt>
            <dd>
              <Schema name="Output" schema={version.output} />
            </dd>
          </div>
          <div>
            <dt>Signals</dt>
            <dd>
              {Object.keys(version.signals).length === 0 ? (
                <span className="muted">none</span>
              ) : (
                <ul className="signal-list">
                  {Object.entries(version.signals).map(([name, schema]) => (
                    <li key={name}>
                      <span className="mono">{name}</span>{" "}
                      <Schema name={`Signal ${name}`} schema={schema} />
                    </li>
                  ))}
                </ul>
              )}
            </dd>
          </div>
          <div>
            <dt>Labels</dt>
            <dd>
              <LabelList labels={version.labels} />
            </dd>
          </div>
        </dl>

        <section className="suite" aria-label={`Suite of ${workflowRef(version)}`}>
          <h3>
            Suite <span className="count">{suite.tests.length}</span>
            {summary.length > 0 && <span className="dim suite-summary">{summary.join(" · ")}</span>}
          </h3>
          {suite.tests.length === 0 ? (
            <p className="muted">No test: this version was saved with an empty suite.</p>
          ) : (
            <ul className="suite-list">
              {suite.tests.map((entry) => (
                <TestRow key={entry.name} entry={entry} workflow={version} parent={suite.parent} />
              ))}
            </ul>
          )}
          {retired.length > 0 && (
            <>
              <h4>Retired from its parent</h4>
              <ul className="suite-list">
                {retired.map((entry) => (
                  <TestRow
                    key={entry.name}
                    entry={entry}
                    workflow={version}
                    parent={suite.parent}
                  />
                ))}
              </ul>
            </>
          )}
        </section>
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
  const navigate = useNavigate();
  const versions = useWorkflowVersions(name);
  const list = versions.data ?? [];
  const { version, asked } = useVersion(list);
  const latest = list[0];
  const pick = (v: string) => navigate(workflowPath(name, v === latest?.version ? null : v));

  return (
    <div className="page">
      <PageHeader
        crumbs={<Link to="/testing">Testing</Link>}
        title={name}
        actions={
          version ? <VersionPicker versions={list} current={version} onPick={pick} /> : undefined
        }
      />
      {versions.isPending ? (
        <Loading>Loading its versions…</Loading>
      ) : versions.isError ? (
        <ProblemView error={versions.error} title={`The workflow ${name} could not be read`} />
      ) : version === null ? (
        <p className="page-intro">
          No version {asked} among the {list.length} versions of {name}.{" "}
          <Link to={workflowPath(name)}>Its latest version</Link>
        </p>
      ) : (
        <>
          <p className="page-intro">
            {latest?.description ? `${latest.description} ` : ""}
            {list.length} version{list.length === 1 ? "" : "s"}. Each passed its suite when it was
            saved; a test changed on purpose says why.
          </p>
          <VersionPanel version={version} versions={list} onPick={pick} />
        </>
      )}
    </div>
  );
}
