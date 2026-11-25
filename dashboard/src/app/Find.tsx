// Find (archi/08, "Find (`F`) opens a palette"): a run by its handle or its id, a view, or the
// runs a query matches. Opened from the sidebar or with `F` anywhere but in a field.
import { Dialog } from "@base-ui/react/dialog";
import { ArrowRight, CornerDownLeft, Search } from "lucide-react";
import { type KeyboardEvent, useEffect, useMemo, useState } from "react";
import { useNavigate } from "react-router";
import { queryLink } from "../model/query";
import { useAccess } from "../state/access";
import { scopedPath } from "../state/scope";
import { isRunName, runPath } from "./runLinks";
import { useShownNamespace } from "./scope";

interface Choice {
  key: string;
  label: string;
  detail?: string;
  to: string;
  kind: "run" | "view" | "query";
}

/** The views one may go to, as the caller's permissions allow, in the scope shown. */
function useViews(): Choice[] {
  const access = useAccess();
  const shown = useShownNamespace();
  const at = (path: string) => scopedPath(path, shown);
  const reads = access.can("runs:read");
  const views: (Choice | false)[] = [
    reads && { key: "overview", label: "Overview", to: at("/overview"), kind: "view" },
    reads && { key: "runs", label: "Runs", detail: "All", to: at("/runs"), kind: "view" },
    reads && {
      key: "running",
      label: "Runs",
      detail: "Running",
      to: at("/runs/running"),
      kind: "view",
    },
    reads && {
      key: "waiting",
      label: "Runs",
      detail: "Waiting",
      to: at("/runs/waiting"),
      kind: "view",
    },
    reads && {
      key: "failures",
      label: "Runs",
      detail: "Failures",
      to: at("/runs/failures"),
      kind: "view",
    },
    reads && { key: "workflows", label: "Workflows", to: at("/workflows"), kind: "view" },
    reads &&
      access.statistics && { key: "actions", label: "Actions", to: at("/actions"), kind: "view" },
    access.can("namespaces:read") &&
      (shown === null
        ? { key: "namespaces", label: "Namespaces", to: "/namespaces", kind: "view" }
        : { key: "usage", label: "Usage", to: at("/usage"), kind: "view" }),
    access.can("secrets:read") && {
      key: "secrets",
      label: "Secrets",
      to: at("/secrets"),
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
  const access = useAccess();
  const shown = useShownNamespace();
  const [text, setText] = useState("");
  const [active, setActive] = useState(0);

  const choices = useMemo<Choice[]>(() => {
    const t = text.trim();
    const lower = t.toLowerCase();
    const out: Choice[] = [];
    if (isRunName(t))
      out.push({ key: "run", label: `Open the run ${t}`, to: runPath(t), kind: "run" });
    const matches = (s: string) => s.toLowerCase().includes(lower);
    out.push(...views.filter((v) => !t || matches(v.label) || matches(v.detail ?? "")));
    if (t && !isRunName(t) && access.can("runs:read"))
      out.push({
        key: "query",
        label: `Search the runs for ${t}`,
        to: queryLink(scopedPath("/runs", shown), t),
        kind: "query",
      });
    return out;
  }, [text, views, access, shown]);

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
              placeholder="A run, a view, a query…"
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
                {c.kind === "query" ? (
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
