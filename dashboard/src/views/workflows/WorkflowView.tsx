// A workflow: its versions, newest first. A version shows its lineage (the version it derives
// from), its script, toolset and schemas, and its suite: the tests it inherited from its parent,
// added (recorded from runs among them), updated and retired, with the reasons (archi/05, "Suites
// and the gate"). A test opens below its suite, on its world and its expectations. The version
// and the test opened are in the URL (`?version=…&test=…`).
import { ArrowRight, GitBranch } from "lucide-react";
import { useEffect, useRef } from "react";
import { Link, useParams, useSearchParams } from "react-router";
import { useWorkflowVersions } from "../../api/queries";
import type { Workflow } from "../../api/types";
import { Ago, Digest, LabelList, Loading, ProblemView } from "../../components/bits";
import { PageHeader } from "../../components/PageHeader";
import { JsonValue } from "../../components/Payload";
import {
  findVersion,
  type SuiteEntry,
  type SuiteKind,
  suiteCounts,
  suiteOf,
  workflowRef,
} from "../../model/workflows";
import { TestDocument } from "./TestDocument";
import { WorkflowsGate } from "./WorkflowsView";

const KIND_TAG: Record<SuiteKind, { label: string; className: string; title: string }> = {
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
};

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

interface Selection {
  version: string | null;
  test: string | null;
}

function TestRow({
  entry,
  open,
  onOpen,
}: {
  entry: SuiteEntry;
  open: boolean;
  onOpen: (digest: string) => void;
}) {
  const tag = KIND_TAG[entry.kind];
  return (
    <li className={`suite-test${open ? " open" : ""}`}>
      <span className={tag.className} title={tag.title}>
        {tag.label}
      </span>
      <button
        type="button"
        className="link-btn suite-name"
        aria-expanded={open}
        onClick={() => onOpen(entry.digest)}
      >
        {entry.name}
      </button>
      {entry.recordedFrom && (
        <Link
          to={`/runs/${entry.recordedFrom}`}
          className="nowrap"
          title={`The run it was recorded from: ${entry.recordedFrom}`}
        >
          its run
        </Link>
      )}
      {entry.kind === "updated" && entry.from && (
        <span className="suite-change">
          <button
            type="button"
            className="link-btn"
            title="The parent's test it replaced"
            onClick={() => onOpen(entry.from as string)}
          >
            <span className="digest">{entry.from.slice(0, 12)}</span>
          </button>
          <ArrowRight size={12} aria-hidden="true" />
          <span className="digest" title={entry.digest}>
            {entry.digest.slice(0, 12)}
          </span>
        </span>
      )}
      {entry.kind !== "updated" && (
        <span className="digest end" title={entry.digest}>
          {entry.digest.slice(0, 12)}
        </span>
      )}
      {entry.kind === "updated" && (
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
  latest,
  selection,
  select,
}: {
  version: Workflow;
  versions: readonly Workflow[];
  latest: boolean;
  selection: Selection;
  select: (next: Selection) => void;
}) {
  const ref = useRef<HTMLElement>(null);
  const suite = suiteOf(version, versions);
  const counts = suiteCounts(suite);
  const current = selection.version === version.version;
  const opened = current ? selection.test : null;
  const openedName =
    opened === null
      ? ""
      : (suite.tests.find((t) => t.digest === opened)?.name ??
        suite.tests.find((t) => t.from === opened)?.name ??
        suite.retired.find((t) => t.digest === opened)?.name ??
        "");

  useEffect(() => {
    if (current) ref.current?.scrollIntoView?.({ block: "nearest", behavior: "smooth" });
  }, [current]);

  const toVersion = (v: string) => select({ version: v, test: null });
  const openTest = (digest: string) =>
    select({ version: version.version, test: opened === digest ? null : digest });
  const summary = [
    counts.inherited && `${counts.inherited} inherited`,
    counts.added && `${counts.added} added`,
    counts.updated && `${counts.updated} updated`,
    counts.kept && `${counts.kept} kept`,
    counts.retired && `${counts.retired} retired`,
  ].filter(Boolean);

  return (
    <section
      ref={ref}
      className={`panel version${current ? " current" : ""}`}
      aria-label={`Version ${workflowRef(version)}`}
      aria-current={current ? "true" : undefined}
    >
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
                  onClick={() => toVersion(suite.parent?.version ?? "")}
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
                <TestRow
                  key={entry.name}
                  entry={entry}
                  open={opened === entry.digest || (opened !== null && opened === entry.from)}
                  onOpen={openTest}
                />
              ))}
            </ul>
          )}
          {suite.retired.length > 0 && (
            <>
              <h4>Retired from its parent</h4>
              <ul className="suite-list">
                {suite.retired.map((t) => (
                  <li key={t.name} className={`suite-test${opened === t.digest ? " open" : ""}`}>
                    <span className="tag critical">retired</span>
                    <button
                      type="button"
                      className="link-btn suite-name"
                      aria-expanded={opened === t.digest}
                      onClick={() => openTest(t.digest)}
                    >
                      {t.name}
                    </button>
                    <span className="digest end" title={t.digest}>
                      {t.digest.slice(0, 12)}
                    </span>
                    <p className="suite-reason">Why: {t.reason}</p>
                  </li>
                ))}
              </ul>
            </>
          )}
        </section>

        {opened !== null && (
          <TestDocument
            key={opened}
            digest={opened}
            name={openedName}
            workflow={version.name}
            versions={versions}
            onPickVersion={toVersion}
            onClose={() => select({ version: version.version, test: null })}
          />
        )}
      </div>
    </section>
  );
}

export function WorkflowView() {
  return (
    <WorkflowsGate>
      <WorkflowPage />
    </WorkflowsGate>
  );
}

function WorkflowPage() {
  const { name = "" } = useParams();
  const [params, setParams] = useSearchParams();
  const versions = useWorkflowVersions(name);
  const list = versions.data ?? [];
  const asked = params.get("version");
  const selection: Selection = {
    version: asked ? (findVersion(list, asked)?.version ?? asked) : null,
    test: params.get("test"),
  };
  const select = (next: Selection) => {
    const out = new URLSearchParams();
    if (next.version) out.set("version", next.version);
    if (next.version && next.test) out.set("test", next.test);
    setParams(out, { replace: true });
  };
  const latest = list[0];

  return (
    <div className="page">
      <PageHeader crumbs={<Link to="/workflows">Workflows</Link>} title={name} />
      {versions.isPending ? (
        <Loading>Loading its versions…</Loading>
      ) : versions.isError ? (
        <ProblemView error={versions.error} title={`The workflow ${name} could not be read`} />
      ) : (
        <>
          <p className="page-intro">
            {latest?.description ? `${latest.description} ` : ""}
            {list.length} version{list.length === 1 ? "" : "s"}, the newest first. Each passed its
            suite when it was saved; a test changed on purpose says why.
          </p>
          {list.map((v, i) => (
            <VersionPanel
              key={v.version}
              version={v}
              versions={list}
              latest={i === 0}
              selection={selection}
              select={select}
            />
          ))}
        </>
      )}
    </div>
  );
}
