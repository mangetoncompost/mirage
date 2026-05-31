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

# --- Speeds tab: navigate rows with vim keys ---
Type "4"
Sleep 1.2s
Type "j"
Sleep 0.4s
Type "j"
Sleep 0.4s
Type "k"
Sleep 1s

# --- Dashboard: multi-select with Space ---
Type "1"
Sleep 1.2s
Type "j"
Sleep 0.5s
Space
Sleep 1s

# --- Detail card ---
Enter
Sleep 2.5s
Escape
Sleep 0.8s

# --- Remove confirmation overlay (then cancel) ---
Type "x"
Sleep 2.5s
Escape
Sleep 1s

# --- Command palette ---
Type ":"
Sleep 1.2s
Type "expo"
Sleep 2s
Escape
Sleep 0.8s

# --- Ratio graph tab (cumulative upload curve) ---
Type "0"
Sleep 3s

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
