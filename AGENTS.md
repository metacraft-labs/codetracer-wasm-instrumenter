# codetracer-wasm-instrumenter — agent instructions

Bytecode-rewriting WASM instrumentation pipeline for CodeTracer (Value Origin
Tracking, milestone M27). `ct instrument <input.wasm>` rewrites a `.wasm`
module so that it reports execution through imported host functions the
embedding page already controls — which is how a module running inside an
*unmodified* host runtime (V8 in a browser, a Stylus host, …) can be recorded
at all. The interpreter-based recorders (`codetracer-wasm-recorder`,
`codetracer-wasmi-recorder`) cover only the native case.

## Dev environment

The repo is self-contained: a Nix flake supplies the whole toolchain and
`direnv` enters it automatically.

```bash
direnv allow            # first time only
nix develop             # or just `cd` in with direnv active
```

`flake.nix` provides:

- `devShells.default` — a `fenix` stable Rust toolchain (cargo, rustc, clippy,
  rustfmt, rust-src) **including the `wasm32-unknown-unknown` rust-std**, plus
  `just`, `nixfmt`, `prek`, `nodejs_22`, `git` and `pkg-config`.
- `packages.default` — the `ct-instrument` CLI, built with
  `rustPlatform.buildRustPackage` off the committed `Cargo.lock`.
- `checks.pre-commit-check` — the `git-hooks.nix` hook set (`just lint`).

Do **not** rely on a sibling repo's dev shell. The flake here is the only
declaration of this repo's toolchain, and CI enters it directly. If you find
yourself needing a tool that is not in `flake.nix`, add it to `flake.nix`.

## Commands

| Command                        | What it does                                                     |
| ------------------------------ | ---------------------------------------------------------------- |
| `just build`                   | `cargo build --workspace --release --locked`                     |
| `just test`                    | `cargo test --workspace --locked` — the whole suite (72 tests)    |
| `just test-plugins`            | `node --test` over the bundler plugin wrappers in `plugins/`      |
| `just test-runtime`            | `node --test` over the shims in `recorder-runtime/`               |
| `just lint`                    | `cargo clippy … -D warnings`, `cargo fmt --check`, `nixfmt --check` |
| `just format` (alias `fmt`)    | `cargo fmt --all` + `nixfmt`                                     |
| `just build-wasm-target-check` | Prove the shell really carries `wasm32-unknown-unknown` rust-std  |
| `just nix-build`               | `nix build .#default`                                            |

`repro.nim` expresses the same build/test graph natively for `reprobuild`
(`repro build`, `repro test`); keep it in sync with the Justfile — see
`codetracer-specs/Repo-Requirements.md` §2.8.1 (`just` ↔ `repro`
equivalence).

## Layout

- `crates/codetracer-wasm-instrumenter/` — the reusable library (`Pipeline`).
  - `lib.rs` — the rewriting pipeline entry points.
  - `hooks.rs` — the injected `__ct_emit_*` host-import signatures.
  - `dwarf.rs` — DWARF line-table / declaration recovery for source
    attribution.
  - `manifest.rs` — the sidecar `ModuleManifest` (`paths` / `functions` /
    `sites` / `boundaries`), deliberately identical in shape to the JS
    instrumenter's manifest so both are consumed by one decoder in
    `codetracer/src/backend-manager/src/browser_stream_host.rs`. The
    `boundaries` table is the M35 addition: each import/export edge's
    parameter and result types, so the replayer can decode the flat
    `__ct_emit_<t>(slot, value)` stream without re-parsing the `.wasm`.
  - `config.rs` — instrumentation configuration.
- `crates/codetracer-wasm-stub-host/` — a minimal walrus-based interpreter that
  consumes instrumented modules; drives the parity test.
  - `runtime.rs` — a *real* embedder (`wasmi`) that executes a module and
    records the values its hooks carried. Boundary-value behaviour can only
    be checked by running: a static walk sees `local.get 0`, never the
    number it pushed. The same entry point runs the un-instrumented module,
    which is how "instrumented computes identically to original" is
    asserted rather than assumed.
  - `v8.rs` + `v8_host.mjs` — a *second* real embedder, this one the host
    V8 driven through `node`, returning the same `RuntimeRecording`. It
    exists because `wasmi` 0.31 cannot enable the exceptions proposal at
    all, which left every `-fwasm-exceptions` module untestable until
    M35c. It is also the closer analogue of the deployment target, since
    spec §1's model is an instrumented module inside an unmodified
    browser V8.
