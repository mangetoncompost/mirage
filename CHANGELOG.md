# Changelog

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
