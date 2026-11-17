// A test's document, on its page in Testing: its input and what its output must contain, the
// world it gives the run (each tool's rules, its mock handlers, the cassette it was recorded
// with), the signals it sends, what must hold at the end, and for a recorded test the run it
// comes from (archi/05, "The test document"). Every JSON value opens whole, colored, in a dialog.
// A recorded test holds its run's data and is read like the run: a caller who may not read the
// runs of its namespace is told so here, and the rest of the page stays.
import { Lock } from "lucide-react";
import { useState } from "react";
import { Link } from "react-router";
import { ApiError } from "../../api/client";
import type { Expectations, Test, TestSignal, TestWorld, Workflow } from "../../api/types";
import { Digest, ProblemView, RunId, Time } from "../../components/bits";
import { CodeView } from "../../components/CodeView";
import { JsonCard, JsonDialog, JsonPeek } from "../../components/JsonPeek";
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
                  {rule.args ? (
                    <JsonPeek
                      value={rule.args}
                      title={`${tool}, rule ${i + 1}: the args it matches`}
                    />
                  ) : (
                    <span className="muted">any call</span>
                  )}
                </td>
                <td className="wrap">
                  {rule.error ? (
                    <span className="rule-error">
                      <span className="tag critical">{rule.error.kind ?? "error"}</span>{" "}
                      {rule.error.message}
                      {rule.error.data !== undefined && rule.error.data !== null && (
                        <>
                          {" "}
                          <JsonPeek
                            value={rule.error.data}
                            title={`${tool}, rule ${i + 1}: the error's data`}
                          />
                        </>
                      )}
                    </span>
                  ) : (
                    <JsonPeek
                      value={rule.returns ?? null}
                      title={`${tool}, rule ${i + 1}: what it returns`}
                    />
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
  const [open, setOpen] = useState(false);
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
      <button type="button" className="btn small" onClick={() => setOpen(true)}>
        Open the cassette
      </button>
      <JsonDialog title="Cassette" value={cassette} open={open} onOpenChange={setOpen} />
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
              {s.payload !== undefined && s.payload !== null && (
                <JsonPeek value={s.payload} title={`The signal ${s.name}: its payload`} />
              )}{" "}
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
                <JsonPeek value={expect.output} title="The output contains" />
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
                          <span className="dim">with args containing</span>{" "}
                          <JsonPeek
                            value={c.args}
                            title={`${c.tool}: the args it is called with`}
                          />
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

function Run({ test, versions }: { test: Test; versions: readonly Workflow[] }) {
  const origin = test.origin;
  const version = origin ? findVersion(versions, origin.version) : null;
  return (
    <section className="test-part" aria-label="Run">
      <h3>Run</h3>
      <dl className="kv">
        <div>
          <dt>Starts</dt>
          <dd>
            {test.start ? (
              <Time ms={Date.parse(test.start)} />
            ) : (
              <span className="dim">2026-01-01 00:00:00 UTC, in virtual time</span>
            )}
          </dd>
        </div>
        <div>
          <dt>Seed</dt>
          <dd>
            {test.seed ? (
              <Digest digest={test.seed} />
            ) : (
              <span className="dim">the fixed seed of every test</span>
            )}
          </dd>
        </div>
        {origin && (
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
                <Link className="mono" to={workflowPath(version.name, version.version)}>
                  {workflowRef(version)}
                </Link>
              ) : (
                <span className="mono">{origin.version.slice(0, 12)}</span>
              )}
            </dd>
          </div>
        )}
      </dl>
    </section>
  );
}

export function Unreadable({ error }: { error: unknown }) {
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

/** What the run must end with: its output, or the error it fails with. */
function Outcome({ expect }: { expect: Expectations }) {
  if (expect.output !== undefined) {
    return <JsonCard title="Expected output" value={expect.output} note="the output contains it" />;
  }
  if (expect.error) {
    return <JsonCard title="Expected error" value={expect.error} note="the run fails with it" />;
  }
  return (
    <section className="json-card" aria-label="Expected output">
      <header className="json-card-head">
        <h3>Expected output</h3>
      </header>
      <p className="muted json-card-empty">Not checked: the test does not look at the output.</p>
    </section>
  );
}

export function TestDocument({ test, versions }: { test: Test; versions: readonly Workflow[] }) {
  return (
    <section className="test-doc" aria-label={`Test ${test.name}`}>
      <div className="json-cards">
        <JsonCard title="Input" value={test.input ?? {}} />
        <Outcome expect={test.expect ?? {}} />
      </div>
      <World test={test} />
      <Signals signals={test.signals ?? []} />
      <Expect expect={test.expect ?? {}} />
      <Run test={test} versions={versions} />
    </section>
  );
}
