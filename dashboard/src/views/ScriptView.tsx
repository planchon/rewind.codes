// One script: its source, and its runs.
import { useParams, useSearchParams } from "react-router";
import { useRuns, useScript } from "../api/queries";
import { RUN_STATUSES, type RunStatus } from "../api/types";
import { Digest, Loading, ProblemView } from "../components/bits";
import { CodeView } from "../components/CodeView";
import { StatusGlyph } from "../components/Status";
import { NO_RUN_FILTERS, toRunsQuery } from "../model/filters";
import { Seg } from "./FilterBits";
import { RunTable } from "./RunTable";

export function ScriptView() {
  const { digest = "" } = useParams();
  const [params, setParams] = useSearchParams();
  const status = (RUN_STATUSES as readonly string[]).includes(params.get("status") ?? "")
    ? (params.get("status") as RunStatus)
    : null;
  const script = useScript(digest);
  const runs = useRuns(`script=${digest}&status=${status ?? ""}`, () =>
    toRunsQuery({ ...NO_RUN_FILTERS, script: digest, status }, Date.now()),
  );
  const headline = script.data?.source.split("\n").find((l) => l.trim() !== "");

  return (
    <div className="page">
      <div className="page-head">
        <h1>Script</h1>
        <Digest digest={digest} full />
        {headline && <span className="muted mono ellipsis">{headline}</span>}
      </div>
      <section className="panel">
        <header className="panel-head">
          <h2>Source</h2>
          <span className="muted">
            script.ts{script.data ? ` · ${script.data.source.split("\n").length} lines` : ""}
          </span>
        </header>
        {script.isPending ? (
          <Loading>Loading the source…</Loading>
        ) : script.isError ? (
          <div className="panel-body">
            <ProblemView error={script.error} title="The source could not be read" />
          </div>
        ) : (
          <CodeView source={script.data.source} />
        )}
      </section>
      <div className="page-head">
        <h2 style={{ fontSize: 13.5 }}>Runs of this script</h2>
        <Seg<RunStatus | "any">
          label="Status"
          value={status ?? "any"}
          onChange={(v) => setParams(v === "any" ? {} : { status: v }, { replace: true })}
          options={[
            { value: "any", label: "Any status" },
            ...RUN_STATUSES.map((s) => ({
              value: s,
              label: (
                <>
                  <span className={`status ${s}`}>
                    <StatusGlyph status={s} size={12} />
                  </span>
                  {s}
                </>
              ),
            })),
          ]}
        />
      </div>
      <RunTable query={runs} hideScript empty="No run of this script matches." />
    </div>
  );
}
