# Contributing

```sh
nix develop                        # rust toolchain, yt-dlp, ffmpeg
cargo test                         # offline suite
cargo test -- --include-ignored    # also the live ListenBrainz / MusicBrainz tests
nix build                          # what CI and the cache build; runs the offline suite
```

CI runs fmt, clippy and the full test suite, builds the Nix package on x86_64-linux and
aarch64-darwin and pushes both to `arunoruto.cachix.org`, and publishes this book to GitHub
Pages from `docs/`. Pushing a `v*` tag that matches `Cargo.toml` cuts a release with
prebuilt binaries and pushes the container to `ghcr.io/arunoruto/tapedeck` as `<version>` and
`latest`; running the Container workflow by hand pushes `sha-<short sha>` only, for trying a
commit without moving `latest`. Sync targets and download backends are each one small trait; the
[architecture](architecture.md#extension-points) page says where. Keep the docs human-sounding;
design decisions go into the architecture page's design notes.
