// A JSON value as a tree, colored like the rest of the dashboard's JSON: each object and array
// folds on its own (a click on its arrow or on its folded `…`, the arrow keys once one has the
// focus), and the whole tree opens or folds at once. A value opens whole up to 100 000 characters
// of JSON; a larger one opens its first two levels (an array and its items), deeper ones folded.
import { ChevronsDownUp, ChevronsUpDown } from "lucide-react";
import { useCallback, useMemo, useState } from "react";
import { JsonView, type Props } from "react-json-view-lite";

/** Above this many characters of JSON, a value opens its first two levels only. */
const OPEN_WHOLE = 100_000;

const STYLE: NonNullable<Props["style"]> = {
  container: "json-tree",
  basicChildStyle: "json-tree-node",
  childFieldsContainer: "json-tree-children",
  label: "j-key",
  clickableLabel: "j-key",
  nullValue: "j-lit",
  undefinedValue: "j-lit",
  numberValue: "j-num",
  stringValue: "j-str",
  booleanValue: "j-lit",
  otherValue: "j-str",
  punctuation: "j-punct",
  expandIcon: "json-tree-toggle folded",
  collapseIcon: "json-tree-toggle open",
  collapsedContent: "json-tree-folded",
  quotesForFieldNames: true,
  noQuotesForStringValues: false,
  stringifyStringValues: true,
  ariaLables: { collapseJson: "Fold", expandJson: "Unfold" },
};

/** Whether a value has something to fold: an object or an array that is not empty. */
export function foldable(value: unknown): boolean {
  return typeof value === "object" && value !== null && Object.keys(value).length > 0;
}

/** How a tree opens: as it first does, every node open, or only its first level. */
export type Opening = { mode: "first" | "all" | "top"; at: number };

/**
 * The opening of a tree and its two buttons' actions. Each action makes a new opening, so the
 * tree applies it again over the nodes folded or unfolded by hand since.
 */
export function useOpening() {
  const [opening, setOpening] = useState<Opening>({ mode: "first", at: 0 });
  const openAll = useCallback(() => setOpening((o) => ({ mode: "all", at: o.at + 1 })), []);
  const foldAll = useCallback(() => setOpening((o) => ({ mode: "top", at: o.at + 1 })), []);
  return { opening, openAll, foldAll };
}

/** "Unfold all" and "Fold all", for a header. */
export function OpeningButtons({ openAll, foldAll }: { openAll: () => void; foldAll: () => void }) {
  return (
    <>
      <button
        type="button"
        className="icon-btn"
        aria-label="Unfold all"
        title="Unfold all"
        onClick={openAll}
      >
        <ChevronsUpDown size={14} />
      </button>
      <button
        type="button"
        className="icon-btn"
        aria-label="Fold all"
        title="Fold all but the first level"
        onClick={foldAll}
      >
        <ChevronsDownUp size={14} />
      </button>
    </>
  );
}

export function JsonTree({
  value,
  size,
  opening = { mode: "first", at: 0 },
  label = "JSON value",
}: {
  value: unknown;
  /** Its JSON's length, in characters, when the caller has it. */
  size?: number;
  opening?: Opening;
  label?: string;
}) {
  const length = size ?? (JSON.stringify(value) ?? "").length;
  // The levels open: every one, the first two of a large value at first, the first when folded.
  const depth =
    opening.mode === "all" || (opening.mode === "first" && length <= OPEN_WHOLE)
      ? Number.POSITIVE_INFINITY
      : opening.mode === "first"
        ? 2
        : 1;
  // A new function each time the opening changes: the tree applies it to every node again.
  // biome-ignore lint/correctness/useExhaustiveDependencies: `opening.at` asks for it again.
  const expand = useMemo(() => (level: number) => level < depth, [depth, opening.at]);
  return (
    <div className="json-tree-wrap">
      <JsonView
        // A value that is not an object (a string, a number) renders as itself.
        data={value as object}
        style={STYLE}
        shouldExpandNode={expand}
        aria-label={label}
      />
    </div>
  );
}
