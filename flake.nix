{
  description = "Ferry — exact-commit recipe execution with durable receipts";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  inputs.rust-overlay.url = "github:oxalica/rust-overlay";
  inputs.rust-overlay.inputs.nixpkgs.follows = "nixpkgs";
  outputs = { self, nixpkgs, rust-overlay }:
    let
      systems = [ "aarch64-darwin" "aarch64-linux" "x86_64-linux" ];
      eachSystem = nixpkgs.lib.genAttrs systems;
      pkgsFor = system: import nixpkgs { inherit system; overlays = [ rust-overlay.overlays.default ]; };
      rustFor = pkgs: pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
      package = system:
        let
          pkgs = pkgsFor system;
          rust = rustFor pkgs;
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
      # programs.ferry: the package, and the skill symlinked where agents look.
      homeManagerModules.default = import ./nix/home-manager.nix {
        inherit self;
        src = ./.;
      };
      # Source tree path, for tools that sync agent skills.
      skillsDir = "${self}/skills";
      # Everything `just check` and the contract tests need, on any machine
      # with Nix: the recipes run in it, so no host needs these installed.
      devShells = eachSystem (system:
        let pkgs = pkgsFor system; in {
          default = pkgs.mkShell {
            packages = [ (rustFor pkgs) pkgs.just pkgs.git pkgs.jujutsu pkgs.python3 pkgs.inetutils pkgs.openssh ]
              ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [ pkgs.procps pkgs.util-linux ];
            # git never discovers a repository at or above $HOME (a stray ~/.git).
            shellHook = ''export GIT_CEILING_DIRECTORIES="$HOME"'';
          };
        });
    };
}
