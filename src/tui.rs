use std::{
    io::{self, Stdout},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal as RatatuiTerminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Gauge, Paragraph},
};

use crate::{
    cast::{Cast, EventKind},
    terminal::AlacrittyEmulator,
};

const SHORT_SEEK_SECS: f64 = 5.0;
const LONG_SEEK_SECS: f64 = 30.0;
const TICK_RATE: Duration = Duration::from_millis(16);

#[derive(Debug)]
enum InputMode {
    Normal,
    Jump(String),
}

struct App {
    cast: Cast,
    emulator: AlacrittyEmulator,
    next_event: usize,
    position: f64,
    speed: f64,
    playing: bool,
    fullscreen: bool,
    dragging_progress: bool,
    pending_progress_seek: Option<f64>,
    progress_area: Option<Rect>,
    rendered_canvas_size: Option<(u16, u16)>,
    last_tick: Instant,
    mode: InputMode,
}

impl App {
    fn new(cast: Cast, paused: bool, fullscreen: bool) -> Self {
        let emulator = AlacrittyEmulator::new(cast.width, cast.height, &cast.terminal);

        Self {
            cast,
            emulator,
            next_event: 0,
            position: 0.0,
            speed: 1.0,
            playing: !paused,
            fullscreen,
            dragging_progress: false,
            pending_progress_seek: None,
            progress_area: None,
            rendered_canvas_size: None,
            last_tick: Instant::now(),
            mode: InputMode::Normal,
        }
    }

    fn tick(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_tick).as_secs_f64();
        self.last_tick = now;

        if !self.playing {
            return;
        }

        self.position = (self.position + elapsed * self.speed).min(self.cast.duration);
        self.apply_pending_events();

        if self.position >= self.cast.duration {
            self.playing = false;
        }
    }

    fn toggle_playback(&mut self) {
        if self.position >= self.cast.duration {
            self.seek(0.0);
        }

        self.playing = !self.playing;
        self.last_tick = Instant::now();
    }

    fn seek_relative(&mut self, delta: f64) {
        self.seek(self.position + delta);
    }

    fn seek_percent(&mut self, percent: f64) {
        self.seek(self.cast.duration * percent.clamp(0.0, 1.0));
    }

    fn seek(&mut self, position: f64) {
        let position = position.clamp(0.0, self.cast.duration);

        if position >= self.position {
            self.position = position;
            self.apply_pending_events();
        } else {
            self.position = position;
            self.replay_until_position();
        }

        self.last_tick = Instant::now();
    }

    fn queue_progress_seek(&mut self, percent: f64) {
        self.pending_progress_seek = Some(percent);
    }

    fn apply_pending_progress_seek(&mut self) {
        if let Some(percent) = self.pending_progress_seek.take() {
            self.seek_percent(percent);
        }
    }

    fn speed_up(&mut self) {
        self.speed = (self.speed + 0.25).min(4.0);
    }

    fn speed_down(&mut self) {
        self.speed = (self.speed - 0.25).max(0.25);
    }

    fn reset_speed(&mut self) {
        self.speed = 1.0;
    }

    fn toggle_fullscreen(&mut self) {
        self.fullscreen = !self.fullscreen;
        if self.fullscreen {
            self.dragging_progress = false;
            self.progress_area = None;
        }
    }

    fn status(&self) -> &'static str {
        if self.playing { "Playing" } else { "Paused" }
    }

    fn screen_lines(&self) -> Vec<Line<'static>> {
        self.emulator.lines(&self.cast.terminal)
    }

    fn apply_pending_events(&mut self) {
        while self.next_event < self.cast.events.len() {
            let event = &self.cast.events[self.next_event];

            if event.time > self.position {
                break;
            }

            match event.kind {
                EventKind::Output => self.emulator.process(event.data.as_bytes()),
                EventKind::Resize => {
                    if let Some((width, height)) = parse_resize_event(&event.data) {
                        self.emulator.resize(width, height);
                    }
                }
                EventKind::Other => {}
            }

            self.next_event += 1;
        }
    }

    fn replay_until_position(&mut self) {
        self.emulator =
            AlacrittyEmulator::new(self.cast.width, self.cast.height, &self.cast.terminal);
        self.next_event = 0;
        self.apply_pending_events();
    }
}

