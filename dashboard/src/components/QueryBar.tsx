// The query bar: one line of the query language, its terms tinted by kind, the term the server
// could not read underlined with its message, and suggestions as the operator types (field names
// from the grammar, values from the facets of the current query and those given). Enter applies
// the query. The views of runs read the whole language; Scripts, Tools and Namespaces read the
// part of it their endpoint understands (`language`), and underline the rest themselves.
import { Search, X } from "lucide-react";
import {
  type FormEvent,
  type KeyboardEvent,
  type ReactNode,
  useEffect,
  useId,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { formatCount } from "../model/format";
import {
  type FieldValues,
  isKnownField,
  type QueryFault,
  type QueryLanguage,
  RUNS_LANGUAGE,
  type Suggestion,
  splitTerms,
  suggest,
  typedField,
  utf16Span,
} from "../model/query";
import type { Facets } from "../model/types";

export type { QueryFault };

export interface QueryBarProps {
  /** The query applied, from the URL. */
  value: string;
  onSubmit: (q: string) => void;
  facets?: Facets | undefined;
  /** Values to suggest that the facets do not count, per field: the namespaces. */
  values?: FieldValues | undefined;
  /** The fields and labels the view reads: the runs' language by default. */
  language?: QueryLanguage;
  /**
   * The field whose value is being typed, and what is typed of it; `null` when no value is.
   * For a parent that looks the values up as they are typed.
   */
  onTyping?: (typed: { field: string; value: string } | null) => void;
  /** The server could not read `value`: where and why. */
  fault?: QueryFault | null;
  placeholder?: string;
  /** Beside the bar: a save button, a sort. */
  children?: ReactNode;
}

/** The query, as spans under the input: each term tinted, the term at fault underlined. */
function Mirror({
  text,
  bad,
  language,
}: {
  text: string;
  bad: [number, number] | null;
  language: QueryLanguage;
}) {
  const parts: ReactNode[] = [];
  let at = 0;
  for (const t of splitTerms(text)) {
    if (t.start > at) parts.push(text.slice(at, t.start));
    const kind =
      t.kind === "label"
        ? "label"
        : t.kind === "text"
          ? "text"
          : isKnownField(t, language)
            ? "field"
            : "text";
    const isBad = bad !== null && t.start < bad[1] && bad[0] < t.end;
    parts.push(
      <span
        key={t.start}
        className={`q-term q-${kind}${t.negated ? " q-not" : ""}${isBad ? " q-bad" : ""}`}
      >
        {t.raw}
      </span>,
    );
    at = t.end;
  }
  if (bad && bad[0] >= at && bad[1] > bad[0]) {
    parts.push(text.slice(at, bad[0]));
    parts.push(
      <span key="bad" className="q-bad">
        {text.slice(bad[0], bad[1])}
      </span>,
    );
    at = bad[1];
  }
  parts.push(text.slice(at));
  return <>{parts}</>;
}

export function QueryBar({
  value,
  onSubmit,
  facets,
  values,
  language = RUNS_LANGUAGE,
  onTyping,
  fault = null,
  placeholder = '@agent:triage status:failed action:github.* error:timeout "rate limit"',
  children,
}: QueryBarProps) {
  const [draft, setDraft] = useState(value);
  const [caret, setCaret] = useState(value.length);
  const [open, setOpen] = useState(false);
  const [active, setActive] = useState(-1);
  const input = useRef<HTMLInputElement>(null);
  const mirror = useRef<HTMLDivElement>(null);
  const listId = useId();
  /** Where the caret goes once the text a suggestion wrote is rendered. */
  const restore = useRef<number | null>(null);
  useLayoutEffect(() => {
    if (restore.current === null) return;
    input.current?.setSelectionRange(restore.current, restore.current);
    restore.current = null;
  });
  const errorId = useId();

  // A query applied elsewhere (a facet, a link, back): the bar shows it.
  useEffect(() => {
    setDraft(value);
    setCaret(value.length);
  }, [value]);

  const suggestions = useMemo(
    () => (open ? suggest(draft, caret, facets, 12, values, language) : null),
    [open, draft, caret, facets, values, language],
  );
  const typing = useRef(onTyping);
  typing.current = onTyping;
  useEffect(() => {
    typing.current?.(open ? typedField(draft, caret) : null);
  }, [open, draft, caret]);
  const items = suggestions?.items ?? [];
  const shown = items.length > 0;
  const bad = fault && draft === value ? utf16Span(value, fault.start, fault.end) : null;

  const edit = (text: string, at: number) => {
    setDraft(text);
    setCaret(at);
    setActive(-1);
    setOpen(true);
  };

  const accept = (s: Suggestion) => {
    if (!suggestions) return;
    const head = draft.slice(0, suggestions.from) + s.insert;
    const tail = draft.slice(suggestions.to);
    const spacer = s.partial || tail.startsWith(" ") ? "" : " ";
    const text = head + spacer + tail;
    const at = head.length + spacer.length;
    setDraft(text);
    setCaret(at);
    setActive(-1);
    setOpen(Boolean(s.partial));
    restore.current = at;
  };

  const submit = (e?: FormEvent) => {
    e?.preventDefault();
    setOpen(false);
    onSubmit(draft.trim());
  };

  const onKeyDown = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.key === "ArrowDown" && shown) {
      e.preventDefault();
      setActive((a) => (a + 1) % items.length);
    } else if (e.key === "ArrowUp" && shown) {
      e.preventDefault();
      setActive((a) => (a <= 0 ? items.length - 1 : a - 1));
    } else if ((e.key === "Enter" || e.key === "Tab") && shown && active >= 0) {
      e.preventDefault();
      const s = items[active];
      if (s) accept(s);
    } else if (e.key === "Escape") {
      if (shown) {
        e.preventDefault();
        setOpen(false);
      } else if (draft !== value) {
        setDraft(value);
      }
    }
  };

  const syncScroll = () => {
    if (mirror.current && input.current) mirror.current.scrollLeft = input.current.scrollLeft;
  };

  return (
    <div className="qbar-wrap">
      <form className="qbar-row" onSubmit={submit} aria-label="Search runs">
        <div className={`qbar${bad ? " invalid" : ""}`}>
          <Search size={13} className="qbar-icon" aria-hidden="true" />
          <div className="qbar-field">
            <div className="qbar-mirror" ref={mirror} aria-hidden="true">
              <Mirror text={draft} bad={bad} language={language} />
            </div>
            <input
              ref={input}
              className="qbar-input mono"
              type="text"
              role="combobox"
              aria-label="Query"
              aria-expanded={shown}
              aria-controls={listId}
              aria-autocomplete="list"
              aria-activedescendant={shown && active >= 0 ? `${listId}-${active}` : undefined}
              aria-invalid={bad !== null}
              aria-describedby={fault ? errorId : undefined}
              spellCheck={false}
              autoComplete="off"
              placeholder={placeholder}
              value={draft}
              onChange={(e) =>
                edit(e.target.value, e.target.selectionStart ?? e.target.value.length)
              }
              onSelect={(e) => {
                setCaret(e.currentTarget.selectionStart ?? draft.length);
                syncScroll();
              }}
              onScroll={syncScroll}
              onFocus={() => setOpen(true)}
              onBlur={() => setOpen(false)}
              onKeyDown={onKeyDown}
            />
          </div>
          {draft && (
            <button
              type="button"
              className="icon-btn"
              aria-label="Clear the query"
              onClick={() => {
                setDraft("");
                onSubmit("");
              }}
            >
              <X size={13} />
            </button>
          )}
          {shown && (
            <div className="qbar-suggestions" id={listId} role="listbox" aria-label="Suggestions">
              {items.map((s, i) => (
                <div
                  key={s.insert}
                  id={`${listId}-${i}`}
                  role="option"
                  tabIndex={-1}
                  aria-selected={i === active}
                  className={i === active ? "active" : undefined}
                  // Before the input's blur closes the list.
                  onMouseDown={(e) => {
                    e.preventDefault();
                    accept(s);
                  }}
                >
                  <span className="mono">{s.label}</span>
                  {s.detail && <span className="muted s-detail">{s.detail}</span>}
                  {s.count !== undefined && (
                    <span className="num muted s-count">{formatCount(s.count)}</span>
                  )}
                </div>
              ))}
            </div>
          )}
        </div>
        <button type="submit" className="btn" disabled={draft.trim() === value.trim()}>
          Apply
        </button>
        {children}
      </form>
      {fault && (
        <p className="qbar-fault" id={errorId} role="alert">
          <span className="tone-critical">The query cannot be read:</span> {fault.message}
        </p>
      )}
    </div>
  );
}
