{
  description = "tapedeck: ListenBrainz playlists → downloaded, tagged library → Plex";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  # CI pushes x86_64-linux builds here. Trusted users with `accept-flake-config` get it automatically.
  nixConfig = {
    extra-substituters = [ "https://arunoruto.cachix.org" ];
    extra-trusted-public-keys = [ "arunoruto.cachix.org-1:GQVw1YDtjt0+ElmQifxEI52a0pRVe9/gdcNEr8v8G14=" ];
  };

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      forAll = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAll (pkgs: rec {
        tapedeck = pkgs.callPackage ./nix/package.nix { };
        default = tapedeck;
      });

      overlays.default = final: _prev: {
        tapedeck = final.callPackage ./nix/package.nix { };
      };

      # Home Manager: programs.tapedeck.{enable,package,settings} and services.tapedeck.enable.
      homeModules.default = { pkgs, lib, ... }: {
        imports = [ ./nix/home-module.nix ];
        programs.tapedeck.package = lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      };
      homeManagerModules = self.homeModules;

      devShells = forAll (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            rustc
            rustfmt
            clippy
            rust-analyzer
            yt-dlp
            ffmpeg-headless
            sqlite
          ];
          RUST_LOG = "info";
        };
      });
    };
}
