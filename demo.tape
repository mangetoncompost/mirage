# Run: vhs demo.tape (from the project root)
# Requires: cargo build --release

Output assets/demo.gif

Set Shell "bash"
Set FontSize 14
Set Width 1200
Set Height 700
Set Theme "Dracula"
Set PlaybackSpeed 1.0

# Launch Mirage pointing at the torrents directory
Type "TORRENT_DIR=torrents ./target/release/mirage"
Enter

# Wait for TUI + first announce
Sleep 5s

# --- [1] Dashboard: the main overview ---
Type "1"
Sleep 2.5s
# Walk the selection, mark a row
Type "j"
Sleep 0.5s
Space
Sleep 1.2s

# --- Detail card + wire sub-view ---
Enter
Sleep 2s
# w shows the last announce wire exchange, i back to info
Type "w"
Sleep 2.5s
Type "i"
Sleep 1.5s
Escape
Sleep 0.8s

# --- [2] Torrents: full list with state ---
Type "2"
Sleep 2.5s

# --- [3] Trackers: per-torrent, then g toggles by-host rollups ---
Type "3"
Sleep 2s
Type "g"
Sleep 3s
Type "g"
Sleep 1.2s

# --- [4] Speeds: edit bands with vim nav ---
Type "4"
Sleep 1.5s
Type "j"
Sleep 0.4s
Type "j"
Sleep 0.8s
Type "k"
Sleep 1.2s

# --- [5] Client: emulated identity + announce query ---
Type "5"
Sleep 3s

# --- [6] Schedule ledger ---
Type "6"
Sleep 2.5s

# --- [7] Network settings ---
Type "7"
Sleep 2.5s

# --- [8] Logs: the event ring ---
Type "8"
Sleep 2.5s

# --- [9] Config: mirror of config.toml ---
Type "9"
Sleep 2.5s

# --- [0] Ratio graph: cumulative upload curve + ETA to next milestone ---
Type "0"
Sleep 3.5s

# --- Plausibility linter overlay (!) ---
Type "!"
Sleep 3.5s
Type "!"
Sleep 0.8s

# --- Command palette ---
Type ":"
Sleep 1.2s
Type "expo"
Sleep 2s
Escape
Sleep 0.8s

# --- Help overlay (full keymap) ---
Type "?"
Sleep 3.5s
Type "?"
Sleep 0.8s

# --- Back to dashboard ---
Type "1"
Sleep 2s

# Quit cleanly (announces STOPPED)
Type "q"
Sleep 2s
