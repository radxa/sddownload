{
  description = "Native Rust SemiDrive downloader";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      forSystems = nixpkgs.lib.genAttrs systems;
      pkgsFor = system: import nixpkgs { inherit system; };
    in {
      packages = forSystems (system:
        let pkgs = pkgsFor system;
        in rec {
          sddownload = pkgs.rustPlatform.buildRustPackage {
            pname = "sddownload";
            version = "2.26.2101";
            src = pkgs.lib.cleanSourceWith {
              src = ./.;
              filter = path: type:
                pkgs.lib.cleanSourceFilter path type
                && builtins.baseNameOf path != "target";
            };
            cargoLock.lockFile = ./Cargo.lock;
            nativeBuildInputs = [ pkgs.pkg-config ];
            buildInputs = [ pkgs.libusb1 ];
            doCheck = true;
            cargoBuildFlags = [ "--workspace" ];
            cargoTestFlags = [ "--workspace" ];
            meta.mainProgram = "sddownload";
          };
          default = sddownload;
        });
      devShells = forSystems (system:
        let pkgs = pkgsFor system;
        in { default = pkgs.mkShell {
          packages = [ pkgs.cargo pkgs.rustc pkgs.rustfmt pkgs.clippy pkgs.pkg-config pkgs.libusb1 ];
          shellHook = ''
            export CARGO_TARGET_DIR="''${CARGO_TARGET_DIR:-''${XDG_CACHE_HOME:-$HOME/.cache}/sddownload/target}"
          '';
        }; });
    };
}
