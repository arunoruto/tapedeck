{ config, lib, pkgs, ... }:

let
  cfg = config.programs.tapedeck;
  toml = pkgs.formats.toml { };
in
{
  options.programs.tapedeck = {
    enable = lib.mkEnableOption "tapedeck, the ListenBrainz → library → Plex sync tool";

    package = lib.mkPackageOption pkgs "tapedeck" { };

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
    home.packages = [ cfg.package ];

    xdg.configFile."tapedeck/config.toml" = lib.mkIf (cfg.settings != { }) {
      source = toml.generate "tapedeck-config.toml" cfg.settings;
    };

    systemd.user.services.tapedeck = lib.mkIf (config.services.tapedeck.enable && pkgs.stdenv.hostPlatform.isLinux) {
      Unit = {
        Description = "tapedeck music sync daemon";
        After = [ "network-online.target" ];
        Wants = [ "network-online.target" ];
        # Pick up a rewritten config without a manual restart.
        X-Restart-Triggers = lib.optional (cfg.settings != { }) config.xdg.configFile."tapedeck/config.toml".source;
      };
      Service = {
        ExecStart = "${lib.getExe cfg.package} daemon";
        Restart = "on-failure";
        RestartSec = 30;
      };
      Install.WantedBy = [ "default.target" ];
    };
  };
}
