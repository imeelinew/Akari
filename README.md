# Akari

A terminal music player for macOS. Browse a Subsonic server or a local music library, stream or download tracks, and play through `mpv`.

## Requirements

- macOS
- Go 1.26 or newer
- `mpv`

## Install

```sh
git clone https://github.com/imeelinew/Akari.git
cd Akari
go build -o akari .
./akari
```

On first launch, Akari creates `~/.config/akari/config.toml`. Add a server or local library, then launch it again.

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
mpv = "mpv"
```

`[server]` is for a Subsonic-compatible server. `external_dirs` accepts album folders containing `manifest.json` or `index.json`.

## Controls

| Key | Action |
| --- | --- |
| `1`-`4` | Albums, Downloaded, Queue, Transfers |
| `j` / `k` | Move |
| `enter` | Open or play |
| `space` | Play or pause |
| `p` / `n` | Previous or next |
| `/` | Search |
| `a` / `N` | Add to queue or play next |
| `d` / `x` | Download or remove |
| `s` / `r` | Shuffle or repeat |
| `+` / `-` | Volume |
| `?` | Full key map |
| `q` | Quit |
