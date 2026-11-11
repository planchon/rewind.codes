// The `ptc` module (epoch 1): the standard library of scripts (archi/01-product.md, "Modules
// available to scripts"). This module does not export `webhook`.
import hooks from "ptc:internal";

export const { sleep, signal, run, ToolError, CancelledError } = hooks;
