// The namespace filter of the top bar, beside the organization picker: a search over the
// organization's namespaces (there can be many thousands, so it asks the server for those whose
// name starts with what is typed), which narrows the run views to one end user. The choice is
// the URL's (`?ns=`); every request of the run views names it (`Rewind-Namespace`).
import { ChevronDown, UsersRound, X } from "lucide-react";
import { type KeyboardEvent, useEffect, useId, useRef, useState } from "react";
import { useLocation, useSearchParams } from "react-router";
import { useNamespaceSuggestions, useSettled } from "../api/namespaces";
import { formatCount } from "../model/format";
import { namespaceNameProblem } from "../model/quotas";
import { useDismiss } from "./AccountMenus";
import { useAccess } from "./access";
import { NS_PARAM, useNamespace, useNavigate } from "./namespace";

interface Choice {
  /** The namespace, or `null` for every namespace. */
  value: string | null;
  /** Runs started this month, when the list says it. */
  runs?: number;
  /** A name typed that the list did not offer. */
  typed?: boolean;
}

/** A run belongs to one namespace: narrowing to another leads back to the runs. */
const RUN_PAGE = /^\/runs\/[^/]+$/;

export function NamespaceFilter() {
  const access = useAccess();
  const current = useNamespace();
  const [, setParams] = useSearchParams();
  const location = useLocation();
  const navigate = useNavigate();
  const menu = useRef<HTMLDetailsElement>(null);
  const input = useRef<HTMLInputElement>(null);
  const close = useDismiss(menu);
  const [open, setOpen] = useState(false);
  const [text, setText] = useState("");
  const [active, setActive] = useState(-1);
  const listId = useId();
  const inputId = useId();
  const canList = access.can("namespaces:read");
  const typed = text.trim();
  const settled = useSettled(typed);
  const suggestions = useNamespaceSuggestions(open && canList ? settled : null);

  useEffect(() => {
    if (!open) return;
    setText("");
    setActive(-1);
    input.current?.focus();
  }, [open]);

  const listed = (suggestions.data?.namespaces ?? []).filter((n) => n.name.startsWith(typed));
  const problem = typed ? namespaceNameProblem(typed) : null;
  const choices: Choice[] = [
    ...(typed ? [] : [{ value: null }]),
    ...listed.map((n) => ({ value: n.name, runs: n.usage.runsStarted })),
    ...(typed && !problem && !listed.some((n) => n.name === typed)
      ? [{ value: typed, typed: true }]
      : []),
  ];

  const choose = (value: string | null) => {
    close();
    setOpen(false);
    if (value === current) return;
    if (value !== null && RUN_PAGE.test(location.pathname)) {
      navigate(`/runs?${new URLSearchParams({ [NS_PARAM]: value })}`);
      return;
    }
    setParams((params) => {
      const next = new URLSearchParams(params);
      if (value === null) next.delete(NS_PARAM);
      else next.set(NS_PARAM, value);
      return next;
    });
  };

  const onKeyDown = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.key === "ArrowDown" && choices.length > 0) {
      e.preventDefault();
      setActive((a) => (a + 1) % choices.length);
    } else if (e.key === "ArrowUp" && choices.length > 0) {
      e.preventDefault();
      setActive((a) => (a <= 0 ? choices.length - 1 : a - 1));
    } else if (e.key === "Enter") {
      e.preventDefault();
      const picked = choices[active] ?? (typed && !problem ? { value: typed } : null);
      if (picked) choose(picked.value);
    }
  };

  return (
    <div className="ns-filter">
      <details
        className={`menu ns-menu${current ? " on" : ""}`}
        ref={menu}
        onToggle={(e) => setOpen((e.target as HTMLDetailsElement).open)}
      >
        <summary
          title="Narrow the views to one end user"
          aria-label={`Namespace: ${current ?? "all namespaces"}`}
        >
          <UsersRound size={13} aria-hidden="true" />
          <span className={`menu-current${current ? " mono" : ""}`}>
            {current ?? "All namespaces"}
          </span>
          <ChevronDown size={12} aria-hidden="true" />
        </summary>
        <div className="menu-pop ns-pop">
          <label className="menu-label" htmlFor={inputId}>
            Namespace
          </label>
          <input
            ref={input}
            id={inputId}
            className="input mono"
            type="text"
            role="combobox"
            aria-expanded={choices.length > 0}
            aria-controls={listId}
            aria-autocomplete="list"
            aria-activedescendant={active >= 0 ? `${listId}-${active}` : undefined}
            aria-invalid={problem !== null}
            spellCheck={false}
            autoComplete="off"
            placeholder={canList ? "Search a namespace…" : "Type a namespace"}
            value={text}
            onChange={(e) => {
              setText(e.target.value);
              setActive(-1);
            }}
            onKeyDown={onKeyDown}
          />
          {problem && <p className="ns-hint tone-critical">{problem}</p>}
          <div className="ns-options" id={listId} role="listbox" aria-label="Namespaces">
            {choices.map((c, i) => (
              <div
                key={c.value ?? ""}
                id={`${listId}-${i}`}
                role="option"
                tabIndex={-1}
                aria-selected={i === active}
                className={`menu-item${i === active ? " active" : ""}`}
                aria-current={c.value === current ? "true" : undefined}
                // Before the input's blur.
                onMouseDown={(e) => {
                  e.preventDefault();
                  choose(c.value);
                }}
              >
                {c.value === null ? (
                  <span>All namespaces</span>
                ) : c.typed ? (
                  <span>
                    Use <span className="mono">{c.value}</span>
                  </span>
                ) : (
                  <span className="mono ns-name">{c.value}</span>
                )}
                {c.runs !== undefined && (
                  <span className="num muted ns-runs" title="Runs started this month">
                    {formatCount(c.runs)} runs
                  </span>
                )}
              </div>
            ))}
          </div>
          {canList && suggestions.isFetching && (
            <span className="spinner" role="status" aria-label="Searching" />
          )}
          <p className="ns-hint muted">
            Narrows Overview, Runs, Waiting, Failures, Scripts and Tools to one end user.
          </p>
        </div>
      </details>
      {current && (
        <button
          type="button"
          className="icon-btn"
          aria-label="Show every namespace"
          title="Show every namespace"
          onClick={() => choose(null)}
        >
          <X size={13} />
        </button>
      )}
    </div>
  );
}
