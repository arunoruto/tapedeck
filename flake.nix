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
      packages = forAll (pkgs: let
        tapedeck-unwrapped = pkgs.callPackage ./nix/package.nix { };
        tapedeck = pkgs.callPackage ./nix/wrapper.nix { inherit tapedeck-unwrapped; };
      in {
        inherit tapedeck-unwrapped tapedeck;
        default = tapedeck;
      } // pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
        # OCI image for docker/podman, pushed to ghcr.io/arunoruto/tapedeck by the Container
        # workflow. `td daemon` by default, config from /config.toml and/or TAPEDECK_* variables,
        # state under /data, the library wherever the config says (/music by convention), the HTTP
        # API on 8687 (Lidarr + 1) unless TAPEDECK_SERVER__LISTEN is set to something else or empty.
        container = pkgs.dockerTools.buildLayeredImage {
          name = "ghcr.io/arunoruto/tapedeck";
          tag = tapedeck.version;
          created = "now";
          contents = [ tapedeck pkgs.cacert ];
          extraCommands = "mkdir -m 1777 tmp";
          config = {
            Entrypoint = [ "/bin/td" ];
            Cmd = [ "daemon" ];
            Env = [
              "TAPEDECK_CONFIG=/config.toml"
              "TAPEDECK_SERVER__LISTEN=0.0.0.0:8687"
              "XDG_DATA_HOME=/data"
              "SSL_CERT_FILE=/etc/ssl/certs/ca-bundle.crt"
            ];
            ExposedPorts."8687/tcp" = { };
            Volumes = { "/data" = { }; "/music" = { }; };
            Labels = {
              "org.opencontainers.image.source" = "https://github.com/arunoruto/tapedeck";
              "org.opencontainers.image.description" = "ListenBrainz playlists → downloaded, tagged library → Plex";
              "org.opencontainers.image.licenses" = "MIT";
            };
          };
        };
      });

      overlays.default = final: _prev: {
        tapedeck-unwrapped = final.callPackage ./nix/package.nix { };
        tapedeck = final.callPackage ./nix/wrapper.nix { };
      };

      # Home Manager: programs.tapedeck.{enable,package,ytdlpPackage,ffmpegPackage,settings} and services.tapedeck.enable.
      homeModules.default = { pkgs, lib, ... }: {
        imports = [ ./nix/home-module.nix ];
        programs.tapedeck.package = lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      };
      homeManagerModules = self.homeModules;

      # NixOS: services.tapedeck.{enable,package,ytdlpPackage,ffmpegPackage,user,group,extraGroups,environmentFile,settings}.
      nixosModules.default = { pkgs, lib, ... }: {
        imports = [ ./nix/nixos-module.nix ];
        services.tapedeck.package = lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.default;
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
            ffmpeg-headless
            sqlite
          ];
          RUST_LOG = "info";
        };
      });
    };
}
