# `td` with yt-dlp and ffmpeg on its PATH. Cheap to rebuild, so overriding the tools
# (`tapedeck.override { yt-dlp = pkgs.yt-dlp; }`) keeps the Rust build a cache hit.
# ffmpeg-headless: the full ffmpeg drags in SDL/GUI libraries for ffplay, ~1 GiB we never use.
# mpv: only the TUI's playback needs it; pass `mpv = null` for a server install.
{ lib, symlinkJoin, makeWrapper, tapedeck-unwrapped, yt-dlp, ffmpeg-headless, mpv ? null }:

symlinkJoin {
  pname = "tapedeck";
  inherit (tapedeck-unwrapped) version meta;
  paths = [ tapedeck-unwrapped ];
  nativeBuildInputs = [ makeWrapper ];
  postBuild = ''
    wrapProgram $out/bin/td --prefix PATH : ${lib.makeBinPath ([ yt-dlp ffmpeg-headless ] ++ lib.optional (mpv != null) mpv)}
  '';
  passthru = {
    unwrapped = tapedeck-unwrapped;
    inherit (tapedeck-unwrapped) updateScript;
  };
}
