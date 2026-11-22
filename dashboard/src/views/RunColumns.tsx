// The headers of the table of runs, each a menu: how to order the runs by the column, and the
// filters of what it shows. A filter is a term of the query, the same as the query bar's and the
// sidebar's (archi/08): a header's menu adds, excludes or removes it.
import { Popover } from "@base-ui/react/popover";
import { ArrowDown, ArrowUp, Check, ChevronDown } from "lucide-react";
import { type ReactNode, useState } from "react";
import { useNamespaceLabel } from "../components/NamespaceName";
import type { RangeKey } from "../model/filters";
import { shortDigest } from "../model/format";
import { askedTerms, countTerms, toggleFacet, withFieldTerm } from "../model/query";
import { ascending, SORT_WORDS } from "../model/sorts";
import type { Facets, FacetValue, RunSort, RunStatus } from "../model/types";
import { useAccess } from "../state/access";
import {
  FacetToggles,
  facetRows,
  NamespaceSearch,
  timelineOf,
  withTimeline,
} from "./observability/Filters";
import { WINDOW_WORDS, WINDOWS } from "./search";

/** What the headers of a table can change: its query and its order. */
export interface RunTableControls {
  /** The terms the operator picked, which the menus edit (not the scope a page forces). */
  q: string;
  onQuery: (q: string) => void;
  sort: RunSort;
  onSort: (sort: RunSort) => void;
  /** The counts of the values of the runs the table lists, for the menus' filters. */
  facets?: Facets | undefined;
}

/** The statuses a run of the read model has: never `pending`. */
const LISTED_STATUSES: readonly RunStatus[] = [
  "active",
  "completed",
  "failed",
  "cancelled",
  "aborted",
];

/** The values of a facet: those counted, else the ones given, without a count. */
function counted(values: readonly FacetValue[] | undefined, all: readonly string[] = []) {
  const found = values ?? [];
  return [
    ...found,
    ...all.filter((v) => !found.some((f) => f.value === v)).map((value) => ({ value, runs: 0 })),
  ];
}

interface HeaderProps {
  title: string;
  /** The orders the column offers. */
  sorts?: readonly RunSort[];
  /** The facets the column filters, to show it filtered (`@` for every label). */
  fields?: readonly string[];
  right?: boolean;
  /** A hint on the header, the column's meaning. */
  hint?: string;
  /** The column, whose width its class sets: one width whatever the rows hold. */
  column: string;
  controls: RunTableControls;
  /** The menu's filters, given a way to close it. */
  children?: (close: () => void) => ReactNode;
}

/** A column's header: its title, the order if it is the column's, its filters' count. */
export function ColumnHeader({
  title,
  sorts = [],
  fields = [],
  right = false,
  hint,
  column,
  controls,
  children,
}: HeaderProps) {
  const [open, setOpen] = useState(false);
  const sorted = sorts.includes(controls.sort) ? controls.sort : null;
  const filters = countTerms(controls.q, fields);
  const described = [
    hint,
    sorted ? `Ordered: ${SORT_WORDS[sorted].toLowerCase()}` : null,
    filters > 0 ? `${filters} filter${filters === 1 ? "" : "s"}` : null,
  ]
    .filter(Boolean)
    .join(" · ");
  const close = () => setOpen(false);
  const Arrow = sorted && ascending(sorted) ? ArrowUp : ArrowDown;
  return (
    <th
      className={right ? `col-${column} right` : `col-${column}`}
      aria-sort={sorted ? (ascending(sorted) ? "ascending" : "descending") : undefined}
    >
      <Popover.Root open={open} onOpenChange={setOpen}>
        <Popover.Trigger
          className={`th-btn${filters > 0 ? " filtered" : ""}${sorted ? " sorted" : ""}`}
          title={described || `Order and filter by ${title.toLowerCase()}`}
        >
          {title}
          {sorted && <Arrow size={12} aria-hidden="true" className="th-arrow" />}
          {filters > 0 && (
            <span className="th-badge" aria-hidden="true">
              {filters}
            </span>
          )}
          <ChevronDown size={12} aria-hidden="true" className="th-chevron" />
        </Popover.Trigger>
        <Popover.Portal>
          <Popover.Positioner
            className="menu-positioner"
            side="bottom"
            align={right ? "end" : "start"}
            sideOffset={4}
            collisionPadding={12}
          >
            <Popover.Popup
              className="menu-popup column-menu"
              aria-label={`${title}: order and filters`}
            >
              {sorts.length > 0 && (
                <MenuSection title="Order">
                  {sorts.map((s) => (
                    <Choice
                      key={s}
                      picked={controls.sort === s}
                      onPick={() => {
                        controls.onSort(controls.sort === s && s !== "started" ? "started" : s);
                        close();
                      }}
                    >
                      {SORT_WORDS[s]}
                    </Choice>
                  ))}
                </MenuSection>
              )}
              {children?.(close)}
            </Popover.Popup>
          </Popover.Positioner>
        </Popover.Portal>
      </Popover.Root>
    </th>
  );
}

