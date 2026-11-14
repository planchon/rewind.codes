# Changelog

Every version of `@rewind.codes/sdk`. The package follows [Semantic Versioning](https://semver.org).

## 0.1.0 (2026-11-14)

The first version, for the HTTP API `/v1` of Rewind 0.1.

- The client of the API: toolsets, scripts, runs (signals, cancellations, forks, traces, journals and their event
  stream), tasks, approvals, payloads, webhook subscriptions, tools, API keys and the server's version.
- The host-tool worker, which executes the harness's own tools when scripts call them.
- The agent tools (`run_script`, `get_run`…), with adapters for the Anthropic and OpenAI tool formats.
- `verifyWebhook`, the Standard Webhooks signature check of deliveries.
