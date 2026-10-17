{ config, lib, pkgs, ... }:

let
  cfg = config.programs.tapedeck;
  toml = pkgs.formats.toml { };
  # Re-wrap with the consumer's tools. The flake's package is `nix/wrapper.nix`, whose override
  # only rebuilds the thin wrapper; the Rust build underneath stays a cache hit.
  package =
    if cfg.package ? override then
      cfg.package.override { yt-dlp = cfg.ytdlpPackage; ffmpeg-headless = cfg.ffmpegPackage; }
    else
      cfg.package;
in
{
  options.programs.tapedeck = {
    enable = lib.mkEnableOption "tapedeck, the ListenBrainz → library → Plex sync tool";

    package = lib.mkPackageOption pkgs "tapedeck" { };

    ytdlpPackage = lib.mkPackageOption pkgs "yt-dlp" {
      extraDescription = "Defaults to your nixpkgs' yt-dlp, so it is shared with the rest of the system.";
    };

    ffmpegPackage = lib.mkPackageOption pkgs "ffmpeg-headless" {
      extraDescription = "Only needs the ffmpeg binary; the headless build is enough.";
    };

    settings = lib.mkOption {
      type = toml.type;
      default = { };
      description = ''
        Written to {file}`$XDG_CONFIG_HOME/tapedeck/config.toml`. All keys are documented in
        docs/configuration.md. Keep secrets out of the Nix store by pointing `token_file` at a
        file managed by sops-nix, agenix or similar instead of setting `token`.
      '';
      example = lib.literalExpression ''
        {
          library = "/srv/music";
          listenbrainz.user = "me";
          sync.targets = [ "plex" ];
          sync.plex = {
            url = "http://plex.local:32400";
            token_file = config.sops.secrets.plex-token.path;
            section = "Music";
          };
        }
      '';
    };
  };

  options.services.tapedeck.enable =
    lib.mkEnableOption "the tapedeck daemon as a systemd user service (runs `td daemon`)";

  config = lib.mkIf cfg.enable {
    home.packages = [ package ];

    xdg.configFile."tapedeck/config.toml" = lib.mkIf (cfg.settings != { }) {
      source = toml.generate "tapedeck-config.toml" cfg.settings;
    };

    systemd.user.services.tapedeck = lib.mkIf (config.services.tapedeck.enable && pkgs.stdenv.hostPlatform.isLinux) {
      Unit = {
        Description = "tapedeck music sync daemon";
        # If sops-nix manages secrets for this user, token_file only exists after its unit ran.
        After = [ "network-online.target" ] ++ lib.optional (config.systemd.user.services ? sops-nix) "sops-nix.service";
        Wants = [ "network-online.target" ] ++ lib.optional (config.systemd.user.services ? sops-nix) "sops-nix.service";
        # Pick up a rewritten config without a manual restart.
        X-Restart-Triggers = lib.optional (cfg.settings != { }) config.xdg.configFile."tapedeck/config.toml".source;
      };
      Service = {
        ExecStart = "${lib.getExe package} daemon";
        Restart = "on-failure";
        RestartSec = 30;
      };
      Install.WantedBy = [ "default.target" ];
    };
  };
}
