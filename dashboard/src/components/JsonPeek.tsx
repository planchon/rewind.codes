// JSON values in Testing: a line, colored, that opens the whole value in a dialog, as a tree that
// folds, with a copy button; and a card for the values a page is about (a test's input and what
// its output must contain), a tree that folds in place.
import { Dialog } from "@base-ui/react/dialog";
import { Maximize2, X } from "lucide-react";
import { type ReactNode, useMemo, useState } from "react";
import { CopyButton } from "./bits";
import { foldable, JsonTree, OpeningButtons, useOpening } from "./JsonTree";
import { JsonTokens, JsonValue } from "./Payload";

/** Characters of a value shown in its line. */
const LINE = 56;

export function JsonDialog({
  title,
  value,
  open,
  onOpenChange,
}: {
  title: string;
  value: unknown;
  open: boolean;
  onOpenChange: (open: boolean) => void;
}) {
  const text = useMemo(() => JSON.stringify(value, null, 2) ?? "undefined", [value]);
  const { opening, openAll, foldAll } = useOpening();
  return (
    <Dialog.Root open={open} onOpenChange={onOpenChange}>
      <Dialog.Portal>
        <Dialog.Backdrop className="dialog-backdrop" />
        <Dialog.Popup className="dialog json-dialog">
          <header className="json-dialog-head">
            <Dialog.Title className="json-dialog-title">{title}</Dialog.Title>
            <span className="end">
              {foldable(value) && <OpeningButtons openAll={openAll} foldAll={foldAll} />}
              <CopyButton text={text} label={`Copy ${title}`} />
              <Dialog.Close className="icon-btn" aria-label="Close">
                <X size={16} />
              </Dialog.Close>
            </span>
          </header>
          <div className="json-dialog-body">
            <JsonValue value={value} opening={opening} />
          </div>
        </Dialog.Popup>
      </Dialog.Portal>
    </Dialog.Root>
  );
}

/** A value in one line, colored and cut; a click opens it whole. */
export function JsonPeek({ value, title }: { value: unknown; title: string }) {
  const [open, setOpen] = useState(false);
  const line = JSON.stringify(value) ?? "undefined";
  const shown = line.length > LINE ? `${line.slice(0, LINE)}…` : line;
  return (
    <>
      <button
        type="button"
        className="json-peek"
        aria-label={`Open ${title}`}
        title={`${title}: open it`}
        onClick={() => setOpen(true)}
      >
        <code>
          <JsonTokens text={shown} />
        </code>
      </button>
      <JsonDialog title={title} value={value} open={open} onOpenChange={setOpen} />
    </>
  );
}

/** A value a page is about, in a card: a tree that folds, its whole a click away in a dialog. */
export function JsonCard({
  title,
  value,
  note,
}: {
  title: string;
  value: unknown;
  /** Said beside the title, such as "the output contains it". */
  note?: ReactNode;
}) {
  const [open, setOpen] = useState(false);
  const text = useMemo(() => JSON.stringify(value, null, 2) ?? "undefined", [value]);
  return (
    <section className="panel json-card" aria-label={title}>
      <header className="panel-head">
        <h2>{title}</h2>
        {note && <span className="muted">{note}</span>}
        <span className="end">
          <CopyButton text={text} label={`Copy ${title}`} />
          <button
            type="button"
            className="icon-btn"
            aria-label={`Open ${title}`}
            title="Open it whole"
            onClick={() => setOpen(true)}
          >
            <Maximize2 size={14} />
          </button>
        </span>
      </header>
      <div className="json-card-body">
        <JsonTree value={value} size={text.length} label={title} />
      </div>
      <JsonDialog title={title} value={value} open={open} onOpenChange={setOpen} />
    </section>
  );
}
