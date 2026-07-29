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
| `just test`                    | `cargo test --workspace --locked` — the whole suite (53 tests)    |
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
