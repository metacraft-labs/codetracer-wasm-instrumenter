## Owning immutable SDK views, matching WASM shell.nix and its committed lock.
## Go's test subprocess invokes rustc and cc by their real executable names;
## declared refs expose this same measured SDK prefix on its scoped PATH.
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
