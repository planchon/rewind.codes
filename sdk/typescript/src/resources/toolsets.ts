import type { Transport } from "../http.js";
import type { CreateToolset, DeclarationFormat, DeclarationsText, Toolset, ToolSearchResults } from "../types.js";

export interface RequestOptions {
  signal?: AbortSignal | undefined;
}

/** Toolsets: MCP servers, host tools and the `http` tool, pinned. */
export class Toolsets {
  constructor(private readonly transport: Transport) {}

  /**
   * Registers and pins a toolset: Rewind connects to its MCP servers, lists their tools and
   * pins their schemas, and pins the workflows it names (`workflows: ["triage"]` the latest
   * version, `"triage@<version>"` one), which its scripts import from `rewind:workflows`. With
   * `base`, the toolset is derived from a registered one: its servers, tools and workflows are
   * kept as pinned, and `workflows` adds or pins again; `mcp`, `host` and `http` must then be
   * empty. Returns the toolset with its declarations for the model's prompt. The toolset is
   * content-addressed: registering it again returns the same one.
   */
  create(toolset: CreateToolset, options: RequestOptions = {}): Promise<Toolset> {
    return this.transport.send(
      () => this.transport.api.POST("/v1/toolsets", { body: toolset, signal: options.signal }),
      { retry: true, signal: options.signal },
    );
  }

  /** The declarations of a toolset: `dts` (`rewind-tools.d.ts`, the default) or `prompt`. */
  declarations(id: string, options: RequestOptions & { format?: DeclarationFormat } = {}): Promise<DeclarationsText> {
    return this.transport.send(
      () =>
        this.transport.api.GET("/v1/toolsets/{id}/declarations", {
          params: { path: { id }, query: { format: options.format } },
          signal: options.signal,
        }),
      { retry: true, signal: options.signal },
    );
  }

  /** The tools matching a query, best first: for toolsets too large for the prompt. */
  search(id: string, query: string, options: RequestOptions & { limit?: number } = {}): Promise<ToolSearchResults> {
    return this.transport.send(
      () =>
        this.transport.api.GET("/v1/toolsets/{id}/search", {
          params: { path: { id }, query: { query, limit: options.limit } },
          signal: options.signal,
        }),
      { retry: true, signal: options.signal },
    );
  }
}