pub fn run(cast: Cast, paused: bool, fullscreen: bool) -> Result<()> {
    enable_raw_mode().context("failed to enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)
        .context("failed to enter alternate screen")?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = RatatuiTerminal::new(backend).context("failed to create terminal")?;
    let result = run_app(&mut terminal, App::new(cast, paused, fullscreen));
    restore_terminal(&mut terminal)?;
    result
}

fn restore_terminal(terminal: &mut RatatuiTerminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode().context("failed to disable raw mode")?;
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen
    )
    .context("failed to leave alternate screen")?;
    terminal.show_cursor().context("failed to show cursor")?;
    Ok(())
}

fn run_app(terminal: &mut RatatuiTerminal<CrosstermBackend<Stdout>>, mut app: App) -> Result<()> {
    terminal
        .draw(|frame| draw(frame, &mut app))
        .context("failed to draw frame")?;

    loop {
        if handle_pending_events(&mut app)? {
            return Ok(());
        }

        app.tick();

        terminal
            .draw(|frame| draw(frame, &mut app))
            .context("failed to draw frame")?;
    }
}

fn handle_pending_events(app: &mut App) -> Result<bool> {
    if !event::poll(TICK_RATE).context("failed to poll events")? {
        return Ok(false);
    }

    loop {
        let event = event::read().context("failed to read event")?;
        if handle_event(app, event)? {
            return Ok(true);
        }

        if !event::poll(Duration::ZERO).context("failed to poll queued events")? {
            break;
        }
    }

    app.apply_pending_progress_seek();
    Ok(false)
}

fn handle_event(app: &mut App, event: Event) -> Result<bool> {
    match event {
        Event::Key(key) => {
            app.apply_pending_progress_seek();
            handle_key_event(app, key)
        }
        Event::Mouse(mouse) => handle_mouse_event(app, mouse),
        _ => {
            app.apply_pending_progress_seek();
            Ok(false)
        }
    }
}

fn handle_key_event(app: &mut App, key: KeyEvent) -> Result<bool> {
    if key.kind != KeyEventKind::Press {
        return Ok(false);
    }

    match std::mem::replace(&mut app.mode, InputMode::Normal) {
        InputMode::Normal => handle_normal_key(app, key),
        InputMode::Jump(input) => handle_jump_key(app, key, input),
    }
}

fn handle_mouse_event(app: &mut App, mouse: MouseEvent) -> Result<bool> {
    let Some(progress_area) = app.progress_area else {
        app.dragging_progress = false;
        return Ok(false);
    };

    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left)
            if rect_contains(progress_area, mouse.column, mouse.row) =>
        {
            app.dragging_progress = true;
            queue_mouse_progress_seek(app, progress_area, mouse.column);
        }
        MouseEventKind::Down(_) => app.dragging_progress = false,
        MouseEventKind::Drag(MouseButton::Left) if app.dragging_progress => {
            queue_mouse_progress_seek(app, progress_area, mouse.column);
        }
        MouseEventKind::Up(MouseButton::Left) => {
            if app.dragging_progress {
                queue_mouse_progress_seek(app, progress_area, mouse.column);
            }
            app.dragging_progress = false;
        }
        _ => {}
    }

    Ok(false)
}

fn queue_mouse_progress_seek(app: &mut App, progress_area: Rect, column: u16) {
    let percent = mouse_column_to_progress(progress_area, column);
    app.queue_progress_seek(percent);
}

fn mouse_column_to_progress(progress_area: Rect, column: u16) -> f64 {
    let track = progress_track_area(progress_area);
    if track.width <= 1 {
        return 0.0;
    }

    let left = track.x;
    let right = track.x.saturating_add(track.width.saturating_sub(1));
    let column = column.clamp(left, right);

    (column - left) as f64 / (track.width - 1) as f64
}

