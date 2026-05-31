# Mirage

[![Build](https://img.shields.io/github/actions/workflow/status/mangetoncompost/mirage/ci.yml?branch=master)](https://github.com/mangetoncompost/mirage/actions)
[![Release](https://img.shields.io/github/v/release/mangetoncompost/mirage)](https://github.com/mangetoncompost/mirage/releases/latest)
[![License](https://img.shields.io/badge/license-MIT-blue)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-1.85%2B-orange?logo=rust)](https://www.rust-lang.org)
[![Platform](https://img.shields.io/badge/platform-linux%20%7C%20macos%20%7C%20windows-lightgrey)](https://github.com/mangetoncompost/mirage)

![Mirage dashboard demo](assets/demo.gif)

Mirage is a command-line tool that announces fake BitTorrent upload to trackers
in order to increase a user's ratio on private or public trackers. It reads real
.torrent files, emulates a legitimate BitTorrent client at the wire level
(peer_id, key, User-Agent, announce parameters), models a full leech-then-seed
lifecycle, and sends periodic HTTP and UDP tracker announces declaring a
time-varying upload curve whose declared integral exactly matches the area under
the curve displayed in the live dashboard.

It does not connect to peers, does not transfer data, and does not modify any
torrent file. The only network traffic it generates is tracker announces.


## How it works

### Client emulation

Mirage uses the `fake-torrent-client` crate to impersonate a real torrent
client. The `client` config key selects which client to emulate. With the
default value `"auto"`, Mirage reads the locally installed Transmission's
`Info.plist` to extract the exact version string, then builds a peer_id and
User-Agent that match that version character for character. If Transmission is
not found at `/Applications/Transmission.app`, it falls back to a built-in
Transmission 4.0.6 profile.

Any client profile supported by `fake-torrent-client` can be specified by name
(for example `"Qbittorrent_4_4_2"`). The key (the `key=` parameter in the HTTP
announce query) is refreshed periodically by a background task for clients whose
profile specifies a refresh interval.

### Download phase simulation

Declaring upload on a file that was never downloaded is the most common ratio
cheating tell. Mirage prevents this by modeling a realistic leech phase. Each
torrent starts in a `Downloading` state. A simulated download rate (drawn
randomly from `[min_download_rate, max_download_rate]` at startup) is applied in
the scheduler's download tick, which fires approximately every 45 seconds. The
torrent declares `downloaded` and `left` values that grow toward `length`. Once
the simulated download completes, a `completed` event is sent and the torrent
transitions to the `Seeding` state, after which upload can be declared.

Download progress is persisted to disk after every scheduler tick (see State
persistence below), so a restart resumes from the last saved position rather
than re-downloading from scratch, which would itself be detectable.

### Upload speed curve

Upload speed is modelled as a sum of four sinusoidal components:

- A slow mean-drift oscillation (period ~1200 s)
- Three texture oscillations (periods ~90 s, ~23 s, ~7 s)

Each component's phase and period are seeded by a per-torrent random value so
no two torrents share a harmonic spectrum. The period is also jittered by up to
+/-6% to defeat FFT-based fingerprinting.

The declared upload bytes sent to trackers are computed as the closed-form
integral of this curve over the window `[last_announce, now]`. The dashboard
displays the instantaneous value of the same curve. Because both the declared
integral and the displayed value derive from the same analytic function, the
total uploaded displayed in the dashboard always matches what was declared to
the trackers.

A global upload multiplier (stepped through `[0.25, 0.5, 1.0, 2.0, 4.0, 8.0]`
with the Up/Down arrow keys) scales the entire curve without breaking the
declared-equals-displayed invariant, because the same scalar is applied to both
the instantaneous value and the integral.

Upload is declared to zero (and `can_upload()` returns false) if:

- The torrent is still in the download phase
- The global pause flag is set
- The swarm reports no leechers (no one to upload to)

### Tracker announces

Mirage supports both HTTP (BEP-3) and UDP (BEP-15) trackers. All URLs from a
torrent's announce-list are contacted. Before the per-URL loop begins, the
upload delta is computed once from the speed curve integral, so every tracker in
the list receives the same declared upload window rather than each one getting a
progressively smaller window as `last_announce` is reset on each success.

The scheduler runs a single loop that:

1. Snapshots the list of torrent handles under a short read lock, then drops it.
2. For each torrent, acquires the torrent's own mutex and decides whether to
   announce (based on elapsed time vs interval, or download tick cadence).
3. Calls `announce()`, which fires HTTP and UDP announces in sequence.
4. Writes the JSON stats file and persists download state.
5. Sleeps until the soonest next announce, with jitter.

The outer `TORRENTS` lock is never held during network I/O. This ensures that
adding or removing a torrent (which takes a write lock) is never blocked by an
in-flight announce.

On startup, `announce(Started)` is sent for all loaded torrents. On clean exit
(Ctrl+C, `q` key, SIGTERM, or closing the macOS Terminal window), `announce(Stopped)`
is sent for all torrents with an 8-second timeout, then download state is saved
and the process exits.


## Installation

### Download a pre-built binary

Pre-compiled binaries for Linux and macOS are available on the
[releases page](https://github.com/mangetoncompost/mirage/releases/latest).

| File | Platform |
|------|----------|
| `mirage-linux-x86_64` | Linux x86_64 (static, no dependencies) |
| `mirage-macos-aarch64` | macOS Apple Silicon |
| `mirage-macos-x86_64` | macOS Intel |
| `mirage-windows-x86_64.exe` | Windows x86_64 |
| `Mirage.app.zip` | macOS app bundle (double-click to launch) |

On Linux and macOS, make the binary executable after downloading:

```
chmod +x mirage-linux-x86_64
./mirage-linux-x86_64
```

On macOS, unzip `Mirage.app.zip` and move `Mirage.app` to `/Applications`. It
opens the live dashboard in a Terminal.app window sized to 92x28 columns.

### Build from source

Requires Rust stable (edition 2024):

```
cargo build --release
```

The release binary is `target/release/mirage`.

To build the macOS app bundle:

```
cargo build --release && scripts/make_app.sh
# or, to install directly to /Applications:
scripts/make_app.sh --install
```


## Configuration

On first run, Mirage looks for a config file at the XDG config path:

```
~/.config/Mirage/config.toml
```

If the file does not exist, Mirage uses defaults. All fields are optional; any
omitted field uses its default value.

```toml
# Client to emulate.
# "auto" detects the locally installed Transmission and emulates its exact version.
# Any ClientVersion name from the fake-torrent-client crate is also valid, e.g.:
#   "Qbittorrent_4_4_2", "Transmission_3_00", "Deluge_2_1_1"
client = "auto"

# Listening port declared in announces (not actually bound).
# Default: random in 49152-65534.
port = 51413

# Upload speed band (bytes/s). The speed curve oscillates within this range.
min_upload_rate = 8192       # 8 KiB/s
max_upload_rate = 2097152    # 2 MiB/s

# Simulated download speed band (bytes/s). Used during the leech phase.
min_download_rate = 8192
max_download_rate = 16777216  # 16 MiB/s

# Number of peers requested from trackers.
# Default: client profile default (80 for Transmission).
numwant = 80

# Directory to watch for .torrent files.
# Defaults to "." (current working directory at launch).
torrent_dir = "/path/to/torrents"

# Write a PID file to the XDG runtime directory (mirage.pid).
use_pid_file = false

# Path to a JSON stats file written after each scheduler tick.
# Optional; no file is written if absent.
output_stats = "/tmp/mirage.json"
```

A specific config file can be passed at launch:

```
mirage -c /path/to/config.toml
mirage --config /path/to/config.toml
```

The `s` key in the dashboard's Config tab saves the current in-memory config
(including live Speeds edits) back to the XDG config file.

### Environment variables

`MIRAGE_NO_UI` disables the live dashboard even on an interactive terminal and
falls back to classic log output. Any value triggers it:

```
MIRAGE_NO_UI=1 mirage
```

`RUST_LOG` controls the log level in non-TUI mode. Valid values are `error`,
`warn`, `info`, `debug`, `trace`. Default is `trace`.

```
RUST_LOG=info mirage
```

In TUI mode the `tracing` subscriber is not initialised, so `RUST_LOG` has no
effect and log output does not corrupt the alternate screen.


## Torrent management

Place `.torrent` files in `torrent_dir`. Mirage scans that directory at startup
and loads every `.torrent` it finds. Torrents with no supported tracker URL are
skipped.

The filesystem watcher monitors the directory while Mirage is running. Dropping
a new `.torrent` file into the directory triggers a `Started` announce and adds
the torrent to the live dashboard within about 500 ms. Deleting or moving a file
out of the directory triggers a `Stopped` announce and removes the torrent.

The watcher also handles atomic-rename additions (rsync default, Transmission
temp-file writes) which arrive as `Modify(Name(To))` events rather than
`Create` events.

Duplicate torrents (same info-hash) are ignored silently and a note appears in
the dashboard feed.


## Live dashboard

When stdout is an interactive terminal and `MIRAGE_NO_UI` is not set, Mirage
enters the alternate screen and displays a full-screen dashboard redrawn
approximately 2.5 times per second. The dashboard is composed of nine tabs
navigated by number keys 1-9, the Left/Right arrow keys, or `h`/`l`.

The dashboard captures key input in raw mode. Bracketed paste is disabled so
pasting text into the window does not trigger accidental commands.

SIGWINCH (window resize) triggers an immediate repaint. SIGCONT (resume after
Ctrl+Z suspend) re-enters the alternate screen and raw mode that the shell may
have dropped.

### Tabs

**1 dash** - The main overview. Shows the emulated client identity (name,
peer_id, key), a table of all loaded torrents, and a scrolling feed of recent
events (connects, announces, upload ticks, watcher events, errors).

The torrent table columns are: torrent name, seeders (S), leechers (L), current
upload speed, total uploaded, and a countdown to the next announce paired with a
progress bar. During the download phase, the progress bar shows download
completion percentage instead of the announce countdown, and the speed column
shows `DL NN%`.

**2 tor** - Full torrent list with state (downloading/seeding/error), seeder and
leecher counts, and total uploaded per torrent.

**3 trk** - Per-torrent tracker URLs with current seeder and leecher counts.

**4 spd** - Upload and download speed band editor, multiplier, and numwant. Use
Up/Down to move between the six settings rows and +/- to double or halve values.
numwant steps by 10.

**5 cli** - Client identity: name, peer_id, User-Agent, and key. Shows the exact
`GET /announce?...` query the tracker receives. The `k` key regenerates the
client (new key, new peer_id).

**6 sch** - Seed mode and global pause state.

**7 net** - Network settings: port, numwant, torrent directory, PID file status.

**8 log** - The in-process event ring (same events as the dashboard feed, kept
for up to 50 entries).

**9 cfg** - Mirror of the active `config.toml` values. The `s` key saves the
current config to disk.

### Keys

Navigation:

| Key | Action |
|-----|--------|
| 1-9, 0 | Jump to tab (0 = ratio graph) |
| Left, Right or h, l | Previous / next tab |
| Up, Down or k, j | Select row on list and Speeds tabs |
| Up, Down | Walk the upload multiplier on non-list tabs |
| Esc | Back to Dashboard, or close the help overlay |
| ? | Toggle the help overlay |

Actions:

| Key | Action |
|-----|--------|
| f | Force-announce the selected torrent (resets its countdown) |
| x | Remove the selected/marked torrent(s); asks `y`/`Esc` to confirm (announces Stopped) |
| p | Toggle global pause (all upload stops) |
| r | Resume (clear global pause) |
| + or = | On Speeds tab: double the selected rate; elsewhere: increase multiplier |
| - or _ | On Speeds tab: halve the selected rate; elsewhere: decrease multiplier |
| k | On Client tab: re-init the emulated client (new key) |
| s | On Config tab: save config.toml |
| q or Ctrl+C | Quit (announces Stopped for all torrents, saves state) |

Force-announce (`f`) works on both seeding and downloading torrents. It resets
`last_announce` far enough back that the scheduler's download-tick gate fires on
the next wake.

When a torrent is mid-announce, its row displays `(announcing...)` and its
info-hash is zeroed out. Pressing `f` or `x` on that row produces a feed
message rather than a silent no-op.


## State persistence

Download phase state (info-hash, length, downloaded bytes, seeding flag) is
written to:

```
~/.local/state/Mirage/state.json
```

or, if the XDG state directory is not available:

```
<torrent_dir>/.mirage_state.json
```

The write is atomic: a temp file is written and fsynced, then renamed over the
target. Mirage reads this file at startup and applies matching entries to loaded
torrents, so a torrent that was 40% downloaded at last run continues from 40%
rather than restarting.

A missing or corrupt state file is handled silently; all torrents restart their
download phase from zero.


## JSON stats output

If `output_stats` is configured, Mirage writes a JSON file after each scheduler
tick (approximately every announce interval). The file contains a snapshot of
all active torrents:

```json
{
  "started": "2024-01-15T10:30:00Z",
  "client": "transmission-4.0.6",
  "torrents": [
    {
      "name": "ubuntu-20.04.iso",
      "length": 3145728000,
      "private": false,
      "uploaded": 10737418240,
      "seeders": 42,
      "leechers": 7,
      "next_upload_speed": 1048576,
      "downloaded": 3145728000,
      "left": 0,
      "state": "seeding",
      "urls": ["https://tracker.example.com/announce"]
    }
  ],
  "total_uploaded": 10737418240
}
```

The file is written atomically (temp + rename). A write failure is reported in
the dashboard feed.


## Building from source

```
cargo build           # debug build
cargo build --release # release build, required for scripts/make_app.sh
cargo test            # run the test suite
```

The test suite covers bencode parsing, torrent parsing from real .torrent files,
the speed curve and integration, download phase state transitions, config
loading, and the event ring.


## Project structure

```
src/
  main.rs              Process entry point, global state, shutdown coordination
  config.rs            Config struct, TOML loading, client initialisation
  torrent.rs           Torrent struct, speed curve, download phase, bencode parser
  bencode.rs           Bencode decoder and encoder
  state.rs             Download phase persistence (atomic JSON write)
  engine.rs            Dashboard command handler (ForceAnnounce, Remove, SaveConfig, ReinitClient)
  control.rs           Global pause flag and command channel
  watcher.rs           Filesystem watcher for dynamic torrent add/remove
  directory.rs         Startup torrent folder scan
  transmission.rs      Transmission version auto-detection (Info.plist / binary)
  json_output.rs       Session stats JSON file writer
  utils.rs             Byte formatting, percent-encoding, SHA1
  announcer/
    mod.rs             Module root
    scheduler.rs       Main announce loop, download tick, sleep cadence
    tracker.rs         HTTP announce, bencode response parsing, URL builder
    udp.rs             UDP tracker announce (BEP-15)
  ui/
    mod.rs             TUI entry point, render loop, SIGWINCH/SIGCONT handling
    draw.rs            Terminal I/O: alternate screen, raw mode, paint
    render.rs          Frame builder: all nine tab views, ANSI output
    snapshot.rs        Lock-safe state snapshots for the renderer
    events.rs          In-process event ring buffer
    view.rs            Tab and row selection atomics
    keys.rs            Blocking key reader (OS thread)
scripts/
  make_app.sh          macOS .app bundle builder
assets/
  Mirage.png           App icon source
tests/
  *.torrent            Real .torrent files used by the test suite
```

## Star history

[![Star History Chart](https://api.star-history.com/svg?repos=mangetoncompost/mirage&type=Date&theme=dark)](https://star-history.com/#mangetoncompost/mirage&Date)

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).
