# Changelog

## v1.1.0 — 2026-05-30

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

## v1.0.0 — 2026-05-30

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