fn progress_track_area(progress_area: Rect) -> Rect {
    progress_area
}

fn rect_contains(rect: Rect, column: u16, row: u16) -> bool {
    column >= rect.x
        && column < rect.x.saturating_add(rect.width)
        && row >= rect.y
        && row < rect.y.saturating_add(rect.height)
}

fn handle_normal_key(app: &mut App, key: KeyEvent) -> Result<bool> {
    match key.code {
        KeyCode::Esc if app.fullscreen => app.toggle_fullscreen(),
        KeyCode::Char('q') | KeyCode::Esc => return Ok(true),
        KeyCode::Char(' ') | KeyCode::Char('p') => app.toggle_playback(),
        KeyCode::Right | KeyCode::Char('l') => app.seek_relative(SHORT_SEEK_SECS),
        KeyCode::Left | KeyCode::Char('h') => app.seek_relative(-SHORT_SEEK_SECS),
        KeyCode::PageDown | KeyCode::Char('f') => app.seek_relative(LONG_SEEK_SECS),
        KeyCode::PageUp | KeyCode::Char('b') => app.seek_relative(-LONG_SEEK_SECS),
        KeyCode::Home => app.seek(0.0),
        KeyCode::End => app.seek(app.cast.duration),
        KeyCode::Char('j') => {
            app.fullscreen = false;
            app.mode = InputMode::Jump(String::new());
        }
        KeyCode::Char('F') => app.toggle_fullscreen(),
        KeyCode::Char('+') | KeyCode::Char('=') => app.speed_up(),
        KeyCode::Char('-') => app.speed_down(),
        KeyCode::Char('1') if key.modifiers.contains(KeyModifiers::CONTROL) => app.reset_speed(),
        KeyCode::Char(ch) if ch.is_ascii_digit() => {
            let percent = ch.to_digit(10).unwrap_or_default() as f64 / 10.0;
            app.seek_percent(percent);
        }
        _ => {}
    }

    Ok(false)
}

fn handle_jump_key(app: &mut App, key: KeyEvent, mut input: String) -> Result<bool> {
    let mut keep_prompt = true;

    match key.code {
        KeyCode::Esc => keep_prompt = false,
        KeyCode::Enter => {
            if let Ok(target) = parse_jump_target(&input, app.cast.duration) {
                app.seek(target);
                keep_prompt = false;
            }
        }
        KeyCode::Backspace => {
            input.pop();
        }
        KeyCode::Char(ch) if ch.is_ascii_digit() || matches!(ch, '.' | ':' | '%') => {
            input.push(ch);
        }
        _ => {}
    }

    if keep_prompt {
        app.mode = InputMode::Jump(input);
    }

    Ok(false)
}

fn parse_jump_target(input: &str, duration: f64) -> Result<f64> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(0.0);
    }

    if let Some(percent) = trimmed.strip_suffix('%') {
        let percent: f64 = percent.parse().context("invalid percentage")?;
        return Ok(duration * (percent / 100.0).clamp(0.0, 1.0));
    }

    let seconds = if trimmed.contains(':') {
        parse_colon_time(trimmed)?
    } else {
        trimmed.parse().context("invalid seconds")?
    };

    Ok(seconds.clamp(0.0, duration))
}

fn parse_colon_time(input: &str) -> Result<f64> {
    let parts = input.split(':').collect::<Vec<_>>();
    match parts.as_slice() {
        [minutes, seconds] => {
            let minutes: f64 = minutes.parse().context("invalid minutes")?;
            let seconds: f64 = seconds.parse().context("invalid seconds")?;
            Ok(minutes * 60.0 + seconds)
        }
        [hours, minutes, seconds] => {
            let hours: f64 = hours.parse().context("invalid hours")?;
            let minutes: f64 = minutes.parse().context("invalid minutes")?;
            let seconds: f64 = seconds.parse().context("invalid seconds")?;
            Ok(hours * 3600.0 + minutes * 60.0 + seconds)
        }
        _ => bail!("time must be seconds, mm:ss, hh:mm:ss, or percent"),
    }
}

