# asciitape

A convenient TUI player for asciinema recordings (v2 and v3 `.cast`).

## Features
- Supports progress bar with mouse drag.
- Supports `.cast.gz`, `.cast.xz`, `.cast.zst`, etc. file formats
- Supports pause, seek, fast forward, rewind, speed control, and fullscreen mode.

## Usage

```bash
asciitape [OPTIONS] <CAST>
```

Options:

```text
--paused       Start paused instead of playing immediately
--fullscreen   Start in fullscreen mode, hiding borders, progress, and controls
-h, --help     Print help
-V, --version  Print version
```

Examples:

```bash
asciitape demo.cast
asciitape --paused demo.cast
asciitape --fullscreen demo.cast
asciitape demo.cast.zst
```

## Controls

| Key / Input | Action |
| --- | --- |
| `Space`, `p` | Play or pause |
| `Left`, `h` | Rewind 5 seconds |
| `Right`, `l` | Fast forward 5 seconds |
| `PageUp`, `b` | Rewind 30 seconds |
| `PageDown`, `f` | Fast forward 30 seconds |
| `Home` | Jump to start |
| `End` | Jump to end |
| `0`-`9` | Jump to 0%-90% |
| `j` | Open jump prompt |
| `+`, `=` | Increase playback speed |
| `-` | Decrease playback speed |
| `Ctrl+1` | Reset speed to 1x |
| `F` | Toggle fullscreen |
| `q`, `Esc` | Quit, or leave fullscreen if fullscreen is active |
| Mouse drag | Drag the progress bar to seek |

Jump prompt formats:

```text
12.5     seconds
01:20    mm:ss
01:02:03 hh:mm:ss
75%      percentage
```

## Supported Cast Formats

Supported:

- asciinema v2 newline-delimited JSON cast files
- asciinema v3 newline-delimited JSON cast files
- plain files and gzip, xz, zstd, bzip2, or lz4-compressed files; compression is detected from file headers, not filename suffixes

The internal timeline is normalized to absolute seconds. v2 absolute event times are used directly, while v3 event intervals are accumulated.

## Known Limitations

- The emulator follows Alacritty's xterm-like behavior, not every historical terminal standard exactly.
- Sixel, iTerm2 images, kitty graphics, and other image protocols are not rendered.
- OSC 8 hyperlinks may be parsed by the emulator but are not exposed in the TUI output.
- Clipboard and terminal query sequences that require a live PTY response are not answered during offline playback.
- Seeking rebuilds terminal state by replaying events from the beginning, which can be slow for very large recordings.

## Build

Install the Rust toolchain first:

- https://www.rust-lang.org/tools/install

```bash
cargo build
cargo build --release
```
