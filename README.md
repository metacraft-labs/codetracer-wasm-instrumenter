# codetracer-wasm-instrumenter

Bytecode-rewriting WASM instrumentation pipeline for CodeTracer
(Value Origin Tracking, Milestone M27).

The interpreter-based recorders (`codetracer-wasm-recorder/`,
`codetracer-wasmi-recorder/`) cover the **native** case but cannot record a
`.wasm` module running inside an unmodified host runtime (e.g. V8 in a
browser). This crate complements them with a **bytecode-rewriting**
pipeline: `ct instrument <input.wasm>` produces a self-contained
instrumented module that emits CodeTracer events through imported host
functions the page already controls.

See:

- `codetracer-specs/Planned-Features/Value-Origin-Tracking.milestones.org`
  § M27.
- `codetracer-specs/GUI/Debugging-Features/Value-Origin-Tracking.md` §
  14.5.
- `codetracer-specs/Recording-Backends/WASM-Instrumentation-Layer.md`.

## Layout

- `crates/codetracer-wasm-instrumenter/` — reusable Rust library
  (`Pipeline`).
- `crates/codetracer-wasm-stub-host/` — minimal walrus-based
  interpreter that consumes instrumented modules, used by the parity
  test and by the test suite.
- `crates/codetracer-wasm-host-module-framework/` — pluggable
  pass-through host-module factory (replaces hard-coded Stylus
  stubs).
- `crates/ct-instrument-cli/` — thin `ct instrument` CLI wrapper.
- `plugins/` — Vite / Webpack / esbuild / Rollup plugin wrappers
  that shell out to the CLI.
