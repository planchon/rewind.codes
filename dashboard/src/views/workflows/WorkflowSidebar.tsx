// A workflow's own sidebar (archi/05, "Workflows"): its versions, the suite of the version shown
// (each test a click away, how it came into the suite as a glyph), then the version's details.
// "Back" leads up to the list of workflows.
import { GitCommitVertical, LayoutList } from "lucide-react";
import { Link } from "react-router";
import { PageSidebar } from "../../app/sidebar";
import { Ago, CopyButton, Digest, LabelList } from "../../components/bits";
import { formatCount } from "../../model/format";
import type { Workflow, WorkflowVersion } from "../../model/types";
import {
  shortIdOf,
  suiteOf,
  suitePlaces,
  testPath,
  workflowLabel,
  workflowPath,
} from "../../model/workflows";
import { useWorkflowLabels } from "../../state/workflows";
import { ORIGIN_TITLE, OriginGlyph, Scope } from "./bits";

function Versions({
  workflow,
  versions,
  current,
  test,
}: {
  workflow: Workflow;
  versions: readonly WorkflowVersion[];
  current: WorkflowVersion;
  test: string | null;
}) {
  return (
    <section className="pane-section" aria-label="Versions">
      <h2 className="pane-label">Versions</h2>
      <nav className="nav" aria-label="Versions">
        {versions.map((v, i) => (
          <Link
            key={v.version}
            to={workflowPath(workflow.id, i === 0 ? null : v.version)}
            className="nav-item"
            aria-current={v.version === current.version && test === null ? "page" : undefined}
            data-open={v.version === current.version ? "" : undefined}
            title={`v${v.version}${i === 0 ? ", its latest" : ""}: ${formatCount(v.tests?.length ?? 0)} tests in its suite`}
          >
            <span className="nav-icon" aria-hidden="true">
              <GitCommitVertical size={16} />
            </span>
            <span className="nav-label num">
              v{v.version}
              {i === 0 && <span className="nav-meta"> latest</span>}
            </span>
            <span className="nav-end nav-meta">{formatCount(v.tests?.length ?? 0)}</span>
          </Link>
        ))}
      </nav>
    </section>
  );
}

function SuiteNav({
  workflow,
  version,
  versions,
  latest,
  test,
}: {
  workflow: Workflow;
  version: WorkflowVersion;
  versions: readonly WorkflowVersion[];
  latest: boolean;
  test: string | null;
}) {
  const at = latest ? null : version.version;
  const places = suitePlaces(suiteOf(version, versions));
  return (
    <section className="pane-section" aria-label="Suite">
      <h2 className="pane-label">Suite of v{version.version}</h2>
      <nav className="nav" aria-label="Tests">
        <Link
          to={workflowPath(workflow.id, at)}
          className="nav-item"
          aria-current={test === null ? "page" : undefined}
        >
          <span className="nav-icon" aria-hidden="true">
            <LayoutList size={16} />
          </span>
          <span className="nav-label">All its tests</span>
          <span className="nav-end nav-meta">{formatCount(version.tests?.length ?? 0)}</span>
        </Link>
        {places.map((place) => (
          <Link
            key={`${place.kind}:${place.name}`}
            to={testPath(workflow.id, place.digest, at)}
            className={`nav-item${place.kind === "retired" ? " retired" : ""}`}
            aria-current={test === place.digest ? "page" : undefined}
            title={`${place.name}: ${ORIGIN_TITLE[place.kind].toLowerCase()}${place.recordedFrom ? ", recorded from a run" : ""}`}
          >
            <span className="nav-icon" aria-hidden="true">
              <OriginGlyph place={place} />
            </span>
            <span className="nav-label">{place.name}</span>
          </Link>
        ))}
      </nav>
    </section>
  );
}

function Details({
  workflow,
  version,
  versions,
}: {
  workflow: Workflow;
  version: WorkflowVersion;
  versions: readonly WorkflowVersion[];
}) {
  const labelOf = useWorkflowLabels();
  const base = version.base ? versions.find((v) => v.digest === version.base) : undefined;
  const secrets = Object.entries(version.secrets ?? {}).sort(([a], [b]) => a.localeCompare(b));
  const source = workflow.source ?? null;
  return (
    <section className="pane-section" aria-label="Details">
      <h2 className="pane-label">Details</h2>
      <dl className="run-details">
        <div>
          <dt>Scope</dt>
          <dd>
            <Scope namespace={workflow.namespace} />
          </dd>
        </div>
        {source && (
          <div>
            <dt>Source</dt>
            <dd>
              <Link
                to={workflowPath(source.workflow, source.version)}
                title="The organization's workflow it forks, at the version it last pulled"
              >
                {labelOf(source.workflow) ?? shortIdOf(source.workflow)} v{source.version}
              </Link>
            </dd>
          </div>
        )}
        <div>
          <dt>Made</dt>
          <dd>
            <Ago ms={Date.parse(version.createdAt)} />
          </dd>
        </div>
        <div>
          <dt>Base</dt>
          <dd>
            {!version.base ? (
              <span className="dim">none</span>
            ) : base ? (
              <Link
                className="num"
                to={workflowPath(
                  workflow.id,
                  versions[0]?.version === base.version ? null : base.version,
                )}
              >
                v{base.version}
              </Link>
            ) : (
              <Digest digest={version.base} />
            )}
          </dd>
        </div>
        <div>
          <dt>Code</dt>
          <dd>
            <Digest digest={version.codeDigest} />
          </dd>
        </div>
        <div>
          <dt>Action set</dt>
          <dd>
            <Digest digest={version.actionSet} />
          </dd>
        </div>
        {secrets.length > 0 && (
          <div>
            <dt>Secrets</dt>
            <dd>
              <ul className="used-secrets">
                {secrets.map(([server, ref]) => (
                  <li key={server}>
                    <span className="muted">{server}</span> <span className="mono">{ref}</span>
                  </li>
                ))}
              </ul>
            </dd>
          </div>
        )}
        {Object.keys(version.labels).length > 0 && (
          <div>
            <dt>Labels</dt>
            <dd>
              <LabelList labels={version.labels} />
            </dd>
          </div>
        )}
        <div>
          <dt>Id</dt>
          <dd>
            <span className="copyable">
              <span className="digest" title={workflow.id}>
                {workflow.shortId}
              </span>
              <CopyButton text={workflow.id} label="Copy the workflow's id" />
            </span>
          </dd>
        </div>
      </dl>
    </section>
  );
}

/** A workflow's sidebar; with the version shown, its suite and its details. */
export function WorkflowSidebar({
  workflow,
  version,
  versions,
  latest,
  test = null,
}: {
  workflow: Workflow | undefined;
  version: WorkflowVersion | null;
  versions: readonly WorkflowVersion[];
  latest: boolean;
  test?: string | null;
}) {
  const title = workflow
    ? workflowLabel({ ...workflow, comment: versions[0]?.comment })
    : "Workflow";
  return (
    <PageSidebar title={title} back="/workflows" backLabel="All the workflows">
      {workflow && version && (
        <>
          <Versions workflow={workflow} versions={versions} current={version} test={test} />
          <SuiteNav
            workflow={workflow}
            version={version}
            versions={versions}
            latest={latest}
            test={test}
          />
          <Details workflow={workflow} version={version} versions={versions} />
        </>
      )}
    </PageSidebar>
  );
}
