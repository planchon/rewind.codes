# Documentation

The documentation of Rewind, for harness developers, served at
[docs.rewind.codes](https://docs.rewind.codes). It is a [Mintlify](https://mintlify.com) site:
MDX pages and `docs.json`. The specification stays in [`archi/`](../archi/README.md); these pages
describe what a user does, and never link to it.

| Path | Holds |
| --- | --- |
| `docs.json` | The site: name, colors, logo, navigation (three tabs: Introduction, SDK, Dashboard) |
| `index.mdx`, `why-rewind.mdx`, `quickstart.mdx`, `concepts/`, `self-hosting.mdx` | What Rewind is, why, and how it works |
| `sdk/` | The TypeScript SDK, what the model writes, the CLI |
| `dashboard/` | The dashboard, view by view |
| `images/dashboard/` | Screenshots of the dashboard, light and dark, made by `scripts/screenshots.ts` |
| `logo/`, `favicon.svg` | The rewind mark, as on the landing |

The code of the pages comes from [`examples/`](../examples/README.md), where each program runs
against a server: change an example there first, then the page that quotes it.

## Commands

Node 22.18 or later and pnpm 10.

```sh
pnpm install
pnpm dev            # the site at http://localhost:3000, reloaded on save
pnpm check          # mint validate (the build) and mint broken-links: what CI runs
pnpm screenshots    # the dashboard's screenshots, from the mocked dashboard (below)
```

## Screenshots

The screenshots are taken from the dashboard running against its mocked API, signed in as Ada in
Acme: no server, no private data. They are lossless WebP, in light and dark; a page shows the one
that matches the reader's theme:

```mdx
<Frame>
  <img src="/images/dashboard/overview-light.webp" alt="…" className="block dark:hidden" />
  <img src="/images/dashboard/overview-dark.webp" alt="…" className="hidden dark:block" />
</Frame>
```

After a change of the dashboard, take them again:

```sh
pnpm --dir ../dashboard dev:mock --port 5199      # in one terminal; restart it before a full run
pnpm screenshots                                  # every shot
pnpm screenshots run-debugger overview            # some
```

The browser is Playwright's Chromium (`pnpm exec playwright install chromium` once). A shot is a
name, how to bring the page to the state to capture, and optionally a height or an element: add
one to `SHOTS` in `scripts/screenshots.ts`.

## Writing

- English, for a developer who builds an agent. Second person, present tense, short sentences.
- Code must run: harness code comes from `examples/`, outputs are copied from a real run, and
  declarations, traces and diagnostics are what a server printed.
- Diagrams are Mermaid (` ```mermaid `), with the ELK layout for flowcharts.
- A new page goes in `docs.json`'s navigation; `pnpm check` fails on a page that is missing or a
  link that breaks.

## Publishing

Mintlify builds and hosts the site from this repository:

1. In the Mintlify dashboard, **Git Settings**: install the GitHub app on `planchon/rewind.codes`,
   branch `main`, and enable **docs.json is in a subdirectory** with the path `/docs`. Every push
   to `main` deploys; a pull request gets a preview.
2. **Custom domain**: `mint add-domain docs.rewind.codes` (after `mint login`), or the dashboard's
   custom domain setup. Add the two `TXT` records it prints to the DNS of `rewind.codes`, wait for
   both to be verified, then add `CNAME docs → cname.mintlify.builders`.
3. Confirm the deploy on the Mintlify dashboard's Publishes page, and that
   [docs.rewind.codes](https://docs.rewind.codes) loads.
