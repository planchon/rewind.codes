// Find (archi/08, "Find (`F`) opens a palette"): a run by its id, a view, a saved search, or the
// runs a query matches. Opened from the sidebar or with `F` anywhere but in a field.
import { Dialog } from "@base-ui/react/dialog";
import { ArrowRight, Bookmark, CornerDownLeft, Search } from "lucide-react";
import { type KeyboardEvent, useEffect, useMemo, useState } from "react";
import { useNavigate } from "react-router";
import { queryLink } from "../model/query";
import { useAccess } from "./access";
import { savedSearchPath, useSavedSearches } from "./searches";

const RUN_ID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

interface Choice {
  key: string;
  label: string;
  detail?: string;
  to: string;
  kind: "run" | "view" | "saved" | "query";
}

/** The views one may go to, as the caller's permissions allow. */
function useViews(): Choice[] {
  const access = useAccess();
  const reads = access.can("runs:read");
  const views: (Choice | false)[] = [
    reads && { key: "overview", label: "Overview", to: "/overview", kind: "view" },
    reads && {
      key: "obs",
      label: "Observability",
      detail: "All runs",
      to: "/observability",
      kind: "view",
    },
    reads && {
      key: "active",
      label: "Observability",
      detail: "Active runs",
      to: "/observability/active",
      kind: "view",
    },
    reads && {
      key: "waiting",
      label: "Observability",
      detail: "Waiting runs",
      to: "/observability/waiting",
      kind: "view",
    },
    reads && {
      key: "failures",
      label: "Observability",
      detail: "Failures",
      to: "/observability/failures",
      kind: "view",
    },
    reads && { key: "scripts", label: "Scripts", to: "/scripts", kind: "view" },
    reads && access.toolStats && { key: "tools", label: "Tools", to: "/tools", kind: "view" },
    access.can("namespaces:read") && {
      key: "namespaces",
      label: "Namespaces",
      to: "/namespaces",
      kind: "view",
    },
    access.sections.length > 0 && {
      key: "organization",
      label: "Organization",
      to: "/organization",
      kind: "view",
    },
  ];
  return views.filter((v): v is Choice => v !== false);
}

export function useFindShortcut(open: () => void) {
  useEffect(() => {
    const onKey = (e: globalThis.KeyboardEvent) => {
      if (e.key !== "f" && e.key !== "F") return;
      if (e.metaKey || e.ctrlKey || e.altKey) return;
      const target = e.target as HTMLElement | null;
      if (
        target?.closest("input, textarea, select, [contenteditable=''], [contenteditable='true']")
      )
        return;
      e.preventDefault();
      open();
    };
    document.addEventListener("keydown", onKey);
    return () => document.removeEventListener("keydown", onKey);
  }, [open]);
}

export function FindDialog({
  open,
  onOpenChange,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const navigate = useNavigate();
  const views = useViews();
  const { searches } = useSavedSearches();
  const access = useAccess();
  const [text, setText] = useState("");
  const [active, setActive] = useState(0);

  const choices = useMemo<Choice[]>(() => {
    const t = text.trim();
    const lower = t.toLowerCase();
    const out: Choice[] = [];
    if (RUN_ID.test(t))
      out.push({ key: "run", label: `Open the run ${t}`, to: `/runs/${t}`, kind: "run" });
    const matches = (s: string) => s.toLowerCase().includes(lower);
    out.push(
      ...searches
        .filter((s) => !t || matches(s.name) || matches(s.q))
        .map<Choice>((s) => ({
          key: `saved:${s.name}`,
          label: s.name,
          detail: s.q || "every run",
          to: savedSearchPath(s),
          kind: "saved",
        })),
    );
    out.push(...views.filter((v) => !t || matches(v.label) || matches(v.detail ?? "")));
    if (t && !RUN_ID.test(t) && access.can("runs:read"))
      out.push({
        key: "query",
        label: `Search the runs for ${t}`,
        to: queryLink("/observability", t),
        kind: "query",
      });
    return out;
  }, [text, views, searches, access]);

  const go = (choice: Choice | undefined) => {
    if (!choice) return;
    onOpenChange(false);
    setText("");
    void navigate(choice.to);
  };

  const onKeyDown = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.key === "ArrowDown") {
      e.preventDefault();
      setActive((a) => Math.min(choices.length - 1, a + 1));
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      setActive((a) => Math.max(0, a - 1));
    } else if (e.key === "Enter") {
      e.preventDefault();
      go(choices[active]);
    }
  };

  return (
    <Dialog.Root
      open={open}
      onOpenChange={(next) => {
        onOpenChange(next);
        if (!next) setText("");
      }}
    >
      <Dialog.Portal>
        <Dialog.Backdrop className="dialog-backdrop" />
        <Dialog.Popup className="dialog find" aria-label="Find">
          <Dialog.Title className="sr-only">Find</Dialog.Title>
          <div className="find-input">
            <Search size={16} aria-hidden="true" />
            <input
              value={text}
              onChange={(e) => {
                setText(e.target.value);
                setActive(0);
              }}
              onKeyDown={onKeyDown}
              placeholder="A run's id, a view, a saved search…"
              aria-label="Find"
              aria-controls="find-choices"
              aria-activedescendant={choices[active] ? `find-${choices[active].key}` : undefined}
              role="combobox"
              aria-expanded="true"
              autoComplete="off"
              spellCheck={false}
            />
            <span className="kbd">Esc</span>
          </div>
          <div className="find-list" id="find-choices" role="listbox" aria-label="Choices">
            {choices.length === 0 && <div className="find-empty">Nothing matches.</div>}
            {choices.map((c, i) => (
              <div
                key={c.key}
                id={`find-${c.key}`}
                role="option"
                tabIndex={-1}
                aria-selected={i === active}
                className="find-choice"
                onMouseEnter={() => setActive(i)}
                // Before the input's blur: the choice is taken with the pointer.
                onMouseDown={(e) => {
                  e.preventDefault();
                  go(c);
                }}
              >
                {c.kind === "saved" ? (
                  <Bookmark size={14} aria-hidden="true" />
                ) : c.kind === "query" ? (
                  <Search size={14} aria-hidden="true" />
                ) : (
                  <ArrowRight size={14} aria-hidden="true" />
                )}
                <span className="find-label">{c.label}</span>
                {c.detail && <span className="find-detail">{c.detail}</span>}
                {i === active && (
                  <CornerDownLeft size={13} className="find-enter" aria-hidden="true" />
                )}
              </div>
            ))}
          </div>
        </Dialog.Popup>
      </Dialog.Portal>
    </Dialog.Root>
  );
}
