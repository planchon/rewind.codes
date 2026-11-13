import type { Transport } from "../http.js";
import type {
  CreateToolset,
  Credentials,
  DeclarationFormat,
  DeclarationsText,
  Toolset,
  ToolSearchResults,
} from "../types.js";

export interface RequestOptions {
  signal?: AbortSignal | undefined;
}

/** Toolsets: MCP servers, host tools and the `http` tool, pinned. */
export class Toolsets {
  constructor(private readonly transport: Transport) {}

  /**
   * Registers and pins a toolset: Rewind connects to its MCP servers, lists their tools and
   * pins their schemas. Returns the toolset with its declarations for the model's prompt. The
   * toolset is content-addressed: registering it again returns the same one.
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

  /**
   * Sets or renews the toolset's credentials. Calls parked by `credentials.expired` are sent
   * again with the same idempotency key.
   */
  async setCredentials(id: string, credentials: Credentials, options: RequestOptions = {}): Promise<void> {
    await this.transport.send(
      () =>
        this.transport.api.PUT("/v1/toolsets/{id}/credentials", {
          params: { path: { id } },
          body: credentials,
          signal: options.signal,
        }),
      { retry: true, signal: options.signal },
    );
  }
}
