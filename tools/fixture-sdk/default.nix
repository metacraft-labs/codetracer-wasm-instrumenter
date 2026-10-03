# Exact compiler SDK of the owning recorder, read from its committed lock.
# This is SDK provisioning; fixture compilation remains the real typed Go test.
{
  system ? builtins.currentSystem,
}:
let
  lock = builtins.fromJSON (builtins.readFile ../../../codetracer-wasm-recorder/flake.lock);
  inputs = lock.nodes.${lock.root}.inputs;
  sourceFor =
    name:
    let
      key = inputs.${name};
    in
    assert builtins.isString key;
    assert lock.nodes.${key}.locked.type == "github";
    builtins.fetchTree lock.nodes.${key}.locked;
  pkgs = import (sourceFor "nixpkgs").outPath { inherit system; };
  fenix = import (sourceFor "fenix").outPath { inherit system pkgs; };
  rust = fenix.combine [
    fenix.stable.cargo
    fenix.stable.rustc
    fenix.targets.wasm32-wasip1.stable.rust-std
  ];
  # Use the supported owning Nix compiler wrapper to supply the real pinned
  # zstd headers/linker flags, not a floating <nixpkgs> lookup or ambient SDK.
  cc = pkgs.wrapCCWith {
    cc = pkgs.stdenv.cc.cc;
    nixSupport = {
      cc-cflags = "-I${pkgs.lib.getDev pkgs.zstd}/include";
      cc-ldflags = "-L${pkgs.lib.getLib pkgs.zstd}/lib -Wl,-rpath,${pkgs.lib.getLib pkgs.zstd}/lib";
    };
  };
in
assert builtins.elem system [
  "x86_64-linux"
  "aarch64-linux"
  "x86_64-darwin"
  "aarch64-darwin"
];
pkgs.symlinkJoin {
  name = "codetracer-wasm-golden-fixture-sdk";
  paths = [
    pkgs.go_1_24
    rust
    cc
  ];
}
