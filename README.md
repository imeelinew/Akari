# Akari

A Rust + Ratatui terminal music player for macOS. Browse a Subsonic server or a local music library, stream or download tracks, and play through macOS AVFoundation, including automatic spatial audio.

## Requirements

- macOS 11 or newer
- Rust 1.88 or newer (stable toolchain)
- Xcode Command Line Tools (`xcode-select --install`)

## Build and run

```sh
cargo run --release --locked
```

Or use `./script/build_and_run.sh`; it builds into the system cache and launches Akari. `./script/build_and_run.sh --build-only` builds without launching. The existing Codex Run action uses this script.

```sh
cargo build --release --locked
cargo install --path . --locked
akari --help
```

On first launch, Akari creates `~/.config/akari/config.toml`. Add a server or local library, then launch it again. `XDG_CONFIG_HOME` and `XDG_CACHE_HOME` are supported.

## Configuration

```toml
[server]
url = "https://music.example.com"
username = "you"
password = "your-password"

[storage]
music_dir = "~/.local/share/akari/music"
external_dirs = ["~/Music/Akari"]

[player]
spatial_audio = true
```

`[server]` is for a Subsonic-compatible server. `external_dirs` accepts a directory containing `manifest.json` or `index.json`, or a parent with indexed album directories. External files are read-only. Akari stores its own downloads separately and only removes files tracked in its own index.

Existing Go-version configuration, namespaced download indexes, metadata caches, Yorune manifests, and playback state remain compatible. The queue and position restore on launch; playback resumes when you press space. The server URL and username determine the storage namespace, so keep them unchanged when migrating.

## Controls

| Key | Action |
| --- | --- |
| `1`–`4` / `tab` / `shift-tab` | Albums, Downloaded, Queue, Transfers |
| `j` / `k` / arrows | Move |
| `g` / `G` / page up / page down | First, last, page |
| `enter` / `esc` | Open or play / back or clear |
| `space` | Play or pause; retry failed playback |
| `p` / `n` | Previous or next |
| `h` / `l` / left / right | Seek 5 seconds |
| `/` | Search; enter applies, esc clears |
| `a` / `N` | Add selected track to queue / play next |
| `J` / `K` | Move selected queue entry down / up |
| `d` / `x` | Download / remove (`x` twice confirms a local deletion) |
| `c` | Cancel selected transfer; in Queue, trim upcoming tracks |
| `C` | Stop playback and clear queue |
| `s` / `r` | Shuffle / repeat off, all, one |
| `o` | Toggle automatic spatial audio |
| `+` / `=` / `-` | Volume up / down |
| `R` | Refresh catalog |
| `?` | Full key map |
| `q` / `ctrl-c` | Save and quit |

After a track has started, `p` returns to the beginning when the position is past 3 seconds. Otherwise, including before playback starts, `p` selects the previous track.

Mouse: click to select, double-click to open or play, scroll the viewport without changing the selection, click the progress bar to seek, and click player controls. Scrolling over volume adjusts it. The interface needs at least 40 × 16 terminal cells.

When the terminal is at least 76 columns wide, the middle area splits into a library on the left and Kitty-protocol **COVER** artwork above **PULSE** on the right. Narrower terminals show only the library. PULSE shows a smoothed 32-band spectrum from the actual audio. Both the spectrum and playback bar take their colors from the playing album, independently of the browsed cover. Scrolling changes only the library viewport; it does not move or retransmit artwork.

Normalized covers use a 64-image memory LRU and a disk cache, with 32 uploaded images retained in the terminal. A single worker coalesces artwork requests. Library rows, playback panels, and spectrum colors stay cached between animation frames; spectrum updates stop while hidden or unfocused.

## Development

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
cargo build --locked
cargo run --locked --example native_audio
python3 tests/tui_smoke.py target/debug/akari
python3 tests/remote_smoke.py target/debug/akari
```

Ratatui 0.30.2 supplies the layout, tabs, stateful table, gauge, and help panel; Crossterm 0.29 handles input. Catalog requests run in background threads. A token-protected loopback HTTP proxy gives AVFoundation range-based playback while sharing cached data with downloads. Only the small AVFoundation bridge is Objective-C; application logic is Rust.

Official API reference: [Ratatui](https://docs.rs/ratatui/0.30.2/ratatui/) and [Crossterm](https://docs.rs/crossterm/0.29.0/crossterm/).
