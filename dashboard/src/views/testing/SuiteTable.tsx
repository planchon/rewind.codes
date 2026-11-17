// A version's suite as a table: each test with where it comes from, its input, what it expects,
// the calls it checks and the world it gives the run; then the tests the version retired, with
// why. Each row reads its test's document; a recorded test the caller may not read says so.
import { Lock } from "lucide-react";
import { Link } from "react-router";
import { ApiError } from "../../api/client";
import { useTest } from "../../api/queries";
import type { Expectations, Test, Workflow } from "../../api/types";
import { JsonPeek } from "../../components/JsonPeek";
import { StatusBadge } from "../../components/Status";
import {
  rulesOf,
  suiteOf,
  suitePlaces,
  summarizeCassette,
  type TestPlace,
  testPath,
} from "../../model/workflows";
import { OriginGlyph } from "./TestingSidebar";
import { KIND_TAG } from "./WorkflowView";

/** The calls a test checks, in short: "2 checked · 1 never". */
export function callsLine(expect: Expectations | undefined): string {
  const calls = expect?.calls ?? [];
  if (calls.length === 0) return "–";
  const never = calls.filter((c) => c.times === 0).length;
  const checked = calls.length - never;
  return [checked > 0 && `${checked} checked`, never > 0 && `${never} never`]
    .filter(Boolean)
    .join(" · ");
}

/** The world of a test, in short: its rules, its mock handlers, its cassette, its signals. */
export function worldLine(test: Test): string {
  const world = test.world ?? {};
  const tools = Object.values(world.tools ?? {});
  const rules = tools.reduce((n, rules) => n + rulesOf(rules).length, 0);
  const cassette = summarizeCassette(world.cassette);
  const signals = test.signals?.length ?? 0;
  const parts = [
    rules > 0 && `${rules} rule${rules === 1 ? "" : "s"}`,
    world.mocks && "mocks",
    cassette && `cassette of ${cassette.calls} call${cassette.calls === 1 ? "" : "s"}`,
    signals > 0 && `${signals} signal${signals === 1 ? "" : "s"}`,
  ].filter(Boolean);
  return parts.length > 0 ? parts.join(" · ") : "nothing answers";
}

function Expects({ test }: { test: Test }) {
  const expect = test.expect ?? {};
  return (
    <span className="expects">
      {expect.status && <StatusBadge status={expect.status} />}
      {expect.output !== undefined && (
        <JsonPeek value={expect.output} title={`${test.name}: the output contains`} />
      )}
      {expect.error && (
        <span className="mono" title={expect.error.message ?? undefined}>
          {expect.error.name}
        </span>
      )}
      {!expect.status && expect.output === undefined && !expect.error && (
        <span className="muted">–</span>
      )}
    </span>
  );
}

function Row({
  place,
  workflow,
  parent,
}: {
  place: TestPlace;
  workflow: Workflow;
  parent: Workflow | null;
}) {
  const test = useTest(place.digest);
  const doc = test.data;
  const tag = KIND_TAG[place.kind];
  const unreadable = test.error instanceof ApiError && [403, 404].includes(test.error.status);
  return (
    <tr>
      <td className="wrap suite-cell">
        <span className="suite-cell-name">
          <OriginGlyph place={place} />
          <Link to={testPath(workflow.name, place.digest, workflow.version)}>{place.name}</Link>
        </span>
        {place.kind === "updated" && (
          <span className="suite-cell-reason">
            {place.from && (
              <>
                replaces{" "}
                <Link to={testPath(workflow.name, place.from, parent?.version)} className="mono">
                  {place.from.slice(0, 12)}
                </Link>
                {" · "}
              </>
            )}
            {place.reason ? `Why: ${place.reason}` : "No reason recorded."}
          </span>
        )}
      </td>
      <td>
        <span className={tag.className} title={tag.title}>
          {tag.label}
        </span>{" "}
        {(doc?.origin || place.recordedFrom) && (
          <Link
            to={`/runs/${doc?.origin?.run ?? place.recordedFrom}`}
            className="tag info"
            title="Recorded from this run"
          >
            recorded
          </Link>
        )}
      </td>
      {unreadable ? (
        <td colSpan={4} className="muted">
          <Lock size={12} aria-hidden="true" />{" "}
          {test.error instanceof ApiError && test.error.status === 403
            ? "You cannot read this test: it holds the data of a run you may not read."
            : "This test is no longer stored."}
        </td>
      ) : doc ? (
        <>
          <td>
            <JsonPeek value={doc.input ?? {}} title={`${doc.name}: input`} />
          </td>
          <td>
            <Expects test={doc} />
          </td>
          <td className="dim nowrap">{callsLine(doc.expect)}</td>
          <td className="dim wrap">{worldLine(doc)}</td>
        </>
      ) : (
        <td colSpan={4} className="muted">
          {test.isError ? "The test could not be read." : "Loading…"}
        </td>
      )}
    </tr>
  );
}

function Retired({ place, workflow }: { place: TestPlace; workflow: Workflow }) {
  return (
    <tr>
      <td className="wrap suite-cell">
        <span className="suite-cell-name">
          <OriginGlyph place={place} />
          <Link to={testPath(workflow.name, place.digest, workflow.version)}>{place.name}</Link>
        </span>
      </td>
      <td className="wrap">{place.reason ?? <span className="muted">No reason recorded.</span>}</td>
      <td className="dim">
        <span className="digest" title={place.digest}>
          {place.digest.slice(0, 12)}
        </span>
      </td>
    </tr>
  );
}

export function SuiteTable({
  version,
  versions,
}: {
  version: Workflow;
  versions: readonly Workflow[];
}) {
  const suite = suiteOf(version, versions);
  const places = suitePlaces(suite);
  const tests = places.filter((p) => p.kind !== "retired");
  const retired = places.filter((p) => p.kind === "retired");
  return (
    <>
      {tests.length === 0 ? (
        <p className="muted">No test: this version was saved with an empty suite.</p>
      ) : (
        <div className="table-wrap">
          <table className="grid suite-table" aria-label={`Suite of ${version.name}`}>
            <thead>
              <tr>
                <th>Test</th>
                <th>Origin</th>
                <th>Input</th>
                <th>Expects</th>
                <th>Calls</th>
                <th>World</th>
              </tr>
            </thead>
            <tbody>
              {tests.map((place) => (
                <Row key={place.name} place={place} workflow={version} parent={suite.parent} />
              ))}
            </tbody>
          </table>
        </div>
      )}
      {retired.length > 0 && (
        <>
          <h3 className="section-subhead">Retired from its parent</h3>
          <div className="table-wrap">
            <table
              className="grid suite-table"
              aria-label={`Retired from the suite of ${version.name}`}
            >
              <thead>
                <tr>
                  <th>Test</th>
                  <th>Why</th>
                  <th>Digest</th>
                </tr>
              </thead>
              <tbody>
                {retired.map((place) => (
                  <Retired key={place.name} place={place} workflow={version} />
                ))}
              </tbody>
            </table>
          </div>
        </>
      )}
    </>
  );
}