fn parse_resize_event(data: &str) -> Option<(u16, u16)> {
    let (width, height) = data.split_once('x')?;
    let width = width.parse().ok()?;
    let height = height.parse().ok()?;

    if width == 0 || height == 0 {
        None
    } else {
        Some((width, height))
    }
}

fn draw(frame: &mut Frame<'_>, app: &mut App) {
    if app.fullscreen {
        app.progress_area = None;
        render_terminal_canvas(frame, app, frame.area());
        return;
    }

    let _title = app.cast.title.as_deref().unwrap_or("asciitape");

    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(frame.area());
    render_terminal_canvas(frame, app, vertical[0]);

    let ratio = if app.cast.duration > 0.0 {
        (app.position / app.cast.duration).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let label = format!(
        "{} / {}  {}  {:.2}x",
        format_time(app.position),
        format_time(app.cast.duration),
        app.status(),
        app.speed
    );
    let progress = Gauge::default()
        .gauge_style(
            Style::default()
                .fg(Color::Cyan)
                .bg(Color::Black)
                .add_modifier(Modifier::BOLD),
        )
        .label(label)
        .ratio(ratio);
    app.progress_area = Some(vertical[1]);
    frame.render_widget(progress, vertical[1]);

    let footer = match &app.mode {
        InputMode::Normal => controls_line(),
        InputMode::Jump(input) => Line::from(vec![
            Span::styled("Jump to ", Style::default().fg(Color::Yellow)),
            Span::raw(input),
            Span::styled("_", Style::default().fg(Color::Yellow)),
            Span::raw("  Enter=go  Esc=cancel  e.g. 12.5, 01:20, 75%"),
        ]),
    };
    let footer_layout = match app.cast.terminal.term_type.as_deref() {
        Some(term_type) => {
            let width = term_type.len().saturating_add(2) as u16;
            Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Min(1), Constraint::Length(width)])
                .split(vertical[2])
        }
        None => Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(1), Constraint::Length(0)])
            .split(vertical[2]),
    };
    frame.render_widget(Paragraph::new(footer), footer_layout[0]);

    if let Some(term_type) = app.cast.terminal.term_type.as_deref() {
        let term_type = Line::from(vec![Span::styled(
            term_type,
            Style::default().fg(Color::DarkGray),
        )]);
        frame.render_widget(
            Paragraph::new(term_type).alignment(Alignment::Right),
            footer_layout[1],
        );
    }
}

fn render_terminal_canvas(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let canvas = terminal_canvas_area(area, app.emulator.size());
    let canvas_size = (canvas.width, canvas.height);

    if let Some(previous_size) = app.rendered_canvas_size {
        for clear_area in canvas_shrink_clear_areas(area, previous_size, canvas_size) {
            frame.render_widget(Clear, clear_area);
        }
    }

    app.rendered_canvas_size = Some(canvas_size);
    frame.render_widget(Paragraph::new(app.screen_lines()), canvas);
}

fn terminal_canvas_area(area: Rect, terminal_size: (u16, u16)) -> Rect {
    Rect::new(
        area.x,
        area.y,
        area.width.min(terminal_size.0),
        area.height.min(terminal_size.1),
    )
}

fn canvas_shrink_clear_areas(
    area: Rect,
    previous_size: (u16, u16),
    current_size: (u16, u16),
) -> impl Iterator<Item = Rect> {
    let previous_width = previous_size.0.min(area.width);
    let previous_height = previous_size.1.min(area.height);
    let current_width = current_size.0.min(area.width);
    let current_height = current_size.1.min(area.height);

    let bottom = (current_height < previous_height).then(|| {
        Rect::new(
            area.x,
            area.y.saturating_add(current_height),
            previous_width,
            previous_height - current_height,
        )
    });
    let right = (current_width < previous_width).then(|| {
        Rect::new(
            area.x.saturating_add(current_width),
            area.y,
            previous_width - current_width,
            current_height,
        )
    });

    [bottom, right].into_iter().flatten()
}

