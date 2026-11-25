# Rewind

`archi/` is the specification: when the code and it disagree, change the code. `archi/08-decisions.md`
is long: read its index first, then only the sections you need, and record each implementation
choice in the section of the work at hand.

## The development loop

Build time and disk come first: this machine has 8 cores, 16 GiB and a tight disk.

| When | Command | About |
| --- | --- | --- |
| While coding | `cargo check -p <crate> --tests` | 20 s |
| Then | `cargo nextest run -p <crate>` (a filter: `-E 'test(name)'`) | 1 min for the engine |
| A contract of `rewind-core` or `rewind-engine` changed | the same for the crates that use it | |
| Before a commit on main | `scripts/gate.sh` | fmt, clippy, every test, doctests, rustdoc |

- Never run the whole workspace's tests to check one crate, and never `cargo test --workspace`:
  nextest runs the test binaries side by side.
- `scripts/gate.sh` is the only check of the Rust workspace: GitHub Actions runs only the pnpm
  packages. It needs Docker (OrbStack) for the Postgres, MinIO and Collector tests.
- No debug symbols by default. When a backtrace needs line numbers or a debugger is needed, for
  that command only: `CARGO_PROFILE_DEV_DEBUG=line-tables-only`.
- `rewind-server`'s build script builds `dashboard/` with pnpm when its sources changed;
  `REWIND_DASHBOARD_DIST=<dir>` uses a build made elsewhere.
- The pnpm packages (`sdk/typescript`, `dashboard`, `docs`, `examples`) check with `pnpm check`
  in their directory.

## Worktrees

- One worktree per feature. Every step of a feature (contract, engine, API, dashboard, docs,
  review, fixes) works in that worktree, one agent after the other. A new worktree is a cold
  build (about 4 minutes and 15 GB).
- A review that only reads code needs no worktree.
- At most four Rust builds at once on this machine.

## Landing and deploying

- Commits go on `main`, without a pull request, once `scripts/gate.sh` passes.
- The server is deployed from this machine: the `deploy` skill. GitHub Actions deploys the sites
  on Vercel and publishes the SDK on npm.