function MenuSection({ title, children }: { title: string; children: ReactNode }) {
  return (
    // biome-ignore lint/a11y/useSemanticElements: a group of the menu, named by its title
    <div className="column-menu-section" role="group" aria-label={title}>
      <p className="menu-label">{title}</p>
      {children}
    </div>
  );
}

/** One of several choices of a section: an order, a window, a threshold. */
function Choice({
  picked,
  onPick,
  children,
}: {
  picked: boolean;
  onPick: () => void;
  children: ReactNode;
}) {
  return (
    <button type="button" className="menu-item" aria-pressed={picked} onClick={onPick}>
      <span className="menu-item-icon" aria-hidden="true">
        {picked && <Check size={14} />}
      </span>
      <span className="menu-item-text">{children}</span>
    </button>
  );
}

/** A facet's values with their counts, each a toggle of the query. */
function FacetSection({
  title,
  field,
  values,
  controls,
  show,
  dot,
  search,
}: {
  title: string;
  field: string;
  values: readonly FacetValue[];
  controls: RunTableControls;
  show?: (value: string) => string;
  dot?: boolean;
  search?: ReactNode;
}) {
  const rows = facetRows(controls.q, field, values);
  if (rows.length === 0 && !search) return null;
  const toggle = (f: string, value: string) => controls.onQuery(toggleFacet(controls.q, f, value));
  return (
    <MenuSection title={title}>
      <div className="filter-box column-menu-box">
        {rows.length === 0 ? (
          <p className="column-menu-empty">No value among these runs.</p>
        ) : (
          <FacetToggles
            title={title}
            field={field}
            rows={rows}
            onToggle={toggle}
            show={show}
            dot={dot ?? false}
          />
        )}
        {search}
      </div>
    </MenuSection>
  );
}

/** A filter that takes one value at a time: a threshold, a window. */
function ChoiceSection({
  title,
  field,
  options,
  controls,
  close,
}: {
  title: string;
  field: string;
  /** Each option's term, `null` for no filter. */
  options: readonly { term: string | null; label: string }[];
  controls: RunTableControls;
  close: () => void;
}) {
  const current = askedTerms(controls.q, field)[0]?.raw ?? null;
  return (
    <MenuSection title={title}>
      {options.map((o) => (
        <Choice
          key={o.term ?? "any"}
          picked={o.term === current}
          onPick={() => {
            controls.onQuery(withFieldTerm(controls.q, field, o.term));
            close();
          }}
        >
          {o.label}
          {o.term && <span className="menu-item-term">{o.term}</span>}
        </Choice>
      ))}
    </MenuSection>
  );
}

export function RunHeader({ controls }: { controls: RunTableControls }) {
  const labels = controls.facets?.labels ?? [];
  return (
    <ColumnHeader
      title="Run"
      column="run"
      fields={["@", "id", "workflow", "parent"]}
      controls={controls}
    >
      {() =>
        labels.length === 0 ? (
          <MenuSection title="Labels">
            <p className="column-menu-empty">No label among these runs.</p>
          </MenuSection>
        ) : (
          labels
            .slice(0, 4)
            .map((l) => (
              <FacetSection
                key={l.key}
                title={`@${l.key}`}
                field={`@${l.key}`}
                values={l.values.slice(0, 8)}
                controls={controls}
              />
            ))
        )
      }
    </ColumnHeader>
  );
}

export function StatusHeader({ controls }: { controls: RunTableControls }) {
  return (
    <ColumnHeader
      title="Status"
      column="status"
      fields={["status", "quarantined"]}
      controls={controls}
    >
      {(close) => (
        <>
          <FacetSection
            title="Status"
            field="status"
            values={counted(controls.facets?.statuses, LISTED_STATUSES)}
            controls={controls}
            dot
          />
          <ChoiceSection
            title="Quarantine"
            field="quarantined"
            options={[
              { term: null, label: "Any run" },
              { term: "quarantined:yes", label: "Quarantined only" },
              { term: "quarantined:no", label: "Not quarantined" },
            ]}
            controls={controls}
            close={close}
          />
        </>
      )}
    </ColumnHeader>
  );
}

