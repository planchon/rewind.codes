// Tools: latency, error rate and retries per tool, grouped by namespace (an MCP server or a
// host-tool group), over a window. A search as on the Overview, one line of text: a part of a
// tool's name, `tool:` (a name or `github.*`) and `toolset:` (the calls of one toolset's runs).
import { Fragment, useMemo } from "react";
import { Link } from "react-router";
import { useToolStats } from "../api/queries";
import type { ToolStats } from "../api/types";
import { Empty, Loading, ProblemView, Time } from "../components/bits";
import { PageHeader } from "../components/PageHeader";
import { QueryBar } from "../components/QueryBar";
import { useTooltip } from "../components/Tooltip";
import { type RangeKey, toToolStatsQuery } from "../model/filters";
import { formatCount, formatDuration, formatPercent } from "../model/format";
import { type FieldValues, fieldTerm, joinQuery, queryLink } from "../model/query";
import { readToolSearch, TOOL_FIELDS } from "../model/searches";
import { Seg } from "./FilterBits";
import { type OlderSearch, useViewSearch, useWindow, WINDOW_WORDS, WINDOWS } from "./search";

/** `/tools?toolset=…`, a link from before the search: the search of its toolset. */
const OLDER: OlderSearch = {
  names: ["toolset"],
  read: (params) =>
    joinQuery(
      params.get("q"),
      ...params
        .getAll("toolset")
        .filter((t) => t.trim() !== "")
        .map((t) => fieldTerm("toolset", t.trim())),
    ),
};

/** The values the search suggests for `tool:`: each tool listed, then each namespace's tools. */
function toolValues(tools: ToolStats[]): FieldValues {
  const namespaces = [
    ...new Set(tools.flatMap((t) => (t.tool.includes(".") ? [namespaceOf(t.tool)] : []))),
  ];
  return {
    tool: [
      ...tools.map((t) => ({ value: t.tool, count: t.calls })),
      ...namespaces.map((ns) => ({
        value: `${ns}.*`,
        detail: "every tool of the namespace",
        raw: true,
      })),
    ],
  };
}

export function namespaceOf(tool: string): string {
  const at = tool.indexOf(".");
  return at > 0 ? tool.slice(0, at) : "host & built-in";
}

interface Group {
  name: string;
  tools: ToolStats[];
  calls: number;
  errors: number;
  retries: number;
}

export function groupTools(tools: ToolStats[]): Group[] {
  const groups = new Map<string, Group>();
  for (const t of tools) {
    const name = namespaceOf(t.tool);
    let g = groups.get(name);
    if (!g) {
      g = { name, tools: [], calls: 0, errors: 0, retries: 0 };
      groups.set(name, g);
    }
    g.tools.push(t);
    g.calls += t.calls;
    g.errors += t.errors;
    g.retries += t.retries;
  }
  return [...groups.values()].sort((a, b) => b.calls - a.calls || a.name.localeCompare(b.name));
}

/** A log scale over the latencies of every tool, so their strips compare. */
export function logScale(tools: ToolStats[]) {
  // Calls answered at once (mock mode, virtual time) would stretch the axis to 1 ms.
  const positive = tools.map((t) => t.latencyMs.p50).filter((v) => v > 0);
  const lo = Math.floor(Math.log10(Math.min(...positive, 1_000)));
  const hi = Math.log10(Math.max(...tools.map((t) => t.latencyMs.max), 10 ** (lo + 1)));
  const span = Math.max(hi - lo, 1);
  return {
    x: (ms: number) => Math.min(1, Math.max(0, (Math.log10(Math.max(ms, 10 ** lo)) - lo) / span)),
    min: 10 ** lo,
    max: 10 ** hi,
  };
}

