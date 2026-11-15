// Scripts: every distinct script executed (content-addressed, so deduplicated), with its runs by
// outcome, the tools it called and the labels it ran with.
import { Link, useNavigate, useSearchParams } from "react-router";
import { useScripts } from "../api/queries";
import { RUN_STATUSES, type RunCounts, type ScriptSummary } from "../api/types";
import { Ago, Digest, Empty, LabelList, Loading, ProblemView } from "../components/bits";
import { PageHeader } from "../components/PageHeader";
import { StatusGlyph } from "../components/Status";
import {
  readScriptFilters,
  type ScriptFilters,
  toScriptsQuery,
  writeScriptFilters,
} from "../model/filters";
import { formatCount } from "../model/format";
import { queryLink } from "../model/query";
import { AddFilter, FilterChip } from "./FilterBits";

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
  const [params, setParams] = useSearchParams();
  const filters = readScriptFilters(params);
  const key = writeScriptFilters(filters).toString();
  const scripts = useScripts(key, toScriptsQuery(filters));
  const update = (patch: Partial<ScriptFilters>) =>
    setParams(writeScriptFilters({ ...filters, ...patch }));
  const addLabel = (pair: string) => {
    if (!filters.labels.includes(pair)) update({ labels: [...filters.labels, pair] });
  };

  const list = scripts.data?.pages.flatMap((p) => p.scripts) ?? [];
  return (
    <div className="page">
      <PageHeader title="Scripts" />
      <p className="page-intro">Every distinct source executed, the most recently run first.</p>
      <section className="filters" aria-label="Filters">
        <AddFilter
          fields={[
            {
              name: "label",
              placeholder: "key:value",
              check: (v) => (v.indexOf(":") > 0 ? null : "A label filter is key:value"),
            },
            { name: "tool", placeholder: "github.close_issue" },
          ]}
          onAdd={(field, value) => (field === "label" ? addLabel(value) : update({ tool: value }))}
        />
        {filters.labels.map((pair) => (
          <FilterChip
            key={pair}
            name="label"
            value={pair}
            onRemove={() => update({ labels: filters.labels.filter((l) => l !== pair) })}
          />
        ))}
        {filters.tool && (
          <FilterChip name="tool" value={filters.tool} onRemove={() => update({ tool: null })} />
        )}
      </section>
      {scripts.isPending ? (
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
                  onPickTool={(tool) => update({ tool })}
                />
              ))}
            </tbody>
          </table>
          {list.length === 0 && <Empty>No script matches these filters.</Empty>}
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