fn controls_line() -> Line<'static> {
    Line::from(vec![
        Span::styled("Space/p", Style::default().fg(Color::Yellow)),
        Span::raw(" play/pause  "),
        Span::styled("left/right", Style::default().fg(Color::Yellow)),
        Span::raw(" +/-5s  "),
        Span::styled("PgUp/PgDn", Style::default().fg(Color::Yellow)),
        Span::raw(" +/-30s  "),
        Span::styled("0-9", Style::default().fg(Color::Yellow)),
        Span::raw(" jump %  "),
        Span::styled("j", Style::default().fg(Color::Yellow)),
        Span::raw(" jump  "),
        Span::styled("F", Style::default().fg(Color::Yellow)),
        Span::raw(" fullscreen  "),
        Span::styled("mouse", Style::default().fg(Color::Yellow)),
        Span::raw(" drag progress  "),
        Span::styled("+/-", Style::default().fg(Color::Yellow)),
        Span::raw(" speed  "),
        Span::styled("q", Style::default().fg(Color::Yellow)),
        Span::raw(" quit"),
    ])
}

fn format_time(seconds: f64) -> String {
    let total_seconds = seconds.max(0.0).round() as u64;
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;

    if hours > 0 {
        format!("{hours:02}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_with_duration(duration: f64) -> App {
        App::new(
            Cast {
                width: 2,
                height: 1,
                duration,
                title: None,
                terminal: crate::terminal::TerminalMetadata::default(),
                events: Vec::new(),
            },
            true,
            false,
        )
    }

    fn assert_close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-9,
            "expected {expected}, got {actual}"
        );
    }

    #[test]
    fn queued_progress_seek_applies_latest_drag_position() {
        let mut app = app_with_duration(100.0);

        app.queue_progress_seek(0.25);
        app.queue_progress_seek(0.75);

        assert_close(app.position, 0.0);

        app.apply_pending_progress_seek();

        assert_close(app.position, 75.0);
        assert!(app.pending_progress_seek.is_none());
    }

    #[test]
    fn mouse_column_maps_to_progress_ratio() {
        let progress_area = Rect::new(10, 5, 12, 3);

        assert_close(mouse_column_to_progress(progress_area, 10), 0.0);
        assert_close(mouse_column_to_progress(progress_area, 11), 1.0 / 11.0);
        assert_close(mouse_column_to_progress(progress_area, 20), 10.0 / 11.0);
        assert_close(mouse_column_to_progress(progress_area, 21), 1.0);
        assert_close(mouse_column_to_progress(progress_area, 15), 5.0 / 11.0);
    }

    #[test]
    fn rect_contains_uses_exclusive_right_and_bottom_edges() {
        let rect = Rect::new(2, 3, 4, 2);

        assert!(rect_contains(rect, 2, 3));
        assert!(rect_contains(rect, 5, 4));
        assert!(!rect_contains(rect, 6, 4));
        assert!(!rect_contains(rect, 5, 5));
    }

    #[test]
    fn canvas_area_is_clipped_to_terminal_size() {
        let area = Rect::new(2, 3, 20, 10);

        assert_eq!(terminal_canvas_area(area, (8, 4)), Rect::new(2, 3, 8, 4));
        assert_eq!(terminal_canvas_area(area, (30, 12)), area);
    }

    #[test]
    fn clear_areas_cover_only_canvas_shrink_regions() {
        let area = Rect::new(0, 0, 20, 10);
        let areas = canvas_shrink_clear_areas(area, (10, 4), (6, 2)).collect::<Vec<_>>();

        assert_eq!(areas, vec![Rect::new(0, 2, 10, 2), Rect::new(6, 0, 4, 2)]);
    }

    #[test]
    fn clear_areas_clip_previous_canvas_to_render_area() {
        let area = Rect::new(2, 3, 8, 3);
        let areas = canvas_shrink_clear_areas(area, (10, 4), (6, 2)).collect::<Vec<_>>();

        assert_eq!(areas, vec![Rect::new(2, 5, 8, 1), Rect::new(8, 3, 2, 2)]);
    }
}
