// A payload: its size, digest and preview at once; its value on demand. A value above the API's
// inline limit is not in the entry: it is read with GET /v1/payloads/{digest} when asked for.
// Any payload opens large in a dialog, to be read at the width of the screen.
import { Dialog } from "@base-ui/react/dialog";
import { ChevronDown, ChevronRight, Maximize2, X } from "lucide-react";
import { Fragment, type ReactNode, useMemo, useState } from "react";
import { formatBytes, shortDigest } from "../model/format";
import type { Payload } from "../model/types";
import { usePayloadValue } from "../state/runs";
import { CopyButton, ProblemView } from "./bits";
import { foldable, JsonTree, type Opening, OpeningButtons, useOpening } from "./JsonTree";

/** Values up to this size are shown without a click. */
const SHOW_AT_ONCE = 16 * 1024;
/** Characters rendered before asking to show the rest. */
const RENDER_LIMIT = 200_000;
/** Above this size, JSON is shown without colors. */
const HIGHLIGHT_LIMIT = 100_000;
/** Above this size, a value is shown as text rather than as a tree that folds. */
const TREE_LIMIT = 1_000_000;

const TOKEN =
  /("(?:[^"\\]|\\.)*")(\s*:)?|(-?\d+(?:\.\d+)?(?:[eE][+-]?\d+)?)|\b(true|false|null)\b|([{}[\],])/g;

/** JSON text with its tokens colored, without a frame: in a line or in a block. */
export function JsonTokens({ text }: { text: string }) {
  const nodes = useMemo(() => {
    if (text.length > HIGHLIGHT_LIMIT) return [text];
    const out: ReactNode[] = [];
    let last = 0;
    let n = 0;
    for (const m of text.matchAll(TOKEN)) {
      const at = m.index ?? 0;
      if (at > last) out.push(<Fragment key={n++}>{text.slice(last, at)}</Fragment>);
      const [all, str, colon, num, lit, punct] = m;
      if (str !== undefined) {
        out.push(
          <span key={n++} className={colon ? "j-key" : "j-str"}>
            {str}
          </span>,
        );
        if (colon) out.push(<Fragment key={n++}>{colon}</Fragment>);
      } else if (num !== undefined) {
        out.push(
          <span key={n++} className="j-num">
            {num}
          </span>,
        );
      } else if (lit !== undefined) {
        out.push(
          <span key={n++} className="j-lit">
            {lit}
          </span>,
        );
      } else if (punct !== undefined) {
        out.push(
          <span key={n++} className="j-punct">
            {punct}
          </span>,
        );
      } else {
        out.push(<Fragment key={n++}>{all}</Fragment>);
      }
      last = at + all.length;
    }
    if (last < text.length) out.push(<Fragment key={n++}>{text.slice(last)}</Fragment>);
    return out;
  }, [text]);
  return <>{nodes}</>;
}

export function JsonText({ text }: { text: string }) {
  return (
    <pre>
      <code>
        <JsonTokens text={text} />
      </code>
    </pre>
  );
}

/**
 * A value whole: a tree whose objects and arrays fold, or, above a megabyte of JSON, its text (the
 * first 200 000 characters, the rest on demand).
 */
export function JsonValue({ value, opening }: { value: unknown; opening?: Opening }) {
  const [all, setAll] = useState(false);
  const text = useMemo(() => JSON.stringify(value, null, 2) ?? "undefined", [value]);
  if (text.length <= TREE_LIMIT) {
    return <JsonTree value={value} size={text.length} {...(opening ? { opening } : {})} />;
  }
  const cut = !all && text.length > RENDER_LIMIT;
  return (
    <>
      <JsonText text={cut ? `${text.slice(0, RENDER_LIMIT)}\n…` : text} />
      {cut && (
        <div className="payload-preview">
          <button type="button" className="btn small" onClick={() => setAll(true)}>
            Show the remaining {formatBytes(text.length - RENDER_LIMIT)}
          </button>
        </div>
      )}
    </>
  );
}

function LoadedValue({
  digest,
  runId,
  opening,
}: {
  digest: string;
  runId: string;
  opening: Opening;
}) {
  const value = usePayloadValue(digest, runId, true);
  if (value.isPending) {
    return (
      <div className="payload-preview muted">
        <span>
          <span className="spinner" /> Loading the value…
        </span>
      </div>
    );
  }
  if (value.isError) {
    return (
      <div className="payload-preview">
        <ProblemView error={value.error} title="The value could not be loaded" />
      </div>
    );
  }
  return <JsonValue value={value.data} opening={opening} />;
}

/**
 * A value in a dialog as wide as the screen allows: the payload's, or one already at hand. A value
 * that folds has its buttons to unfold or fold it all; `children` then takes the tree's opening.
 */
