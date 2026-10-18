{ config, lib, pkgs, ... }:

# tapedeck as a NixOS system service: its own user from boot, no linger, secrets from the
# system's sops-nix/agenix, state and socket shared with a group so admins can use the CLI and
# TUI against the running daemon, logs in `journalctl -u tapedeck`. Home Manager users have
# nix/home-module.nix instead.

let
  cfg = config.services.tapedeck;
  toml = pkgs.formats.toml { };
  # Re-wrap with the consumer's tools; only the thin wrapper is rebuilt, the Rust build stays a
  # cache hit.
  package =
    if cfg.package ? override then
      cfg.package.override { yt-dlp = cfg.ytdlpPackage; ffmpeg-headless = cfg.ffmpegPackage; }
    else
      cfg.package;
  # The daemon's state and socket live where systemd puts them unless the config says otherwise.
  settings = {
    database = "/var/lib/tapedeck/tapedeck.db";
    socket = "/run/tapedeck/tapedeck.sock";
  } // cfg.settings;
  configFile = toml.generate "tapedeck-config.toml" settings;
in
{
  options.services.tapedeck = {
    enable = lib.mkEnableOption "the tapedeck daemon as a system service (runs `td daemon`)";

    package = lib.mkPackageOption pkgs "tapedeck" { };

    ytdlpPackage = lib.mkPackageOption pkgs "yt-dlp" {
      extraDescription = "Defaults to your nixpkgs' yt-dlp, so it is shared with the rest of the system.";
    };

    ffmpegPackage = lib.mkPackageOption pkgs "ffmpeg-headless" {
      extraDescription = "Only needs the ffmpeg binary; the headless build is enough.";
    };

    user = lib.mkOption {
      type = lib.types.str;
      default = "tapedeck";
      description = "User the daemon runs as. Created when left at the default.";
    };

    group = lib.mkOption {
      type = lib.types.str;
      default = "tapedeck";
      description = ''
        Group the daemon runs as; created when left at the default. Its members can use `td` and
        `td tui` against the running daemon: the database directory and the socket are group-writable.
      '';
    };

    extraGroups = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      example = [ "media" ];
      description = "Supplementary groups, typically the one that owns the music library.";
    };

    environmentFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      description = "Optional EnvironmentFile for the unit (e.g. RUST_LOG=debug).";
    };

    settings = lib.mkOption {
      type = toml.type;
      default = { };
      description = ''
        Written to {file}`/etc/tapedeck/config.toml`. All keys are documented in
        https://arunoruto.github.io/tapedeck/configuration.html. `library` is required. `database` and `socket` default to
        {file}`/var/lib/tapedeck/tapedeck.db` and {file}`/run/tapedeck/tapedeck.sock`. Keep secrets
        out of the Nix store: point `token_file` at a file managed by sops-nix or agenix, owned by
        the service user.
      '';
      example = lib.literalExpression ''
        {
          library = "/srv/music";
          listenbrainz = { user = "me"; playlists = [ "weekly-jams" "weekly-exploration" "daily-jams" ]; };
          sync.targets = [ "plex" ];
          sync.plex = {
            url = "http://127.0.0.1:32400";
            token_file = config.sops.secrets."tapedeck/plex-token".path;
            section = "Music";
          };
        }
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = cfg.settings ? library;
        message = "services.tapedeck.settings.library must be set";
      }
    ];

    environment.systemPackages = [ package ];
    # `td` from any shell talks to this config, database and socket.
    environment.variables.TAPEDECK_CONFIG = "/etc/tapedeck/config.toml";
    environment.etc."tapedeck/config.toml".source = configFile;

    users.users = lib.mkIf (cfg.user == "tapedeck") {
      tapedeck = {
        isSystemUser = true;
        group = cfg.group;
        extraGroups = cfg.extraGroups;
        home = "/var/lib/tapedeck";
        description = "tapedeck music sync daemon";
      };
    };
    users.groups = lib.mkIf (cfg.group == "tapedeck") { tapedeck = { }; };

    systemd.services.tapedeck = {
      description = "tapedeck music sync daemon";
      wantedBy = [ "multi-user.target" ];
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];
      restartTriggers = [ configFile ];
      serviceConfig = {
        ExecStart = "${lib.getExe package} daemon --config /etc/tapedeck/config.toml";
        User = cfg.user;
        Group = cfg.group;
        SupplementaryGroups = cfg.extraGroups;
        EnvironmentFile = lib.mkIf (cfg.environmentFile != null) cfg.environmentFile;
        Restart = "on-failure";
        RestartSec = 30;

        # State and socket shared with the group; UMask makes the database, its WAL files and
        # every downloaded track group-writable too.
        StateDirectory = "tapedeck";
        StateDirectoryMode = "0770";
        RuntimeDirectory = "tapedeck";
        RuntimeDirectoryMode = "0770";
        UMask = "0002";

        # Hardening. The daemon needs the network (ListenBrainz, MusicBrainz, YouTube, Plex), the
        # library, and the playlist folder when that lies outside the library.
        ProtectSystem = "strict";
        ReadWritePaths = [ settings.library ] ++ lib.optional (settings ? playlist_dir) settings.playlist_dir;
        ProtectHome = true;
        PrivateTmp = true;
        PrivateDevices = true;
        NoNewPrivileges = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectControlGroups = true;
        RestrictSUIDSGID = true;
        LockPersonality = true;
        RestrictRealtime = true;
        RestrictAddressFamilies = [ "AF_UNIX" "AF_INET" "AF_INET6" ];
        SystemCallArchitectures = "native";
        CapabilityBoundingSet = "";
      };
    };
  };
}
