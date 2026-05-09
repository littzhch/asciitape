use std::{collections::HashMap, fs, path::PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::Value;

use crate::terminal::{TerminalMetadata, TerminalThemeRaw, terminal_metadata};

#[derive(Debug)]
pub struct Cast {
    pub width: u16,
    pub height: u16,
    pub duration: f64,
    pub title: Option<String>,
    pub terminal: TerminalMetadata,
    pub events: Vec<CastEvent>,
}

#[derive(Debug)]
pub struct CastEvent {
    pub time: f64,
    pub kind: EventKind,
    pub data: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    Output,
    Resize,
    Other,
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

pub fn load_cast(path: &PathBuf) -> Result<Cast> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read cast file {}", path.display()))?;

    parse_cast(&contents)
}

fn parse_cast(contents: &str) -> Result<Cast> {
    let mut lines = contents.lines();
    let header_line = lines.next().context("empty cast file")?;
    let version: VersionHeader =
        serde_json::from_str(header_line).context("invalid cast header")?;

    match version.version {
        2 => parse_cast_v2(header_line, lines.enumerate()),
        3 => parse_cast_v3(header_line, lines.enumerate()),
        version => {
            bail!("unsupported asciinema cast version {version}; only v2 and v3 are supported")
        }
    }
}

fn parse_cast_v2<'a>(
    header_line: &str,
    lines: impl Iterator<Item = (usize, &'a str)>,
) -> Result<Cast> {
    let header: CastHeaderV2 = serde_json::from_str(header_line).context("invalid v2 header")?;

    validate_size(header.width, header.height)?;
    let idle_time_limit = validate_idle_time_limit(header.idle_time_limit)?;

    let mut events = Vec::new();
    let mut previous_raw_time = 0.0;
    let mut adjusted_time = 0.0;

    for (line_number, line) in lines {
        let Some(raw_event) = parse_event_line(line, line_number + 2)? else {
            continue;
        };

        if raw_event.time < previous_raw_time {
            bail!(
                "event time at line {} is earlier than the previous event",
                line_number + 2
            );
        }

        let event_time = match idle_time_limit {
            Some(limit) => {
                adjusted_time += (raw_event.time - previous_raw_time).min(limit);
                adjusted_time
            }
            None => raw_event.time,
        };
        previous_raw_time = raw_event.time;

        events.push(CastEvent {
            time: event_time,
            kind: raw_event.kind,
            data: raw_event.data,
        });
    }

    let event_duration = events.last().map_or(0.0, |event| event.time);
    let duration = if idle_time_limit.is_some() {
        event_duration
    } else {
        header
            .duration
            .unwrap_or(event_duration)
            .max(event_duration)
    };
    let term_type = header.env.as_ref().and_then(|env| env.get("TERM").cloned());
    let terminal = terminal_metadata(term_type, header.theme)?;

    Ok(Cast {
        width: header.width,
        height: header.height,
        duration,
        title: header.title,
        terminal,
        events,
    })
}

fn parse_cast_v3<'a>(
    header_line: &str,
    lines: impl Iterator<Item = (usize, &'a str)>,
) -> Result<Cast> {
    let header: CastHeaderV3 = serde_json::from_str(header_line).context("invalid v3 header")?;

    let width = header.term.cols;
    let height = header.term.rows;
    validate_size(width, height)?;
    let idle_time_limit = validate_idle_time_limit(header.idle_time_limit)?;
    let terminal = terminal_metadata(header.term.term_type, header.term.theme)?;

    let mut events = Vec::new();
    let mut position = 0.0;

    for (line_number, line) in lines {
        let Some(raw_event) = parse_event_line(line, line_number + 2)? else {
            continue;
        };

        position += match idle_time_limit {
            Some(limit) => raw_event.time.min(limit),
            None => raw_event.time,
        };

        events.push(CastEvent {
            time: position,
            kind: raw_event.kind,
            data: raw_event.data,
        });
    }

    Ok(Cast {
        width,
        height,
        duration: events.last().map_or(0.0, |event| event.time),
        title: header.title,
        terminal,
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
    use ratatui::style::Color;

    use super::*;
    use crate::terminal::TerminalProfile;

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
}
