// Scripts: every distinct script executed (content-addressed, so deduplicated), with its runs by
// outcome, the tools it called and the labels it ran with. A search as on the Overview, one line
// of text: `tool:` (the scripts that called it) and labels, `@key:value`; a click on a row's tool
// or label adds it.
import { useMemo } from "react";
import { Link, useNavigate } from "react-router";
import { useScripts } from "../api/queries";
import { RUN_STATUSES, type RunCounts, type ScriptSummary } from "../api/types";
import { Ago, Digest, Empty, LabelList, Loading, ProblemView } from "../components/bits";
import { PageHeader } from "../components/PageHeader";
import { QueryBar } from "../components/QueryBar";
import { StatusGlyph } from "../components/Status";
import { toScriptsQuery } from "../model/filters";
import { formatCount } from "../model/format";
import {
  fieldTerm,
  joinQuery,
  labelTerm,
  queryLink,
  removeTerm,
  splitTerms,
  withTerm,
} from "../model/query";
import { readScriptSearch, SCRIPT_FIELDS, scriptQueryOf } from "../model/searches";
import { type OlderSearch, useViewSearch } from "./search";

/** `/scripts?label=…&tool=…`, a link from before the search. */
const OLDER: OlderSearch = {
  names: ["label", "tool"],
  read: (params) => scriptQueryOf(params) ?? "",
};

/** The search with its tool replaced: a script search names one tool. */
function withTool(q: string, tool: string): string {
  let rest = q;
  for (const term of [...splitTerms(q)].reverse())
    if (term.kind === "field" && term.field === "tool") rest = removeTerm(rest, term);
  return joinQuery(rest, fieldTerm("tool", tool));
}

export function totalRuns(counts: RunCounts): number {
  return RUN_STATUSES.reduce((sum, s) => sum + counts[s], 0);
}

/** Runs by status: a bar to compare at a glance, counts with glyphs to read. */
export function Outcomes({ counts, digest }: { counts: RunCounts; digest?: string }) {
  const total = totalRuns(counts);
  const present = RUN_STATUSES.filter((s) => counts[s] > 0);
  const title = present.map((s) => `${counts[s]} ${s}`).join(", ");
  return (
    <span className="outcomes" title={title}>
      <span className="outcome-bar" aria-hidden="true">
        {present.map((s) => (
          <span key={s} className={`seg-${s}`} style={{ flexGrow: counts[s] }} />
        ))}
        {total === 0 && <span className="seg-pending" style={{ flexGrow: 1 }} />}
      </span>
      <span className="outcome-counts">
        {present.map((s) =>
          digest ? (
            <Link
              key={s}
              to={queryLink("/observability", `status:${s} script:${digest}`)}
              className={`status ${s}`}
              title={`${counts[s]} ${s}`}
            >
              <StatusGlyph status={s} size={11} />
              <span className="num">{formatCount(counts[s])}</span>
            </Link>
          ) : (
            <span key={s} className={`status ${s}`} title={`${counts[s]} ${s}`}>
              <StatusGlyph status={s} size={11} />
              <span className="num">{formatCount(counts[s])}</span>
            </span>
          ),
        )}
      </span>
    </span>
  );
}

function topLabels(labels: Record<string, number> | undefined, n: number): string[] {
  return Object.entries(labels ?? {})
    .sort(([a, x], [b, y]) => y - x || a.localeCompare(b))
    .slice(0, n)
    .map(([pair]) => pair);
}

