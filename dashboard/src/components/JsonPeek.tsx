// JSON values in Testing: a line, colored, that opens the whole value in a dialog, colored and
// indented, with a copy button; and a card for the values a page is about (a test's input and
// what its output must contain).
import { Dialog } from "@base-ui/react/dialog";
import { Maximize2, X } from "lucide-react";
import { type ReactNode, useMemo, useState } from "react";
import { CopyButton } from "./bits";
import { JsonText, JsonTokens, JsonValue } from "./Payload";

/** Characters of a value shown in its line. */
const LINE = 56;
/** Lines of a value shown in its card. */
const CARD_LINES = 12;

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
  return (
    <Dialog.Root open={open} onOpenChange={onOpenChange}>
      <Dialog.Portal>
        <Dialog.Backdrop className="dialog-backdrop" />
        <Dialog.Popup className="dialog json-dialog">
          <header className="json-dialog-head">
            <Dialog.Title className="json-dialog-title">{title}</Dialog.Title>
            <span className="end">
              <CopyButton text={text} label={`Copy ${title}`} />
              <Dialog.Close className="icon-btn" aria-label="Close">
                <X size={16} />
              </Dialog.Close>
            </span>
          </header>
          <div className="json-dialog-body">
            <JsonValue value={value} />
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

/** A value a page is about: its first lines in a card, the whole of it a click away. */
export function JsonCard({
  title,
  value,
  note,
}: {
  title: string;
  value: unknown;
  /** Said under the title, such as "contains". */
  note?: ReactNode;
}) {
  const [open, setOpen] = useState(false);
  const text = useMemo(() => JSON.stringify(value, null, 2) ?? "undefined", [value]);
  const lines = text.split("\n");
  const cut = lines.length > CARD_LINES;
  return (
    <section className="json-card" aria-label={title}>
      <header className="json-card-head">
        <h3>{title}</h3>
        {note && <span className="dim">{note}</span>}
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
      <button
        type="button"
        className={`json-card-body${cut ? " cut" : ""}`}
        tabIndex={-1}
        onClick={() => setOpen(true)}
      >
        <JsonText text={cut ? lines.slice(0, CARD_LINES).join("\n") : text} />
      </button>
      <JsonDialog title={title} value={value} open={open} onOpenChange={setOpen} />
    </section>
  );
}
