import type { Transport } from "../http.js";
import type { RequestOptions } from "../params.js";
import type { ActionSearchResults, ActionSet, CreateActionSet, DeclarationFormat, DeclarationsText } from "../types.js";

/** Action sets: MCP servers, APIs from their OpenAPI documents, harness actions and the `http` action, pinned. */
export class ActionSets {
  constructor(private readonly transport: Transport) {}

  /**
   * Registers and pins an action set: Rewind connects to its MCP servers, lists their tools and pins their schemas,
   * reads its APIs' OpenAPI documents (`apis`: one action per operation, the operations it leaves out in `skipped`),
   * and pins the organization's workflows it names (`workflows: ["triage"]` their latest version, `"triage@v12"` one),
   * which its code imports from `rewind:workflows`. With `base`, the action set is derived from a registered one: its
   * servers, actions and workflows are kept as pinned, and `workflows` adds or pins again; `mcp`, `harness` and `http`
   * must then be empty. Returns the action set with its declarations for the model's prompt. The action set is
   * content-addressed: registering it again returns the same one. Each server and API says in its `auth` how its calls
   * prove who calls: nothing, a signature of the organization's key (`{ type: "rewind" }`), or a secret the run uses
   * (`{ type: "secret", header }`). An action set holds no secret; `secrets` names organization's secrets only to list
   * the tools of an MCP server that needs one, and no run keeps them.
   */
  create(actionSet: CreateActionSet, options: RequestOptions = {}): Promise<ActionSet> {
    return this.transport.send(
      () => this.transport.api.POST("/v1/action-sets", { body: actionSet, signal: options.signal }),
      { retry: true, signal: options.signal },
    );
  }

  /** The declarations of an action set: `dts` (`rewind-actions.d.ts`, the default) or `prompt`. */
  declarations(id: string, options: RequestOptions & { format?: DeclarationFormat } = {}): Promise<DeclarationsText> {
    return this.transport.send(
      () =>
        this.transport.api.GET("/v1/action-sets/{id}/declarations", {
          params: { path: { id }, query: { format: options.format } },
          signal: options.signal,
        }),
      { retry: true, signal: options.signal },
    );
  }

  /** The actions matching a query, best first: for action sets too large for the prompt (`search_actions`). */
  search(id: string, query: string, options: RequestOptions & { limit?: number } = {}): Promise<ActionSearchResults> {
    return this.transport.send(
      () =>
        this.transport.api.GET("/v1/action-sets/{id}/search", {
          params: { path: { id }, query: { query, limit: options.limit } },
          signal: options.signal,
        }),
      { retry: true, signal: options.signal },
    );
  }
}
