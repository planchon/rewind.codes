# Dashboard

Rewind's view of every run, all the code executed, and the tools they called, and of who may see
them ([05](../archi/05-testing-and-observability.md#dashboard)). A React single-page application,
served as static files by `rewind-server` (and from `app.rewind.codes`), and a client of the public
API `/v1` and of the sign-in `/auth` only.

| View | Shows |
| --- | --- |
| Overview | The home page, over a window (1 h, 24 h, 7 d, 30 d): runs started (against the previous window), the share that completed, the failed ones, the runs waiting now, durations (p95, p50), quarantined runs; runs by outcome over time; what fails most; failing tools. Every figure, bar and row opens the runs behind it. Polled every 10 s. |
| Observability | The runs, looked at and filtered, in four views of the same query: every run (the runs started over time, then the list, ordered by start, activity or next wake), the active ones, the waiting ones (by tab: signals, approvals, timers, tools; grouped by what they wait for, with the line each is paused on and when it wakes, the soonest first; polled every 10 s) and the failures (failed, cancelled and aborted runs of a window grouped by fingerprint: signature, runs, their series, scripts, first and last seen, the line most of them ended at, top labels, the latest run). One query bar: suggestions as you type, the term the server cannot read underlined. Its sidebar holds the view picker with each view's count, the saved searches (kept in the browser) and the filters: the timeline, then each field's values with their counts, the namespaces among them (a click asks for a value, a second excludes it, a third removes it: a filter is a term of the query). |
| Run | One run, analysed in depth. Its sidebar holds its status and id, its sections, and its details (namespace, workflow and version, the run that started it, detached or not, script, toolset, calls, trace, labels). Its sections: the debugger (the source across the page, with each tool call, timer, signal wait and log next to the line that caused it; the timeline above it; the step and the selection under it; payloads on demand, each a tree whose objects and arrays fold, opened large in a dialog; a time cursor that steps through the history, `?step=`: the marks of each line, hovers that show a call's arguments and result or a variable's value at that step, watches, a playhead on the timeline), the journal, the logs, the LLM trace, and the runs it started, each with how it ended (`triage.start(…)`, `run.signal("approval", …)` and the `rewind:ended:` signals read as the script wrote them). Live through the event stream while the run goes on. "Back" returns to the list of Observability it was opened from. |
| Scripts | Every distinct script, with its runs by outcome, its tools and its labels; one script's source, the suites of the workflow versions it is the script of (`GET /v1/workflows?script=`), and its runs. Searched with the query bar of the runs, which reads labels and one tool here (`@agent:triage tool:github.close_issue`), the parameters of `GET /v1/scripts`, and one namespace (`namespace:alice`), sent as `Rewind-Namespace`: the scripts of one end user, their runs counted and linked within it. |
| Testing | Its own sidebar: the workflows (each with the size of its latest suite), the suite of the version shown (each test with its origin as a glyph: inherited, added, recorded, updated, retired) and the version's details (saved, parent, script, toolset, labels). A workflow's page shows one version, switched in the breadcrumb (`Testing / triage / <version> ▾`, the latest by default, `?version=`): its interface (input, output and signal schemas) and its suite as a table: each test's origin (the parent's test an updated one replaces, and why), its input, what it expects (status, output, error), the calls it checks and its world (rules, mock handlers, cassette, signals); then the tests it retired, with why. Each test has its own page (`/testing/<workflow>/tests/<digest>?version=…`), its breadcrumb switching the version (to the test of the same name, or to the version itself when it has none) and the test, with previous and next: where it stands in the suite and why it changed, its input and expected output, its world, its signals, its expectations and its run (start, seed, and for a recorded test the run, namespace and version it comes from). Every JSON value opens whole, colored, in a dialog, a tree whose objects and arrays fold (one by one, or all from the dialog's header); so does the whole document. A recorded test the caller may not read (403, the runs of its namespace) or one no longer stored (404) is said so in place. Needs `toolsets:read`; a grant limited to labels sees the workflows carrying them. |
| Tools | Latency (p50, p95, p99, max, and on one log axis for every tool), error rate by kind and retries per tool, grouped by namespace (each group folds), over a window; its figures are the Overview's tiles. Searched with the query bar of the runs, which reads `toolset:` (sent to the server), `tool:` and a tool's name (applied in the browser). Not for a grant limited to labels. |
| Namespaces | The end users of the organization this month: the operator's ceiling with the organization's totals, then, folding under it, the default limits and the namespaces sorted by CPU, runs or scripts of the month (the query bar of the runs, which reads the start of a name here and suggests the names, a filter of those at 80 % or more of a limit), each quota a meter, orange from 80 %, red at 100 %, a CPU above its quota full with its share (`160%`); the alerts of the month. Whoever holds `namespaces:write` edits the defaults, in a dialog. Polled every 30 s. |
| Namespace | One end user, over a month: its cumulative CPU time against its quota (where it reached 80 % and 100 %), its use of every limit, its alerts and whether they were sent, its tokens in use, its exceptions to the defaults; links to its runs and its scripts. Edit its limits and erase it (typing its name to confirm), each in a dialog, with `namespaces:write`, revoke its tokens with `tokens:write`. |
| Organization | Opened from the account menu, in its own sidebar: Members (their grant: role and allowed labels; change it, remove a member), Invitations (invite an email with a role and labels, resend, revoke), Roles (built-in and the organization's own, with their permissions; create, edit, delete), API keys (role, labels, creator, dates; create, with the secret shown once; revoke), Connections (the people's MCP clients that open the organization: person, client, access, labels, last use; "Remove from organization" with `members:manage`), Settings (name, default role). Each part is shown to whoever holds its permission; the roles offered are those the caller may give. A grant, an invitation, a role and a new key are written in a dialog. |
| Connect Claude | In the sidebar, below the views: a page read top to bottom in five steps, to use Rewind from Claude (archi/01, "Claude as the harness"). A toolset of the organization (`GET /v1/toolsets`, with a warning for host tools, which Claude reaches only while the worker runs, and a link to create one); the connector's URL (`mcpUrl` of `GET /v1/server`) with the exact steps of claude.ai, Team and Enterprise, Claude Code and other MCP clients; the authorization; the connection seen arriving (`GET /v1/connections` every 2 s while the page is open: a connection new, or used, since it opened); a first message to copy, naming the organization, a toolset and a namespace. Beside them, the good practices and the person's connections. |
| Consent | `#/connect/authorize?request=…`, where `/oauth/authorize` sends the browser: a page of its own, outside the frame, as the sign-in is. "Claude wants to access Rewind", the host it returns to (a warning for a loopback one, and for a client that registered itself), the person's organizations with their role and an access each (Read or Run, only those the person may give; the organization open in the dashboard ticked in advance), "Only what Claude starts (via=claude)", Deny and Allow, then the browser goes back to the client. A request expired or unknown says to start again from Claude. A person signed out signs in and comes back to it; one without organization meets the onboarding first. |
| Connections | Account › Connections, from the account menu (a member's): the MCP clients the person let act for them, each with the organizations it opens, their access and labels, when it was made and last used; what it opens is changed in a dialog, and it is revoked after asking once more. |

The views follow what `GET /v1/whoami` says the caller may do: the run views need `runs:read`,
Testing `toolsets:read`, the namespace views `namespaces:read`, and a member without
them is told so. Connect Claude is open to everyone; connections are a person's, so a key sees
the steps without them. The query (`q`) and
the window follow the reader from the Overview to Observability in the URL. Links of earlier
versions are read as what they meant: `#/runs`, `#/waiting` and `#/failures` open Observability,
their filters (`?status=failed&label=…`) as a query, and the namespace filter they named
(`?ns=bob`) as the term `namespace:bob`.

**The frame** is Geist's, Vercel's design system ([08](../archi/08-decisions.md#taken-for-the-dashboards-redesign-november-2026)):
one sidebar, with the organization picker and Find (`F`: a run by its id, a view, a saved search)
at its top, the views below (Overview, Observability, Scripts, Testing, Tools, Namespaces, then Connect
Claude) and the account at its bottom (the organization, the member's connections, the theme, the
time zone, the server, the sign-out). Observability,
a run and the organization turn the sidebar into their own, under a "back"; each page has a
56 px header with its name in the middle and its actions on the right; what an action changes is
written in a dialog over the page (`components/FormDialog.tsx`), which a click outside does not
close. No namespace is picked: the dashboard reads every namespace of the organization, and sends
`Rewind-Namespace` only for the Scripts of one end user (`namespace:` in their query bar).

## Signing in

| Server | The dashboard |
| --- | --- |
| With WorkOS (`GET /auth/config` answers) | Its own pages: "Continue with GitHub", or an email and the 6-digit code sent to it; then whatever step the server answers (an organization to choose, an email to verify, sign-ups closed and the waitlist). A member without an organization joins one they were invited to or creates one. The top bar has the organization switcher and the user menu. |
| Without WorkOS (404) | An API key, typed by the operator, kept for the tab (`sessionStorage`) or on the device when asked (`localStorage`). A server that needs no key (the embedded one) lets the operator in at once. |

- The session is the cookie `rewind_session`, which only `/auth` reads (`Path=/auth`,
  `SameSite=Strict`, same origin). Every `POST` to `/auth` sends JSON.
- The access token lives in memory only. It is asked for with `POST /auth/session` at load (after
  GitHub's callback, the app lands on `/#/` and asks), a minute before it expires (corrected by the
  server's `Date`), and once when `/v1` answers 401; a second refusal goes back to the sign-in.
- An invitation link (`/?invitation_token=…`) is read at load, kept through the sign-in (GitHub's
  `invitationToken`, the code's `invitationToken`), and dropped from the address once signed in.
- The address a sign-in starts from is where it ends: a code by email signs in on the same page,
  and before leaving for GitHub, whose callback lands on `/#/`, the route in the fragment is kept
  in the tab's `sessionStorage` (`app/pendingRoute.ts`) and put back at the next load, once, within
  15 minutes. Claude's consent (`#/connect/authorize?request=…`) is what this is for.
- The query cache belongs to one key, or one member in one organization: the tree under it
  remounts when that changes (a sign-out, a switch of organization), so nothing read for one is
  shown to the other.

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
pnpm check                                # Biome: lint and format
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
consent at `/#/connect/authorize?request=claude-demo`, which Allow answers by making the connection
at once (the browser then leaves for claude.ai, as it would), and "Claude" uses a new connection 3 s
later.

## The build

`pnpm build` writes `dist/`: `index.html`, `favicon.svg` and `assets/` (scripts, styles and fonts,
named by their content hash). `dist/` is not committed; `rewind-server` embeds it
([07](../archi/07-code-architecture.md#dashboard)).

- Every path in the build is relative, and the routes live in the URL fragment (`#/runs/…`): the
  server can serve the directory under any prefix, with no fallback route.
- The app calls the API at `/v1` and the sign-in at `/auth` on the origin that serves it. On
  Vercel (`vercel.json`), both are rewritten to `https://api.rewind.codes`; the cookies the API sets
  come through the rewrite on `app.rewind.codes`, whose `WORKOS_REDIRECT_URI` is
  `https://app.rewind.codes/auth/callback`.
- Nothing is loaded from elsewhere: the fonts (Geist Sans and Mono, Latin) are in the build. A member's
  picture is the one image from another origin (GitHub's avatar, given by WorkOS), asked for
  without a referrer; initials stand in when it does not load.
- `index.html` should not be cached; the files of `assets/` never change.

## How it uses the API

- Types come from `api/openapi.json` (`openapi-typescript`, committed in `src/generated/api.ts`);
  calls go through `openapi-fetch` (`src/api/client.ts`). The Bearer token goes to `/v1` only.
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
- The namespace views read `GET /v1/namespaces` (paged, `prefix`, `above=0.8`, `order`),
  `GET /v1/namespaces/{name}?month=`, `GET /v1/organization/namespace-defaults` and
  `GET /v1/organization/quota-alerts`, and write `PUT /v1/namespaces/{name}` (its exceptions),
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
  Their types are the generated ones, named in `src/api/connections.ts`, where the calls go
  through `Api.raw`.

## Layout

| Path | Holds |
| --- | --- |
| `src/api/` | The client (`/v1` and `/auth`), the event stream, the data hooks (TanStack Query), the DTO names |
| `src/model/` | Pure code: a run's activities and steps from its journal, the runs it started and a workflow version's suite (`workflows.ts`), callsites and bindings in the source, the query language, the timeline's axis, filters, formats, highlighting, what a caller may do and give, quotas (shares, levels, durations, the month's cumulative CPU), the Claude pages' rules (host tools of a toolset, the first message, a connection arrived, the consent's defaults) |
| `src/views/` | Overview, Observability (`observability/`: its views, filters, waiting runs and failures), Run (`run/`, with the time cursor), Scripts, Script, Testing (`testing/`: the workflows, one workflow at a version, one test), Tools, Namespaces and Namespace (`namespaces/`), Organization (`org/`), Connect Claude, the consent and the person's connections (`claude/`) |
| `src/components/` | Shared pieces: the page's header, statuses, payloads, the source view, the query bar, charts (the cumulative CPU among them), tooltips, refusals explained |
| `src/app/` | Routes, the session (key or member), the gate, the sign-in pages, the onboarding, the frame (`Layout.tsx`: the sidebar and its two panes; `sidebar.tsx`: a page's own sidebar), the organization picker and the account menu, Find, where a run's "back" leads (`runLinks.tsx`), what the caller may do (`access.tsx`), settings (theme, time zone), saved searches |
| `src/mocks/` | The mocked API: a world of fixtures (with the e2e corpus's `triage_workflow` run, and a run that starts runs of workflows), a small read model that reads the query language, a world of identity and access (users, organizations, invitations, the waitlist, one browser's session, roles, grants, keys), the namespaces of the tenant (their limits, use day by day, alerts and tokens) and the header that narrows the reads, the workflows' versions and the stored tests, people's connections (grants of `connection:cn_…`), the requests waiting for a consent and the toolsets, MSW handlers, the Vite plugin of `dev:mock` |
| `src/styles/` | Geist's tokens for light and dark (with the names the views use), the shared styles and the views' |
