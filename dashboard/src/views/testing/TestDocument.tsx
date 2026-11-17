// A test's document, on its page in Testing: the world it gives the run (its input, start and seed, each tool's
// rules, its mock handlers, the cassette it was recorded with), the signals it sends, what must
// hold at the end, and for a recorded test the run it comes from (archi/05, "The test document").
// A recorded test holds its run's data and is read like the run: a caller who may not read the
// runs of its namespace is told so here, and the rest of the page stays.
import { FlaskConical, Lock } from "lucide-react";
import { useState } from "react";
import { Link } from "react-router";
import { ApiError } from "../../api/client";
import { useTest } from "../../api/queries";
import type { Expectations, Test, TestSignal, TestWorld, Workflow } from "../../api/types";
import { Digest, Loading, ProblemView, RunId, Time } from "../../components/bits";
import { CodeView } from "../../components/CodeView";
import { JsonValue } from "../../components/Payload";
import { StatusBadge } from "../../components/Status";
import {
  expectedTimes,
  findVersion,
  rulesOf,
  ruleTimes,
  signalWhen,
  summarizeCassette,
  workflowPath,
  workflowRef,
} from "../../model/workflows";

/** Values up to this length are written on one line; longer ones open in a box. */
const INLINE = 72;

/** A JSON value: on one line when short, in a scrolling box otherwise. */
function Json({ value }: { value: unknown }) {
  const text = JSON.stringify(value) ?? "undefined";
  if (text.length <= INLINE) return <code className="json-inline">{text}</code>;
  return (
    <div className="json-box">
      <JsonValue value={value} />
    </div>
  );
}

function Empty({ children }: { children: string }) {
  return <p className="muted">{children}</p>;
}

function Tools({ world }: { world: TestWorld }) {
  const tools = Object.entries(world.tools ?? {}).sort(([a], [b]) => a.localeCompare(b));
  if (tools.length === 0) return null;
  return (
    <div className="table-wrap flat">
      <table className="grid compact" aria-label="Tool rules">
        <thead>
          <tr>
            <th>Tool</th>
            <th>When its args contain</th>
            <th>Answers</th>
            <th>For</th>
          </tr>
        </thead>
        <tbody>
          {tools.flatMap(([tool, rules]) =>
            rulesOf(rules).map((rule, i) => (
              // biome-ignore lint/suspicious/noArrayIndexKey: a rule's place is its identity.
              <tr key={`${tool}-${i}`}>
                <td className="mono">{i === 0 ? tool : ""}</td>
                <td className="wrap">
                  {rule.args ? <Json value={rule.args} /> : <span className="muted">any call</span>}
                </td>
                <td className="wrap">
                  {rule.error ? (
                    <span className="rule-error">
                      <span className="tag critical">{rule.error.kind ?? "error"}</span>{" "}
                      {rule.error.message}
                      {rule.error.data !== undefined && rule.error.data !== null && (
                        <>
                          {" "}
                          <Json value={rule.error.data} />
                        </>
                      )}
                    </span>
                  ) : (
                    <Json value={rule.returns ?? null} />
                  )}
                </td>
                <td className="dim">{ruleTimes(rule.times)}</td>
              </tr>
            )),
          )}
        </tbody>
      </table>
    </div>
  );
}

function Cassette({ cassette }: { cassette: unknown }) {
  const [shown, setShown] = useState(false);
  const summary = summarizeCassette(cassette);
  if (!summary) return null;
  return (
    <section className="cassette" aria-label="Cassette">
      <p>
        <strong>Cassette</strong>{" "}
        <span className="dim">
          {summary.calls} call{summary.calls === 1 ? "" : "s"} and {summary.signals.length} signal
          {summary.signals.length === 1 ? "" : "s"} recorded
          {summary.matching
            ? `, matched ${summary.matching === "by_args" ? "by tool and args" : "in order"}`
            : ""}
          .
        </span>
      </p>
      {summary.byTool.length > 0 && (
        <ul className="labels">
          {summary.byTool.map((t) => (
            <li key={t.tool} className="label">
              {t.tool} ×{t.count}
            </li>
          ))}
          {summary.signals.map((s, i) => (
            // biome-ignore lint/suspicious/noArrayIndexKey: signals repeat their names.
            <li key={`signal-${i}`} className="label">
              signal {s.name}
              {s.at !== null ? ` at +${Math.round(s.at / 1000)}s` : ""}
            </li>
          ))}
        </ul>
      )}
      <button type="button" className="btn small" onClick={() => setShown(!shown)}>
        {shown ? "Hide the cassette" : "Show the cassette"}
      </button>
      {shown && (
        <div className="json-box">
          <JsonValue value={cassette} />
        </div>
      )}
    </section>
  );
}

