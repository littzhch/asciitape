use std::{
    collections::HashMap,
    fs::File,
    io::{BufRead, BufReader, Read},
    path::Path,
};

use anyhow::{Context, Result, bail};
use bzip2::read::BzDecoder;
use flate2::read::MultiGzDecoder;
use lz4_flex::frame::FrameDecoder as Lz4Decoder;
use serde::Deserialize;
use serde_json::Value;
use xz2::read::XzDecoder;
use zstd::stream::read::Decoder as ZstdDecoder;

use crate::terminal::{TerminalMetadata, TerminalThemeRaw, terminal_metadata};

#[derive(Debug, Clone)]
pub struct CastHeader {
    pub width: u16,
    pub height: u16,
    pub title: Option<String>,
    pub terminal: TerminalMetadata,
    declared_duration: Option<f64>,
    format: CastFormat,
    idle_time_limit: Option<f64>,
}

#[cfg(test)]
#[derive(Debug)]
struct Cast {
    width: u16,
    height: u16,
    duration: f64,
    title: Option<String>,
    terminal: TerminalMetadata,
    events: Vec<CastEvent>,
}

#[derive(Debug, Clone)]
pub struct CastEvent {
    pub time: f64,
    pub kind: EventKind,
    pub data: String,
}

#[derive(Debug, Clone, Copy)]
enum CastFormat {
    V2,
    V3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    Output,
    Resize,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompressionFormat {
    Plain,
    Gzip,
    Xz,
    Zstd,
    Bzip2,
    Lz4,
}

#[derive(Debug)]
struct RawEvent {
    time: f64,
    kind: EventKind,
    data: String,
}

#[derive(Debug, Deserialize)]
struct VersionHeader {
    version: u64,
}

#[derive(Debug, Deserialize)]
struct CastHeaderV2 {
    width: u16,
    height: u16,
    duration: Option<f64>,
    idle_time_limit: Option<f64>,
    title: Option<String>,
    env: Option<HashMap<String, String>>,
    theme: Option<TerminalThemeRaw>,
}

#[derive(Debug, Deserialize)]
struct CastHeaderV3 {
    term: CastTerm,
    idle_time_limit: Option<f64>,
    title: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CastTerm {
    cols: u16,
    rows: u16,
    #[serde(rename = "type")]
    term_type: Option<String>,
    theme: Option<TerminalThemeRaw>,
}

pub fn load_cast_header(path: &Path) -> Result<CastHeader> {
    let mut reader = open_cast_reader(path)?;
    let mut header_line = String::new();
    let bytes_read = reader
        .read_line(&mut header_line)
        .with_context(|| format!("failed to read cast file {}", path.display()))?;

    if bytes_read == 0 {
        bail!("empty cast file");
    }

    parse_cast_header_line(&header_line)
}

pub(crate) fn open_cast_reader(path: &Path) -> Result<BufReader<Box<dyn Read>>> {
    let file =
        File::open(path).with_context(|| format!("failed to open cast file {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let compression = CompressionFormat::detect(
        reader
            .fill_buf()
            .with_context(|| format!("failed to read file header {}", path.display()))?,
    );

    let reader: Box<dyn Read> = match compression {
        CompressionFormat::Plain => Box::new(reader),
        CompressionFormat::Gzip => Box::new(MultiGzDecoder::new(reader)),
        CompressionFormat::Xz => Box::new(XzDecoder::new(reader)),
        CompressionFormat::Zstd => Box::new(
            ZstdDecoder::new(reader)
                .with_context(|| format!("failed to initialize zstd decoder {}", path.display()))?,
        ),
        CompressionFormat::Bzip2 => Box::new(BzDecoder::new(reader)),
        CompressionFormat::Lz4 => Box::new(Lz4Decoder::new(reader)),
    };

    Ok(BufReader::new(reader))
}

impl CompressionFormat {
    fn detect(bytes: &[u8]) -> Self {
        if bytes.starts_with(&[0x1f, 0x8b]) {
            Self::Gzip
        } else if bytes.starts_with(&[0xfd, b'7', b'z', b'X', b'Z', 0x00]) {
            Self::Xz
        } else if bytes.starts_with(&[0x28, 0xb5, 0x2f, 0xfd])
            || is_zstd_skippable_frame_header(bytes)
        {
            Self::Zstd
        } else if bytes.starts_with(b"BZh") {
            Self::Bzip2
        } else if bytes.starts_with(&[0x04, 0x22, 0x4d, 0x18]) {
            Self::Lz4
        } else {
            Self::Plain
        }
    }
}

fn is_zstd_skippable_frame_header(bytes: &[u8]) -> bool {
    bytes.len() >= 4
        && (0x50..=0x5f).contains(&bytes[0])
        && bytes[1] == 0x2a
        && bytes[2] == 0x4d
        && bytes[3] == 0x18
}

fn parse_cast_header_line(header_line: &str) -> Result<CastHeader> {
    let version: VersionHeader =
        serde_json::from_str(header_line).context("invalid cast header")?;

    match version.version {
        2 => parse_cast_header_v2(header_line),
        3 => parse_cast_header_v3(header_line),
        version => {
            bail!("unsupported asciinema cast version {version}; only v2 and v3 are supported")
        }
    }
}

fn parse_cast_header_v2(header_line: &str) -> Result<CastHeader> {
    let header: CastHeaderV2 = serde_json::from_str(header_line).context("invalid v2 header")?;

    validate_size(header.width, header.height)?;
    let idle_time_limit = validate_idle_time_limit(header.idle_time_limit)?;
    let term_type = header.env.as_ref().and_then(|env| env.get("TERM").cloned());
    let terminal = terminal_metadata(term_type, header.theme)?;

    Ok(CastHeader {
        width: header.width,
        height: header.height,
        title: header.title,
        terminal,
        declared_duration: header.duration,
        format: CastFormat::V2,
        idle_time_limit,
    })
}

fn parse_cast_header_v3(header_line: &str) -> Result<CastHeader> {
    let header: CastHeaderV3 = serde_json::from_str(header_line).context("invalid v3 header")?;

    let width = header.term.cols;
    let height = header.term.rows;
    validate_size(width, height)?;
    let idle_time_limit = validate_idle_time_limit(header.idle_time_limit)?;
    let terminal = terminal_metadata(header.term.term_type, header.term.theme)?;

    Ok(CastHeader {
        width,
        height,
        title: header.title,
        terminal,
        declared_duration: None,
        format: CastFormat::V3,
        idle_time_limit,
    })
}

pub struct CastEventParser {
    format: CastFormat,
    idle_time_limit: Option<f64>,
    declared_duration: Option<f64>,
    previous_raw_time: f64,
    adjusted_time: f64,
    position: f64,
    event_duration: f64,
    line_number: usize,
}

impl CastEventParser {
    pub fn new(header: &CastHeader) -> Self {
        Self {
            format: header.format,
            idle_time_limit: header.idle_time_limit,
            declared_duration: header.declared_duration,
            previous_raw_time: 0.0,
            adjusted_time: 0.0,
            position: 0.0,
            event_duration: 0.0,
            line_number: 2,
        }
    }

    pub fn parse_line(&mut self, line: &str) -> Result<Option<CastEvent>> {
        let line_number = self.line_number;
        self.line_number += 1;

        let Some(raw_event) = parse_event_line(line, line_number)? else {
            return Ok(None);
        };

        match self.format {
            CastFormat::V2 => self.parse_v2_event(raw_event, line_number),
            CastFormat::V3 => Ok(Some(self.parse_v3_event(raw_event))),
        }
    }

    pub fn duration(&self) -> f64 {
        match self.format {
            CastFormat::V2 if self.idle_time_limit.is_none() => self
                .declared_duration
                .unwrap_or(self.event_duration)
                .max(self.event_duration),
            CastFormat::V2 | CastFormat::V3 => self.event_duration,
        }
    }

    fn parse_v2_event(
        &mut self,
        raw_event: RawEvent,
        line_number: usize,
    ) -> Result<Option<CastEvent>> {
        if raw_event.time < self.previous_raw_time {
            bail!("event time at line {line_number} is earlier than the previous event");
        }

        let event_time = match self.idle_time_limit {
            Some(limit) => {
                self.adjusted_time += (raw_event.time - self.previous_raw_time).min(limit);
                self.adjusted_time
            }
            None => raw_event.time,
        };
        self.previous_raw_time = raw_event.time;
        self.event_duration = event_time;

        Ok(Some(CastEvent {
            time: event_time,
            kind: raw_event.kind,
            data: raw_event.data,
        }))
    }

    fn parse_v3_event(&mut self, raw_event: RawEvent) -> CastEvent {
        self.position += match self.idle_time_limit {
            Some(limit) => raw_event.time.min(limit),
            None => raw_event.time,
        };
        self.event_duration = self.position;

        CastEvent {
            time: self.position,
            kind: raw_event.kind,
            data: raw_event.data,
        }
    }
}

#[cfg(test)]
fn parse_cast(contents: &str) -> Result<Cast> {
    let mut lines = contents.lines();
    let header_line = lines.next().context("empty cast file")?;
    let header = parse_cast_header_line(header_line)?;
    let mut parser = CastEventParser::new(&header);
    let mut events = Vec::new();

    for line in lines {
        if let Some(event) = parser.parse_line(line)? {
            events.push(event);
        }
    }

    Ok(Cast {
        width: header.width,
        height: header.height,
        duration: parser.duration(),
        title: header.title,
        terminal: header.terminal,
        events,
    })
}

fn validate_size(width: u16, height: u16) -> Result<()> {
    if width == 0 || height == 0 {
        bail!("cast header must contain non-zero terminal size");
    }

    Ok(())
}

fn validate_idle_time_limit(idle_time_limit: Option<f64>) -> Result<Option<f64>> {
    match idle_time_limit {
        Some(limit) if !limit.is_finite() || limit < 0.0 => {
            bail!("idle_time_limit must be a non-negative finite number")
        }
        _ => Ok(idle_time_limit),
    }
}

fn parse_event_line(line: &str, line_number: usize) -> Result<Option<RawEvent>> {
    let trimmed = line.trim_start();

    if trimmed.is_empty() || trimmed.starts_with('#') {
        return Ok(None);
    }

    let value: Value = serde_json::from_str(line)
        .with_context(|| format!("invalid event JSON at line {line_number}"))?;
    let event =
        parse_event(value).with_context(|| format!("invalid event shape at line {line_number}"))?;

    Ok(Some(event))
}

fn parse_event(value: Value) -> Result<RawEvent> {
    let values = value.as_array().context("event must be a JSON array")?;
    let [time, kind, data, ..] = values.as_slice() else {
        bail!("event must contain at least time, type, and data");
    };

    let time = time.as_f64().context("event time must be a number")?;
    if !time.is_finite() || time < 0.0 {
        bail!("event time must be a non-negative finite number");
    }

    let kind = match kind.as_str().context("event type must be a string")? {
        "o" => EventKind::Output,
        "r" => EventKind::Resize,
        _ => EventKind::Other,
    };
    let data = data
        .as_str()
        .context("event data must be a string")?
        .to_owned();

    Ok(RawEvent { time, kind, data })
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::{Read as _, Write as _},
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    use ratatui::style::Color;

    use super::*;
    use crate::terminal::TerminalProfile;

    const SIMPLE_CAST: &str = r#"{"version":2,"width":80,"height":24,"duration":1.0}
[0.1,"o","hello"]
"#;

    static NEXT_TEST_FILE_ID: AtomicU64 = AtomicU64::new(0);

    struct TestFile {
        path: PathBuf,
    }

    impl Drop for TestFile {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
        }
    }

    fn write_temp_file(label: &str, bytes: &[u8]) -> TestFile {
        let file_id = NEXT_TEST_FILE_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "asciitape-cast-test-{}-{file_id}-{label}",
            std::process::id()
        ));
        fs::write(&path, bytes).unwrap();
        TestFile { path }
    }

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    fn xz(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = xz2::write::XzEncoder::new(Vec::new(), 6);
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    fn zstd(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), 0).unwrap();
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    fn bzip2(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::default());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    fn lz4(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = lz4_flex::frame::FrameEncoder::new(Vec::new());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    fn assert_close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-9,
            "expected {expected}, got {actual}"
        );
    }

    #[test]
    fn parses_v2_absolute_timeline() {
        let cast = parse_cast(
            r#"{"version":2,"width":80,"height":24,"duration":2.0,"title":"v2"}
[0.5,"o","hello"]
[1.25,"o"," world"]
"#,
        )
        .unwrap();

        assert_eq!(cast.width, 80);
        assert_eq!(cast.height, 24);
        assert_eq!(cast.title.as_deref(), Some("v2"));
        assert_close(cast.duration, 2.0);
        assert_eq!(cast.events.len(), 2);
        assert_close(cast.events[0].time, 0.5);
        assert_close(cast.events[1].time, 1.25);
        assert_eq!(cast.events[0].kind, EventKind::Output);
    }

    #[test]
    fn parses_v3_relative_timeline_and_comments() {
        let cast = parse_cast(
            r#"{"version":3,"term":{"cols":100,"rows":40},"title":"v3"}
# ignored comment
[0.5,"o","hello"]
[1.25,"o"," world"]
"#,
        )
        .unwrap();

        assert_eq!(cast.width, 100);
        assert_eq!(cast.height, 40);
        assert_eq!(cast.title.as_deref(), Some("v3"));
        assert_eq!(cast.events.len(), 2);
        assert_close(cast.events[0].time, 0.5);
        assert_close(cast.events[1].time, 1.75);
        assert_close(cast.duration, 1.75);
    }

    #[test]
    fn parses_v2_term_from_env() {
        let cast = parse_cast(
            r#"{"version":2,"width":80,"height":24,"env":{"TERM":"vt100"}}
[0.5,"o","hello"]
"#,
        )
        .unwrap();

        assert_eq!(cast.terminal.term_type.as_deref(), Some("vt100"));
        assert_eq!(cast.terminal.profile, TerminalProfile::Monochrome);
    }

    #[test]
    fn parses_v3_term_type_and_theme() {
        let cast = parse_cast(
            r##"{"version":3,"term":{"cols":80,"rows":24,"type":"xterm-256color","theme":{"fg":"#d0d0d0","bg":"#212121","palette":"#000000:#aa0000:#00aa00"}}}
[0.5,"o","hello"]
"##,
        )
        .unwrap();
        let theme = cast.terminal.theme.as_ref().unwrap();

        assert_eq!(cast.terminal.term_type.as_deref(), Some("xterm-256color"));
        assert_eq!(cast.terminal.profile, TerminalProfile::Ansi256);
        assert_eq!(theme.fg, Color::Rgb(0xd0, 0xd0, 0xd0));
        assert_eq!(theme.bg, Color::Rgb(0x21, 0x21, 0x21));
        assert_eq!(theme.palette[1], Color::Rgb(0xaa, 0x00, 0x00));
    }

    #[test]
    fn v3_idle_time_limit_caps_intervals() {
        let cast = parse_cast(
            r#"{"version":3,"term":{"cols":80,"rows":24},"idle_time_limit":1.0}
[5.0,"o","a"]
[0.25,"o","b"]
[2.0,"o","c"]
"#,
        )
        .unwrap();

        assert_close(cast.events[0].time, 1.0);
        assert_close(cast.events[1].time, 1.25);
        assert_close(cast.events[2].time, 2.25);
        assert_close(cast.duration, 2.25);
    }

    #[test]
    fn v2_idle_time_limit_caps_absolute_gaps() {
        let cast = parse_cast(
            r#"{"version":2,"width":80,"height":24,"duration":8.0,"idle_time_limit":1.5}
[2.0,"o","a"]
[7.0,"o","b"]
"#,
        )
        .unwrap();

        assert_close(cast.events[0].time, 1.5);
        assert_close(cast.events[1].time, 3.0);
        assert_close(cast.duration, 3.0);
    }

    #[test]
    fn open_cast_reader_detects_common_compression_by_magic() {
        let cases = [
            ("gzip-without-cast-suffix", gzip(SIMPLE_CAST.as_bytes())),
            ("xz-without-cast-suffix", xz(SIMPLE_CAST.as_bytes())),
            ("zstd-without-cast-suffix", zstd(SIMPLE_CAST.as_bytes())),
            ("bzip2-without-cast-suffix", bzip2(SIMPLE_CAST.as_bytes())),
            ("lz4-without-cast-suffix", lz4(SIMPLE_CAST.as_bytes())),
        ];

        for (label, bytes) in cases {
            let file = write_temp_file(label, &bytes);
            let mut reader = open_cast_reader(&file.path).unwrap();
            let mut contents = String::new();

            reader.read_to_string(&mut contents).unwrap();

            assert_eq!(contents, SIMPLE_CAST, "{label}");
            assert_eq!(load_cast_header(&file.path).unwrap().width, 80, "{label}");
        }
    }

    #[test]
    fn load_cast_header_ignores_suffix_for_plain_input() {
        let file = write_temp_file("plain.cast.gz", SIMPLE_CAST.as_bytes());
        let header = load_cast_header(&file.path).unwrap();

        assert_eq!(header.width, 80);
        assert_eq!(header.height, 24);
    }
}
