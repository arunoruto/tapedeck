# The Rust build alone, with no reference to yt-dlp or ffmpeg. This is the expensive part and
# what the binary cache holds; nix/wrapper.nix puts the runtime tools on PATH around it, so
# swapping those never invalidates this derivation.
{ lib, rustPlatform, cacert, versionCheckHook, nix-update-script }:

rustPlatform.buildRustPackage (finalAttrs: {
  pname = "tapedeck-unwrapped";
  version = (lib.importTOML ../Cargo.toml).package.version;

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [ ../Cargo.toml ../Cargo.lock ../src ../tests ../themes ../crates ../config.example.toml ];
  };
  cargoLock.lockFile = ../Cargo.lock;

  # Tests that need the network are #[ignore]d in the source and therefore skipped here.
  # The remaining suite covers parsing, the database, templating and the CLI.
  # The CLI tests start `td`, which builds its HTTPS client eagerly; rustls refuses to start
  # without a CA store, and the Linux sandbox has none, so hand it cacert's bundle.
  nativeCheckInputs = [ cacert ];
  cargoTestFlags = [ "--workspace" ];

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