function World({ test }: { test: Test }) {
  const world = test.world ?? {};
  const empty = !world.mocks && !world.cassette && Object.keys(world.tools ?? {}).length === 0;
  return (
    <section className="test-part" aria-label="World">
      <h3>World</h3>
      {empty ? (
        <Empty>Nothing answers: any tool call fails the test.</Empty>
      ) : (
        <p className="muted">
          A call is answered by the rules, then the mock handlers, then the cassette; a call none
          answers fails the test.
        </p>
      )}
      <Tools world={world} />
      {world.mocks && (
        <section className="mocks" aria-label="Mock handlers">
          <p>
            <strong>Mock handlers</strong> <span className="dim">run in a separate isolate</span>
          </p>
          <CodeView source={world.mocks} />
        </section>
      )}
      {world.cassette && <Cassette cassette={world.cassette} />}
    </section>
  );
}

function Signals({ signals }: { signals: TestSignal[] }) {
  return (
    <section className="test-part" aria-label="Signals">
      <h3>Signals</h3>
      {signals.length === 0 ? (
        <Empty>None sent.</Empty>
      ) : (
        <ol className="test-list">
          {signals.map((s, i) => (
            // biome-ignore lint/suspicious/noArrayIndexKey: signals are sent in this order.
            <li key={i}>
              <span className="mono">{s.name}</span>{" "}
              {s.payload !== undefined && s.payload !== null && <Json value={s.payload} />}{" "}
              <span className="dim">{signalWhen(s.when)}</span>
            </li>
          ))}
        </ol>
      )}
    </section>
  );
}

function Expect({ expect }: { expect: Expectations }) {
  const calls = expect.calls ?? [];
  const nothing =
    !expect.status &&
    expect.output === undefined &&
    !expect.error &&
    calls.length === 0 &&
    expect.order !== "listed";
  return (
    <section className="test-part" aria-label="Expectations">
      <h3>Expectations</h3>
      {nothing ? (
        <Empty>Nothing is checked: the test passes when every call is answered.</Empty>
      ) : (
        <dl className="kv">
          {expect.status && (
            <div>
              <dt>Status</dt>
              <dd>
                <StatusBadge status={expect.status} />
              </dd>
            </div>
          )}
          {expect.output !== undefined && (
            <div>
              <dt>Output contains</dt>
              <dd>
                <Json value={expect.output} />
              </dd>
            </div>
          )}
          {expect.error && (
            <div>
              <dt>Fails with</dt>
              <dd>
                <span className="mono">{expect.error.name}</span>
                {expect.error.message ? (
                  <>
                    {" "}
                    <span className="dim">whose message contains</span>{" "}
                    <code>{expect.error.message}</code>
                  </>
                ) : null}
              </dd>
            </div>
          )}
          {calls.length > 0 && (
            <div>
              <dt>Calls</dt>
              <dd>
                <ol className="test-list" aria-label="Expected calls">
                  {calls.map((c, i) => (
                    // biome-ignore lint/suspicious/noArrayIndexKey: entries are matched in order.
                    <li key={i}>
                      <span className="mono">{c.tool}</span>
                      {c.args ? (
                        <>
                          {" "}
                          <span className="dim">with args containing</span> <Json value={c.args} />
                        </>
                      ) : null}{" "}
                      <span className={c.times === 0 ? "tag critical" : "tag"}>
                        {expectedTimes(c)}
                      </span>
                    </li>
                  ))}
                </ol>
              </dd>
            </div>
          )}
          <div>
            <dt>Order</dt>
            <dd className="dim">
              {expect.order === "listed"
                ? "listed: the first call matching each entry comes in this order"
                : "any"}
            </dd>
          </div>
        </dl>
      )}
    </section>
  );
}

