# vhs tape for the README: `TAPEDECK_CONFIG=<scratch config> vhs assets/demo.tape` from the repo root, with `td` on PATH.
Output assets/tui.gif
Set Shell "bash"
Set FontSize 15
Set Width 1520
Set Height 760
Set Padding 12
Set Theme "Catppuccin Mocha"
Set TypingSpeed 60ms

Hide
Type "export TAPEDECK_CONFIG=$TAPEDECK_CONFIG" Enter
Type "clear" Enter
Show

Type "td" Sleep 500ms Enter
Sleep 2s
Screenshot assets/tui-library.png
Type "/" Sleep 300ms
Type "bicep" Sleep 1s
Enter Sleep 500ms
Type "l" Sleep 600ms
Type "l" Sleep 600ms
Type "j" Sleep 800ms
Type "i" Sleep 2.5s
Screenshot assets/tui-info.png
Escape Sleep 500ms
Type "/" Sleep 200ms
Escape Sleep 800ms
Type "]" Sleep 1.5s
Screenshot assets/tui-playlists.png
Type "l" Sleep 600ms
Type "jjjj" Sleep 1.5s
Type "/" Sleep 300ms
Type "bonobo" Sleep 1.5s
Escape Sleep 1s
Type "q"
Sleep 500ms
