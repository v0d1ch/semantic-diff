{
  description = "A development environment for Rust toolchain";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";

    # The best and most up-to-date Rust overlay
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.flake-utils.follows = "flake-utils";
    };
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };

        # You can pin a specific version or just use the latest stable
        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = [ "rust-analyzer" "rust-src" ];
        };

      in {
        devShells.default = pkgs.mkShell {
          name = "rust-dev";

          # Make the Rust toolchain available in the shell.
          nativeBuildInputs = [
            rustToolchain
          ];

          # Supporting tools for Rust and Go development.
          buildInputs = with pkgs; [
            pkg-config
          ];

          # Makes SSL certificates work for cargo crates that fetch git deps, etc.
          shellHook = ''
            export PKG_CONFIG_PATH="${pkgs.openssl.dev}/lib/pkgconfig:$PKG_CONFIG_PATH"
          '';
        };

        # Optional: expose cargo as a default package (so `nix build .#cargo` works)
        packages.cargo = rustToolchain;

        # If you want `nix build` to just give you the toolchain itself
        packages.default = rustToolchain;
      });
}
