# Run: vhs demo.tape (from the project root)
# Requires: cargo build --release

Output assets/demo.gif

Set Shell "bash"
Set FontSize 14
Set Width 1100
Set Height 680
Set Theme "Dracula"
Set PlaybackSpeed 1.0

# Launch Mirage pointing at the torrents directory
Type "./target/release/mirage -c ~/.config/Mirage/config.toml"
Enter

# Wait for TUI to come up and first announce to land
Sleep 5s

# Cycle through tabs
Type "2"
Sleep 2s
Type "3"
Sleep 2s
Type "4"
Sleep 2s
Type "5"
Sleep 2s
Type "1"
Sleep 3s

# Show the help overlay then close it
Type "?"
Sleep 2.5s
Type "?"
Sleep 1s

# Quit cleanly (announces STOPPED)
Type "q"
Sleep 2s
