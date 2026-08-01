## Reprobuild dev env + build recipe for codetracer-wasm-instrumenter.
##
## A self-contained Rust cargo workspace: the bytecode-rewriting WASM
## instrumentation pipeline (Value Origin Tracking, M27) plus its stub
## host, host-module framework, and the ``ct instrument`` CLI. The
## shipping binary is the ``ct-instrument`` bin target of the
## ``ct-instrument-cli`` workspace member; the remaining members compile
## as its transitive deps.
##
## Per ``codetracer-specs/Repo-Requirements.md`` §2.8 the recipe expresses
## build and test execution NATIVELY through typed-tool edges
## (``cargo.build``, ``cargo.test``). It does NOT delegate to
## ``shell(command = "bash scripts/...")`` wrappers — delegation defeats
## the engine's incremental-build, action-cache, per-test invalidation,
## and the CI sharding the engine grows into per
## ``reprobuild-specs/CI-Sharding.md``. This mirrors the sibling
## Rust-recorder recipes (``codetracer-wasmi-recorder``,
## ``codetracer-evm-recorder``, ``codetracer-trace-format``).
##
## **This repo is a LEAF (from reprobuild's perspective) and a
## SELF-CONTAINED cargo workspace.** The root ``Cargo.toml`` declares four
## members (``crates/codetracer-wasm-instrumenter``,
## ``crates/codetracer-wasm-stub-host``,
## ``crates/codetracer-wasm-host-module-framework``,
## ``crates/ct-instrument-cli``) that resolve against each other via
## IN-REPO ``path = "../<member>"`` deps only. Unlike the wasmi / evm
## recorders there is NO cross-repo cargo ``path`` dependency on the
## ``codetracer-trace-format`` sibling: every dependency is either an
## in-repo member or a crates.io crate (``walrus``, ``wasmparser``,
## ``wat``, ``clap``, ``serde``, ...). So there is no ``uses:
## "<sibling>"`` edge, no develop-override, and no vcs LockedDep for any
## sibling — the lock is self-only (``deps = [<this repo>]``).
##
## Because there is no cross-repo Nim FFI build.rs (grep confirms the tree
## carries no ``build.rs`` at all), the toolchain floor is JUST the Rust
## toolchain — no ``nim`` / ``nimble`` / ``capnp`` / ``zstd`` /
## ``pkg-config`` that the trace-format-consuming recorders need.
##
## **Test corpus & per-test platform gating.** ``cargo test --locked`` is
## one whole-workspace cargo run. No test FILE in this repo carries a
## per-file host gate: grep finds no ``#[cfg(target_os = …)]`` /
## ``#[cfg(unix)]`` / ``#[cfg(windows)]`` / ``#[ignore]`` attribute on any
## test function or module in ``crates/*/tests/*.rs`` or in the crates'
## ``src/`` ``#[cfg(test)]`` unit tests. The integration tests are
## portable:
##
##   * ``ct-instrument-cli/tests/cli_smoke.rs`` drives the just-built
##     ``ct-instrument`` binary (via ``CARGO_BIN_EXE_ct-instrument``) on a
##     WAT fixture compiled in-process.
##   * ``codetracer-wasm-instrumenter/tests/verification.rs`` and
##     ``codetracer-wasm-stub-host/tests/parity.rs`` run the pipeline +
##     stub-host walk over in-tree / sibling ``.wasm`` fixtures.
##   * ``codetracer-wasm-host-module-framework/tests/{factory_logs_all_calls,
##     stylus_parity}.rs`` exercise the TOML-driven plan formatter over
##     in-repo config fixtures.
##   * ``codetracer-wasm-instrumenter/tests/exception_handling.rs`` shells
##     out to ``node`` (M35c). It is portable in the same sense as the
##     rest: ``node`` is supplied by this repo's own ``flake.nix``
##     (``nodejs_22``), exactly as it already is for ``just test-runtime``
##     and ``just test-plugins``, so this adds no NEW toolchain floor
##     beyond what those recipes already require — it only moves a node
##     dependency into the ``cargo test`` edge. It carries no ``#[ignore]``
##     and no skip arm: without ``node`` the suite FAILS, deliberately,
##     because a silently skipped oracle is how the gap it covers survived
##     two milestones. The reason it cannot use the in-process ``wasmi``
##     embedder is that ``wasmi`` 0.31 hard-codes ``exceptions: false``.
##
## The four ``parity.rs`` tests read golden ``.wasm`` fixtures from the
## sibling ``../codetracer-wasm-recorder/cmd/wazero/testdata/recorder-golden/``
## checkout. That is a RUNTIME file read the test performs itself (not a
## cargo build-graph input, so it is NOT a cross-repo ``uses:`` edge), and
## the fixtures ARE present in this workspace — so all four parity tests
## execute REAL ``assert_parity`` assertions here, not the source-level
## SKIP arm the test defines for shallow-clone environments. Nothing is
## weakened or skipped by this recipe.
##
## So the corpus runs identically on every host cargo supports, and the
## single whole-workspace ``cargo.test`` execute edge below matches the
## repo's own ``cargo test --locked`` one-for-one — there is no per-OS
## partition to model and no ``when defined(...)`` extraction gate.
##
## **Tool provisioning.** ``defaultToolProvisioning "path"`` matches the
## canonical Rust-recorder recipes: the dev shell (rustup toolchain on
## Linux/macOS, ``env.ps1`` on Windows) puts ``cargo`` / ``rustc`` on
## ``PATH``, so the weak-local PATH resolver is the right default. Without
## it ``repro build`` refuses to run with "typed tool provisioning is
## required for uses declarations".

