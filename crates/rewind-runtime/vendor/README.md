# Vendored JavaScript

The JavaScript of the two Deno crates whose globals the sandbox installs (`archi/08-decisions.md`,
"Taken while building the runtime"), copied byte for byte from the crates the workspace pins:

| Directory | Crate | License |
| --- | --- | --- |
| `deno_web/` | `deno_web` 0.291.0 | MIT, Copyright 2018-2026 the Deno authors |
| `deno_webidl/` | `deno_webidl` 0.260.0 | MIT, Copyright 2018-2026 the Deno authors |

deno_core records an extension's sources by their path on the build machine and reads them when
a V8 snapshot is built. rewind-runtime builds its snapshots at run time, on any machine, so it gives
the snapshot build these copies instead (`src/vendored.rs`).

The copies must be the files of the pinned crates: `cargo test -p rewind-runtime vendored` fails when
one differs or is missing. After a change of either pinned version (a change of the determinism
epoch), copy them again with:

```sh
REWIND_RUNTIME_VENDOR=1 cargo test -p rewind-runtime vendored
```
