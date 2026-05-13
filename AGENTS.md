# AGENTS.md

## Repo Facts

- Single Rust binary crate; the package and CLI binary are `asciitape`, not `asciinema-player`.
- `Cargo.toml` requires Rust `1.95` and edition `2024`; CI installs stable Rust with `rustfmt` and `clippy`.
- `Cargo.lock` is checked in for this binary crate; update it through Cargo when dependencies change.

## Commands

- CI order is `cargo fmt --check`, then `cargo clippy --all-targets -- -D warnings`, then `cargo test`, then `cargo build --release`.
- Run a focused test with `cargo test <test_name>`; tests are inline in `src/cast.rs`, `src/terminal.rs`, and `src/tui.rs` with no external services.
- Run the player locally with `cargo run -- [OPTIONS] <path.cast>`; supported options are `--paused` and `--fullscreen`.

## Source Map

- `src/main.rs` is only the clap entrypoint: parse args, `cast::load_cast_header`, then `tui::run`.
- `src/cast.rs` opens plain or compressed cast streams by magic bytes, then parses asciinema v2/v3 headers/events; v2 times are absolute, v3 times are accumulated intervals, `idle_time_limit` caps gaps/intervals, and blank/comment event lines are ignored.
- `src/terminal.rs` wraps `alacritty_terminal` and converts terminal cells/styles/colors into ratatui `Line`s; TERM/theme metadata selects monochrome, ANSI16, ANSI256, or truecolor behavior.
- `src/tui.rs` owns crossterm/ratatui, streams cast events in the background, builds progress previews after load, and seeks by replaying events through `SeekEngine` with cached playback states.

## Gotchas

- Resize events are cast events of kind `"r"` with data like `80x24`; invalid or zero dimensions are ignored in the TUI path.
- Compression support is extension-independent: gzip, xz, zstd, bzip2, and lz4 are detected from file headers, then the decompressed stream must parse as a cast.
- Startup reads only the cast header; do not assume all cast events are available until the loader sends `EventsComplete`.
- The release workflow only creates/publishes GitHub releases on `master` when the package version in `Cargo.toml` changes; Linux musl artifacts are built with `cross`.
