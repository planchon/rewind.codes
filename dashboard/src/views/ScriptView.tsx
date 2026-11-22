// One script: its source, the suites of the workflow versions it is the script of, and its runs
// in Observability's table forced to the script.
import { Link, useParams } from "react-router";
import { Digest, Loading, ProblemView } from "../components/bits";
import { CodeView } from "../components/CodeView";
import { PageHeader } from "../components/PageHeader";
import { shortDigest } from "../model/format";
import { fieldTerm } from "../model/query";
import { useScript } from "../state/scripts";
import { ScopedRuns } from "./ScopedRuns";
import { ScriptSuites } from "./testing/ScriptSuites";

export function ScriptView() {
  const { digest = "" } = useParams();
  const script = useScript(digest);
  const headline = script.data?.source.split("\n").find((l) => l.trim() !== "");

  return (
    <div className="page">
      <PageHeader crumbs={<Link to="/scripts">Scripts</Link>} title={<Digest digest={digest} />} />
      {headline && <p className="page-intro mono ellipsis">{headline}</p>}
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
      <ScriptSuites digest={digest} />
      <ScopedRuns
        name={`script|${digest}`}
        label="Runs of this script"
        title="Runs of this script"
        scope={fieldTerm("script", digest)}
        locked={[{ label: <>Script {shortDigest(digest)}</>, title: `Only the runs of ${digest}` }]}
        limit={50}
        hideScript
        empty="No run of this script yet."
      />
    </div>
  );
}
