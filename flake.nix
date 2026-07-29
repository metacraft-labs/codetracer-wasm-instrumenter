{
  description = "CodeTracer WASM instrumenter — bytecode-rewriting instrumentation pipeline";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
    flake-parts.url = "github:hercules-ci/flake-parts";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    pre-commit-hooks.url = "github:cachix/git-hooks.nix";
  };

  outputs =
    inputs@{ flake-parts, ... }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      perSystem =
        { pkgs, system, ... }:
        let
          fenixPkgs = inputs.fenix.packages.${system};

          # The repo is a plain cargo workspace (no build.rs anywhere), so the
          # toolchain floor is just Rust.  `wasm32-unknown-unknown` rust-std is
          # part of the toolchain because the instrumentation pipeline is
          # exercised against WASM modules and downstream consumers compile
          # guest crates for that target from this shell.
          rustToolchain = fenixPkgs.combine [
            fenixPkgs.stable.cargo
            fenixPkgs.stable.rustc
            fenixPkgs.stable.rust-src
            fenixPkgs.stable.clippy
            fenixPkgs.stable.rustfmt
            fenixPkgs.targets.wasm32-unknown-unknown.stable.rust-std
          ];

          preCommit = inputs.pre-commit-hooks.lib.${system}.run {
            src = ./.;
            hooks = {
              lint = {
                enable = true;
                name = "Lint";
                entry = "just lint";
                language = "system";
                pass_filenames = false;
              };
            };
          };
        in
        {
          checks.pre-commit-check = preCommit;

          devShells.default = pkgs.mkShell {
            packages = [
              rustToolchain

              # Build automation, formatters and hook runner.
              pkgs.just
              pkgs.nixfmt-rfc-style
              pkgs.prek

              # `plugins/*` are plain ESM bundler wrappers tested with
              # `node --test`; `just test-plugins` needs a Node runtime.
              pkgs.nodejs_22

              pkgs.git
              pkgs.pkg-config
            ]
            ++ preCommit.enabledPackages;

            shellHook = preCommit.shellHook;
          };

          # The shipping artifact: the `ct-instrument` CLI (the `ct-instrument`
          # bin target of the `ct-instrument-cli` workspace member).  The other
          # three members build as its transitive dependencies.
          packages.default = pkgs.rustPlatform.buildRustPackage {
            pname = "ct-instrument";
            version = "0.1.0";
            src = ./.;

            cargoLock.lockFile = ./Cargo.lock;

            # `crates/codetracer-wasm-stub-host/tests/parity.rs` reads golden
            # `.wasm` fixtures out of the sibling `codetracer-wasm-recorder`
            # checkout, which does not exist inside the Nix sandbox.  The test
            # suite is run by `just test` from the dev shell (where the sibling
            # is present) rather than during the package build.
            doCheck = false;

            meta = {
              description = "Bytecode-rewriting WASM instrumentation pipeline for CodeTracer";
              mainProgram = "ct-instrument";
            };
          };
        };
    };
}
