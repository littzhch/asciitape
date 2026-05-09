use alacritty_terminal::{
    event::VoidListener,
    grid::Dimensions,
    index::{Column, Line as TermLine},
    term::{
        Config, Term,
        cell::{Cell as AlacrittyCell, Flags as AlacrittyFlags},
        color::Colors as AlacrittyColors,
    },
    vte::ansi::{Color as AlacrittyColor, NamedColor, Processor, Rgb},
};
use anyhow::{Context, Result, bail};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct TerminalMetadata {
    pub term_type: Option<String>,
    pub profile: TerminalProfile,
    pub theme: Option<TerminalTheme>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalProfile {
    Monochrome,
    Ansi16,
    Ansi256,
    TrueColor,
}

#[derive(Debug, Clone)]
pub struct TerminalTheme {
    pub fg: Color,
    pub bg: Color,
    pub palette: Vec<Color>,
}

impl Default for TerminalMetadata {
    fn default() -> Self {
        Self {
            term_type: None,
            profile: TerminalProfile::Ansi256,
            theme: None,
        }
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct TerminalThemeRaw {
    fg: String,
    bg: String,
    palette: String,
}

pub(crate) fn terminal_metadata(
    term_type: Option<String>,
    raw_theme: Option<TerminalThemeRaw>,
) -> Result<TerminalMetadata> {
    let theme = raw_theme.map(parse_terminal_theme).transpose()?;
    let profile = terminal_profile(term_type.as_deref());

    Ok(TerminalMetadata {
        term_type,
        profile,
        theme,
    })
}

fn terminal_profile(term_type: Option<&str>) -> TerminalProfile {
    let Some(term_type) = term_type else {
        return TerminalProfile::Ansi256;
    };
    let term_type = term_type.to_ascii_lowercase();

    if term_type == "dumb" || term_type.starts_with("vt100") || term_type.starts_with("vt102") {
        TerminalProfile::Monochrome
    } else if term_type.contains("truecolor") || term_type.contains("24bit") {
        TerminalProfile::TrueColor
    } else if term_type.contains("256color") {
        TerminalProfile::Ansi256
    } else if term_type.contains("xterm")
        || term_type.contains("screen")
        || term_type.contains("tmux")
        || term_type.contains("rxvt")
        || term_type.contains("ansi")
        || term_type.contains("color")
    {
        TerminalProfile::Ansi16
    } else {
        TerminalProfile::Ansi256
    }
}

fn parse_terminal_theme(raw: TerminalThemeRaw) -> Result<TerminalTheme> {
    let fg = parse_hex_color(&raw.fg).context("invalid theme foreground color")?;
    let bg = parse_hex_color(&raw.bg).context("invalid theme background color")?;
    let palette = raw
        .palette
        .split(':')
        .enumerate()
        .map(|(index, color)| {
            parse_hex_color(color).with_context(|| format!("invalid theme palette color {index}"))
        })
        .collect::<Result<Vec<_>>>()?;

    if palette.is_empty() {
        bail!("theme palette must contain at least one color");
    }

    Ok(TerminalTheme { fg, bg, palette })
}

fn parse_hex_color(value: &str) -> Result<Color> {
    let hex = value
        .strip_prefix('#')
        .context("color must use #rrggbb format")?;
    if hex.len() != 6 {
        bail!("color must use #rrggbb format");
    }

    let red = u8::from_str_radix(&hex[0..2], 16).context("invalid red channel")?;
    let green = u8::from_str_radix(&hex[2..4], 16).context("invalid green channel")?;
    let blue = u8::from_str_radix(&hex[4..6], 16).context("invalid blue channel")?;

    Ok(Color::Rgb(red, green, blue))
}

pub struct AlacrittyEmulator {
    term: Term<VoidListener>,
    parser: Processor,
    width: u16,
    height: u16,
}

impl AlacrittyEmulator {
    pub fn new(width: u16, height: u16, terminal: &TerminalMetadata) -> Self {
        let (width, height) = normalize_terminal_size(width, height);
        let size = AlacrittySize::new(width, height);
        let term = Term::new(alacritty_config(terminal.profile), &size, VoidListener);

        Self {
            term,
            parser: Processor::new(),
            width,
            height,
        }
    }

    pub fn process(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }

    pub fn resize(&mut self, width: u16, height: u16) {
        let (width, height) = normalize_terminal_size(width, height);
        self.term.resize(AlacrittySize::new(width, height));
        self.width = width;
        self.height = height;
    }

    pub fn size(&self) -> (u16, u16) {
        (self.width, self.height)
    }

    pub fn lines(&self, terminal: &TerminalMetadata) -> Vec<Line<'static>> {
        let renderable_content = self.term.renderable_content();
        let colors = renderable_content.colors;
        let grid = self.term.grid();

        (0..self.height)
            .map(|row| {
                let mut spans = Vec::new();
                let mut current_text = String::new();
                let mut current_style = Style::default();
                let mut has_span = false;

                for col in 0..self.width {
                    let cell = &grid[TermLine(row as i32)][Column(col as usize)];

                    if cell.flags.contains(AlacrittyFlags::WIDE_CHAR_SPACER) {
                        continue;
                    }

                    let style = alacritty_cell_style(cell, terminal, colors);
                    let contents = alacritty_cell_contents(cell);

                    if has_span && style != current_style {
                        spans.push(Span::styled(
                            std::mem::take(&mut current_text),
                            current_style,
                        ));
                    }

                    current_style = style;
                    current_text.push_str(&contents);
                    has_span = true;
                }

                if has_span {
                    spans.push(Span::styled(current_text, current_style));
                }

                Line::from(spans)
            })
            .collect()
    }
}

struct AlacrittySize {
    columns: usize,
    screen_lines: usize,
}

impl AlacrittySize {
    fn new(width: u16, height: u16) -> Self {
        Self {
            columns: width as usize,
            screen_lines: height as usize,
        }
    }
}

impl Dimensions for AlacrittySize {
    fn total_lines(&self) -> usize {
        self.screen_lines
    }

    fn screen_lines(&self) -> usize {
        self.screen_lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

fn normalize_terminal_size(width: u16, height: u16) -> (u16, u16) {
    (width.max(2), height.max(1))
}

fn alacritty_config(profile: TerminalProfile) -> Config {
    let mut config = Config::default();

    match profile {
        TerminalProfile::Monochrome => {
            config.scrolling_history = 0;
            config.kitty_keyboard = false;
        }
        TerminalProfile::Ansi16 | TerminalProfile::Ansi256 | TerminalProfile::TrueColor => {}
    }

    config
}

fn alacritty_cell_contents(cell: &AlacrittyCell) -> String {
    if cell.flags.contains(AlacrittyFlags::HIDDEN) {
        return String::from(" ");
    }

    let mut contents = String::new();
    contents.push(cell.c);

    if let Some(zerowidth) = cell.zerowidth() {
        contents.extend(zerowidth.iter());
    }

    contents
}

fn alacritty_cell_style(
    cell: &AlacrittyCell,
    terminal: &TerminalMetadata,
    colors: &AlacrittyColors,
) -> Style {
    let mut style = Style::default()
        .fg(alacritty_color(
            cell.fg,
            terminal,
            colors,
            ColorRole::Foreground,
        ))
        .bg(alacritty_color(
            cell.bg,
            terminal,
            colors,
            ColorRole::Background,
        ));

    if cell.flags.contains(AlacrittyFlags::BOLD) {
        style = style.add_modifier(Modifier::BOLD);
    }
    if cell.flags.contains(AlacrittyFlags::DIM) {
        style = style.add_modifier(Modifier::DIM);
    }
    if cell.flags.contains(AlacrittyFlags::ITALIC) {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if cell.flags.intersects(AlacrittyFlags::ALL_UNDERLINES) {
        style = style.add_modifier(Modifier::UNDERLINED);
    }
    if cell.flags.contains(AlacrittyFlags::INVERSE) {
        style = style.add_modifier(Modifier::REVERSED);
    }
    if cell.flags.contains(AlacrittyFlags::STRIKEOUT) {
        style = style.add_modifier(Modifier::CROSSED_OUT);
    }

    style
}

#[derive(Clone, Copy)]
enum ColorRole {
    Foreground,
    Background,
}

fn alacritty_color(
    color: AlacrittyColor,
    terminal: &TerminalMetadata,
    colors: &AlacrittyColors,
    role: ColorRole,
) -> Color {
    if terminal.profile == TerminalProfile::Monochrome {
        return default_color(terminal, role);
    }

    match color {
        AlacrittyColor::Named(named) => named_color(named, terminal, colors, role),
        AlacrittyColor::Indexed(index) => indexed_color(index, terminal, colors, role),
        AlacrittyColor::Spec(rgb) => rgb_color(rgb, terminal, role),
    }
}

fn default_color(terminal: &TerminalMetadata, role: ColorRole) -> Color {
    let Some(theme) = &terminal.theme else {
        return Color::Reset;
    };

    match role {
        ColorRole::Foreground => theme.fg,
        ColorRole::Background => theme.bg,
    }
}

fn named_color(
    named: NamedColor,
    terminal: &TerminalMetadata,
    colors: &AlacrittyColors,
    role: ColorRole,
) -> Color {
    if let Some(rgb) = colors[named] {
        return rgb_color(rgb, terminal, role);
    }

    if matches!(
        named,
        NamedColor::Foreground | NamedColor::BrightForeground | NamedColor::DimForeground
    ) {
        return default_color(terminal, ColorRole::Foreground);
    }

    if matches!(named, NamedColor::Background) {
        return default_color(terminal, ColorRole::Background);
    }

    named_color_index(named)
        .map(|index| indexed_color(index, terminal, colors, role))
        .unwrap_or_else(|| default_color(terminal, role))
}

fn named_color_index(named: NamedColor) -> Option<u8> {
    match named {
        NamedColor::Black | NamedColor::DimBlack => Some(0),
        NamedColor::Red | NamedColor::DimRed => Some(1),
        NamedColor::Green | NamedColor::DimGreen => Some(2),
        NamedColor::Yellow | NamedColor::DimYellow => Some(3),
        NamedColor::Blue | NamedColor::DimBlue => Some(4),
        NamedColor::Magenta | NamedColor::DimMagenta => Some(5),
        NamedColor::Cyan | NamedColor::DimCyan => Some(6),
        NamedColor::White | NamedColor::DimWhite => Some(7),
        NamedColor::BrightBlack => Some(8),
        NamedColor::BrightRed => Some(9),
        NamedColor::BrightGreen => Some(10),
        NamedColor::BrightYellow => Some(11),
        NamedColor::BrightBlue => Some(12),
        NamedColor::BrightMagenta => Some(13),
        NamedColor::BrightCyan => Some(14),
        NamedColor::BrightWhite => Some(15),
        _ => None,
    }
}

fn indexed_color(
    index: u8,
    terminal: &TerminalMetadata,
    colors: &AlacrittyColors,
    role: ColorRole,
) -> Color {
    if let Some(rgb) = colors[index as usize] {
        return rgb_color(rgb, terminal, role);
    }

    match terminal.profile {
        TerminalProfile::Monochrome => default_color(terminal, role),
        TerminalProfile::Ansi16 => {
            palette_color(nearest_ansi16(xterm_256_rgb(index), terminal), terminal)
        }
        TerminalProfile::Ansi256 | TerminalProfile::TrueColor => terminal
            .theme
            .as_ref()
            .and_then(|theme| theme.palette.get(index as usize).copied())
            .unwrap_or(Color::Indexed(index)),
    }
}

fn rgb_color(rgb: Rgb, terminal: &TerminalMetadata, role: ColorRole) -> Color {
    match terminal.profile {
        TerminalProfile::Monochrome => default_color(terminal, role),
        TerminalProfile::Ansi16 => {
            palette_color(nearest_ansi16((rgb.r, rgb.g, rgb.b), terminal), terminal)
        }
        TerminalProfile::Ansi256 => Color::Indexed(nearest_ansi256(rgb)),
        TerminalProfile::TrueColor => Color::Rgb(rgb.r, rgb.g, rgb.b),
    }
}

fn palette_color(index: u8, terminal: &TerminalMetadata) -> Color {
    terminal
        .theme
        .as_ref()
        .and_then(|theme| theme.palette.get(index as usize).copied())
        .unwrap_or(Color::Indexed(index))
}

fn nearest_ansi16(rgb: (u8, u8, u8), terminal: &TerminalMetadata) -> u8 {
    (0..16)
        .min_by_key(|index| color_distance(rgb, ansi16_rgb(*index, terminal)))
        .unwrap_or(7)
}

fn ansi16_rgb(index: u8, terminal: &TerminalMetadata) -> (u8, u8, u8) {
    terminal
        .theme
        .as_ref()
        .and_then(|theme| theme.palette.get(index as usize))
        .and_then(color_rgb)
        .unwrap_or(ANSI16_RGB[index as usize])
}

fn color_rgb(color: &Color) -> Option<(u8, u8, u8)> {
    match *color {
        Color::Rgb(red, green, blue) => Some((red, green, blue)),
        _ => None,
    }
}

fn nearest_ansi256(rgb: Rgb) -> u8 {
    if rgb.r == rgb.g && rgb.g == rgb.b && (8..=238).contains(&rgb.r) {
        return 232 + ((rgb.r - 8) / 10);
    }

    16 + 36 * nearest_color_cube_level(rgb.r)
        + 6 * nearest_color_cube_level(rgb.g)
        + nearest_color_cube_level(rgb.b)
}

fn nearest_color_cube_level(channel: u8) -> u8 {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

    LEVELS
        .iter()
        .enumerate()
        .min_by_key(|(_, level)| channel.abs_diff(**level))
        .map(|(index, _)| index as u8)
        .unwrap_or(0)
}

fn xterm_256_rgb(index: u8) -> (u8, u8, u8) {
    const CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

    match index {
        0..=15 => ANSI16_RGB[index as usize],
        16..=231 => {
            let index = index - 16;
            let red = CUBE_LEVELS[(index / 36) as usize];
            let green = CUBE_LEVELS[((index % 36) / 6) as usize];
            let blue = CUBE_LEVELS[(index % 6) as usize];
            (red, green, blue)
        }
        232..=255 => {
            let gray = 8 + 10 * (index - 232);
            (gray, gray, gray)
        }
    }
}

fn color_distance(left: (u8, u8, u8), right: (u8, u8, u8)) -> u32 {
    let red = left.0 as i32 - right.0 as i32;
    let green = left.1 as i32 - right.1 as i32;
    let blue = left.2 as i32 - right.2 as i32;

    (red * red + green * green + blue * blue) as u32
}

const ANSI16_RGB: [(u8, u8, u8); 16] = [
    (0x00, 0x00, 0x00),
    (0xcd, 0x00, 0x00),
    (0x00, 0xcd, 0x00),
    (0xcd, 0xcd, 0x00),
    (0x00, 0x00, 0xee),
    (0xcd, 0x00, 0xcd),
    (0x00, 0xcd, 0xcd),
    (0xe5, 0xe5, 0xe5),
    (0x7f, 0x7f, 0x7f),
    (0xff, 0x00, 0x00),
    (0x00, 0xff, 0x00),
    (0xff, 0xff, 0x00),
    (0x5c, 0x5c, 0xff),
    (0xff, 0x00, 0xff),
    (0x00, 0xff, 0xff),
    (0xff, 0xff, 0xff),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alacritty_emulator_preserves_cell_styles() {
        let terminal = TerminalMetadata::default();
        let mut emulator = AlacrittyEmulator::new(3, 1, &terminal);
        emulator.process(b"a\x1b[31mb\x1b[0mc");

        let lines = emulator.lines(&terminal);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans.len(), 3);
        assert_eq!(lines[0].spans[0].content, "a");
        assert_eq!(lines[0].spans[1].content, "b");
        assert_eq!(lines[0].spans[2].content, "c");
        assert_eq!(lines[0].spans[1].style.fg, Some(Color::Indexed(1)));
    }

    #[test]
    fn alacritty_emulator_applies_recorded_theme_palette() {
        let terminal = TerminalMetadata {
            term_type: Some(String::from("xterm-256color")),
            profile: TerminalProfile::Ansi256,
            theme: Some(TerminalTheme {
                fg: Color::Rgb(0xd0, 0xd0, 0xd0),
                bg: Color::Rgb(0x21, 0x21, 0x21),
                palette: vec![Color::Rgb(0x00, 0x00, 0x00), Color::Rgb(0xaa, 0x00, 0x00)],
            }),
        };
        let mut emulator = AlacrittyEmulator::new(3, 1, &terminal);
        emulator.process(b"a\x1b[31mb\x1b[0mc");

        let lines = emulator.lines(&terminal);

        assert_eq!(
            lines[0].spans[0].style.fg,
            Some(Color::Rgb(0xd0, 0xd0, 0xd0))
        );
        assert_eq!(
            lines[0].spans[0].style.bg,
            Some(Color::Rgb(0x21, 0x21, 0x21))
        );
        assert_eq!(
            lines[0].spans[1].style.fg,
            Some(Color::Rgb(0xaa, 0x00, 0x00))
        );
        assert_eq!(
            lines[0].spans[2].style.fg,
            Some(Color::Rgb(0xd0, 0xd0, 0xd0))
        );
    }

    #[test]
    fn ansi16_strategy_quantizes_truecolor() {
        let terminal = TerminalMetadata {
            term_type: Some(String::from("xterm")),
            profile: TerminalProfile::Ansi16,
            theme: None,
        };
        let mut emulator = AlacrittyEmulator::new(1, 1, &terminal);
        emulator.process(b"\x1b[38;2;250;10;10mR");

        let lines = emulator.lines(&terminal);

        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Indexed(9)));
    }

    #[test]
    fn resize_event_changes_alacritty_viewport() {
        let terminal = TerminalMetadata::default();
        let mut emulator = AlacrittyEmulator::new(3, 1, &terminal);
        emulator.process(b"abc");
        emulator.resize(4, 2);

        let lines = emulator.lines(&terminal);

        assert_eq!(lines.len(), 2);
        assert_eq!(emulator.width, 4);
        assert_eq!(emulator.height, 2);
    }
}