function ScriptRow({
  script,
  onPickLabel,
  onPickTool,
}: {
  script: ScriptSummary;
  onPickLabel: (p: string) => void;
  onPickTool: (t: string) => void;
}) {
  const navigate = useNavigate();
  const shown = script.tools.slice(0, 3);
  return (
    <tr
      className="link"
      onClick={(e) => {
        if ((e.target as HTMLElement).closest("a, button")) return;
        navigate(`/scripts/${script.digest}`);
      }}
    >
      <td className="script-cell">
        <Link to={`/scripts/${script.digest}`} className="headline mono" title={script.headline}>
          {script.headline || <span className="muted">(empty)</span>}
        </Link>
        <Digest digest={script.digest} />
      </td>
      <td className="right">
        <Link
          to={queryLink("/observability", `script:${script.digest}`)}
          className="num"
          title="Its runs"
        >
          {formatCount(totalRuns(script.runs))}
        </Link>
      </td>
      <td>
        <Outcomes counts={script.runs} digest={script.digest} />
      </td>
      <td className="wrap">
        <span className="labels">
          {shown.map((tool) => (
            <button
              key={tool}
              type="button"
              className="label"
              title={`Only scripts that called ${tool}`}
              onClick={() => onPickTool(tool)}
            >
              {tool}
            </button>
          ))}
          {script.tools.length > shown.length && (
            <span className="muted" title={script.tools.slice(3).join(", ")}>
              +{script.tools.length - shown.length}
            </span>
          )}
          {script.tools.length === 0 && <span className="muted">–</span>}
        </span>
      </td>
      <td className="wrap">
        <LabelList
          labels={topLabels(script.labels, 3)}
          counts={script.labels ?? {}}
          onPick={onPickLabel}
        />
      </td>
      <td className="dim">
        <Ago ms={Date.parse(script.lastRunAt)} />
      </td>
      <td className="dim">
        <Ago ms={Date.parse(script.firstRunAt)} />
      </td>
    </tr>
  );
}

export function ScriptsView() {
  const { q, setQ } = useViewSearch("q", OLDER);
  const search = readScriptSearch(q);
  const query = toScriptsQuery(search);
  const scripts = useScripts(JSON.stringify(query), query);
  const addLabel = (pair: string) => {
    const at = pair.indexOf(":");
    setQ(withTerm(q, labelTerm(pair.slice(0, at), pair.slice(at + 1))));
  };

  const list = scripts.data?.pages.flatMap((p) => p.scripts) ?? [];
  const pages = scripts.data?.pages;
  // The tools of the scripts listed, to suggest after `tool:`.
  const values = useMemo(() => {
    const tools = [...new Set((pages ?? []).flatMap((p) => p.scripts.flatMap((s) => s.tools)))];
    return { tool: tools.sort().map((value) => ({ value })) };
  }, [pages]);
  return (
    <div className="page">
      <PageHeader title="Scripts" />
      <QueryBar
        value={q}
        onSubmit={setQ}
        fields={SCRIPT_FIELDS}
        values={values}
        fault={search.fault}
        label="Search scripts"
        placeholder="tool:github.close_issue  @team:infra"
      />
      <p className="page-intro">Every distinct source executed, the most recently run first.</p>
      {search.fault ? (
        <Empty>Fix the search to see the scripts: the term at fault is underlined above.</Empty>
      ) : scripts.isPending ? (
        <Loading>Loading scripts…</Loading>
      ) : scripts.isError ? (
        <ProblemView error={scripts.error} title="The scripts could not be read" />
      ) : (
        <div className="table-wrap">
          <table className="grid">
            <thead>
              <tr>
                <th>Script</th>
                <th className="right">Runs</th>
                <th>Outcomes</th>
                <th>Tools</th>
                <th>Labels</th>
                <th>Last run</th>
                <th>First run</th>
              </tr>
            </thead>
            <tbody>
              {list.map((s) => (
                <ScriptRow
                  key={s.digest}
                  script={s}
                  onPickLabel={addLabel}
                  onPickTool={(tool) => setQ(withTool(q, tool))}
                />
              ))}
            </tbody>
          </table>
          {list.length === 0 && (
            <Empty>{q ? "No script matches the search." : "No script ran yet."}</Empty>
          )}
          {scripts.hasNextPage && (
            <div className="list-foot">
              <span className="num">{formatCount(list.length)} scripts shown</span>
              <button
                type="button"
                className="btn small"
                disabled={scripts.isFetchingNextPage}
                onClick={() => void scripts.fetchNextPage()}
              >
                {scripts.isFetchingNextPage ? "Loading…" : "Load more"}
              </button>
            </div>
          )}
        </div>
      )}
    </div>
  );
}
