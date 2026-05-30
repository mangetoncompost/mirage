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

# --- Schedule ledger ---
Type "6"
Sleep 2.5s

# --- Speeds tab: navigate rows ---
Type "4"
Sleep 1.5s
Down
Sleep 0.4s
Down
Sleep 0.4s
Down
Sleep 0.4s
Down
Sleep 0.4s
Down
Sleep 0.4s
Up
Sleep 0.4s
Up
Sleep 1s

# --- Dashboard: multi-select ---
Type "1"
Sleep 1.5s
Space
Sleep 1s

# --- Detail card ---
Enter
Sleep 2.5s
Escape
Sleep 0.8s

# --- Command palette: open + fuzzy filter ---
Type ":"
Sleep 1.5s
Type "expo"
Sleep 2s
Escape
Sleep 0.8s

# --- Ratio graph tab ---
Type "0"
Sleep 2.5s

# --- Help overlay ---
Type "?"
Sleep 3s
Type "?"
Sleep 0.8s

# --- Back to dashboard ---
Type "1"
Sleep 2s

# Quit cleanly (announces STOPPED)
Type "q"
Sleep 2s