- `crates/codetracer-wasm-host-module-framework/` — pluggable pass-through
  host-module factory (replaces hard-coded Stylus stubs).
- `crates/ct-instrument-cli/` — the thin `ct instrument` CLI (`ct-instrument`
  binary).
- `plugins/` — Vite / Webpack / esbuild / Rollup wrappers that shell out to the
  CLI.
- `recorder-runtime/` — the browser-side runtime shim.

The four members resolve against each other through in-repo `path` deps only.
There is **no** cross-repo cargo `path` dependency, so the toolchain floor is
just Rust — no Nim, capnp or zstd as in the trace-format-consuming recorders.

## Testing notes

`crates/codetracer-wasm-stub-host/tests/parity.rs` reads golden `.wasm`
fixtures from the sibling checkout
`../codetracer-wasm-recorder/cmd/wazero/testdata/recorder-golden/`. In this
workspace the sibling is present, so all four parity tests execute their real
`assert_parity` assertions. The test defines a source-level skip arm for
shallow clones that lack the sibling — **never** widen that arm to make a
failure go away, and never add `#[ignore]`. If a parity test fails, the
instrumented module and the interpreter recorder genuinely disagree.

Because the sibling fixtures are not visible inside the Nix sandbox,
`packages.default` sets `doCheck = false`; the suite is run from the dev shell
via `just test`.

**A `BoundarySignature` is not a description of a function's wasm type.**
When a signature mentions `externref`, `funcref` or `v128` it carries *empty*
`params`/`results` and sets `unrepresentable` — a deliberate design (a partial
tuple would shift every slot index after the gap). The rejection pass that
turns that into a hard error runs **only when `capture_boundary_values` is on**,
so with `PipelineConfig::capture_boundary_values = false` such a module is
instrumented rather than refused, and any rewrite that needs the function's
real types must read them from `module.types`, never from the signature. This
already bit once: the branch-exit rewrite typed its inner block from
`sig.results` and emitted a module that dropped the results on the floor and
failed validation in the embedder. Guarded by
`a_shape_only_unrepresentable_boundary_may_also_exit_by_branch` in
`tests/boundary_values.rs`.

**Exception-handling and GC sequences are walked, and `wasmi` cannot test
them.** `collect_block_ids` (in the instrumenter) and `nested_sequences` (in
the stub host's static walks) descend into `try_table` bodies, legacy `try`
bodies and legacy catch handlers as well as `block` / `loop` / `if`-`else`;
`visit_branch_targets` covers `br_on_null` / `br_on_non_null` / `br_on_cast` /
`br_on_cast_fail` and `try_table` catch-clause labels alongside the three MVP
branch forms. The two lists differ on purpose: a `try_table` catch clause names
an *enclosing label* (a branch target), while a legacy `catch` handler is an
*owned sequence*. Keep the instrumenter's traversal and the stub host's in
step — an under-walk in either is a silent under-report, which is exactly how
M35 stayed open for two milestones.

`wasmi` 0.31 hard-codes `exceptions: false` and exposes no setter, so it
refuses every such module. `crates/codetracer-wasm-stub-host/src/v8.rs` is the
second oracle that exists for this: it runs a module under the host V8 via
`node` (declared in `flake.nix`) and returns the same `RuntimeRecording`, so
`tests/exception_handling.rs` asserts on values that actually crossed. Prefer
`wasmi` for anything it can run — it is in-process and exact for NaN payloads
in the export call's own arguments and results, which V8 is not. Never
"simplify" the exception tests back onto `wasmi`;
`the_wasmi_oracle_still_refuses_exception_handling_modules` exists to make that
attempt fail loudly.

The one residual: the hook surface has no unwind event, so an exception that
propagates past an open crossing leaves it open. That is a property of the
surface, not of the walk, and it is loud — the stream is left unbalanced and a
§6 replayer refuses it outright.

## Conventions

- `Cargo.lock` is committed and every cargo invocation passes `--locked`.
- Rust 2021 edition; workspace-level dependency versions live in the root
  `Cargo.toml` `[workspace.dependencies]`.
- `just lint` must stay clean at `-D warnings`.

## Specs

- `codetracer-specs/Planned-Features/Value-Origin-Tracking.milestones.org`
  §§ M27, M34–M37.
- `codetracer-specs/Recording-Backends/WASM-Instrumentation-Layer.md`.
- `codetracer-specs/GUI/Debugging-Features/Value-Origin-Tracking.md` § 14.5.
- `codetracer-specs/Repo-Requirements.md` — the repo conventions this layout
  implements.