function LatencyStrip({
  stats,
  scale,
  onHover,
  onLeave,
}: {
  stats: ToolStats;
  scale: ReturnType<typeof logScale>;
  onHover: (e: React.MouseEvent) => void;
  onLeave: () => void;
}) {
  const { p50, p95, p99, max } = stats.latencyMs;
  const pct = (v: number) => `${(scale.x(v) * 100).toFixed(2)}%`;
  return (
    <div
      className="latency-strip"
      onMouseMove={onHover}
      onMouseLeave={onLeave}
      role="img"
      aria-label={`p50 ${formatDuration(p50)}, p95 ${formatDuration(p95)}, p99 ${formatDuration(p99)}, max ${formatDuration(max)}`}
    >
      <span
        className="whisker"
        style={{ left: pct(p50), width: `calc(${pct(max)} - ${pct(p50)})` }}
      />
      <span
        className="box"
        style={{ left: pct(p50), width: `max(3px, calc(${pct(p95)} - ${pct(p50)}))` }}
      />
      <span className="tick p99" style={{ left: pct(p99) }} />
      <span className="tick max" style={{ left: pct(max) }} />
    </div>
  );
}

export function ToolsView() {
  const { q, setQ } = useViewSearch("q", OLDER);
  const [win, setWin] = useWindow();
  const search = readToolSearch(q);
  const toolset = search.toolset;
  const stats = useToolStats(`tools:${win}|${toolset ?? ""}`, () =>
    toToolStatsQuery(win, toolset, Date.now()),
  );
  const tip = useTooltip();

  const every = stats.data?.tools;
  const values = useMemo(() => toolValues(every ?? []), [every]);
  const tools = (every ?? []).filter((t) => search.keeps(t.tool));
  const groups = groupTools(tools);
  const scale = logScale(tools);
  const calls = tools.reduce((s, t) => s + t.calls, 0);
  const errors = tools.reduce((s, t) => s + t.errors, 0);
  const retries = tools.reduce((s, t) => s + t.retries, 0);
  const slowest = [...tools].sort((a, b) => b.latencyMs.p95 - a.latencyMs.p95)[0];

  return (
    <div className="page">
      <PageHeader
        title="Tools"
        actions={
          <Seg<RangeKey>
            label="Window"
            value={win}
            onChange={setWin}
            options={WINDOWS.map((w) => ({ value: w, label: w, title: WINDOW_WORDS[w] }))}
          />
        }
      />
      <QueryBar
        value={q}
        onSubmit={setQ}
        fields={TOOL_FIELDS}
        values={values}
        fault={search.fault}
        label="Search tools"
        placeholder="github  tool:github.*  toolset:5f3a9c…"
      />
      {stats.data && (
        <p className="page-intro">
          Calls completed from <Time ms={Date.parse(stats.data.since)} /> to{" "}
          <Time ms={Date.parse(stats.data.until)} />.
        </p>
      )}

      {search.fault ? (
        <Empty>Fix the search to see the tools: the term at fault is underlined above.</Empty>
      ) : stats.isPending ? (
        <Loading>Loading tool statistics…</Loading>
      ) : stats.isError ? (
        <ProblemView error={stats.error} title="The tool statistics could not be read" />
      ) : tools.length === 0 ? (
        <div className="table-wrap">
          <Empty>
            {(every ?? []).length === 0
              ? "No tool call completed in this window."
              : "No tool of this window matches the search."}
          </Empty>
        </div>
      ) : (
        <>
          <section className="stat-strip" aria-label="Totals">
            <div>
              <span className="k">Calls</span>
              <span className="v num">{formatCount(calls)}</span>
            </div>
            <div>
              <span className="k">Failed</span>
              <span className="v num">
                {formatCount(errors)}{" "}
                <span className="muted">· {formatPercent(errors, calls)}</span>
              </span>
            </div>
            <div>
              <span className="k">Retries</span>
              <span className="v num">
                {formatCount(retries)}{" "}
                <span className="muted">· {formatPercent(retries, calls)} of calls</span>
              </span>
            </div>
            {slowest && (
              <div>
                <span className="k">Slowest p95</span>
                <span className="v">
                  <span className="mono">{slowest.tool}</span>{" "}
                  <span className="num">{formatDuration(slowest.latencyMs.p95)}</span>
                </span>
              </div>
            )}
          </section>
          <div className="table-wrap">
            <table className="grid tools-table">
              <thead>
                <tr>
                  <th>Tool</th>
                  <th className="right">Calls</th>
                  <th className="right">Failed</th>
                  <th>Error rate</th>
                  <th>By kind</th>
                  <th className="right">Retries</th>
                  <th className="right">p50</th>
                  <th className="right">p95</th>
                  <th className="right">p99</th>
                  <th className="right">max</th>
                  <th className="latency-head">
                    <span>Latency</span>
                    <span className="muted num">
                      {formatDuration(scale.min)} – {formatDuration(scale.max)}, log
                    </span>
                  </th>
                </tr>
              </thead>
              <tbody>
                {groups.map((g) => (
                  <Fragment key={g.name}>
                    <tr className="group-row">
                      <td className="mono">{g.name}</td>
                      <td className="right num">{formatCount(g.calls)}</td>
                      <td className="right num">{formatCount(g.errors)}</td>
                      <td className="num dim">{formatPercent(g.errors, g.calls)}</td>
                      <td />
                      <td className="right num">{formatCount(g.retries)}</td>
                      <td colSpan={5} />
                    </tr>
                    {g.tools.map((t) => (
                      <tr key={t.tool}>
                        <td className="mono tool-name">
                          <Link
                            to={queryLink("/observability", fieldTerm("tool", t.tool))}
                            title={`Runs that called ${t.tool}`}
                          >
                            {t.tool}
                          </Link>
                        </td>
                        <td className="right num">{formatCount(t.calls)}</td>
                        <td className={`right num${t.errors > 0 ? " tone-critical" : " muted"}`}>
                          {formatCount(t.errors)}
                        </td>
                        <td>
                          <span className="rate">
                            <span className="rate-bar" aria-hidden="true">
                              <span
                                style={{ width: `${t.calls ? (100 * t.errors) / t.calls : 0}%` }}
                              />
                            </span>
                            <span className="num">{formatPercent(t.errors, t.calls)}</span>
                          </span>
                        </td>
                        <td>
                          <span className="labels">
                            {Object.entries(t.errorKinds)
                              .filter(([, n]) => n > 0)
                              .sort(([, a], [, b]) => b - a)
                              .map(([kind, n]) => (
                                <Link
                                  key={kind}
                                  className="label"
                                  to={queryLink(
                                    "/observability",
                                    `${fieldTerm("tool", t.tool)} ${fieldTerm("error", kind)}`,
                                  )}
                                  title={`Runs where ${t.tool} failed with ${kind}`}
                                >
                                  <span className="k">{kind}</span>&nbsp;{n}
                                </Link>
                              ))}
                          </span>
                        </td>
                        <td
                          className="right num"
                          title={`${formatPercent(t.retries, t.calls)} of calls`}
                        >
                          {formatCount(t.retries)}
                        </td>
                        <td className="right num">{formatDuration(t.latencyMs.p50)}</td>
                        <td className="right num">{formatDuration(t.latencyMs.p95)}</td>
                        <td className="right num">{formatDuration(t.latencyMs.p99)}</td>
                        <td className="right num dim">{formatDuration(t.latencyMs.max)}</td>
                        <td>
                          <LatencyStrip
                            stats={t}
                            scale={scale}
                            onLeave={tip.hide}
                            onHover={(e) =>
                              tip.show(
                                e,
                                <>
                                  <div className="tt-title">{t.tool}</div>
                                  <dl>
                                    <dt>p50</dt>
                                    <dd>{formatDuration(t.latencyMs.p50)}</dd>
                                    <dt>p95</dt>
                                    <dd>{formatDuration(t.latencyMs.p95)}</dd>
                                    <dt>p99</dt>
                                    <dd>{formatDuration(t.latencyMs.p99)}</dd>
                                    <dt>max</dt>
                                    <dd>{formatDuration(t.latencyMs.max)}</dd>
                                    <dt>calls</dt>
                                    <dd>{formatCount(t.calls)}</dd>
                                  </dl>
                                </>,
                              )
                            }
                          />
                        </td>
                      </tr>
                    ))}
                  </Fragment>
                ))}
              </tbody>
            </table>
            <div className="list-foot">
              <span>
                Strip: p50 to p95 as a bar, then a line to the slowest call; a tick at p99. Latency
                runs from the call's schedule to its completion.
              </span>
            </div>
          </div>
        </>
      )}
      {tip.view}
    </div>
  );
}
