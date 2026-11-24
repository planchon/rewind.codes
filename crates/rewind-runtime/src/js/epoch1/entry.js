// rewind:entry (epoch 1): the module the runtime loads. `rewind:internal` is evaluated first, so the
// hand-over global is gone before any user code runs.
import hooks from "rewind:internal";
import main from "file:///workflow.ts";

hooks.setMain(main);
