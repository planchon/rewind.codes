// The journal itself, the script's logs, and the LLM trace: each a section of the run's page.
import { useVirtualizer } from "@tanstack/react-virtual";
import { useEffect, useRef, useState } from "react";
import { useRunTrace } from "../../api/queries";
import { Empty, Loading, ProblemView } from "../../components/bits";
import { formatCount, formatOffset } from "../../model/format";
import type { RunModel } from "../../model/run";
import { entryFamily, entryLine, entrySummary } from "./describe";

const ROW = 24;

type Tab = "journal" | "logs" | "trace";

export function JournalList({
  model,
  selected,
  onSelect,
}: {
  model: RunModel;
  selected: number | null;
  onSelect: (idx: number) => void;
}) {
  const scroller = useRef<HTMLDivElement>(null);
  const entries = model.entries;
  const virtual = useVirtualizer({
    count: entries.length,
    getScrollElement: () => scroller.current,
    estimateSize: () => ROW,
    overscan: 16,
    initialRect: { width: 800, height: 360 },
  });
  useEffect(() => {
    if (selected !== null && selected < entries.length)
      virtual.scrollToIndex(selected, { align: "auto" });
  }, [selected, entries.length, virtual]);

  if (entries.length === 0) return <Empty>No entry yet.</Empty>;
  const t0 = model.firstAt ?? 0;
  return (
    <div className="journal">
      <div className="journal-row journal-head" aria-hidden="true">
        <span>#</span>
        <span>at</span>
        <span>event</span>
        <span>detail</span>
        <span>line</span>
      </div>
      <div className="journal-scroll" ref={scroller} title="Journal entries">
        <div style={{ height: virtual.getTotalSize(), position: "relative" }}>
          {virtual.getVirtualItems().map((item) => {
            const entry = entries[item.index];
            if (!entry) return null;
            const line = entryLine(entry);
            const summary = entrySummary(entry);
            return (
              <button
                type="button"
                key={entry.idx}
                aria-pressed={selected === entry.idx}
                className={`journal-row family-${entryFamily(entry)}${selected === entry.idx ? " selected" : ""}`}
                style={{ position: "absolute", top: item.start, left: 0, right: 0, height: ROW }}
                onClick={() => onSelect(entry.idx)}
              >
                <span className="num muted">{entry.idx}</span>
                <span className="num">{formatOffset(Date.parse(entry.recordedAt) - t0)}</span>
                <span className="mono ev-type">{entry.event.type}</span>
                <span className="mono ev-detail" title={summary}>
                  {summary}
                </span>
                <span className="num muted">{line ?? ""}</span>
              </button>
            );
          })}
        </div>
      </div>
    </div>
  );
}

export function Logs({
  model,
  onSelect,
  onSelectLine,
}: {
  model: RunModel;
  onSelect: (idx: number) => void;
  onSelectLine: (line: number) => void;
}) {
  if (model.logs.length === 0) return <Empty>The script logged nothing.</Empty>;
  const t0 = model.firstAt ?? 0;
  return (
    <ol className="logs">
      {model.logs.map((l) => (
        <li key={l.idx} className={`level-${l.level}`}>
          <span className="num muted">{formatOffset(l.at - t0)}</span>
          <span className={`log-level level-${l.level}`}>{l.level}</span>
          <button type="button" className="log-text mono" onClick={() => onSelect(l.idx)}>
            {l.message}
          </button>
          {l.callsite && (
            <button
              type="button"
              className="link-btn mono"
              onClick={() => l.callsite && onSelectLine(l.callsite.line)}
            >
              L{l.callsite.line}
            </button>
          )}
        </li>
      ))}
    </ol>
  );
}

export function LlmTrace({ runId, historyLen }: { runId: string; historyLen: number }) {
  const trace = useRunTrace(runId, historyLen);
  if (trace.isPending) return <Loading>Loading the trace…</Loading>;
  if (trace.isError) return <ProblemView error={trace.error} title="The trace could not be read" />;
  return <pre className="llm-trace">{trace.data.text}</pre>;
}

export function JournalPanel({
  runId,
  model,
  selected,
  onSelect,
  onSelectLine,
}: {
  runId: string;
  model: RunModel;
  selected: number | null;
  onSelect: (idx: number) => void;
  onSelectLine: (line: number) => void;
}) {
  const [tab, setTab] = useState<Tab>("journal");
  return (
    <section className="panel">
      <header className="panel-head">
        <div className="tabs" role="tablist">
          <button
            type="button"
            role="tab"
            aria-selected={tab === "journal"}
            onClick={() => setTab("journal")}
          >
            Journal <span className="muted num">{formatCount(model.entries.length)}</span>
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={tab === "logs"}
            onClick={() => setTab("logs")}
          >
            Logs <span className="muted num">{formatCount(model.logs.length)}</span>
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={tab === "trace"}
            onClick={() => setTab("trace")}
          >
            LLM trace
          </button>
        </div>
        <span className="end muted">
          {tab === "journal" && "lifecycle · commands · completions · annotations"}
          {tab === "trace" && "what a model reads with get_run"}
        </span>
      </header>
      <div role="tabpanel">
        {tab === "journal" && <JournalList model={model} selected={selected} onSelect={onSelect} />}
        {tab === "logs" && <Logs model={model} onSelect={onSelect} onSelectLine={onSelectLine} />}
        {tab === "trace" && <LlmTrace runId={runId} historyLen={model.entries.length} />}
      </div>
    </section>
  );
}