import repro_project_dsl

package codetracer_wasm_instrumenter:
  defaultToolProvisioning "path"

  uses:
    # Rust toolchain — declared by version so the tarball-direct
    # provisioning entries in repro_dsl_stdlib/packages/cargo.nim /
    # rustc.nim resolve on Windows; on Linux/macOS the dev shell supplies
    # the same versions. The root ``Cargo.toml`` pins ``edition = "2021"``
    # and pins no ``rust-version`` floor, so the floor mirrors the sibling
    # Rust-recorder recipes (``>=1.83``), comfortably below the toolchain
    # that resolves the crates.io deps (walrus 0.26 / wasmparser 0.245).
    "rustc >=1.83"
    "cargo >=1.83"

  # The shipping binary — the ``ct-instrument`` bin target of the
  # ``ct-instrument-cli`` workspace member. The per-app cargo build edge
  # is emitted in the ``build:`` block below.
  executable ctInstrument:
    name: "ct-instrument"

  devEnv:
    activity "default"

  build:
    # ---- Primary build edge (the `default` collection) ----------------
    #
    # Native whole-workspace cargo build. The root ``Cargo.toml`` is a
    # real workspace with four members; bare ``cargo build`` builds every
    # member including the ``ct-instrument`` binary. Enrolled into the
    # conventional ``default`` collection per
    # reprobuild-specs/Build-Graph-Collections.md §"`default`" so
    # ``repro build`` (no positional target) materialises this edge's
    # closure.
    #
    # ``locked = true`` because the root ``Cargo.lock`` IS committed
    # (``git ls-files`` tracks it): the build must fail rather than
    # silently regenerate the lock if a member's ``Cargo.toml`` drifts
    # from the pinned resolution.
    #
    # The workspace tree (root manifest/lock + the ``crates`` dir) is
    # declared as ``extraInputs`` so the engine tracks the whole workspace
    # as the build edge's input set (cargo's own ``.d`` depfiles under
    # ``target/*/deps`` refine this per-crate at action-end via the
    # makeDepfile dependency policy the cargo package declares).
    const binarySuffix = (when defined(windows): ".exe" else: "")
    const cliBinary = "target/release/ct-instrument" & binarySuffix

    let workspaceInputs = @[
      "Cargo.toml", "Cargo.lock",
      "crates",
    ]

    let cliBuild = cargo.build(
      release = true,
      locked = true,
      actionId = "codetracer-wasm-instrumenter.cargo-build",
      extraInputs = workspaceInputs,
      extraOutputs = @[cliBinary])
    discard collect("default", @[cliBuild])

    # ---- Test-binary build + run edges (the `test` collection) -------
    #
    # Two-stage shape per Repo-Requirements.md §2.8: ``cargo.test(noRun =
    # true)`` builds every workspace test binary into
    # ``target/debug/deps/<crate>-<hash>`` (the engine tracks the deps
    # directory as the build edge's effect set because the hashed filename
    # floats with input content); the second ``cargo.test`` (``noRun``
    # defaulting to false) then runs the binaries in one cargo invocation
    # — the same whole-workspace pass ``cargo test --locked`` performs.
    # The execute edge depends on the build edge so the engine only
    # re-runs tests when an input changed since the last successful
    # execution.
    #
    # Per-test execute edges fall out automatically once the
    # ct-test-runner cargo adapter lands per
    # reprobuild-specs/Test-Edges-And-Parallel-Runner.milestones.org §M4 —
    # the whole-binary edge becomes a fan-out point without changing this
    # recipe.

    let testsBuild = cargo.test(
      noRun = true,
      locked = true,
      actionId = "codetracer-wasm-instrumenter.cargo-test-build",
      after = @[cliBuild],
      extraInputs = workspaceInputs,
      extraOutputs = @["target/debug/deps"])

    let testsRun = cargo.test(
      locked = true,
      actionId = "codetracer-wasm-instrumenter.cargo-test-run",
      after = @[testsBuild.action],
      extraInputs = workspaceInputs & @["target/debug/deps"])

    discard collect("test", @[testsRun.action])
