// The old names code written before actions were named still uses, which the runtime still
// resolves (archi/08-decisions.md, R1): the module `rewind:tools`, `rewind`'s `ToolError` and its
// `tool`, and `WorkflowError`, the name `RunError` had. The type-checker gives this file to tsgo next to the code, so that such code still
// checks; nothing else declares, lists or shows them, and new code is written with the new names.

declare module "rewind:tools" {
  export * from "rewind:actions";
}

declare module "rewind" {
  import { ActionError as Renamed, RunError as RunRenamed } from "rewind";
  export const ToolError: typeof Renamed;
  export type ToolError = Renamed;
  export const WorkflowError: typeof RunRenamed;
  export type WorkflowError = RunRenamed;
  interface ActionError {
    /** The name `action` had. */
    readonly tool: string;
  }
}
