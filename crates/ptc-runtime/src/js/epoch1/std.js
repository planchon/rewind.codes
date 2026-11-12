// The `ptc` module (epoch 1): the standard library of scripts (archi/01-product.md, "Modules
// available to scripts").
import hooks from "ptc:internal";

export const { sleep, signal, webhook, run, ToolError, CancelledError } = hooks;