export function ValueDialog({
  open,
  onOpenChange,
  name,
  meta,
  copyText,
  folds = false,
  children,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  name: string;
  meta?: ReactNode;
  copyText?: string | null;
  folds?: boolean;
  children: ReactNode | ((opening: Opening) => ReactNode);
}) {
  const { opening, openAll, foldAll } = useOpening();
  return (
    <Dialog.Root open={open} onOpenChange={onOpenChange}>
      <Dialog.Portal>
        <Dialog.Backdrop className="dialog-backdrop" />
        <Dialog.Popup className="dialog value-dialog">
          <header className="value-dialog-head">
            <Dialog.Title className="value-dialog-title mono">{name}</Dialog.Title>
            {meta && <span className="meta">{meta}</span>}
            <span className="end">
              {folds && <OpeningButtons openAll={openAll} foldAll={foldAll} />}
              {copyText && <CopyButton text={copyText} label={`Copy ${name}`} />}
              <Dialog.Close className="icon-btn" aria-label="Close">
                <X size={14} />
              </Dialog.Close>
            </span>
          </header>
          <div className="value-dialog-body">
            {typeof children === "function" ? children(opening) : children}
          </div>
        </Dialog.Popup>
      </Dialog.Portal>
    </Dialog.Root>
  );
}

/** The button that opens a value large. */
export function ExpandButton({ name, onClick }: { name: string; onClick: () => void }) {
  return (
    <button
      type="button"
      className="copy-btn"
      aria-label={`Open ${name} large`}
      title="Open large"
      onClick={(e) => {
        e.stopPropagation();
        onClick();
      }}
    >
      <Maximize2 size={12} />
    </button>
  );
}

/** A JSON value with a button that opens it large, for values already at hand. */
export function JsonBox({ name, value }: { name: string; value: unknown }) {
  const [large, setLarge] = useState(false);
  const text = useMemo(() => JSON.stringify(value, null, 2) ?? "undefined", [value]);
  return (
    <div className="json-box">
      <div className="json-box-tools">
        <ExpandButton name={name} onClick={() => setLarge(true)} />
      </div>
      <JsonValue value={value} />
      <ValueDialog
        open={large}
        onOpenChange={setLarge}
        name={name}
        copyText={text}
        folds={foldable(value)}
      >
        {(opening) => <JsonValue value={value} opening={opening} />}
      </ValueDialog>
    </div>
  );
}

export function PayloadView({
  name,
  payload,
  runId,
  extra,
}: {
  name: string;
  payload: Payload;
  /** The run whose history holds the payload: its value is read through it. */
  runId: string;
  extra?: ReactNode;
}) {
  const inline = payload.value !== undefined;
  const [open, setOpen] = useState(true);
  const [asked, setAsked] = useState(inline && payload.bytes <= SHOW_AT_ONCE);
  const [large, setLarge] = useState(false);
  const copyText = inline ? JSON.stringify(payload.value, null, 2) : null;
  const { opening, openAll, foldAll } = useOpening();
  // A value read on demand may fold: its buttons show once it is asked for.
  const folds = open && asked && (inline ? foldable(payload.value) : true);
  const meta = (
    <span title={payload.digest}>
      {formatBytes(payload.bytes)} · {shortDigest(payload.digest)}
    </span>
  );

  return (
    <section className="payload" aria-label={name}>
      <header className="payload-head">
        <button
          type="button"
          className="icon-btn"
          style={{ width: 18, height: 18 }}
          aria-expanded={open}
          aria-label={open ? `Hide ${name}` : `Show ${name}`}
          onClick={() => setOpen(!open)}
        >
          {open ? <ChevronDown size={13} /> : <ChevronRight size={13} />}
        </button>
        <span className="name">{name}</span>
        <span className="meta">{meta}</span>
        {extra}
        <span className="end">
          {folds && <OpeningButtons openAll={openAll} foldAll={foldAll} />}
          {copyText !== null && <CopyButton text={copyText} label={`Copy ${name}`} />}
          <ExpandButton name={name} onClick={() => setLarge(true)} />
        </span>
      </header>
      <ValueDialog
        open={large}
        onOpenChange={setLarge}
        name={name}
        meta={meta}
        copyText={copyText}
        folds={inline ? foldable(payload.value) : true}
      >
        {(opening) =>
          inline ? (
            <JsonValue value={payload.value} opening={opening} />
          ) : (
            <LoadedValue digest={payload.digest} runId={runId} opening={opening} />
          )
        }
      </ValueDialog>
      {open && (
        <div className="payload-body">
          {asked ? (
            inline ? (
              <JsonValue value={payload.value} opening={opening} />
            ) : (
              <LoadedValue digest={payload.digest} runId={runId} opening={opening} />
            )
          ) : (
            <div className="payload-preview">
              {payload.preview && <code>{payload.preview}</code>}
              <button type="button" className="btn small" onClick={() => setAsked(true)}>
                {inline ? "Show" : "Load"} the value ({formatBytes(payload.bytes)})
              </button>
            </div>
          )}
        </div>
      )}
    </section>
  );
}
