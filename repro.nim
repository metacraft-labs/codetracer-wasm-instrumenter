## Reprobuild dev env + build recipe for codetracer-wasm-instrumenter.
##
## A Rust cargo workspace: the bytecode-rewriting WASM instrumentation
## pipeline plus its stub host, host-module framework, and the
## ``ct instrument`` CLI. The shipping binary is the ``ct-instrument`` bin
## target of the ``ct-instrument-cli`` workspace member; the remaining
## members compile as its transitive deps.
##
## The recipe expresses every build and test the Justfile runs NATIVELY,
## through typed tool edges (``cargo.build``, ``cargo.test`` and direct
## ``buildAction`` calls on declared tools). It does not delegate to
## ``just``, to a recursive ``repro`` or to a ``sh -c`` wrapper: a wrapped
## command is opaque to the engine, which then cannot see what it reads,
## cannot cache or invalidate it, and cannot tell which tools ran.
##
## **Inventory, one-for-one with the Justfile.**
##
##   * ``default`` -- ``just build``: ``cargo build --workspace --release
##     --locked``, producing ``target/release/ct-instrument``.
##   * ``test`` -- ``just test`` = ``test-rust``, which first runs the
##     prerequisite ``test-golden-fixtures`` and then ``cargo test
##     --workspace --locked``. Here that is three edges: the golden-fixture
##     Go test (below), the cargo test-binary build, and the cargo test run,
##     which depends on both.
##   * ``test-plugins`` -- ``just test-plugins``: ``node --test
##     index.test.js`` in each ``plugins/*/`` directory, one edge each.
##   * ``test-runtime`` -- ``just test-runtime``: ``node --test`` over the
##     three ``recorder-runtime`` suites.
##
## ``test-plugins`` and ``test-runtime`` are separate collections because
## the Justfile keeps them out of ``just test``; folding them into ``test``
## would make ``repro test`` run more than ``just test`` does.
##
## **Cross-repository inputs.** The cargo suite reads two sibling
## repositories at run time, and both are pinned through this repo's
## ``repro.lock`` (the workspace fragment declares
## ``depends = ["codetracer-evm-recorder", "codetracer-wasm-recorder"]``):
##
##   * ``codetracer-wasm-host-module-framework/tests/stylus_parity.rs``
##     reads ``../codetracer-evm-recorder/stylus/codetracer.toml`` and the
##     WASM recorder's legacy ``internal/stylus/stylus_funcs.go``;
##   * ``codetracer-wasm-stub-host/tests/parity.rs`` and
##     ``codetracer-wasm-instrumenter/tests/boundary_values.rs`` read the
##     WASM recorder's golden ``.wasm`` fixtures from
##     ``cmd/wazero/testdata/recorder-golden/build/``.
##
## Those fixtures are NOT committed anywhere. The WASM recorder compiles
## them from their Rust sources during its own Go test run, with a compiler
## its flake pins, and stamps the result with the compiler identity and the
## source bytes. So this recipe does not assume they are present: it runs
## that owning Go test as a typed edge, and the cargo run consumes the
## directory the edge produces. Without that edge the parity tests would
## take the skip arm they keep for checkouts that lack the sibling, and a
## skipped parity test is not evidence of anything.
##
## **The golden-fixture edge.** It runs exactly the command ``just
## test-golden-fixtures`` runs, in the WASM recorder's checkout:
##
##     go test ./cmd/wazero -run "^TestRecorderGoldenFixturesBuild$" -count=1
##
## The selector is the owning builder's own completeness test; it does not
## stand in for the recorder's full suite. Its contract with this graph:
##
##   * ``cmd/wazero`` is built WITH cgo, as the dev shell builds it.
##     ``tracewriter/ctfs_writer.go`` (``//go:build cgo``) links
##     ``-lcodetracer_trace_writer -lzstd -lm -lpthread`` and includes the
##     trace-writer header and the recorder's own column header. The
##     cgo-disabled stub would also compile, but switching to it would test
##     a package variant nobody ships.
##   * The C ABI writer library is the owning producer's own typed edge,
##     ``build_writer_artifacts.buildSharedLib`` from
##     ``codetracer-trace-format-nim``, constructed in this graph and
##     declared as an input and prerequisite of the Go edge, together with
##     its public header directory. ``CGO_CFLAGS`` names the header
##     directory and ``CGO_LDFLAGS`` the library directory, plus an rpath to
##     it so the test binary finds the shared library at run time.
##   * The compilers are the immutable SDK the WASM recorder's ``shell.nix``
##     uses, read from that repository's committed ``flake.lock``: Go 1.24
##     and fenix stable ``rustc`` with its matching ``wasm32-wasip1``
##     standard library, plus the native C compiler and pinned zstd that
##     cgo links through (``fixture_sdk_tools.nim``,
##     ``tools/fixture-sdk/default.nix``). They are provisioned as declared
##     packages, not taken from whatever ``go`` or ``rustc`` the host's
##     ``PATH`` holds. The same compiler wrapper is handed to
##     ``buildSharedLib`` through ``compilerSdkRefs``, so the writer library
##     and the Go test link against one zstd.
##   * **Relinking.** Go's build cache keys a cgo link on its flags, not on
##     the bytes of the libraries they name: a rebuilt writer library in an
##     unchanged directory would be served from a cached link. Two things
##     close that gap. The writer library (with its headers) is a declared
##     input, so its bytes are part of the edge's action-cache key: a changed
##     library misses the cache and the engine runs the edge. And the Go
##     build cache (``GOCACHE``) is an edge-owned scratch directory, which
##     the engine empties before every run, so a run that does happen
##     compiles and links from an empty cache and cannot reuse a link made
##     against the old library. The edge is therefore cacheable: a hit is
##     only possible when every declared input, the writer library included,
##     is unchanged. ``-count=1`` (in the owning command) keeps Go from
##     replaying a cached test result within a run.
##   * The builder writes its outputs atomically (``<name>.wasm.tmp<pid>``,
##     then rename) and reuses them only when its compiler/source stamp
##     matches; ``rustc`` writes its own temporaries there too. The whole
##     output directory is therefore declared as the edge's write root, and
##     the stamp and the five ``.wasm`` files as the outputs it must
##     produce. Nothing here bypasses or pre-creates the directory.
##   * **Nim sources of the writer.** ``buildSharedLib`` compiles the writer
##     against the Nim packages its ``.nimble`` requires (``stew``,
##     ``results``). They are passed as ``nimDependencyDirs`` from the
##     workspace's sibling checkouts, so the compile does not resolve them
##     from whatever the host's ``~/.nimble`` happens to hold. Two gaps
##     remain. ``buildSharedLib`` does not pass ``--noNimblePath``, so Nim
##     still searches its default nimble package directory after these
##     paths: a module missing from a sibling would be resolved from there
##     silently, not refused (with both siblings complete, the monitor
##     records no read under it). And this repository's lock pins
##     ``nim-stew`` but not ``nim-results``, so the ``results`` the writer
##     compiles against is whatever revision that checkout holds.
##
## **Node.** ``codetracer-wasm-instrumenter/tests/exception_handling.rs``
## runs every exceptions-proposal module under the host V8 through
## ``node`` (``codetracer-wasm-stub-host/src/v8.rs``), because ``wasmi``
## 0.31 cannot enable that proposal. ``node`` is therefore a declared tool
## of the cargo test-run edge (and of the plugin and runtime edges), so
## its identity is part of the edge's identity. Without ``node`` the suite
## fails, deliberately: there is no skip arm.
##
## **Platforms.** Linux and macOS run the whole inventory. On Windows the
## cargo build and the plugin/runtime suites are unchanged, but the
## golden-fixture edge cannot run yet: Windows has no Nix, so the SDK above
## has no realization there, and the Windows Go/cgo toolchain (Go, a MinGW
## gcc for cgo, a MinGW-linkable zstd, and the writer as a DLL the MinGW
## linker can consume) has no supported provisioning in reprobuild today.
## The Windows ``test`` collection therefore holds an edge that fails with
## a message naming exactly that, instead of guessing at a toolchain or
## dropping the edge; see ``windowsFixtureCapabilityUnavailable`` below.
##
## **Tool provisioning.** ``defaultToolProvisioning "path"`` for the cargo
## toolchain: the dev shell (``flake.nix``) puts ``cargo`` / ``rustc`` /
## ``node`` on ``PATH``. The fixture SDK packages are not on any ``PATH``,
## so path mode realizes them from their Nix expression.

