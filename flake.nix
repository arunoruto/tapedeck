{
  description = "tapedeck: ListenBrainz playlists → downloaded, tagged library → Plex";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

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

      devShells = forAll (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            rustc
            rustfmt
            clippy
            rust-analyzer
            yt-dlp
            ffmpeg
            sqlite
          ];
          RUST_LOG = "info";
        };
      });
    };
}
