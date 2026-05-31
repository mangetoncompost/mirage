# Changelog

## v1.3.2 - 2026-05-31

### Fixed

- With no torrents found, the dashboard now opens with the onboarding hint instead of exiting instantly with no visible message (the "No torrent, exiting" log was silent in TUI mode); in non-interactive mode it prints where it looked and exits
- The default `torrent_dir` is `torrents/` (relative to the launch directory), so a fresh checkout runs without setting `TORRENT_DIR` or a config path

## v1.3.1 - 2026-05-31

### Fixed

- The help overlay (`?`) is laid out in two columns so the full keymap fits the macOS app window (92x28) instead of clipping the tabs and session bindings; it falls back to one column on a narrow terminal

## v1.3.0 - 2026-05-31

### Added

- Trackers tab (`3 trk`): press `g` to toggle a by-tracker rollup (one row per host with torrent count, summed upload, speed, seeders/leechers, and errors, sorted by upload)
- Plausibility linter overlay (`!`): flags settings a private tracker might find implausible (upload far past the torrent size, near-line-speed upload to an almost empty swarm)
- Ratio tab (`0 rto`): ETA to the next ratio milestone, projected from the average credited rate
- Swarm-proportional upload cap: optional `per_leecher_kib_s` config key scales each torrent's fake speed with its leecher count, so Mirage never declares near-line-speed to an almost empty swarm. Off by default
- Discreet milestone notifications: optional `notify_milestones` config key emits a terminal OSC 9 escape and a best-effort `osascript`/`notify-send` notification on each ratio milestone. Off by default, no terminal bell
- Detail card wire sub-tab (`w`): the last announce exchange (request, status, response body) for the selected torrent

## v1.2.1 - 2026-05-31

### Changed

- Windows: detect a UTF-8 console (output code page 65001, also forced at startup) and truecolor on Windows Terminal / VS Code, so modern Windows terminals get Unicode box-drawing and 24-bit color instead of the ASCII / 256-color fallback
- Windows: the dashboard now reflows on window resize (no SIGWINCH on Windows; the key thread wakes a repaint on the resize event)

### Notes

- The Windows binary is built in CI but has not been verified at runtime on a real Windows machine; see the README install note

## v1.2.0 - 2026-05-30

### Added

- `j`/`k` and `h`/`l` as vim-style aliases for row selection and tab navigation, alongside the arrow keys
- Context-sensitive footer: each tab shows the keys actionable on it, degrading to `? q` on narrow terminals
- First-run onboarding on the Dashboard: with no torrents, a hint points to the watch directory and the `?`/`:` overlays

### Changed

- Removing torrents (`x`, or the palette remove command) now asks for `y`/`Esc` confirmation before stopping announces and dropping seeding state
- Keypresses wake the render loop immediately, so action feedback no longer waits up to one redraw tick
- macOS app window opens at 92x28 (was 110x34)

### Fixed

- Selecting a torrent row no longer shifts its content one column to the right (the selected-row gutter was 3 cells wide instead of 2)
- Ratio graph (tab 0) now draws the cumulative-upload curve as a filled staircase rising from 0 to the session total, with sub-cell ramp glyphs and start/end time labels; it previously rendered as an unreadable solid block
- Uploaded totals now climb live between announces (Dashboard, footer, Speeds, ratio graph, detail card) instead of staying flat until the next announce; the value declared to trackers is unchanged

## v1.1.0 - 2026-05-30

### Added

- 10th tab `[0]rto`: cumulative upload graph for the session (ASCII block raster, auto-scales)
- Schedule ledger (`[6]sch`): torrents sorted by time-to-next-announce with countdown bar and cadence reason (interval / warm-up / re-check / dl-tick)
- Per-row speed meter bar on seeding rows: color-coded green/amber/dim by share of session peak upload
- Command palette (`:` key): fuzzy search over all 19 actions with arrow navigation and Enter to execute
- Detail card overlay (`Enter` on a torrent): name, upload, peers, next announce, progress, tracker URLs
- Multi-select: `Space` toggles mark, `a` marks all visible, `A` clears; `f`/`x` act on the whole marked set
- Ratio cap (`g` key, `Cmd::SetRatioTarget`): per-torrent uploaded-bytes ceiling; auto-stops upload when reached; persisted in state.json
- Snapshot export (`e` key): writes a timestamped JSON file and copies the path to the clipboard via OSC-52
- Ratio milestone flash: footer highlights when the session ratio crosses 1.0x / 1.5x / 2.0x / 3.0x / 5.0x / 10.0x
- Help overlay updated with all new key bindings

### Changed

- state.json bumped to version 2 (adds `upload_target` field; v1 files load without changes)
- `can_upload()` also checks the per-torrent upload target
- Schedule tab rewritten as a live next-announce ledger

## v1.0.0 - 2026-05-30

### Added

- Live full-screen TUI dashboard with 9 tabs (dash, torrents, trackers, speeds, client, schedule, network, logs, config)
- HTTP (BEP-3) and UDP (BEP-15) tracker announce support
- Simulated leech-to-seed download phase with persistence across restarts
- Time-varying upload speed curve (sum of sines) with exact closed-form integral
- Global upload multiplier (0.25x to 8x) adjustable from the dashboard
- Automatic Transmission version detection and faithful wire-level emulation
- Filesystem watcher for hot add/remove of .torrent files
- JSON stats output file for external monitoring
- macOS app bundle (Mirage.app) via scripts/make_app.sh
- Pre-built binaries for Linux x86_64, macOS ARM, macOS Intel via GitHub Releases
