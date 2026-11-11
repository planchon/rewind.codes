// The `ptc` module (epoch 1): the standard library of scripts (archi/01-product.md, "Modules
// available to scripts"). `webhook` is designed but not implemented yet.
import hooks from "ptc:internal";

export const { sleep, signal, run, ToolError, CancelledError } = hooks;
