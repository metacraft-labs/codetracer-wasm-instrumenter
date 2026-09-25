default:
    @just --list

alias t := test
alias fmt := format

# --- Build ------------------------------------------------------------

# Build every workspace member (library, stub host, host-module framework
# and the `ct-instrument` CLI).
build: build-rust

# Release build of the whole cargo workspace.  `--locked` because
# `Cargo.lock` is committed: the build must fail rather than silently
# re-resolve if a member's manifest drifts from the pinned resolution.
build-rust:
    cargo build --workspace --release --locked

# Cross-check that the dev shell really carries the `wasm32-unknown-unknown`
# rust-std the instrumentation pipeline's consumers compile guest code with.
build-wasm-target-check:
    rustc --print target-libdir --target wasm32-unknown-unknown

# --- Test -------------------------------------------------------------

# Run the full test suite.
test: test-rust

# Whole-workspace cargo test run (unit tests + the `verification`,
# `parity`, `cli_smoke`, `boundary_values`, `boundary_decode`,
# `exception_handling`, `factory_logs_all_calls` and `stylus_parity`
# integration suites).
#
# `exception_handling` executes its modules under the host V8 through
# `node`, because `wasmi` cannot enable the exceptions proposal. `node`
# comes from this repo's own flake, so the suite is self-contained from
# inside the dev shell.
# Reuse the producer's pinned compiler and content-stamped fixture builder.
# The recorder removed committed wasm binaries; consumers use its build output.
test-golden-fixtures:
    repro exec ../codetracer-wasm-recorder -- sh -c 'cd ../codetracer-wasm-recorder && go test ./cmd/wazero -run "^TestRecorderGoldenFixturesBuild$" -count=1'

test-rust: test-golden-fixtures
    cargo test --workspace --locked

# The bundler plugin wrappers under `plugins/` are plain ESM modules
# tested with node's built-in runner.  They are not part of `test`
# because they only shell out to the CLI built by `build-rust`.
test-plugins:
    for p in plugins/*/; do (cd "$p" && node --test index.test.js); done

# The browser-side runtime shims under `recorder-runtime/` are plain ESM
# modules, likewise tested with node's built-in runner.  Kept out of
# `test` for the same reason as `test-plugins`: `test` is the cargo
# graph `repro test` mirrors one-for-one.
test-runtime:
    cd recorder-runtime && node --test host_runtime.test.js browser_session.test.js host_state.test.js

# --- Lint -------------------------------------------------------------

lint: lint-rust lint-nix

lint-rust:
    cargo clippy --workspace --all-targets --locked -- -D warnings
    cargo fmt --all -- --check

lint-nix:
    nixfmt --check flake.nix

# --- Format -----------------------------------------------------------

format: format-rust format-nix

format-rust:
    cargo fmt --all

format-nix:
    nixfmt flake.nix

# --- Nix --------------------------------------------------------------

# Verify the flake's default package builds.
nix-build:
    nix build .#default
