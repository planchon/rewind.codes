# Dashboard

Rewind's view of every run, all the code executed, and the tools they called, and of who may see
them ([05](../archi/05-testing-and-observability.md#dashboard)). A React single-page application,
served as static files by `rewind-server` (and from `app.rewind.codes`), and a client of the public
API `/v1` and of the sign-in `/auth` only. Every change of its UI follows the owner's design rules
and their before/after review with screenshots: the skill
[`.claude/skills/dashboard-ui`](../.claude/skills/dashboard-ui/SKILL.md).

| View | Shows |
| --- | --- |
| Overview | The home page, over a window (1 h, 24 h, 7 d, 30 d): runs started (against the previous window), the share that completed, the failed ones, the runs waiting now, durations (p95, p50), quarantined runs; runs by outcome over time; what fails most; failing tools. Every figure, bar and row opens the runs behind it. Its figures are those of the query's namespace, or of the organization. Live: its figures change in place within 10 s of a run's change. Not for a grant limited to labels, who lands on Observability. |
| Observability | The runs, looked at and filtered, in four views of the same query: every run (the runs started over time, then the list, ordered and filtered from its headers: each a menu with the orders of its column (newest or oldest first, latest activity, longest, most tool calls, soonest to wake, in the URL as `sort`) and its filters, terms of the query (statuses and quarantine, namespaces, scripts, a window of start, a duration, a number of calls, failed calls and the tools called, what a run waits for, errors, labels); each run's labels whole, those that fit, the others behind "+N", and how it ended, the error's name over its message, each with a card on hover, focus or tap: every label, or the end's cause, line, time and the run), the active ones, the waiting ones (by tab: signals, approvals, timers, tools; grouped by what they wait for, with the line each is paused on and when it wakes, the soonest first; live) and the failures (failed, cancelled and aborted runs of a window grouped by fingerprint: signature, runs, their series, scripts, first and last seen, the line most of them ended at, top labels, the latest run). One query bar: suggestions as you type, the term the server cannot read underlined. Its sidebar holds the view picker with each view's count, the saved searches (kept in the browser) and the filters: the timeline, then each field's values with their counts, the namespaces among them (a click asks for a value, a second excludes it, a third removes it: a filter is a term of the query). |
| Run | One run, analysed in depth. At its top, how it ended: a cancellation says why (its deadline and TTL, a request and its message, or its script) and what the run awaited then, with the line; a failure what threw and where; each with its error, the name apart from the message. Its sidebar holds its status and id, its sections, and its details (namespace, workflow and version, the run that started it, detached or not, script, toolset, calls, trace, labels, the secrets it attached by server, names only). Its sections: the debugger (the source across the page, with each tool call, timer, signal wait and log next to the line that caused it; the timeline above it; the step and the selection under it; payloads on demand, each a tree whose objects and arrays fold, opened large in a dialog; a time cursor that steps through the history, `?step=`: the marks of each line, hovers that show a call's arguments and result or a variable's value at that step, watches, a playhead on the timeline), the journal, the logs, the LLM trace, and the runs it started, each with how it ended (`triage.start(…)`, `run.signal("approval", …)` and the `rewind:ended:` signals read as the script wrote them). Live through the event stream while the run goes on. "Back" returns to the list of Observability it was opened from. |
| Scripts | Every distinct script of a window (1 h, 24 h, 7 d, 30 d), with its runs by outcome and its tools, from the hourly counters; one script's source, the suites of the workflow versions it is the script of (`GET /v1/workflows?script=`), and its runs (the table of Observability forced to the script). Searched with the query bar of the runs, which reads one tool here (`tool:github.close_issue`), the parameter of `GET /v1/scripts`, and one namespace (`namespace:alice`), sent as `Rewind-Namespace`: the scripts of one end user, their runs counted and linked within it; a label is underlined, since counters know none. Not for a grant limited to labels. |
| Testing | Its own sidebar: the workflows (each with the size of its latest suite), the suite of the version shown (each test with its origin as a glyph: inherited, added, recorded, updated, retired) and the version's details (saved, parent, script, toolset, labels). A workflow's page shows one version, switched in the breadcrumb (`Testing / triage / <version> ▾`, the latest by default, `?version=`): its interface (input, output and signal schemas) and its suite as a table: each test's origin (the parent's test an updated one replaces, and why), its input, what it expects (status, output, error), the calls it checks and its world (rules, mock handlers, cassette, signals); then the tests it retired, with why. Each test has its own page (`/testing/<workflow>/tests/<digest>?version=…`), its breadcrumb switching the version (to the test of the same name, or to the version itself when it has none) and the test, with previous and next: where it stands in the suite and why it changed, its input and expected output, its world, its signals, its expectations and its run (start, seed, and for a recorded test the run, namespace and version it comes from). Every JSON value opens whole, colored, in a dialog, a tree whose objects and arrays fold (one by one, or all from the dialog's header); so does the whole document. A recorded test the caller may not read (403, the runs of its namespace) or one no longer stored (404) is said so in place. Needs `toolsets:read`; a grant limited to labels sees the workflows carrying them. |
| Tools | Latency (p50, p95, p99, max, and on one log axis for every tool), error rate by kind and retries per tool, grouped by namespace (each group folds), over a window; its figures are the Overview's tiles. Searched with the query bar of the runs, which reads `namespace:` and one `tool:github.*` (sent to the server as the scope and `prefix`), `tool:` and a tool's name (applied in the browser). Not for a grant limited to labels. |
| Namespaces | The end users of the organization this month: the operator's ceiling with the organization's totals, then, folding under it, the default limits and the namespaces sorted by CPU, runs or scripts of the month (the query bar of the runs, which reads the start of a name or of a display name here and suggests the names, a filter of those at 80 % or more of a limit), each quota a meter, orange from 80 %, red at 100 %, a CPU above its quota full with its share (`160%`); the alerts of the month. Whoever holds `namespaces:write` edits the defaults, in a dialog. Read again within 10 s of a run's change, and when the change stream says a namespace or the defaults changed. |
| Namespace | One end user, titled by its display name (its id on hover and under the title), over a month: how its runs went (started, the share completed, failed, waiting now, duration p95 and p50, from `GET /v1/runs/stats?q=namespace:…`; the runs of each UTC day by outcome; what fails most; the tools its runs call), its cumulative CPU time against its quota (where it reached 80 % and 100 %), its use of every limit, its alerts and whether they were sent, its tokens in use, its exceptions to the defaults; then its runs of the month (the table of Observability forced to the namespace and the month, shown as locked chips; the filters its headers add are chips one removes, in the page's URL) and the scripts its runs ran (that of Scripts, with `Rewind-Namespace`), each linking to its full view; then its secrets, as Secrets lists the organization's (`secrets:read`), written and deleted with `secrets:write` by a grant not limited to labels. Rename it (`PATCH /v1/namespaces/{name}`, emptied to remove the name), edit its limits and erase it (typing its id to confirm), each in a dialog, with `namespaces:write`; revoke its tokens with `tokens:write`. |
| Secrets | The organization's secrets (`GET /v1/secrets`), for whoever holds `secrets:read`: each one's name and reference (`org/zendesk`), its labels, who wrote it last and when, its reads per day over 30 days and in all, its last read (when, the run, the namespace), and what attaches it now (`GET /v1/secrets/{name}/usage`: the active runs, those waiting on a call it serves, in a card; the workflows whose latest version attaches it, linked to Testing). With `secrets:write`: a new secret or a new value, in a dialog, the value typed in a password field, sent once and never shown nor read back, the labels kept (a grant limited to labels gives its own); a deletion, in a dialog that first reads the usage and says what it would stop (the active runs, those waiting, the workflows and the servers they attach it for), with a red Delete. A grant limited to labels sees only the secrets carrying them. |
| Organization | Opened from the account menu, in its own sidebar: Members (their grant: role and allowed labels; change it, remove a member), Invitations (invite an email with a role and labels, resend, revoke), Roles (built-in and the organization's own, with their permissions; create, edit, delete), API keys (role, labels, creator, dates; create, with the secret shown once; revoke), Signing keys (`secrets:read`: the issuer and the address of the public keys to copy, each key's id, when it was made and until when a retired one stays published; rotate the key with `organization:write`, confirmed in a dialog), Connections (the people's MCP clients that open the organization: person, client, access, labels, last use; "Remove from organization" with `members:manage`), Settings (name, default role). Each part is shown to whoever holds its permission; the roles offered are those the caller may give. A grant, an invitation, a role and a new key are written in a dialog, and every removal or revocation is confirmed in one, with a red button. |
| Connect Claude | In the account menu, beside Connections: a page read top to bottom in five steps, to use Rewind from Claude (archi/01, "Claude as the harness"). A toolset of the organization (`GET /v1/toolsets`, with a warning for host tools, which Claude reaches only while the worker runs, and a link to create one); the connector's URL (`mcpUrl` of `GET /v1/server`) with the exact steps of claude.ai, Team and Enterprise, Claude Code and other MCP clients; the authorization; the connection seen arriving (`GET /v1/connections` read again on each `connection` event of the change stream: a connection new, or used, since the page opened); a first message to copy, naming the organization, a toolset and a namespace. Beside them, the good practices and the person's connections. |
| Consent | `/connect/authorize?request=…`, where `/oauth/authorize` sends the browser: a page of its own, outside the frame, as the sign-in is. "Claude wants to access Rewind", the host it returns to (a warning for a loopback one, and for a client that registered itself), the person's organizations with their role and an access each (Read or Run, only those the person may give; the organization open in the dashboard ticked in advance), "Only what Claude starts (via=claude)", Deny and Allow, then the browser goes back to the client. A request expired or unknown says to start again from Claude. A person signed out signs in and comes back to it; one without organization meets the onboarding first. |
| Connections | Account › Connections, from the account menu (a member's): the MCP clients the person let act for them, each with the organizations it opens, their access and labels, when it was made and last used; what it opens is changed in a dialog, and it is revoked after asking once more, in a dialog with a red Revoke. |

The views follow what `GET /v1/whoami` says the caller may do: the run views need `runs:read`,
Testing `toolsets:read`, the namespace views `namespaces:read`, Secrets `secrets:read` (its writes
`secrets:write`), and a member without them is told so. The statistics come from counters, which
know no labels: a grant limited to labels sees neither the Overview, Scripts nor Tools, nor the
counts and failure groups of Observability, and no view asks for them. Connect Claude is open to everyone; connections are a person's, so a key sees
the steps without them. The query (`q`) and
the window follow the reader from the Overview to Observability in the URL. Links of earlier
versions are read as what they meant: a route in the fragment (`/#/runs/abc?step=3`) as the path
it names (`/runs/abc?step=3`), before the router reads the address; `/runs`, `/waiting` and
`/failures` open Observability, their filters (`?status=failed&label=…`) as a query, and the
namespace filter they named (`?ns=bob`) as the term `namespace:bob`.

A namespace is shown by its display name wherever one is shown (the lists, a run's details, the
filters and suggestions of the query bar, a recorded test's origin), its id on hover
(`components/NamespaceName.tsx`); the display names come from `GET
/v1/organization/namespace-names`, read once and invalidated with every namespace write. Links,
the query (`namespace:`) and `Rewind-Namespace` keep the id.

**The frame** is Geist's, Vercel's design system ([08](../archi/08-decisions.md#taken-for-the-dashboards-redesign-november-2026)):
one sidebar, with the organization picker and Find (`F`: a run by its handle or its id, a view, a query)
at its top, the views below (Overview, Observability, Scripts, Testing, Tools, Namespaces, Secrets, then Connect
Claude) and the account at its bottom (the organization, the member's connections, the theme, the
time zone, the server, the sign-out). Observability,
a run and the organization turn the sidebar into their own, under a "back"; each page has a
56 px header with its name in the middle and its actions on the right; what an action changes is
written in a dialog over the page (`components/FormDialog.tsx`), which a click outside does not
close. No namespace is picked: the dashboard reads every namespace of the organization, and sends
`Rewind-Namespace` only for the Scripts of one end user (`namespace:` in their query bar).

**The type** is one scale of tokens ([08](../archi/08-decisions.md#taken-for-the-dashboards-type-scale-november-2026)):
16 px for a page's section, 14 px for a card's title (600) and the controls (500), 13 px for the
body and the code, 12 px for labels, metadata and chips, 11 px for axis ticks and keys; no 700,
no half pixel. A cancellation's mark is gray, a failure's red.

## Signing in

| Server | The dashboard |
| --- | --- |
| With WorkOS (`GET /auth/config` answers) | Its own pages: "Continue with GitHub", or an email and the 6-digit code sent to it; then whatever step the server answers (an organization to choose, an email to verify, sign-ups closed and the waitlist). A member without an organization joins one they were invited to or creates one. The top bar has the organization switcher and the user menu. |
| Without WorkOS (404) | An API key, typed by the operator, kept for the tab (`sessionStorage`) or on the device when asked (`localStorage`). A server that needs no key (the embedded one) lets the operator in at once. |

- The session is the cookie `rewind_session`, which only `/auth` reads (`Path=/auth`,
  `SameSite=Strict`, same origin). Every `POST` to `/auth` sends JSON.
- The access token lives in memory only. It is asked for with `POST /auth/session` at load (after
  GitHub's callback, the app lands on `/` and asks), a minute before it expires (corrected by the
  server's `Date`), and once when `/v1` answers 401; a second refusal goes back to the sign-in.
- An invitation link (`/?invitation_token=…`) is read at load, kept through the sign-in (GitHub's
  `invitationToken`, the code's `invitationToken`), and dropped from the address once signed in.
- The address a sign-in starts from is where it ends: a code by email signs in on the same page,
  and before leaving for GitHub, whose callback lands on `/`, the route (path and query) is kept
  in the tab's `sessionStorage` (`app/pendingRoute.ts`) and put back at the next load, once, within
  15 minutes, with what the callback added (`?auth_error=…`). Claude's consent
  (`/connect/authorize?request=…`) is what this is for.
- The query cache and the state's collections belong to one key, or one member in one
  organization: the tree under them remounts when that changes (a sign-out, a switch of
  organization), so nothing read for one is shown to the other, and the change stream is opened
  again for the new organization. A new access token opens it again with that token.

## Commands

Node 22.12 or later and pnpm 10.

```sh
pnpm install
pnpm dev:mock                             # the app against the mocked API of src/mocks, signing in as with WorkOS
REWIND_MOCK_KEY=rewind_sk_x pnpm dev:mock # a server without WorkOS, which asks for that key
REWIND_MOCK_WORKOS=0 pnpm dev:mock        # a server that needs no key, as the embedded one
REWIND_URL=http://localhost:8080 pnpm dev # the app against a rewind server; /v1 and /auth are proxied to it
pnpm test                                 # vitest, against the mocked API
pnpm typecheck                            # TypeScript 7, strict
pnpm check                                # Biome: lint and format (biome.jsonc: the layers' rule)
pnpm build                                # type-check, then build to dist/
pnpm generate:api                         # regenerate src/generated/api.ts from ../api/openapi.json
```

`pnpm dev:mock` prints, in its terminal, the accounts of its world, the codes it "emails" and the
invitation links it "sends": Ada (`ada@example.net`) owns Acme and reads in Globex, Grace is
`harness` in Acme, Linus has a role without `runs:read`, Alan verifies his email first and reads
only `agent:triage`, Margaret has no organization and an invitation to Globex, any other email
meets the waitlist. "Continue with GitHub" signs in as Ada. A run goes on live, and the access
tokens live 5 minutes, as WorkOS's: the event stream is closed at their expiry and resumed. Each
run acts for an end user: of the tenant's 1,200 or so namespaces, bob is above his CPU quota and
at his active runs, chen pays for more (exceptions), dana and user_7Kd02 passed 80 %, erin is
blocked. The workflow triage has three versions, whose suites inherit, add, update and retire
tests with reasons; its latest adds two tests recorded from runs, one of them from a namespace no
caller may read (its `GET /v1/tests/{digest}` answers 403). Ada connected Claude (Acme with Run,
Globex with Read) and Claude Code (Acme), Grace Claude (Acme); a request of claude.ai waits for a
consent at `/connect/authorize?request=claude-demo`, which Allow answers by making the connection
at once (the browser then leaves for claude.ai, as it would), and "Claude" uses a new connection 3 s
later.

## The build

`pnpm build` writes `dist/`: `index.html`, `favicon.svg` and `assets/` (scripts, styles and fonts,
named by their content hash). `dist/` is not committed; `rewind-server` embeds it
([07](../archi/07-code-architecture.md#dashboard)).

- The build is served at the root of its origin (`base: "/"`), and its routes are paths
  (`/runs/…`, `/namespaces/alice`): `rewind-server` and Vercel (`vercel.json`) answer `index.html`
  for a route that is not a file, as the dev server does.
- The app calls the API at `/v1` and the sign-in at `/auth` on the origin that serves it. On
  Vercel (`vercel.json`), both are rewritten to `https://api.rewind.codes`; the cookies the API sets
  come through the rewrite on `app.rewind.codes`, whose `WORKOS_REDIRECT_URI` is
  `https://app.rewind.codes/auth/callback`.
- Nothing is loaded from elsewhere: the fonts (Geist Sans and Mono, Latin) are in the build. A member's
  picture is the one image from another origin (GitHub's avatar, given by WorkOS), asked for
  without a referrer; initials stand in when it does not load.
- `index.html` should not be cached; the files of `assets/` never change.

## How it uses the API

- Types come from `api/openapi.json` (`openapi-typescript`, committed in `src/generated/api.ts`,
  named in `src/model/types.ts`); calls go through `openapi-fetch` (`src/sources/client.ts`). The
  Bearer token goes to `/v1` only.
- Layers: `views/`, `app/` and `components/` read state through the hooks of `src/state/<domain>.ts`
  and call its actions; they never see a URL, a query key or a refresh interval. Only `state/`
  calls the API, through `sources/`. Biome's `noRestrictedImports` refuses `sources/`, `api/` and
  `@tanstack/*` there (`biome.jsonc`).
- The state lives in TanStack DB collections (`@tanstack/db`, `@tanstack/react-db`,
  `@tanstack/query-db-collection`, pinned to exact versions): `runs`, each run's summary once, by
  id; a search's hits (which runs it returned, in which order, a query collection per search); the
  aggregates (statistics, failure groups, tools' statistics: a query collection each); a run's
  details (a query collection); a run's journal entries (appended by its event stream); the lists
  of scripts, namespaces and workflows (a query collection each, every row shown read again
  together). A table of runs is a search's hits joined to `runs` by id; the query `q` is read by
  the server only. A write whose answer is known (a namespace's name, limits, tokens, erasure, the
  defaults) is shown at once in a TanStack DB transaction, kept until what it changed is read
  again, and undone if the server refuses it.
- **Live.** Each tab follows the organization through `GET /v1/changes` (server-sent events, read
  with `fetch`): a `run` event writes the run's summary into `runs`, and every row showing it
  changes at once, without a request; the searches on screen are read again a second after the last
  change (at the latest 5 s after the first of a burst), their counts, charts and failure groups at
  most every 10 s, a run's details when it changes. A `script`, `workflow`, `secret`, `namespace` or
  `connection` event reads again the views that show it (`state/live.ts`, `state/queries.ts`); a
  `resync`, or a stream opened again, reads once what is on screen. Nothing is read on a timer:
  while the stream is down, the views keep what they show. The stream opens with the first view
  that follows the organization, again with each new access token, and after a drop (1 s, doubling
  up to 30 s). Its state is the dot beside the organization picker: green while live, amber while
  it reconnects. The lint refuses `refetchInterval` and `setInterval` in `src/` but the mocked API
  (a Biome plugin, `lint/no-polling.grit`); a clock that reads no store (`useNow`) says why.
- **Statistics** come from hourly counters (archi/05, "Statistics"): `state/aggregates.ts` sends
  the statistics routes only the query's one `namespace:` term (`model/scope.ts`), the window, and
  buckets of whole hours; the query's other terms narrow the runs a mark opens. The runs waiting now
  are the gauge of the active runs, by kind.
- **Nothing moves on its own.** After a view's first answer, nothing replaces it with a loading
  state: a refresh, a reconnection or a resync swaps the data in place, and a search the operator
  changed keeps the previous rows, dimmed, until its answer. An answer equal to what is shown
  renders nothing: rows keep their objects, and are keyed by run. No cell is cut: each column of a
  table is at least its width and as wide as what its rows hold (prose wraps), and a table wider
  than its page scrolls sideways, a thin shadow on the edge past which columns hide; its header
  follows the page under the page's header either way (a scroll-driven animation,
  `TableScroll.tsx`). A column shown for a search stays while the search does. An answer that
  adds, drops or reorders rows applies at once while the table is scrolled to its top; otherwise it
  waits behind "N new · Show" (or "Updated · Show"), in the slot the table's toolbar always has,
  until a click, a new search or order, a return to the page, or an answer that arrives with the
  table at its top; a run that no longer matches stays until then, with its new status. A read
  after "Load more" asks for as many runs as are shown, and "Load more" reads them and a page more.
- **The run's controls**, a "Controls" menu of its header beside "Forks" (the sidebar holds no
  action), for whoever holds `runs:control`, while the run has not ended: "Send <name>" for each signal it waits for, "Send a signal" for any name (for a run of a
  workflow, the signals its version declares, each payload started from its JSON Schema), and
  "Cancel run" after a confirmation that names the run. One `Idempotency-Key` per opening of a
  form; the server's refusal (422, 409, 403) shows under the payload. A signal shows "sent" until
  its `signal_received` arrives on the run's event stream; a cancellation, "Cancelling…" until the
  run's summary arrives cancelled; each beside the menu.
- The event stream is read with `fetch` and `eventsource-parser`, since `EventSource` cannot send the
  `Authorization` header; a stream that closes before the run ends (a drop, a proxy, an access
  token's expiry) resumes from the next entry (`from`) with the current token.
- A payload shows its size, digest and preview; its value is shown on demand, and read with
  `GET /v1/payloads/{digest}` when the entry left it out (above 256 KiB).
- Menus, popovers and dialogs are Base UI's (`@base-ui/react`), unstyled and styled with the
  tokens. The run's page, a script's, the workflows', the namespaces' and the organization's are
  loaded when first opened.
- Lists search with `q` (`GET /v1/runs`, `/v1/runs/stats`, `/v1/runs/failures`); the server
  reads the query and answers `invalid-query` with the position of the term at fault, in
  characters. Suggestions and facets come from `GET /v1/runs/stats?q=…&top=…`; after
  `namespace:`, the names that start with what is typed come from `GET /v1/namespaces?prefix=…`
  too, for whoever holds `namespaces:read`.
- Scripts, Tools and Namespaces search with the same query bar, which reads the part of the
  language their endpoint understands (`model/filters.ts`): the terms become the endpoint's
  parameters, and a term the list cannot read is underlined in the browser, as the server's are.
  The parameters of older links (`?label=`, `?tool=`, `?toolset=`, `?prefix=`) are read as the
  query they mean.
- The namespace views read `GET /v1/namespaces` (paged, `prefix`, which matches a name or a
  display name, `above=0.8`, `order`), `GET /v1/namespaces/{name}?month=`,
  `GET /v1/organization/namespace-defaults`, `GET /v1/organization/namespace-names` and
  `GET /v1/organization/quota-alerts`, and write `PUT /v1/namespaces/{name}` (its exceptions),
  `PATCH /v1/namespaces/{name}` (its display name),
  `PUT /v1/organization/namespace-defaults`, `DELETE /v1/namespaces/{name}/tokens` and
  `DELETE /v1/namespaces/{name}`. Durations are read and written as the API writes them (`2h`,
  `1h30m`).
- The workflow views read `GET /v1/workflows` (paged with `after`), `GET /v1/workflows/{name}`
  (the versions, newest first) and, when a test is opened, `GET /v1/tests/{digest}`, which never
  changes. Whether a test was inherited or added is found against the parent's suite when the
  parent is among the workflow's versions; `updated` and `retired` come with the version. A test
  of a suite is shown as recorded from its name (`recorded <run id>`) until its document, whose
  `origin` says so, is read.
- The organization views write: grants, invitations, roles, keys, the organization. Before the
  server enforces them, they follow its rules: only the roles whose permissions the caller holds
  are offered, a role editor's permissions the caller lacks are disabled, a member or role beyond
  the caller cannot be changed, and a grant limited to labels administers nothing. A refusal is
  explained where it happened (`last-owner`, `role-in-use`, `forbidden`).
- The time cursor is computed in the browser from the journal: a step is a trigger
  (`run_started`, a tool's result or failure, `timer_fired`, `signal_received`,
  `cancel_requested`) and the entries committed with it. What a hover shows is found with
  Lezer's syntax tree: a callsite points at its callee's last name, which leads to the call and
  to the declaration that binds its awaited result. Only values that crossed the journal are
  known; the dashboard runs no code.
- Charts are small SVG components in the dashboard's tokens; no chart library.
- The Claude pages read `GET /v1/connections`, `GET /v1/organization/connections`,
  `GET /v1/oauth/consent?request=…` and `GET /v1/toolsets`, and write `PATCH` and `DELETE
  /v1/connections/{id}`, `DELETE /v1/organization/connections/{id}` and `POST /v1/oauth/consent`.
  Their types are the generated ones, named in `src/model/types.ts`; their calls are the client's.

## Layout

| Path | Holds |
| --- | --- |
| `src/sources/` | How the server is reached: the client (`/v1` and `/auth`, `client.ts`), a run's event stream (`runEvents.ts`) and the organization's change stream (`changes.ts`). A new way for the server to push changes changes this folder only |
| `src/state/` | The state the views read (archi/07, "Dashboard": layers): the TanStack DB collections over the TanStack Query client of the identity (`store.tsx`), what is on screen and when it is read again (`live.ts`), and the hooks and actions of each domain, imported from their own file, with no index: `runs.ts` (searches of runs, a run's details, journal, entries, payloads, trace), `runActions.ts` (signal, cancel, a workflow's declared signals), `aggregates.ts` (statistics, failure groups, tools' statistics), `scripts.ts` (a script, the list of scripts), `namespaces.ts` (display names, suggestions, the list, a namespace, the defaults, the alerts, and their writes), `workflows.ts` (the workflows, a workflow's versions, the tests), `paged.ts` (a list read page after page), `list.ts` (a list the server answers whole, by key, its writes shown at once where their outcome is known), `actions.ts` (a write, then what it changed read again; shown at once where its answer is known), `session.tsx` (the session, key or member: the sign-in's steps, the token and its renewal, the organization switch, who calls; a state per identity), `organization.ts` (members, invitations, roles, keys, settings, and their writes), `connections.ts` (a person's connections, the organization's, the consent), `toolsets.ts`, `connection.ts` (the change stream's state), `errors.ts`. The only folder that imports `sources/` and `@tanstack/*` (but `@tanstack/react-virtual`, a view's) |
| `src/api/` | Phase 2: the former TanStack Query hooks, no longer imported; they go, with the session's re-export in `src/app/session.tsx` |
| `src/model/` | The DTO names of the API (`types.ts`), and pure code: a run's activities and steps from its journal, the runs it started and a workflow version's suite (`workflows.ts`), callsites and bindings in the source, the query language, the timeline's axis, filters, formats, highlighting, what a caller may do and give, quotas (shares, levels, durations, the month's cumulative CPU), the Claude pages' rules (host tools of a toolset, the first message, a connection arrived, the consent's defaults) |
| `src/views/` | Overview, Observability (`observability/`: its views, filters, waiting runs and failures), Run (`run/`, with the time cursor), Scripts, Script, Testing (`testing/`: the workflows, one workflow at a version, one test), Tools, Namespaces and Namespace (`namespaces/`), Organization (`org/`), Connect Claude, the consent and the person's connections (`claude/`) |
| `src/components/` | Shared pieces: the page's header, statuses, payloads, the source view, the query bar, charts (the cumulative CPU among them), tooltips, refusals explained |
| `src/app/` | Routes, the gate, the sign-in pages, the onboarding, the frame (`Layout.tsx`: the sidebar and its two panes; `sidebar.tsx`: a page's own sidebar), the organization picker and the account menu, Find, where a run's "back" leads (`runLinks.tsx`), what the caller may do (`access.tsx`), settings (theme, time zone) |
| `src/mocks/` | The mocked API: a world of fixtures (with the e2e corpus's `triage_workflow` run, and a run that starts runs of workflows), a small read model that reads the query language, a world of identity and access (users, organizations, invitations, the waitlist, one browser's session, roles, grants, keys), the namespaces of the tenant (their limits, use day by day, alerts and tokens) and the header that narrows the reads, the workflows' versions and the stored tests, people's connections (grants of `connection:cn_…`), the requests waiting for a consent and the toolsets, MSW handlers, the Vite plugin of `dev:mock` |
| `src/styles/` | Geist's tokens for light and dark (with the names the views use), the shared styles and the views' |
