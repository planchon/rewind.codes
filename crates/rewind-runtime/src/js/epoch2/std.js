// The `rewind` module (epoch 2): the standard library of workflows (archi/01-product.md, "Modules
// available to workflows").
import hooks from "rewind:internal";

export const { sleep, signal, signalUrl, webhook, run, ActionError, ToolError, CancelledError, RunError, WorkflowError } = hooks;