function Origin({
  test,
  workflow,
  versions,
}: {
  test: Test;
  workflow: string;
  versions: readonly Workflow[];
}) {
  const origin = test.origin;
  if (!origin) return null;
  const version = findVersion(versions, origin.version);
  return (
    <div>
      <dt>Recorded from</dt>
      <dd className="origin">
        <Link to={`/runs/${origin.run}`} title="The run it was recorded from">
          <RunId id={origin.run} />
        </Link>{" "}
        <span className="dim">of the namespace</span>{" "}
        <span className="mono">{origin.namespace}</span>
        <span className="dim">, which ran</span>{" "}
        {version ? (
          <Link className="mono" to={workflowPath(workflow, version.version)}>
            {workflowRef(version)}
          </Link>
        ) : (
          <span className="mono">{workflowRef({ name: workflow, version: origin.version })}</span>
        )}
      </dd>
    </div>
  );
}

function Unreadable({ error }: { error: unknown }) {
  if (error instanceof ApiError && (error.status === 403 || error.status === 404)) {
    const forbidden = error.status === 403;
    return (
      <div className="test-unreadable" role="note">
        <p>
          <Lock size={13} aria-hidden="true" />{" "}
          <strong>
            {forbidden ? "You cannot read this test" : "This test is no longer stored"}
          </strong>
        </p>
        <p className="dim">
          {forbidden
            ? "It was recorded from a run and holds that run's data, so it is read like the run: only someone who may read the runs of its namespace opens it. The gate still runs it whoever saves."
            : "A recorded test is erased with the namespace of its run; a test no version references is deleted a day after it was stored."}
        </p>
        {error.problem.detail && <p className="muted">{error.problem.detail}</p>}
      </div>
    );
  }
  return <ProblemView error={error} title="The test could not be read" />;
}

export function TestDocument({
  digest,
  name,
  workflow,
  versions,
}: {
  digest: string;
  /** Its name in the suite, shown until the document is read. */
  name: string;
  workflow: string;
  versions: readonly Workflow[];
}) {
  const test = useTest(digest);
  const doc = test.data;
  return (
    <section className="test-doc" aria-label={`Test ${doc?.name ?? name}`}>
      <header className="test-doc-head">
        <FlaskConical size={14} aria-hidden="true" />
        <h3>Document</h3>
        {doc?.origin && <span className="tag info">recorded</span>}
        <span className="end">
          <Digest digest={digest} />
        </span>
      </header>
      <div className="test-doc-body">
        {test.isPending ? (
          <Loading>Loading the test…</Loading>
        ) : test.isError ? (
          <Unreadable error={test.error} />
        ) : (
          <>
            <dl className="kv">
              <div>
                <dt>Input</dt>
                <dd>
                  <Json value={test.data.input ?? {}} />
                </dd>
              </div>
              <div>
                <dt>Starts</dt>
                <dd>
                  {test.data.start ? (
                    <Time ms={Date.parse(test.data.start)} />
                  ) : (
                    <span className="dim">2026-01-01 00:00:00 UTC, in virtual time</span>
                  )}
                </dd>
              </div>
              <div>
                <dt>Seed</dt>
                <dd>
                  {test.data.seed ? (
                    <Digest digest={test.data.seed} />
                  ) : (
                    <span className="dim">the fixed seed of every test</span>
                  )}
                </dd>
              </div>
              <Origin test={test.data} workflow={workflow} versions={versions} />
            </dl>
            <World test={test.data} />
            <Signals signals={test.data.signals ?? []} />
            <Expect expect={test.data.expect ?? {}} />
          </>
        )}
      </div>
    </section>
  );
}
