{
  description = "Ferry — exact-commit recipe execution with durable receipts";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  inputs.rust-overlay.url = "github:oxalica/rust-overlay";
  inputs.rust-overlay.inputs.nixpkgs.follows = "nixpkgs";
  outputs = { self, nixpkgs, rust-overlay }:
    let
      systems = [ "aarch64-darwin" "aarch64-linux" "x86_64-linux" ];
      eachSystem = nixpkgs.lib.genAttrs systems;
      package = system:
        let
          pkgs = import nixpkgs { inherit system; overlays = [ rust-overlay.overlays.default ]; };
          rust = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
          rustPlatform = pkgs.makeRustPlatform { cargo = rust; rustc = rust; };
        in pkgs.callPackage ./package.nix {
          inherit rustPlatform;
          # Compiled in so receipts join each executable to its source.
          sourceRev = self.rev or self.dirtyRev or null;
        };
    in {
      packages = eachSystem (system: { default = package system; });
      apps = eachSystem (system: {
        default = { type = "app"; program = "${self.packages.${system}.default}/bin/ferry"; };
      });
      checks = eachSystem (system: { package = self.packages.${system}.default; });
    };
}
