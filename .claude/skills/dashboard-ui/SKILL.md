---
name: dashboard-ui
description: The owner's design rules for Rewind's dashboard and the before/after screenshot review every UI change goes through. Use whenever you change, add or review anything a person sees in dashboard/ (views, components, styles, the mocks behind a page), and whenever the owner gives feedback on the UI.
---

# Rewind dashboard: design rules and UI review

Every rule below comes from the owner's reviews of November 2026 (decisions and reasons in
[archi/08-decisions.md](../../../archi/08-decisions.md)). Most were said about one page and hold
for all of them. They are written so that the owner never has to say them again: a change that
breaks one is not done, and a change that must break one asks the owner first.

**New feedback.** When the owner comments on the UI, fix it on every page where the pattern
appears, then add the rule behind it to [Rules](#rules) below, said for every page, and the
decision to `archi/08-decisions.md`.

## Review workflow

Every change of the UI is reviewed with screenshots taken **before** and **after** it, by the
same shot list, and shown as proof. Tests passing is not the UI being right.

1. **Scope.** List the views the change touches, every other view with the same pattern (search
   with `rg`: the class, the component, the wording), and the states to look at
   ([checklist](#checklist)). Write them as a shot list (`shots.json`, format in
   [scripts/shoot.mjs](scripts/shoot.mjs), example in [shots.example.json](shots.example.json)).
2. **Start the mock UI**, from `dashboard/`:
   `REWIND_MOCK_WORKOS=0 pnpm dev:mock --port 5391 --strictPort` (in the background). Its Vite
   plugin loads `src/mocks/` once: restart it after changing the mocks.
3. **Before.** Before editing anything, shoot the list into a review folder outside the
   repository (your scratchpad, or `$TMPDIR/rewind-ui-review/<date>-<slug>/`):
   `node .claude/skills/dashboard-ui/scripts/shoot.mjs shots.json <review>/before`.
   If the code is already changed, shoot the base instead: `git worktree add` the base commit
   elsewhere, run its mock UI on another port (`"base"` in the shot list), shoot it, remove the
   worktree.
4. **Change**, following the [rules](#rules); fix the same pattern everywhere it appears.
5. **After.** Shoot the same list into `<review>/after`, then compare:
   `node .claude/skills/dashboard-ui/scripts/compare.mjs <review>/before <review>/after`
   writes `<review>/compare.html` and lists each pair, changed or not (a pair can differ by the
   mock's clock alone: relative times move).
6. **Review the pairs yourself**, as the owner would, beyond the item asked: every
   [rule](#rules), every [checklist](#checklist) item, the 4× corner zooms. Anything wrong:
   fix it, shoot again. A page error printed by `shoot.mjs` is a failure.
7. **Checks.** In `dashboard/`: `pnpm check`, `pnpm typecheck`, `pnpm test`; with an API change,
   `pnpm generate:api` and the API's tests.
8. **Report with the proof**: for each view changed, the before and the after side by side
   (Markdown images with absolute paths), the path of `compare.html`, the checklist with what
   was looked at, and the other views where the same pattern was fixed. Say what you could not
   check.

Never commit the screenshots.

## Checklist

For every view in the shot list:

- [ ] 1280 × 800 with the sidebar open, then narrower (1100) and wider (1600).
- [ ] Light and dark themes.
- [ ] Scrolled: the table's header stuck under the page's, its corners zoomed 4× (`corners` in
      the shot list): one continuous curve, the top line the same color as the card's sides.
- [ ] A table scrolled sideways: its header still follows the page; the edge shadows.
- [ ] Every menu, popover and dialog the change touches, open.
- [ ] A row hovered; the longest values (a 40-character origin, a long title, a long label, a
      long error, a namespace id): nothing cut, nothing overlapping.
- [ ] Empty, loading and error states; a live update arriving.
- [ ] A classic scrollbar (headless Chromium hides it: emulate it or reason about it): the
      paddings still symmetric.
- [ ] The same pattern on every other view: fixed too.

## Rules

### Working on the dashboard

- **Fix the rule, not the instance.** A remark about one page is a remark about every page.
- **Vercel's dashboard is the reference** (Geist: its grays, materials, type and motion). In
  doubt, do what Vercel does.
- **Add nothing the owner did not ask for.** Every element earns its place: the owner removed
  saved searches, the LLM trace, the connection dot, the run's status, id and "live" in its
  sidebar, its status in its header. A feature that seems useful is proposed, not built.

### Frame and navigation

- **The sidebar navigates; it holds no action.** A page's actions are in its header (a run's
  controls are a menu beside "Forks"). No status, id, "live" mark or connection dot in it.
- An entry that opens its own sidebar (Observability, Testing, a run, Organization) has the "›"
  chevron.
- The sidebar folds by a click on its right edge (a drag resizes it): no fold button.
- Paddings are symmetric: the same space left and right of what a sidebar or a card holds. A
  scrollbar that takes room on one side breaks it: the sidebar's panes scroll without one.
- **The page header holds the title (and its crumbs) and the actions, nothing else.** No status
  beside a run's handle; mock and quarantine tags only, when they apply.

### Text

- **No explanatory paragraph, no metadata line under a title** ("A namespace is…", "Created … ·
  active …", "Calls completed from … to …"): what a page or a section is goes in the card of an
  (i) beside its title (`PageHeader info`, `InfoCard`), shown on hover, focus or click.
- **No counts of what the screen shows** ("50 runs shown"), no toolbar band to hold them, no
  empty-state hint that explains a feature ("Save a query to find it here").
- One type scale (`tokens.css`): 13 px body and code, 14 px controls and card titles, 12 px
  labels and metadata, 11 px axis ticks only; weights 400, 500, 600, never 700; figures 20 to
  24 px.

### Names, never ids

- **Never show a UUID or a digest as a name.** A script shows its title (a person's, else its
  top comment's, else its workflow's name, else "Untitled script"); a run its handle
  `namespace/origin/suffix` (`RunHandle`); a namespace its display name. Ids appear only on
  hover, in a copy button, or in an (i) card.
- Every reference to a run carries its handle: when the API gives a run's UUID only, add the
  handle to the API rather than show the id (as `FailureSample.handle`).
- What a person names, they can rename from the UI (a script's title and description, a
  namespace's display name).

### Tables

- **No cell is cut.** No ellipsis, no value shortened in its middle, no column left out to fit.
  Each column has a least width and grows to what its rows hold; names, handles (their namespace
  whole: `RunHandle whole`), labels and numbers stay on one line; prose (an error's message)
  wraps, with a least width so its column never shrinks to a letter; a JSON value shows whole
  and, past 400 characters, says how many more its dialog holds. Labels show whole up to a
  budget, then "+N" opens them all.
- **A table wider than its page scrolls sideways** (`TableScroll`), a thin shadow on the edge
  past which columns hide. Never a fade over the text.
- **The header follows the page** under the page's header, whether the table scrolls sideways
  or not, and keeps the card's ring and rounded corners when stuck, in exactly the color of the
  card's sides (`--card-ring-solid`, `.table-corner`).
- A table is one card: no toolbar band above it, no card in it. "N new · Show" floats over its
  top edge and takes no room. Nothing on screen moves on its own: answers swap in place, rows
  keyed by run, a changed search keeps its previous rows dimmed.
- A header's menu orders and filters its column; its filters are terms of the query.

### Menus, popovers and dialogs

- **One flat list**: groups split by a stroke, no section titles ("Order", "Quarantine"), no
  card inside the menu's card, tight paddings.
- Each item reads on its own ("No call", "A call failed", "Started in the last hour"); no "Any"
  item: picking the chosen item again removes it.
- **In a long list, the search comes first**, then the list it narrows (a namespace filter).
- What changes a setting opens in a dialog over its page; the page keeps its layout.

### Spacing, sizes and alignment

- Tight: no doubled gap (an empty or zero-height element in a `gap` column doubles it), no empty
  band, nothing between a search and the table it filters beyond the page's gap.
- **Controls breathe**: a segmented control has 4 px around its choice and 12 px each side of a
  label. Controls side by side share a height (36 px: the query bar, a segmented control).
- Buttons of one row share one size and one icon size, centered on one line (a value's fold,
  copy and open buttons: 24 px, 28 px in a dialog).
- A label in a sidebar is one line, never wider than the sidebar; its title says it whole.

### Color

- **Red only for what goes wrong**; gray for counts and neutral series (a secret's reads); green
  says "completed" only; a cancellation is gray, never a failure.
- One stroke color per material: a card's ring is the same on every side and in every state.

### Order of content

- **What needs attention comes first**: alerts above the overview and the table (Namespaces, a
  namespace's page), shown only when there are some.

### Motion

- Vercel's: a segmented control slides its choice's background to the new choice, a lighter one
  follows the pointer; nothing moves under `prefers-reduced-motion`.
- Charts and the timeline label only what can be read: no tick inside a break, room left before
  a break's label, a break in its largest unit.

## Known gaps

What still breaks a rule, to fix when a change touches these views:

- Explanatory text under titles: the Scripts page ("Every distinct source executed…"), Connect
  Claude's intro, a workflow's page ("Each passed its suite when it was saved…"), the count line
  of Failures ("2 runs, 2 signatures, ended in the last 24 hours").
- A run of a workflow shows its workflow as a tag beside its handle, whose middle already says it.
- The documentation's screenshots (`docs/images/dashboard/`) predate these rules.

## Where the rules come from

| Review | The owner's remarks |
| --- | --- |
| Redesign (Nov 2026) | Geist's look; one sidebar that becomes the page's own; times relative, exact on hover; green says "completed" only |
| Polish (17 Nov) | The source spans the page; cards clip to their corners and never sit in a scroller; settings in dialogs |
| Type scale (18 Nov) | One scale; no 700; a cancellation told as one; labels whole then "+N"; the error's name over its message |
| Live dashboard (22 Nov) | Nothing moves on its own; "N new · Show" |
| Sidebar and run page (22 Nov) | The table cropped; no connection dot; controls in a menu beside Forks, not in the sidebar; no status, id or "live" in the run's sidebar; labels cut in the sidebar; misaligned viewer buttons ("the same everywhere"); no LLM trace; intros in an (i) card; Vercel's animation on 1h/24h/7d/30d; search first in the namespace filter; fold the sidebar by its edge |
| Tables and menus (22 Nov) | Header menus padded, titled, a card in a card; no sticky header; still cropped; why red?; spacing; remove the namespace's "Created…" and the tools' "Calls completed…" lines; sidebar paddings not equal |
| Names (22 Nov) | No "50 runs shown"; the card's top border missing; always the script's title, never its id; titles and descriptions for scripts; readable ids `ns/workflow/random` with no collision; a chevron on Testing |
| Stuck header (22 Nov) | The radius kept when stuck; the corner's border; nothing readable in the timeline |
| Details (23 Nov) | Padding of the 24h choice; no saved searches; no status in the run's header; alerts before the overview |
| Tables again (23 Nov) | The corner and top border a different color when stuck; no ellipsis in a table: least widths and sideways scrolling |
| This skill (23 Nov) | "I don't want to give the same feedback all the time, on small details": the rules in a skill, and a before/after review with screenshots as proof for every UI change |
