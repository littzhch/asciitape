# asciitape

A terminal UI player for asciinema v2 and v3 `.cast` recordings.

The player uses Alacritty's terminal emulator core to replay recorded terminal output, then renders the resulting terminal grid with Ratatui and Crossterm.

## Features

- Plays asciinema v2 and v3 recordings.
- Supports play, pause, seek, fast forward, rewind, speed control, and fullscreen mode.
- Supports mouse dragging on the progress bar.
- Uses Alacritty terminal emulation for ANSI/CSI/OSC processing.
- Preserves common cell styles including foreground/background color, bold, dim, italic, underline, inverse, and strikeout.
- Reads recorded terminal metadata such as `TERM`, `term.type`, and v3 `term.theme`.
- Applies profile-aware color strategy for monochrome, ANSI 16-color, ANSI 256-color, and truecolor recordings.
- Handles v3 relative event timing, comments, resize events, and `idle_time_limit`.

## Installation

From this repository:

```bash
cargo install --path .
```

Or run without installing:

```bash
cargo run -- path/to/recording.cast
```

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

The internal timeline is normalized to absolute seconds. v2 absolute event times are used directly, while v3 event intervals are accumulated.

## Terminal Profiles

The player reads terminal metadata from the cast file:

- v2: `env.TERM`
- v2: optional `theme`
- v3: `term.type`
- v3: `term.theme`

Profile strategy:

| Recorded terminal | Strategy |
| --- | --- |
| `dumb`, `vt100`, `vt102` | Monochrome |
| `xterm`, `screen`, `tmux`, `rxvt`, `ansi`, `color` | ANSI 16-color |
| `*-256color` | ANSI 256-color |
| `truecolor`, `24bit` | RGB truecolor |
| Unknown | ANSI 256-color |

When v3 theme data is available, default foreground/background colors and the 0-15 palette are mapped to the recorded colors.

## Architecture

The source is organized into three modules:

- `src/cast.rs`: cast file parsing, v2/v3 timeline normalization, and metadata loading.
- `src/terminal.rs`: Alacritty terminal emulation and conversion from terminal cells to Ratatui styled spans.
- `src/tui.rs`: playback state, keyboard/mouse input, progress bar, fullscreen mode, and rendering layout.

## Development

Useful commands:

```bash
cargo fmt
cargo test
cargo build
cargo run -- --help
```

## CI/CD

- `CI` runs on every push to `master` and on every pull request.
- `Release` runs on pushes to `master`, compares the package version in `Cargo.toml` with the previous commit, and only publishes when the version changed.
- When a new version is detected, the workflow creates a draft GitHub release tagged as `vX.Y.Z`, uploads these binary archives, and then publishes the release after all builds succeed:
  - `x86_64-unknown-linux-musl`
  - `aarch64-unknown-linux-musl`
  - `aarch64-apple-darwin`

To publish a new version, bump `package.version` in `Cargo.toml` and merge that change into `master`.

Release sanity check:

```bash
cargo test
cargo build --release
cargo package --list --allow-dirty
```

## Known Limitations

- The emulator follows Alacritty's xterm-like behavior, not every historical terminal standard exactly.
- Sixel, iTerm2 images, kitty graphics, and other image protocols are not rendered.
- OSC 8 hyperlinks may be parsed by the emulator but are not exposed in the TUI output.
- Clipboard and terminal query sequences that require a live PTY response are not answered during offline playback.
- Seeking rebuilds terminal state by replaying events from the beginning, which can be slow for very large recordings.

## Release Checklist

- Confirm the project license and add the corresponding `license` or `license-file` entry to `Cargo.toml`.
- Run `cargo fmt`, `cargo test`, and `cargo build --release`.
- Test with real v2 and v3 recordings, including color, resize, alternate screen, and mouse-free playback scenarios.
- Update the changelog or release notes before tagging a release.
