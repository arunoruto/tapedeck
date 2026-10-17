{ lib, rustPlatform, makeWrapper, cacert, versionCheckHook, nix-update-script, yt-dlp, ffmpeg-headless }:

rustPlatform.buildRustPackage (finalAttrs: {
  pname = "tapedeck";
  version = (lib.importTOML ../Cargo.toml).package.version;

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [ ../Cargo.toml ../Cargo.lock ../src ../tests ../config.example.toml ];
  };
  cargoLock.lockFile = ../Cargo.lock;

  nativeBuildInputs = [ makeWrapper ];

  # Tests that need the network are #[ignore]d in the source and therefore skipped here.
  # The remaining suite covers parsing, the database, templating and the CLI.
  # The CLI tests start `td`, which builds its HTTPS client eagerly; rustls refuses to start
  # without a CA store, and the Linux sandbox has none, so hand it cacert's bundle.
  nativeCheckInputs = [ cacert ];

  # yt-dlp needs ffmpeg for audio extraction; put both on td's PATH.
  # ffmpeg-headless: the full ffmpeg drags in SDL/GUI libraries for ffplay, ~1 GiB we never use.
  postInstall = ''
    wrapProgram $out/bin/td --prefix PATH : ${lib.makeBinPath [ yt-dlp ffmpeg-headless ]}
  '';

  nativeInstallCheckInputs = [ versionCheckHook ];
  doInstallCheck = true;

  passthru.updateScript = nix-update-script { };

  meta = {
    description = "ListenBrainz playlists → downloaded, tagged library → Plex";
    homepage = "https://github.com/arunoruto/tapedeck";
    changelog = "https://github.com/arunoruto/tapedeck/releases/tag/v${finalAttrs.version}";
    license = lib.licenses.mit;
    mainProgram = "td";
  };
})
