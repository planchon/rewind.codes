// A change is made in a dialog over its page rather than inside it: the limits of a namespace or
// their defaults, a role, a member's grant, a new key or invitation, an erasure. The page keeps its
// place; the dialog holds the form and its error.
//
// `FormDialog` is the dialog; `DialogForm`, rendered inside it, the form. What the form holds lives
// in the component that renders `DialogForm`, which the dialog mounts each time it opens: an edit
// starts again from what is saved. A click outside does not close it, so nothing typed is lost to a
// stray click; Escape and Cancel do.
import { Dialog } from "@base-ui/react/dialog";
import { X } from "lucide-react";
import type { FormEvent, ReactNode } from "react";
import { ErrorNote } from "./ErrorNote";

export function FormDialog({
  open,
  onOpenChange,
  wide = false,
  children,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** For a form of many fields: the permissions of a role, six limits. */
  wide?: boolean;
  children: ReactNode;
}) {
  return (
    <Dialog.Root open={open} onOpenChange={onOpenChange} disablePointerDismissal>
      <Dialog.Portal>
        <Dialog.Backdrop className="dialog-backdrop" />
        <Dialog.Popup className={wide ? "dialog form-dialog wide" : "dialog form-dialog"}>
          {children}
        </Dialog.Popup>
      </Dialog.Portal>
    </Dialog.Root>
  );
}

export function DialogForm({
  title,
  meta,
  description,
  submit,
  pending = false,
  pendingLabel = "Saving…",
  disabled = false,
  danger = false,
  error,
  errorTitle,
  onSubmit,
  children,
}: {
  title: string;
  /** Beside the title: a count, a scope. */
  meta?: ReactNode;
  description?: ReactNode;
  submit: ReactNode;
  pending?: boolean;
  pendingLabel?: string;
  disabled?: boolean;
  /** The submit button is red: the change cannot be undone. */
  danger?: boolean;
  error?: unknown;
  errorTitle?: string;
  onSubmit: () => void;
  children: ReactNode;
}) {
  const send = (e: FormEvent) => {
    e.preventDefault();
    onSubmit();
  };
  return (
    <form className="form-dialog-form" aria-label={title} onSubmit={send}>
      <header className="form-dialog-head">
        <Dialog.Title className="dialog-title">{title}</Dialog.Title>
        {meta && <span className="muted">{meta}</span>}
        <Dialog.Close className="icon-btn" aria-label="Close">
          <X size={14} />
        </Dialog.Close>
      </header>
      <div className="form-dialog-body">
        {description && (
          <Dialog.Description className="dialog-text" render={<div />}>
            {description}
          </Dialog.Description>
        )}
        {children}
        {error ? <ErrorNote error={error} title={errorTitle} /> : null}
      </div>
      <footer className="dialog-foot">
        <Dialog.Close className="btn" type="button">
          Cancel
        </Dialog.Close>
        <button
          type="submit"
          className={danger ? "btn danger" : "btn primary"}
          disabled={disabled || pending}
        >
          {pending ? pendingLabel : submit}
        </button>
      </footer>
    </form>
  );
}
