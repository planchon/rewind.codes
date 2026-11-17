// A test of a workflow, on its own page in Testing, in the suite of a version (the latest unless
// `?version=` names another). The header picks the version (the test of the same name in its
// suite, or the version itself when it has none), another test of the suite, or the previous or
// next one; the page says where the test stands in the suite (inherited, added,
// updated with the parent's test it replaced, retired) and why, then its document.
import { ArrowRight, ChevronLeft, ChevronRight } from "lucide-react";
import { Link, useNavigate, useParams } from "react-router";
import { useWorkflowVersions } from "../../api/queries";
import type { Workflow } from "../../api/types";
import { Loading, ProblemView } from "../../components/bits";
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
import { TestDocument } from "./TestDocument";
import { TestingGate } from "./TestingView";
import { KIND_TAG, useVersion, VersionPicker } from "./WorkflowView";

/** The tests of a version's suite, to pick one: its tests, then those it retired. */
function TestPicker({
  places,
  current,
  version,
  onPick,
}: {
  places: readonly TestPlace[];
  current: string;
  version: Workflow;
  onPick: (digest: string) => void;
}) {
  const tests = places.filter((p) => p.kind !== "retired");
  const retired = places.filter((p) => p.kind === "retired");
  const known = places.some((p) => p.digest === current);
  return (
    <select
      className="select test-picker"
      aria-label="Test"
      value={known ? current : ""}
      onChange={(e) => onPick(e.target.value)}
    >
      {!known && (
        <option value="" disabled>
          Not in this suite
        </option>
      )}
      <optgroup label={`Suite of ${workflowRef(version)}`}>
        {tests.map((p) => (
          <option key={p.digest} value={p.digest}>
            {p.name}
          </option>
        ))}
      </optgroup>
      {retired.length > 0 && (
        <optgroup label="Retired">
          {retired.map((p) => (
            <option key={p.digest} value={p.digest}>
              {p.name}
            </option>
          ))}
        </optgroup>
      )}
    </select>
  );
}

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
  const versions = useWorkflowVersions(name);
  const list = versions.data ?? [];
  const { version, asked } = useVersion(list);
  const suite = version ? suiteOf(version, list) : null;
  const places = suite ? suitePlaces(suite) : [];
  const index = places.findIndex((p) => p.digest === digest);
  const place = index >= 0 ? (places[index] ?? null) : null;
  const previous = index > 0 ? places[index - 1] : undefined;
  const next = index >= 0 ? places[index + 1] : undefined;
  const open = (test: string) => navigate(testPath(name, test, version?.version));
  // Another version keeps the test of the same name when its suite has one.
  const toVersion = (v: string) => {
    const target = findVersion(list, v);
    const same =
      target && place
        ? suitePlaces(suiteOf(target, list)).find((p) => p.name === place.name)
        : undefined;
    navigate(
      same ? testPath(name, same.digest, v) : workflowPath(name, v === list[0]?.version ? null : v),
    );
  };

  return (
    <div className="page">
      <PageHeader
        crumbs={
          <>
            <Link to="/testing">Testing</Link>
            <span className="page-header-sep" aria-hidden="true">
              /
            </span>
            <Link
              to={workflowPath(
                name,
                version?.version === list[0]?.version ? null : version?.version,
              )}
            >
              {name}
            </Link>
          </>
        }
        title={place?.name ?? digest.slice(0, 12)}
        actions={
          version ? (
            <div className="test-nav">
              <VersionPicker versions={list} current={version} onPick={toVersion} />
              <TestPicker places={places} current={digest} version={version} onPick={open} />
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
            </div>
          ) : undefined
        }
      />
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
          <TestDocument
            key={digest}
            digest={digest}
            name={place?.name ?? ""}
            workflow={name}
            versions={list}
          />
        </div>
      )}
    </div>
  );
}
