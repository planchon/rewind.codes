// A test of a workflow, on its own page in Testing, in the suite of a version (the latest unless
// `?version=` names another). The breadcrumb switches the version (to the test of the same name
// in its suite, or to the version itself when it has none) and the test; previous and next go
// through the suite, and the whole document opens as JSON. The page says where the test stands
// in the suite (inherited, added, updated with the parent's test it replaced, retired) and why,
// then its document.
import { ArrowRight, Braces, ChevronLeft, ChevronRight } from "lucide-react";
import { useState } from "react";
import { Link, useNavigate, useParams } from "react-router";
import { useTest, useWorkflowVersions } from "../../api/queries";
import type { Workflow } from "../../api/types";
import { Loading, ProblemView } from "../../components/bits";
import { JsonDialog } from "../../components/JsonPeek";
import { PageHeader } from "../../components/PageHeader";
import {
  findVersion,
  suiteOf,
  suitePlaces,
  type TestPlace,
  testPath,
  workflowPath,
  workflowRef,
} from "../../model/workflows";
import { CrumbLink, Crumbs, Switcher, type SwitchItem } from "./Crumbs";
import { TestDocument, Unreadable } from "./TestDocument";
import { OriginGlyph, TestingSidebar } from "./TestingSidebar";
import { TestingGate } from "./TestingView";
import { KIND_TAG, useVersion, VersionSwitcher } from "./WorkflowView";

/** Where the test stands in the suite of the version, and why it changed. */
function Place({
  place,
  version,
  parent,
}: {
  place: TestPlace | null;
  version: Workflow;
  parent: Workflow | null;
}) {
  const suite = (
    <Link className="mono" to={workflowPath(version.name, version.version)}>
      {workflowRef(version)}
    </Link>
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
        {place.kind === "retired" ? (
          <>retired by {suite} from its parent</>
        ) : (
          <>in the suite of {suite}</>
        )}
        {place.kind === "updated" && place.from && (
          <>
            {" "}
            <span className="dim">replacing</span>{" "}
            <Link
              to={testPath(version.name, place.from, parent?.version)}
              title="The parent's test"
            >
              <span className="digest">{place.from.slice(0, 12)}</span>
            </Link>{" "}
            <ArrowRight size={12} aria-hidden="true" />{" "}
            <span className="digest">{place.digest.slice(0, 12)}</span>
          </>
        )}
      </p>
      {(place.kind === "updated" || place.kind === "retired") && (
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
  return (
    <TestingGate>
      <TestPage />
    </TestingGate>
  );
}

function TestPage() {
  const { name = "", digest = "" } = useParams();
  const navigate = useNavigate();
  const [json, setJson] = useState(false);
  const versions = useWorkflowVersions(name);
  const test = useTest(digest);
  const list = versions.data ?? [];
  const { version, asked } = useVersion(list);
  const suite = version ? suiteOf(version, list) : null;
  const places = suite ? suitePlaces(suite) : [];
  const index = places.findIndex((p) => p.digest === digest);
  const place = index >= 0 ? (places[index] ?? null) : null;
  const previous = index > 0 ? places[index - 1] : undefined;
  const next = index >= 0 ? places[index + 1] : undefined;
  const open = (to: string) => navigate(testPath(name, to, version?.version));
  const title = place?.name ?? test.data?.name ?? digest.slice(0, 12);

  // Another version keeps the test of the same name when its suite has one.
  const toVersion = (v: Workflow, latest: boolean) => {
    const target = findVersion(list, v.version);
    const same =
      target && place
        ? suitePlaces(suiteOf(target, list)).find((p) => p.name === place.name)
        : undefined;
    return same
      ? testPath(name, same.digest, v.version)
      : workflowPath(name, latest ? null : v.version);
  };
  const item = (p: TestPlace): SwitchItem => ({
    key: p.digest,
    label: (
      <span className="crumb-test">
        <OriginGlyph place={p} />
        {p.name}
      </span>
    ),
    to: testPath(name, p.digest, version?.version),
    current: p.digest === digest,
  });

  return (
    <div className="page">
      <TestingSidebar workflow={name} version={version} versions={list} test={digest} />
      <PageHeader
        crumbs={
          <Crumbs>
            <CrumbLink to="/testing">Testing</CrumbLink>
            <CrumbLink to={workflowPath(name)}>{name}</CrumbLink>
            {version ? (
              <VersionSwitcher versions={list} current={version} to={toVersion} />
            ) : (
              <span className="mono">{asked?.slice(0, 12) ?? ""}</span>
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
                  label: `Suite of ${workflowRef(version)}`,
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
      {versions.isPending ? (
        <Loading>Loading the workflow…</Loading>
      ) : versions.isError ? (
        <ProblemView error={versions.error} title={`The workflow ${name} could not be read`} />
      ) : version === null ? (
        <p className="page-intro">
          No version {asked} among the {list.length} versions of {name}.{" "}
          <Link to={testPath(name, digest)}>This test in its latest version</Link>
        </p>
      ) : (
        <div className="stack">
          {index >= 0 && (
            <p className="page-intro">
              Test {index + 1} of {places.length} of {workflowRef(version)}.
            </p>
          )}
          <Place place={place} version={version} parent={suite?.parent ?? null} />
          {test.isPending ? (
            <Loading>Loading the test…</Loading>
          ) : test.isError ? (
            <Unreadable error={test.error} />
          ) : (
            <TestDocument test={test.data} versions={list} />
          )}
        </div>
      )}
    </div>
  );
}
