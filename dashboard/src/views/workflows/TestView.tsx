// A test of a workflow, on its own page (`/workflows/<id>/tests/<digest>`), in the suite of a
// version (the latest unless `?version=` names another). The breadcrumb switches the version (to
// the test of the same name in its suite, or to the version itself when it has none) and the
// test; previous and next go through the suite, and the whole document opens as JSON. The page
// says where the test stands in the suite (inherited, added, replacing its base's test, retired)
// and why, then its document.
import { ArrowRight, Braces, ChevronLeft, ChevronRight } from "lucide-react";
import { useState } from "react";
import { Link, useNavigate, useParams } from "react-router";
import { useObjectNamespace } from "../../app/scope";
import { Loading, ProblemView } from "../../components/bits";
import { JsonDialog } from "../../components/JsonPeek";
import { PageHeader } from "../../components/PageHeader";
import type { WorkflowVersion } from "../../model/types";
import {
  shortIdOf,
  suiteOf,
  suitePlaces,
  type TestPlace,
  testPath,
  versionLabel,
  workflowLabel,
  workflowPath,
} from "../../model/workflows";
import { useTest } from "../../state/workflows";
import { KIND_TAG, OriginGlyph, VersionSwitcher } from "./bits";
import { CrumbLink, Crumbs, Switcher, type SwitchItem } from "./Crumbs";
import { useShownWorkflow } from "./shown";
import { TestDocument, Unreadable } from "./TestDocument";
import { WorkflowSidebar } from "./WorkflowSidebar";

/** Where the test stands in the suite of the version, and why it changed. */
function Place({
  place,
  index,
  count,
  workflow,
  label,
  version,
  base,
}: {
  place: TestPlace | null;
  /** Its position among the suite's tests, then the tests it retired; their number. */
  index: number;
  count: number;
  workflow: string;
  label: string;
  version: WorkflowVersion;
  base: WorkflowVersion | null;
}) {
  const suite = (
    <Link to={workflowPath(workflow, version.version)}>{versionLabel(label, version.version)}</Link>
  );
  if (place === null) {
    return (
      <p className="test-place" role="note">
        This test is not in the suite of {suite}.
      </p>
    );
  }
  const tag = KIND_TAG[place.kind];
  return (
    <div className="test-place">
      <p>
        <span className={tag.className} title={tag.title}>
          {tag.label}
        </span>{" "}
        test {index + 1} of {count}
        {place.kind === "retired" ? (
          <>, retired by {suite} from its base</>
        ) : (
          <> in the suite of {suite}</>
        )}
        {place.kind === "replaced" && place.from && (
          <>
            {" "}
            <span className="dim">replacing</span>{" "}
            <Link
              to={testPath(workflow, place.from, base?.version)}
              title={`The test of the same name in v${base?.version ?? "?"}`}
            >
              <span className="digest">{place.from.slice(0, 12)}</span>
            </Link>{" "}
            <ArrowRight size={12} aria-hidden="true" />{" "}
            <span className="digest">{place.digest.slice(0, 12)}</span>
          </>
        )}
      </p>
      {(place.kind === "replaced" || place.kind === "retired") && (
        <p className="suite-reason">
          {place.reason ? (
            <>Why: {place.reason}</>
          ) : (
            <span className="muted">No reason recorded.</span>
          )}
        </p>
      )}
    </div>
  );
}

export function TestView() {
  const { id = "" } = useParams();
  // Another workflow is another page: it never shows the one before while it loads.
  return <TestPage key={id} reference={id} />;
}

