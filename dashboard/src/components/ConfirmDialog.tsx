// A change that cannot be undone, confirmed in a dialog: what it does, and a red button that does
// it. The dialog stays open while the request runs and shows its error; whoever opens it closes it
// once the change is done.
import type { ReactNode } from "react";
import { DialogForm, FormDialog } from "./FormDialog";

export function ConfirmDialog({
  open,
  onOpenChange,
  title,
  description,
  confirm,
  pendingLabel,
  pending = false,
  error,
  onConfirm,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** The question, such as `Revoke Claude?`. */
  title: string;
  /** What happens, and what remains. */
  description: ReactNode;
  /** The red button's label: the action, such as `Revoke`. */
  confirm: string;
  pendingLabel: string;
  pending?: boolean;
  error?: unknown;
  onConfirm: () => void;
}) {
  return (
    <FormDialog open={open} onOpenChange={onOpenChange}>
      <DialogForm
        title={title}
        description={description}
        submit={confirm}
        pending={pending}
        pendingLabel={pendingLabel}
        danger
        error={error}
        onSubmit={onConfirm}
      >
        {null}
      </DialogForm>
    </FormDialog>
  );
}
