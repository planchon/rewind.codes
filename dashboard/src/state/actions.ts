// What the views change on the server, beside the runs (`runActions.ts`): a write, then what it
// changed read again, with whether it is on its way and the server's refusal. A write whose answer
// is known before the server gives it shows at once: the change is shown over what was read, in a
// TanStack DB transaction kept while the server takes it and what it changed is read again, and
// undone if the server refuses it.
import { createTransaction } from "@tanstack/db";
import { useMutation } from "@tanstack/react-query";
import { useCallback } from "react";

export interface Action<I, R = void> {
  /** Sends it; answers whether the server took it. `then` gets the server's answer, once taken. */
  run: (input: I, then?: (answer: R) => void) => Promise<boolean>;
  /** On its way. */
  pending: boolean;
  /** The server's refusal of the last one sent (an `ApiError`, with its problem); else `null`. */
  error: unknown;
  /** The server's answer to the last one sent, once it took it. */
  data: R | undefined;
  /** Forgets the last one sent: its refusal, its answer. */
  reset: () => void;
}

/** An action of a view: each view that shows it has its own pending state and refusal. */
export function useAction<I, R = void>(write: (input: I) => Promise<R>): Action<I, R> {
  const mutation = useMutation({ mutationFn: write });
  const { mutateAsync, reset } = mutation;
  const run = useCallback(
    (input: I, then?: (answer: R) => void) =>
      mutateAsync(input).then(
        (answer) => {
          then?.(answer);
          return true;
        },
        () => false,
      ),
    [mutateAsync],
  );
  return { run, pending: mutation.isPending, error: mutation.error, data: mutation.data, reset };
}

export interface Optimistic<R> {
  /** Changes what is on screen (`Resource.change`), at once. */
  show: () => void;
  /** Undoes what `show` changed outside the resources (a query's data), on a refusal. */
  undo?: () => void;
  write: () => Promise<R>;
  /** Reads again what the write changed, once the server took it. */
  reread: () => Promise<void>;
}

/**
 * A write whose answer is known: shown at once, sent, then what it changed read again, after which
 * what was shown gives way to what was read. A refusal undoes what was shown, and is thrown.
 */
export async function optimistic<R>({ show, undo, write, reread }: Optimistic<R>): Promise<R> {
  const shown = createTransaction({ autoCommit: false, mutationFn: async () => {} });
  shown.mutate(show);
  let answer: R;
  try {
    answer = await write();
  } catch (error) {
    try {
      shown.rollback();
    } finally {
      undo?.();
    }
    throw error;
  }
  try {
    await reread();
  } finally {
    await shown.commit().catch(() => undefined);
  }
  return answer;
}
