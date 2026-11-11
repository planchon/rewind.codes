// ptc:entry (epoch 1): the module the runtime loads. `ptc:internal` is evaluated first, so the
// hand-over global is gone before any user code runs.
import hooks from "ptc:internal";
import main from "file:///script.ts";

hooks.setMain(main);
