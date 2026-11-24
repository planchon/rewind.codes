// The namespace picker, at the left of the page's header (archi/05, "Namespaces"): every
// namespace, or one. Its search comes first; then every namespace; then the namespaces whose name
// or display name starts with what is typed, the most active this month first. Picking one opens
// the same view in that scope (`switchedPath`).
import { Popover } from "@base-ui/react/popover";
import { Check, ChevronsUpDown, Layers, Search } from "lucide-react";
import { type KeyboardEvent, useMemo, useRef, useState } from "react";
import { useLocation, useNavigate } from "react-router";
import { useSettled } from "../components/bits";
import { NamespaceMark } from "../components/NamespaceMark";
import { useAccess } from "../state/access";
import { useNamespaceNames, useNamespaceSuggestions } from "../state/namespaces";
import { isNamespaceName } from "../state/scope";
import { switchedPath, useShownNamespace } from "./scope";

/** The namespaces the list shows at once. */
const SHOWN = 8;

interface Choice {
  /** `null`: every namespace. */
  name: string | null;
  label: string;
  /** The id, when the label is a display name. */
  id?: string;
}

/** A namespace as the picker says it: its display name, else its name. */
function NamespaceLabel({ name, display }: { name: string; display: string | undefined }) {
  return display ? (
    <span className="ns-pick-label">{display}</span>
  ) : (
    <span className="ns-pick-label mono">{name}</span>
  );
}

export function NamespacePicker() {
  const shown = useShownNamespace();
  const names = useNamespaceNames();
  const access = useAccess();
  const navigate = useNavigate();
  const { pathname, search } = useLocation();
  const [open, setOpen] = useState(false);
  const [typed, setTyped] = useState("");
  const [active, setActive] = useState(0);
  const input = useRef<HTMLInputElement>(null);
  const reads = access.can("namespaces:read");
  const prefix = useSettled(open && reads ? typed.trim() : null);
  const found = useNamespaceSuggestions(prefix, SHOWN);

  const choices = useMemo<Choice[]>(() => {
    const t = typed.trim();
    const out: Choice[] = [];
    if (!t) out.push({ name: null, label: "All namespaces" });
    const listed = (found.data?.namespaces ?? []).map((n) => n.name);
    // The namespace shown leads the list when the most active do not hold it.
    const current = shown !== null && !t && !listed.includes(shown) ? [shown] : [];
    for (const name of [...current, ...listed]) {
      const display = names.get(name) ?? undefined;
      out.push({ name, label: display ?? name, ...(display ? { id: name } : {}) });
    }
    // Without `namespaces:read`, a name typed whole is taken as it is.
    if (!reads && isNamespaceName(t)) out.push({ name: t, label: t });
    return out;
  }, [typed, found.data, shown, names, reads]);

  const pick = (choice: Choice | undefined) => {
    if (!choice) return;
    setOpen(false);
    setTyped("");
    if (choice.name !== shown) void navigate(switchedPath(pathname, search, choice.name));
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
      pick(choices[active]);
    }
  };

  const display = shown !== null ? names.get(shown) : undefined;
  return (
    <Popover.Root
      open={open}
      onOpenChange={(next) => {
        setOpen(next);
        setActive(0);
        if (!next) setTyped("");
      }}
    >
      <Popover.Trigger
        className="ns-pick"
        aria-label={`Namespace: ${shown === null ? "all namespaces" : (display ?? shown)}`}
        title={shown !== null && display ? `Namespace ${shown}` : undefined}
      >
        {shown === null ? (
          <>
            <Layers size={14} aria-hidden="true" className="ns-pick-all" />
            <span className="ns-pick-label">All namespaces</span>
          </>
        ) : (
          <>
            <NamespaceMark name={shown} />
            <NamespaceLabel name={shown} display={display} />
          </>
        )}
        <ChevronsUpDown size={14} aria-hidden="true" className="ns-pick-chevron" />
      </Popover.Trigger>
      <Popover.Portal>
        <Popover.Positioner
          className="menu-positioner"
          side="bottom"
          align="start"
          sideOffset={6}
          collisionPadding={12}
        >
          <Popover.Popup className="menu-popup ns-pick-menu" initialFocus={input}>
            <div className="ns-pick-search">
              <Search size={14} aria-hidden="true" />
              <input
                ref={input}
                value={typed}
                onChange={(e) => {
                  setTyped(e.target.value);
                  setActive(0);
                }}
                onKeyDown={onKeyDown}
                placeholder="Find a namespace…"
                aria-label="Find a namespace"
                aria-controls="ns-pick-choices"
                aria-activedescendant={choices[active] ? `ns-pick-${active}` : undefined}
                role="combobox"
                aria-expanded="true"
                autoComplete="off"
                spellCheck={false}
              />
              <span className="kbd">Esc</span>
            </div>
            <div className="ns-pick-list" id="ns-pick-choices" role="listbox" aria-label="Scopes">
              {choices.length === 0 && (
                <div className="ns-pick-empty">
                  {found.status === "pending" && prefix !== null ? "Searching…" : "No namespace."}
                </div>
              )}
              {choices.map((c, i) => (
                <div key={c.name ?? "*"} className="ns-pick-group">
                  {c.name !== null && i > 0 && choices[i - 1]?.name === null && (
                    <div className="menu-separator" />
                  )}
                  <div
                    id={`ns-pick-${i}`}
                    role="option"
                    tabIndex={-1}
                    aria-selected={i === active}
                    aria-current={c.name === shown ? "true" : undefined}
                    className="ns-pick-choice"
                    title={c.id ? `Namespace ${c.id}` : undefined}
                    onMouseEnter={() => setActive(i)}
                    onMouseDown={(e) => {
                      e.preventDefault();
                      pick(c);
                    }}
                  >
                    {c.name === null ? (
                      <Layers size={14} aria-hidden="true" className="ns-pick-all" />
                    ) : (
                      <NamespaceMark name={c.name} />
                    )}
                    <span className={`ns-pick-label${c.id || c.name === null ? "" : " mono"}`}>
                      {c.label}
                    </span>
                    {c.name === shown && (
                      <Check size={14} aria-hidden="true" className="ns-pick-check" />
                    )}
                  </div>
                </div>
              ))}
            </div>
          </Popover.Popup>
        </Popover.Positioner>
      </Popover.Portal>
    </Popover.Root>
  );
}
