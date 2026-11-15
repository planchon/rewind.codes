// The `rewind` module (epoch 1): the standard library of scripts (archi/01-product.md, "Modules
// available to scripts").
import hooks from "rewind:internal";

export const { sleep, signal, webhook, run, ToolError, CancelledError, WorkflowError } = hooks;