export function NamespaceHeader({ controls }: { controls: RunTableControls }) {
  const access = useAccess();
  const label = useNamespaceLabel();
  return (
    <ColumnHeader
      title="Namespace"
      column="namespace"
      hint="The end user the run acts for"
      fields={["namespace"]}
      controls={controls}
    >
      {() => (
        <FacetSection
          title="Namespace"
          field="namespace"
          values={controls.facets?.namespaces ?? []}
          controls={controls}
          show={(name) => label(name) ?? name}
          search={
            access.can("namespaces:read") ? (
              <NamespaceSearch
                onPick={(name) => controls.onQuery(toggleFacet(controls.q, "namespace", name))}
              />
            ) : null
          }
        />
      )}
    </ColumnHeader>
  );
}

export function ScriptHeader({ controls }: { controls: RunTableControls }) {
  return (
    <ColumnHeader title="Script" column="script" fields={["script"]} controls={controls}>
      {() => (
        <FacetSection
          title="Script"
          field="script"
          values={controls.facets?.scripts ?? []}
          controls={controls}
          show={shortDigest}
        />
      )}
    </ColumnHeader>
  );
}

export function StartedHeader({ controls }: { controls: RunTableControls }) {
  const timeline = timelineOf(controls.q);
  const pick = (win: RangeKey | null, close: () => void) => {
    controls.onQuery(withTimeline(controls.q, win));
    close();
  };
  return (
    <ColumnHeader
      title="Started"
      column="started"
      sorts={["started", "oldest", "activity"]}
      fields={["started"]}
      controls={controls}
    >
      {(close) => (
        <MenuSection title="Started in">
          <Choice picked={timeline === null} onPick={() => pick(null, close)}>
            Any time
          </Choice>
          {WINDOWS.map((w) => (
            <Choice key={w} picked={timeline === w} onPick={() => pick(w, close)}>
              {WINDOW_WORDS[w].replace(/^the /, "The ")}
            </Choice>
          ))}
        </MenuSection>
      )}
    </ColumnHeader>
  );
}

export function DurationHeader({ controls }: { controls: RunTableControls }) {
  return (
    <ColumnHeader
      title="Duration"
      column="duration"
      right
      hint="From start to end; a run that goes on counts up"
      sorts={["duration"]}
      fields={["duration"]}
      controls={controls}
    >
      {(close) => (
        <ChoiceSection
          title="Ended runs that took"
          field="duration"
          options={[
            { term: null, label: "Any duration" },
            { term: "duration:<1s", label: "Under a second" },
            { term: "duration:>1m", label: "Over a minute" },
            { term: "duration:>10m", label: "Over 10 minutes" },
            { term: "duration:>1h", label: "Over an hour" },
          ]}
          controls={controls}
          close={close}
        />
      )}
    </ColumnHeader>
  );
}

export function CallsHeader({ controls }: { controls: RunTableControls }) {
  return (
    <ColumnHeader
      title="Calls"
      column="calls"
      right
      hint="Tool calls, and those that failed"
      sorts={["calls"]}
      fields={["calls", "errors", "tool"]}
      controls={controls}
    >
      {(close) => (
        <>
          <ChoiceSection
            title="Tool calls"
            field="calls"
            options={[
              { term: null, label: "Any number" },
              { term: "calls:0", label: "None" },
              { term: "calls:>=10", label: "10 or more" },
              { term: "calls:>=100", label: "100 or more" },
            ]}
            controls={controls}
            close={close}
          />
          <ChoiceSection
            title="Failed calls"
            field="errors"
            options={[
              { term: null, label: "Any" },
              { term: "errors:>0", label: "At least one" },
              { term: "errors:0", label: "None" },
            ]}
            controls={controls}
            close={close}
          />
          <FacetSection
            title="Called"
            field="tool"
            values={controls.facets?.tools ?? []}
            controls={controls}
          />
        </>
      )}
    </ColumnHeader>
  );
}

export function WakesHeader({ controls }: { controls: RunTableControls }) {
  return (
    <ColumnHeader
      title="Wakes"
      column="wakes"
      hint="When a waiting run wakes by itself"
      sorts={["wakes"]}
      fields={["waiting", "signal"]}
      controls={controls}
    >
      {() => (
        <FacetSection
          title="Waiting for"
          field="waiting"
          values={controls.facets?.waiting ?? []}
          controls={controls}
        />
      )}
    </ColumnHeader>
  );
}

export function ErrorHeader({ controls }: { controls: RunTableControls }) {
  return (
    <ColumnHeader
      title="Error"
      column="error"
      hint="The error the run ended with: hover it for the cause"
      fields={["error", "fingerprint"]}
      controls={controls}
    >
      {() => (
        <FacetSection
          title="Error"
          field="error"
          values={controls.facets?.errors ?? []}
          controls={controls}
        />
      )}
    </ColumnHeader>
  );
}