function TestPage({ reference }: { reference: string }) {
  const { digest = "" } = useParams();
  const navigate = useNavigate();
  const [json, setJson] = useState(false);
  const { read, workflow, versions, version, asked, latest } = useShownWorkflow(reference);
  // A namespace's workflow: the picker and the sidebar show its namespace.
  useObjectNamespace(workflow?.namespace);
  const test = useTest(digest);
  const id = workflow?.id ?? reference;
  const label = workflow
    ? workflowLabel({ ...workflow, comment: versions[0]?.comment })
    : reference.startsWith("wf_")
      ? shortIdOf(reference)
      : reference;
  const at = latest ? null : (version?.version ?? null);
  const suite = version ? suiteOf(version, versions) : null;
  const places = suite ? suitePlaces(suite) : [];
  const index = places.findIndex((p) => p.digest === digest);
  const place = index >= 0 ? (places[index] ?? null) : null;
  const previous = index > 0 ? places[index - 1] : undefined;
  const next = index >= 0 ? places[index + 1] : undefined;
  const open = (to: string) => navigate(testPath(id, to, at));
  const title = place?.name ?? test.data?.name ?? digest.slice(0, 12);

  // Another version keeps the test of the same name when its suite has one.
  const toVersion = (v: WorkflowVersion, isLatest: boolean) => {
    const same = place
      ? suitePlaces(suiteOf(v, versions)).find((p) => p.name === place.name)
      : undefined;
    const n = isLatest ? null : v.version;
    return same ? testPath(id, same.digest, n) : workflowPath(id, n);
  };
  const item = (p: TestPlace): SwitchItem => ({
    key: p.digest,
    label: (
      <span className="crumb-test">
        <OriginGlyph place={p} />
        {p.name}
      </span>
    ),
    to: testPath(id, p.digest, at),
    current: p.digest === digest,
  });

  return (
    <div className="page">
      <WorkflowSidebar
        workflow={workflow}
        version={version}
        versions={versions}
        latest={latest}
        test={digest}
      />
      <PageHeader
        crumbs={
          <Crumbs>
            <CrumbLink to="/workflows">Workflows</CrumbLink>
            <CrumbLink to={workflowPath(id)}>{label}</CrumbLink>
            {version ? (
              <VersionSwitcher versions={versions} current={version} to={toVersion} />
            ) : (
              <span className="num">{asked ?? ""}</span>
            )}
          </Crumbs>
        }
        title={
          version ? (
            <Switcher
              title="Test"
              label={<span className="crumb-switch-text">{title}</span>}
              groups={[
                {
                  label: `Suite of ${versionLabel(label, version.version)}`,
                  items: places.filter((p) => p.kind !== "retired").map(item),
                },
                { label: "Retired", items: places.filter((p) => p.kind === "retired").map(item) },
              ]}
            />
          ) : (
            title
          )
        }
        actions={
          <>
            {test.data && (
              <button
                type="button"
                className="btn small"
                title="The test's whole document, as stored"
                onClick={() => setJson(true)}
              >
                <Braces size={14} aria-hidden="true" /> JSON
              </button>
            )}
            {version && (
              <>
                <button
                  type="button"
                  className="icon-btn"
                  aria-label="Previous test"
                  title={previous ? `Previous: ${previous.name}` : "The first test"}
                  disabled={!previous}
                  onClick={() => previous && open(previous.digest)}
                >
                  <ChevronLeft size={16} />
                </button>
                <button
                  type="button"
                  className="icon-btn"
                  aria-label="Next test"
                  title={next ? `Next: ${next.name}` : "The last test"}
                  disabled={!next}
                  onClick={() => next && open(next.digest)}
                >
                  <ChevronRight size={16} />
                </button>
              </>
            )}
          </>
        }
      />
      {test.data && (
        <JsonDialog title={`Test ${title}`} value={test.data} open={json} onOpenChange={setJson} />
      )}
      {read.isPending ? (
        <Loading>Loading the workflow…</Loading>
      ) : read.isError ? (
        <ProblemView error={read.error} title={`${label} could not be read`} />
      ) : version === null ? (
        <p className="page-intro">
          No version {asked} among the {versions.length} versions of {label}.{" "}
          <Link to={testPath(id, digest)}>This test in its latest version</Link>
        </p>
      ) : (
        <div className="stack">
          <Place
            place={place}
            index={index}
            count={places.length}
            workflow={id}
            label={label}
            version={version}
            base={suite?.base ?? null}
          />
          {test.isPending ? (
            <Loading>Loading the test…</Loading>
          ) : test.isError ? (
            <Unreadable error={test.error} />
          ) : (
            <TestDocument test={test.data} workflow={id} label={label} versions={versions} />
          )}
        </div>
      )}
    </div>
  );
}
