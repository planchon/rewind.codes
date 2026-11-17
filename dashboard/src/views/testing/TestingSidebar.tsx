// Testing's own sidebar, as Observability has one: the workflows, then the suite of the version
// shown (each test a click away, its origin as a glyph), then the version's details.
import {
  Circle,
  CircleArrowRight,
  CircleDot,
  CircleMinus,
  CirclePlus,
  LayoutList,
  Workflow as WorkflowIcon,
} from "lucide-react";
import { Link } from "react-router";
import { useWorkflows } from "../../api/queries";
import type { Workflow } from "../../api/types";
import { PageSidebar } from "../../app/sidebar";
import { Ago, Digest, LabelList } from "../../components/bits";
import { formatCount } from "../../model/format";
import {
  suiteOf,
  suitePlaces,
  type TestPlace,
  testPath,
  workflowPath,
  workflowRef,
} from "../../model/workflows";

/** How a test came into the suite, as a glyph: kept, added, recorded, updated, retired. */
export function OriginGlyph({ place }: { place: TestPlace }) {
  const size = 14;
  if (place.kind === "retired") return <CircleMinus size={size} className="origin retired" />;
  if (place.kind === "updated") return <CircleArrowRight size={size} className="origin updated" />;
  if (place.recordedFrom) return <CircleDot size={size} className="origin added" />;
  if (place.kind === "added") return <CirclePlus size={size} className="origin added" />;
  return <Circle size={size} className="origin" />;
}

const ORIGIN_TITLE: Record<TestPlace["kind"], string> = {
  inherited: "Inherited from its parent",
  added: "Added by this version",
  updated: "Updated by this version",
  kept: "In its suite",
  retired: "Retired by this version",
};

function SuiteNav({
  version,
  versions,
  test,
}: {
  version: Workflow;
  versions: readonly Workflow[];
  test: string | null;
}) {
  const latest = versions[0]?.version === version.version;
  const at = latest ? null : version.version;
  const places = suitePlaces(suiteOf(version, versions));
  return (
    <section className="pane-section" aria-label="Suite">
      <h2 className="pane-label">Suite of {workflowRef(version)}</h2>
      <nav className="nav" aria-label="Tests">
        <Link
          to={workflowPath(version.name, at)}
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
            to={testPath(version.name, place.digest, version.version)}
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

function Details({ version, versions }: { version: Workflow; versions: readonly Workflow[] }) {
  const parent = version.parent ? versions.find((v) => v.version === version.parent) : undefined;
  return (
    <section className="pane-section" aria-label="Details">
      <h2 className="pane-label">Details</h2>
      <dl className="run-details">
        <div>
          <dt>Saved</dt>
          <dd>
            <Ago ms={Date.parse(version.createdAt)} />
          </dd>
        </div>
        <div>
          <dt>Derives from</dt>
          <dd>
            {!version.parent ? (
              <span className="dim">nothing</span>
            ) : parent ? (
              <Link
                className="mono"
                to={workflowPath(
                  version.name,
                  versions[0]?.version === parent.version ? null : parent.version,
                )}
              >
                {parent.version.slice(0, 12)}
              </Link>
            ) : (
              <Digest digest={version.parent} />
            )}
          </dd>
        </div>
        <div>
          <dt>Script</dt>
          <dd>
            <Link to={`/scripts/${version.script}`} className="mono" title="Its source and runs">
              {version.script.slice(0, 12)}
            </Link>
          </dd>
        </div>
        <div>
          <dt>Toolset</dt>
          <dd>
            <Digest digest={version.toolset} />
          </dd>
        </div>
        {Object.keys(version.labels).length > 0 && (
          <div>
            <dt>Labels</dt>
            <dd>
              <LabelList labels={version.labels} />
            </dd>
          </div>
        )}
      </dl>
    </section>
  );
}

/** Testing's sidebar; with a version, its suite and its details too. */
export function TestingSidebar({
  workflow,
  version,
  versions = [],
  test = null,
}: {
  workflow?: string;
  version?: Workflow | null;
  versions?: readonly Workflow[];
  test?: string | null;
}) {
  const workflows = useWorkflows();
  const list = workflows.data?.pages.flatMap((p) => p.workflows) ?? [];
  return (
    <PageSidebar title="Testing" back={null} backLabel="Show the views">
      <section className="pane-section" aria-label="Workflows">
        <h2 className="pane-label">Workflows</h2>
        <nav className="nav" aria-label="Workflows">
          {list.map((w) => (
            <Link
              key={w.name}
              to={workflowPath(w.name)}
              className="nav-item"
              aria-current={w.name === workflow && !version ? "page" : undefined}
              data-open={w.name === workflow ? "" : undefined}
            >
              <span className="nav-icon" aria-hidden="true">
                <WorkflowIcon size={16} />
              </span>
              <span className="nav-label">{w.name}</span>
              <span className="nav-end nav-meta" title="Tests in its latest version's suite">
                {formatCount(w.tests?.length ?? 0)}
              </span>
            </Link>
          ))}
        </nav>
      </section>
      {version && <SuiteNav version={version} versions={versions} test={test} />}
      {version && <Details version={version} versions={versions} />}
    </PageSidebar>
  );
}