import std/[json, os]
import repro_project_dsl
import repro_dsl_stdlib/foreign_env
import fixture_sdk_tools
import "../codetracer-trace-format-nim/build_writer_artifacts"

const
  recorderRoot = "../codetracer-wasm-recorder"
    ## The WASM recorder checkout, relative to this repository's root. The
    ## golden-fixture edge runs in it and writes its fixture directory.
  nimRoot = "../codetracer-trace-format-nim"
    ## The trace-format-nim checkout that owns the C ABI writer library.
  writerNimDependencyRoots = ["../nim-stew", "../nim-results"]
    ## Sibling checkouts of the Nim packages ``codetracer_trace_format.nimble``
    ## requires (``stew``, ``results``), handed to ``buildSharedLib``.
  fixtureScratch = ".repro/build/golden-fixtures"
    ## Scratch tree the golden-fixture edge owns (Go caches, TMPDIR).
  goldenFixtureDir = recorderRoot / "cmd/wazero/testdata/recorder-golden/build"
    ## The owning builder's output directory (``goldenFixtureOutDir`` in
    ## ``cmd/wazero/recorder_golden_fixtures_test.go``).
  goldenFixtureNames = ["collections", "column_aware", "control_flow",
                        "nested_calls", "panic_path"]
    ## ``goldenFixtureNames`` in the same file. The builder itself fails if
    ## this list and the ``.rs`` sources disagree; the copy here only names
    ## the declared outputs.
  windowsFixtureCapabilityUnavailable =
    "codetracer-wasm-instrumenter: the WASM golden-fixture prerequisite " &
    "(`go test ./cmd/wazero -run \"^TestRecorderGoldenFixturesBuild$\" " &
    "-count=1` in codetracer-wasm-recorder, with CGO_ENABLED=1) cannot run " &
    "on Windows: capability unavailable. It needs, provisioned and cached " &
    "by reprobuild rather than taken from the runner's PATH: (1) Go 1.24 " &
    "(the WASM recorder's flake pins go_1_24); (2) a MinGW-w64 gcc for cgo; " &
    "(3) zstd headers and a MinGW-linkable libzstd; (4) the " &
    "codetracer-trace-format-nim C ABI writer built as a library the MinGW " &
    "linker can link (its Windows buildSharedLib edge builds an MSVC DLL); " &
    "(5) fenix stable rustc with the wasm32-wasip1 standard library. " &
    "Reprobuild has no supported per-tool provisioning for this toolchain " &
    "on Windows yet, so this edge fails instead of guessing one.\n"
    ## Printed by the Windows stand-in for the golden-fixture edge.

