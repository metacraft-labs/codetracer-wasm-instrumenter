## Owning immutable SDK views, matching WASM shell.nix and its committed lock.
## Go's test subprocess invokes rustc and cc by their real executable names;
## declared refs expose this same measured SDK prefix on its scoped PATH.
##
## The three packages are views of ONE Nix store path
## (``tools/fixture-sdk/default.nix``): Go 1.24, fenix stable rustc with the
## ``wasm32-wasip1`` standard library, and a native C compiler wrapper whose
## flags carry the pinned zstd headers, library directory and rpath. Their
## lock identities hash that expression together with the WASM recorder's
## ``flake.lock``, so moving the recorder's pins selects a new SDK.
##
## They have no Windows realization: Windows has no Nix, and no supported
## Windows provisioning for this toolchain exists yet. ``repro.nim``
## declares them only off Windows and gives the Windows graph an explicit
## capability-unavailable edge instead.
import blake3
import repro_project_dsl

const sdkSourceBytes = staticRead("tools/fixture-sdk/default.nix") & "\0" &
  staticRead("../codetracer-wasm-recorder/flake.lock")
let sdkSourceIdentity = blake3.toHex(blake3.digest(sdkSourceBytes))

package `fixture-go-sdk`:
  provisioning:
    nixPackage "codetracer-wasm-golden-fixture-sdk", executablePath = "bin/go",
      expressionFile = "tools/fixture-sdk/default.nix",
      lockIdentity = "owning-wasm-flake-lock:go1.24+fenix-stable-wasip1+native-zstd:" & sdkSourceIdentity

package `fixture-rustc-sdk`:
  provisioning:
    nixPackage "codetracer-wasm-golden-fixture-sdk", executablePath = "bin/rustc",
      expressionFile = "tools/fixture-sdk/default.nix",
      lockIdentity = "owning-wasm-flake-lock:fenix-stable-wasip1:" & sdkSourceIdentity

package `fixture-cc-sdk`:
  provisioning:
    nixPackage "codetracer-wasm-golden-fixture-sdk", executablePath = "bin/cc",
      expressionFile = "tools/fixture-sdk/default.nix",
      lockIdentity = "owning-wasm-flake-lock:native-cc+zstd:" & sdkSourceIdentity
