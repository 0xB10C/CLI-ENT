{
  description = "cli-ent — CLI Extended Node Talker: an interactive Bitcoin P2P client over v1/v2 transport";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      forAllSystems = f:
        nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          # nativeBuildInputs run on the build host: the Rust toolchain plus the
          # compiler + linker that secp256k1-sys (via bitcoin 0.32) and ring (via
          # the test download stack) need — gcc is what provides the `cc` that a
          # bare cargo build was missing. Cargo.toml declares rust-version = "1.85";
          # nixpkgs stable rustc satisfies it.
          nativeBuildInputs = [
            pkgs.rustc
            pkgs.cargo
            pkgs.clippy
            pkgs.rustfmt
            pkgs.rust-analyzer
            pkgs.gcc
            pkgs.pkg-config
          ];

          # Available at runtime in the shell:
          # - bitcoind: spin up a regtest node to generate the embedded `block1`
          #   sample (PLAN §10) and for manual poking; the integration tests
          #   (PLAN §15) spawn their own via corepc-node.
          packages = [
            pkgs.bitcoind
          ];

          # Lets rust-analyzer find the standard library sources.
          RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";

          shellHook = ''
            echo "cli-ent dev shell"
            echo "  rustc    $(rustc --version 2>/dev/null)"
            echo "  cargo    $(cargo --version 2>/dev/null)"
            echo "  bitcoind $(bitcoind --version 2>/dev/null | head -1)"
            echo
            echo "  build:  cargo build"
            echo "  test:   cargo test                 (integration tests are #[ignore] by default)"
            echo "          cargo test -- --ignored     (needs the 'download' feature bitcoind or BITCOIND_EXE)"
            echo
            echo "  To reuse this shell's bitcoind for integration tests instead of downloading:"
            echo "    export BITCOIND_EXE=$(command -v bitcoind)"
          '';
        };
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