package codetracer_wasm_instrumenter:
  defaultToolProvisioning "path"

  uses:
    # Rust toolchain -- declared by version so the tarball-direct
    # provisioning entries in repro_dsl_stdlib/packages/cargo.nim /
    # rustc.nim resolve on Windows; on Linux/macOS the dev shell supplies
    # the same versions. The root ``Cargo.toml`` pins ``edition = "2021"``
    # and no ``rust-version`` floor, so the floor mirrors the sibling
    # Rust-recorder recipes (``>=1.83``).
    "rustc >=1.83"
    "cargo >=1.83"
    # C compiler driver -- rustc links every binary (the ``ct-instrument``
    # CLI and each test executable) through ``cc`` on Linux/macOS, and
    # ``buildSharedLib`` names the same backend on its edge (it refuses a
    # consumer that does not declare
    # ``build_writer_artifacts.SharedLibToolSelectors``). Windows links the
    # cargo outputs with MSVC instead.
    when defined(linux):
      "gcc"
    elif defined(macosx):
      "clang"
    # Node.js -- the V8 oracle of ``tests/exception_handling.rs`` and the
    # ``node --test`` plugin/runtime suites. The flake's dev shell carries
    # ``nodejs_22``.
    "node >=22"
    when not defined(windows):
      # The owning WASM recorder's pinned compiler SDK
      # (``fixture_sdk_tools.nim``) for the golden-fixture Go test, and the
      # Nim compiler that ``buildSharedLib`` drives.
      "fixture-go-sdk"
      "fixture-rustc-sdk"
      "fixture-cc-sdk"
      "nim >=2.2 <3.0"

  # The shipping binary -- the ``ct-instrument`` bin target of the
  # ``ct-instrument-cli`` workspace member. The per-app cargo build edge
  # is emitted in the ``build:`` block below.
  executable ctInstrument:
    name: "ct-instrument"

  devEnv:
    when not defined(windows):
      useFlakeDevShell()

    activity "default"

  build:
    let root = activeProviderProjectRoot()

    proc rooted(paths: openArray[string]): seq[string] =
      ## The engine resolves an action's relative input, output and scratch
      ## paths against the action's working directory. The edges below run
      ## in another directory (the WASM recorder checkout, a plugin
      ## directory), so their paths are anchored to this repository's root
      ## explicitly; written relative, they would land inside the directory
      ## the command runs in.
      for path in paths:
        result.add(normalizedPath(root / path))

    # ---- Primary build edge (the `default` collection) ----------------
    #
    # Native whole-workspace cargo build: bare ``cargo build`` builds every
    # member, including the ``ct-instrument`` binary. ``locked = true``
    # because the root ``Cargo.lock`` is committed: the build must fail
    # rather than silently regenerate the lock if a member's ``Cargo.toml``
    # drifts from the pinned resolution. The workspace tree is declared as
    # ``extraInputs``; cargo's own ``.d`` depfiles refine it per crate.
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

    # ---- Golden-fixture prerequisite (part of the `test` collection) --
    #
    # See the module doc, "The golden-fixture edge".
    # The files the edge must produce; the directory that holds them, with
    # the builder's and rustc's temporaries, is its declared write root.
    var fixtureOutputs = @[goldenFixtureDir / "stamp.txt"]
    for name in goldenFixtureNames:
      fixtureOutputs.add(goldenFixtureDir / (name & ".wasm"))

    when defined(windows):
      # No supported Windows provisioning exists for this edge's toolchain
      # (module doc, "Platforms"). This stand-in keeps the edge, and the
      # cargo run's dependency on it, in the Windows graph and makes it
      # fail with the capability that is missing; it builds nothing.
      let fixtureCall = publicCliCall("node", "node", "",
        "codetracer-wasm-instrumenter.golden-fixtures.windows-unavailable", @[
          cliArgSeq("args", @["-e",
            "process.stderr.write(" & escapeJson(
              windowsFixtureCapabilityUnavailable) & ");process.exit(1)"],
            kind = cpkPositional)])
      let goldenFixtures = buildAction(
        "codetracer-wasm-instrumenter.golden-fixtures", fixtureCall,
        toolIdentityRefs = @["node"], cacheable = false)
    else:
      # The owning trace-writer producer, compiled by the same SDK compiler
      # wrapper the Go edge links with (its zstd include/link/rpath flags
      # come from that wrapper; see tools/fixture-sdk/default.nix).
      # Its Nim package dependencies come from the sibling checkouts (module
      # doc, "Nim sources of the writer"); ``buildSharedLib`` requires them
      # absolute and existing, and declares them as inputs of its edge.
      var writerNimDependencyDirs: seq[string] = @[]
      for dependencyRoot in writerNimDependencyRoots:
        writerNimDependencyDirs.add(normalizedPath(root / dependencyRoot))
      let writerLibrary = buildSharedLib(nimRoot,
        compilerSdkRefs = @["fixture-cc-sdk"],
        nimDependencyDirs = writerNimDependencyDirs)
      appendRegisteredActionToolIdentityRefs(writerLibrary.id, ["nim"])
      let writerArtifact = sharedLibraryPath(nimRoot)
      let writerDir = normalizedPath(root / nimRoot)
      let writerIncludeDir = writerDir / "include"

      let goEnv = @[
        # The cgo package variant the dev shell builds and ships.
        ("CGO_ENABLED", "1"),
        # The SDK's C compiler wrapper, by the name it carries in the SDK.
        ("CC", "cc"),
        ("CGO_CFLAGS", "-I" & writerIncludeDir),
        ("CGO_LDFLAGS", "-L" & writerDir & " -Wl,-rpath," & writerDir),
        # Edge-owned Go state: a build cache the engine empties before every
        # run (see the module doc, "Relinking"), a module cache and GOPATH
        # under the same scratch tree (the module vendors its dependencies,
        # so nothing is downloaded), and no user-level `go env` file.
        ("GOCACHE", root / fixtureScratch / "go-build-cache"),
        ("GOPATH", root / fixtureScratch / "gopath"),
        ("GOMODCACHE", root / fixtureScratch / "gopath/pkg/mod"),
        ("GOENV", "off"),
        ("GOTOOLCHAIN", "local"),
        ("GOPROXY", "off"),
        ("TMPDIR", root / fixtureScratch / "tmp")]

      let goCall = publicCliCall("fixture-go-sdk", "fixture-go-sdk", "",
        "codetracer-wasm-instrumenter.golden-fixtures", @[
          cliArgSeq("args", @["test", "./cmd/wazero",
            "-run", "^TestRecorderGoldenFixturesBuild$", "-count=1"],
            kind = cpkPositional)])
      let goldenFixtures = buildAction(
        "codetracer-wasm-instrumenter.golden-fixtures", goCall,
        deps = @[writerLibrary.id],
        inputs = rooted(@[
          # The Go module: manifests, vendored dependencies, and every
          # package `cmd/wazero` and its tests compile.
          recorderRoot / "go.mod", recorderRoot / "go.sum",
          recorderRoot / "vendor", recorderRoot / "*.go",
          recorderRoot / "api", recorderRoot / "sys",
          recorderRoot / "imports", recorderRoot / "experimental",
          recorderRoot / "internal", recorderRoot / "tracewriter",
          recorderRoot / "cmd/wazero/*.go",
          recorderRoot / "cmd/wazero/testdata/*.wasm",
          recorderRoot / "cmd/wazero/testdata/*.wat",
          recorderRoot / "cmd/wazero/testdata/boundary-log",
          recorderRoot / "cmd/wazero/testdata/cat",
          recorderRoot / "cmd/wazero/testdata/fs",
          recorderRoot / "cmd/wazero/testdata/stylus",
          # The fixture sources.
          recorderRoot / "cmd/wazero/testdata/recorder-golden/*.rs",
          # The compiler configuration the SDK is read from.
          recorderRoot / "flake.lock", recorderRoot / "shell.nix",
          "tools/fixture-sdk/default.nix", "fixture_sdk_tools.nim",
          # The writer library and the headers cgo compiles against.
          writerArtifact, nimRoot / "include",
          recorderRoot / "tracewriter/codetracer_trace_writer.h",
          recorderRoot / "tracewriter/codetracer_trace_writer_columns.h"]),
        outputs = rooted(fixtureOutputs),
        # The whole fixture output directory: the builder's
        # ``<name>.wasm.tmp<pid>`` files and rustc's temporaries land there
        # next to the outputs.
        declaredOutputs = rooted(@[goldenFixtureDir]),
        # The Go build cache is scratch the engine empties before every run,
        # so each run relinks against the current writer library (module
        # doc, "Relinking"); as scratch it is neither an input nor a cached
        # output.
        scratchDirs = rooted(@[fixtureScratch / "go-build-cache",
          fixtureScratch / "gopath", fixtureScratch / "tmp"]),
        env = goEnv,
        toolIdentityRefs = @["fixture-go-sdk", "fixture-rustc-sdk",
          "fixture-cc-sdk"],
        cwdKind = acwdCustom, cwdCustomPath = recorderRoot,
        dependencyPolicy = automaticMonitorPolicy())

    # ---- Test-binary build + run edges (the `test` collection) -------
    #
    # ``cargo.test(noRun = true)`` builds every workspace test binary into
    # ``target/debug/deps/<crate>-<hash>``; the second ``cargo.test`` runs
    # them in one cargo invocation -- the same whole-workspace pass
    # ``cargo test --workspace --locked`` performs. The run edge also
    # follows the golden-fixture edge and declares the fixture files it
    # reads, so a rebuilt fixture reruns the suite.
    let testsBuild = cargo.test(
      noRun = true,
      locked = true,
      actionId = "codetracer-wasm-instrumenter.cargo-test-build",
      after = @[cliBuild],
      extraInputs = workspaceInputs,
      extraOutputs = @["target/debug/deps"])

    let crossRepoTestInputs = @[
      "../codetracer-evm-recorder/stylus/codetracer.toml",
      recorderRoot / "internal/stylus/stylus_funcs.go"]
    let testsRun = cargo.test(
      locked = true,
      actionId = "codetracer-wasm-instrumenter.cargo-test-run",
      after = @[testsBuild.action, goldenFixtures],
      extraInputs = workspaceInputs & @["target/debug/deps"] &
        fixtureOutputs & crossRepoTestInputs)

    # Tool identities the cargo edges execute. Reprobuild's stdlib attaches
    # only a typed tool's own sub-tools, so every other program an action
    # runs must be attached here (and declared in ``uses:`` above): the
    # Rust toolchain itself, the C linker driver rustc invokes, and -- for
    # the test-run edge only -- the ``node`` the V8 oracle spawns.
    const linkerRefs = (when defined(linux): @["gcc"]
                        elif defined(macosx): @["clang"]
                        else: newSeq[string]())
    for actionId in [cliBuild.id, testsBuild.action.id, testsRun.action.id]:
      appendRegisteredActionToolIdentityRefs(actionId,
        @["cargo", "rustc"] & linkerRefs)
    appendRegisteredActionToolIdentityRefs(testsRun.action.id, ["node"])

    discard collect("test", @[testsRun.action])

    # ---- Node suites (the `test-plugins` / `test-runtime` collections) -
    #
    # ``just test-plugins`` runs ``node --test index.test.js`` inside each
    # ``plugins/*/`` directory; ``just test-runtime`` runs the three
    # ``recorder-runtime`` suites. Each edge runs the identical command in
    # the identical directory. ``os.tmpdir()`` (where the wrappers stage
    # their rewritten modules) is pointed at an edge-owned scratch tree.
    #
    # The vite and webpack suites resolve the CLI they drive by running
    # ``cargo build --bin ct-instrument`` against this workspace
    # (``CT_INSTRUMENT_BIN`` is unset under ``just`` too), so their edges also
    # run cargo and the linker. If that build fails the suites print
    # ``[skip]`` and pass, so the tools must be declared for the suites to
    # test anything. Both write the same ``target/debug/ct-instrument``: the
    # vite edge owns it as its output, and the webpack edge follows it and
    # reads it, so the two cargo builds never run at once and only one edge
    # claims the file.
    const pluginNames = ["esbuild-plugin-codetracer-wasm",
                         "rollup-plugin-codetracer-wasm",
                         "vite-plugin-codetracer-wasm",
                         "webpack-plugin-codetracer-wasm"]
    const cargoDrivingPlugins = ["vite-plugin-codetracer-wasm",
                                 "webpack-plugin-codetracer-wasm"]
    const debugCli = "target/debug/ct-instrument" & binarySuffix
    var pluginTests: seq[BuildActionDef] = @[]
    var debugCliOwner = ""
    for plugin in pluginNames:
      let pluginDir = "plugins" / plugin
      let scratch = ".repro/build/node-tests" / plugin
      let call = publicCliCall("node", "node", "",
        "codetracer-wasm-instrumenter.test-plugins." & plugin, @[
          cliArgSeq("args", @["--test", "index.test.js"],
            kind = cpkPositional)])
      let drivesCargo = plugin in cargoDrivingPlugins
      let ownsDebugCli = drivesCargo and debugCliOwner.len == 0
      var inputs = @[pluginDir / "index.js", pluginDir / "index.test.js",
                     pluginDir / "package.json"]
      var deps: seq[string] = @[]
      if drivesCargo:
        inputs.add(workspaceInputs)
        if not ownsDebugCli:
          inputs.add(debugCli)
          deps.add(debugCliOwner)
      let outputs = (if ownsDebugCli: @[debugCli] else: newSeq[string]())
      let action = buildAction(
        "codetracer-wasm-instrumenter.test-plugins." & plugin, call,
        deps = deps,
        inputs = rooted(inputs),
        outputs = rooted(outputs), declaredOutputs = rooted(outputs),
        scratchDirs = rooted(@[scratch]),
        env = @[("TMPDIR", root / scratch)],
        toolIdentityRefs = @["node"] &
          (if drivesCargo: @["cargo", "rustc"] & linkerRefs else: @[]),
        cwdKind = acwdCustom, cwdCustomPath = pluginDir,
        dependencyPolicy = automaticMonitorPolicy(), cacheable = false)
      if ownsDebugCli:
        debugCliOwner = action.id
      pluginTests.add(action)
    discard collect("test-plugins", pluginTests)

    let runtimeScratch = ".repro/build/node-tests/recorder-runtime"
    let runtimeCall = publicCliCall("node", "node", "",
      "codetracer-wasm-instrumenter.test-runtime", @[
        cliArgSeq("args", @["--test", "host_runtime.test.js",
          "browser_session.test.js", "host_state.test.js"],
          kind = cpkPositional)])
    let runtimeTests = buildAction(
      "codetracer-wasm-instrumenter.test-runtime", runtimeCall,
      inputs = rooted(@["recorder-runtime/*.js"]),
      scratchDirs = rooted(@[runtimeScratch]),
      env = @[("TMPDIR", root / runtimeScratch)],
      toolIdentityRefs = @["node"],
      cwdKind = acwdCustom, cwdCustomPath = "recorder-runtime",
      dependencyPolicy = automaticMonitorPolicy(), cacheable = false)
    discard collect("test-runtime", @[runtimeTests])
