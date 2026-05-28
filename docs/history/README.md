# Phase plan archive

These are the **work plans** for each phase of the TypeScript-to-Rust migration
(phase 0 spike → phase 3 retirement of the TS surface). They are kept here as
a record of *what was intended at the time* — not as documentation of how the
project works today. For current state, see `docs/architecture.md` and
`docs/operations.md`.

Each plan was written before its phase started, accepted by the user, and then
executed across one or more commits. The plans were not updated after the work
landed, so they may reference interim names (e.g. `cdp-spike`, `ai-browser`)
and intermediate trade-offs that were later revisited.

## Timeline

| Phase | Plan | Outcome |
|---|---|---|
| 0 | [rust-chromium-spike-plan.md](rust-chromium-spike-plan.md) | Standalone Rust crate driving Chromium over CDP. Validated that a single Rust binary could replace the TS daemon. |
| 1a | [spike-phase-1a-text-semantics-plan.md](spike-phase-1a-text-semantics-plan.md) | `get-text` semantics + `data-testid` aliasing parity with the TS surface. |
| 1b | [spike-phase-1b-accessibility-plan.md](spike-phase-1b-accessibility-plan.md) | `--role` / `--name` accessibility-tree queries via CDP `Accessibility` domain. |
| 1c | [spike-phase-1c-query-all-plan.md](spike-phase-1c-query-all-plan.md) | `query-all` — multi-element text extraction returning a JSON array. |
| 1d | [spike-phase-1d-find-all-meta-plan.md](spike-phase-1d-find-all-meta-plan.md) | `find-all` — element metadata array (selector / role / rect / classes). |
| 2 | [spike-phase-2-daemon-plan.md](spike-phase-2-daemon-plan.md) | Rust daemon over Unix domain socket with newline-delimited JSON-RPC, byte-compatible with the TS protocol. |
| 2b | [spike-phase-2b-driver-actions-plan.md](spike-phase-2b-driver-actions-plan.md) | `click` / `type` / `wait-selector` / `wait-url` added to the daemon driver. |
| 3a-3i | (no individual plan files) | Tier 1–5 daemon actions, secrets vault, supervisor, release pipeline, TS retirement. See git log between commits `c5b33c9` and `a641de3`. |

## Why keep them?

- The "why" behind some current design choices (e.g. why the reader task cannot
  call `dispatch()`, why `coerce` keeps integers as `i64`, why we use 3-event
  network stitching) is captured in the plan that introduced them.
- Future migrations can reference the same incremental shape: small phases,
  parity-tested, with a separate plan per non-trivial chunk.

## Anything to update here?

No. Treat these as immutable historical records. If a plan contains something
wrong about the current code, fix the code or update `architecture.md` /
`operations.md` — do not edit the historical plan.
