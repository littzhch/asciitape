use std::{
    fs::File,
    io::{self, BufRead, BufReader, Stdout},
    path::PathBuf,
    sync::{
        Arc,
        mpsc::{self, Receiver, Sender},
    },
    thread,
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
    widgets::{Gauge, Paragraph},
};

use crate::{
    cast::{CastEvent, CastEventParser, CastHeader, EventKind},
    terminal::AlacrittyEmulator,
};

const PREVIEW_FRAME_COUNT: usize = 2000;
const SHORT_SEEK_SECS: f64 = 5.0;
const LONG_SEEK_SECS: f64 = 30.0;
const TICK_RATE: Duration = Duration::from_millis(16);
const EVENT_BATCH_SIZE: usize = 512;
const EVENT_BATCH_LATENCY: Duration = Duration::from_millis(8);
const WORKER_MESSAGES_PER_TICK: usize = 32;

#[derive(Debug)]
enum InputMode {
    Normal,
    Jump(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoadState {
    StreamingEvents,
    BuildingFrames,
    Ready,
}

enum EventBuffer {
    Streaming(Vec<CastEvent>),
    Loaded(Arc<Vec<CastEvent>>),
}

impl EventBuffer {
    fn len(&self) -> usize {
        match self {
            Self::Streaming(events) => events.len(),
            Self::Loaded(events) => events.len(),
        }
    }

    fn get(&self, index: usize) -> Option<&CastEvent> {
        match self {
            Self::Streaming(events) => events.get(index),
            Self::Loaded(events) => events.get(index),
        }
    }

    fn push_batch(&mut self, batch: Vec<CastEvent>) {
        if let Self::Streaming(events) = self {
            events.extend(batch);
        }
    }

    fn promote_to_loaded(&mut self) -> Arc<Vec<CastEvent>> {
        match std::mem::replace(self, Self::Streaming(Vec::new())) {
            Self::Streaming(events) => {
                let events = Arc::new(events);
                *self = Self::Loaded(Arc::clone(&events));
                events
            }
            Self::Loaded(events) => {
                *self = Self::Loaded(Arc::clone(&events));
                events
            }
        }
    }
}

struct PreviewCache {
    duration: f64,
    frames: Vec<PreviewFrame>,
}

struct PreviewFrame {
    lines: Vec<Line<'static>>,
}

impl PreviewCache {
    fn nearest_frame(&self, position: f64) -> Option<&PreviewFrame> {
        let index = preview_frame_index(position, self.duration, self.frames.len())?;
        self.frames.get(index)
    }
}

enum WorkerMessage {
    EventsLoaded(Vec<CastEvent>),
    EventsComplete { duration: f64 },
    PreviewReady(PreviewCache),
    Failed(String),
}

struct App {
    header: CastHeader,
    events: EventBuffer,
    emulator: AlacrittyEmulator,
    next_event: usize,
    position: f64,
    duration: Option<f64>,
    loaded_until: f64,
    speed: f64,
    playing: bool,
    buffering: bool,
    fullscreen: bool,
    dragging_progress: bool,
    preview_position: Option<f64>,
    preview_cache: Option<PreviewCache>,
    progress_area: Option<Rect>,
    last_tick: Instant,
    mode: InputMode,
    load_state: LoadState,
    worker_rx: Receiver<WorkerMessage>,
    worker_tx: Sender<WorkerMessage>,
}

impl App {
    fn new(path: PathBuf, header: CastHeader, paused: bool, fullscreen: bool) -> Self {
        let emulator = AlacrittyEmulator::new(header.width, header.height, &header.terminal);
        let (worker_tx, worker_rx) = mpsc::channel();
        start_event_loader(path, header.clone(), worker_tx.clone());

        Self {
            header,
            events: EventBuffer::Streaming(Vec::new()),
            emulator,
            next_event: 0,
            position: 0.0,
            duration: None,
            loaded_until: 0.0,
            speed: 1.0,
            playing: !paused,
            buffering: false,
            fullscreen,
            dragging_progress: false,
            preview_position: None,
            preview_cache: None,
            progress_area: None,
            last_tick: Instant::now(),
            mode: InputMode::Normal,
            load_state: LoadState::StreamingEvents,
            worker_rx,
            worker_tx,
        }
    }

    fn process_worker_messages(&mut self) -> Result<()> {
        for _ in 0..WORKER_MESSAGES_PER_TICK {
            let Ok(message) = self.worker_rx.try_recv() else {
                break;
            };

            self.handle_worker_message(message)?;
        }

        Ok(())
    }

    fn handle_worker_message(&mut self, message: WorkerMessage) -> Result<()> {
        match message {
            WorkerMessage::EventsLoaded(batch) => {
                if let Some(last_event) = batch.last() {
                    self.loaded_until = self.loaded_until.max(last_event.time);
                }
                self.events.push_batch(batch);
            }
            WorkerMessage::EventsComplete { duration } => {
                self.duration = Some(duration);
                self.load_state = LoadState::BuildingFrames;

                let events = self.events.promote_to_loaded();
                start_preview_builder(
                    events,
                    self.header.clone(),
                    duration,
                    self.worker_tx.clone(),
                );

                if self.position >= duration {
                    self.playing = false;
                }
            }
            WorkerMessage::PreviewReady(cache) => {
                self.preview_cache = Some(cache);
                self.load_state = LoadState::Ready;
            }
            WorkerMessage::Failed(error) => bail!("{error}"),
        }

        Ok(())
    }

    fn tick(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_tick).as_secs_f64();
        self.last_tick = now;
        self.buffering = false;

        if !self.playing {
            return;
        }

        let target = self.position + elapsed * self.speed;
        let max_position = self.max_playable_position();
        if target > max_position {
            self.position = max_position;
            self.buffering = matches!(self.load_state, LoadState::StreamingEvents);
        } else {
            self.position = target;
        }

        self.apply_pending_events();

        if let Some(duration) = self.duration
            && self.position >= duration
        {
            self.playing = false;
        }
    }

    fn max_playable_position(&self) -> f64 {
        match self.load_state {
            LoadState::StreamingEvents => self.loaded_until,
            LoadState::BuildingFrames | LoadState::Ready => {
                self.duration.unwrap_or(self.loaded_until)
            }
        }
    }

    fn toggle_playback(&mut self) {
        if let Some(duration) = self.duration
            && self.position >= duration
        {
            self.seek(0.0);
        }

        self.playing = !self.playing;
        self.last_tick = Instant::now();
    }

    fn seek_relative(&mut self, delta: f64) {
        self.seek(self.position + delta);
    }

    fn seek_percent(&mut self, percent: f64) {
        if let Some(duration) = self.duration {
            self.seek(duration * percent.clamp(0.0, 1.0));
        }
    }

    fn seek_to_end(&mut self) {
        if let Some(duration) = self.duration {
            self.seek(duration);
        }
    }

    fn seek(&mut self, position: f64) {
        let Some(duration) = self.duration else {
            return;
        };
        let position = position.clamp(0.0, duration);

        if position >= self.position {
            self.position = position;
            self.apply_pending_events();
        } else {
            self.position = position;
            self.replay_until_position();
        }

        self.last_tick = Instant::now();
    }

    fn update_progress_preview(&mut self, percent: f64) {
        if !self.is_ready() {
            return;
        }

        if let Some(duration) = self.duration {
            self.preview_position = Some(duration * percent.clamp(0.0, 1.0));
        }
    }

    fn finish_progress_drag(&mut self, percent: f64) {
        if self.is_ready() {
            let target = self
                .duration
                .map(|duration| duration * percent.clamp(0.0, 1.0));
            self.finish_progress_drag_at(target);
        } else {
            self.cancel_progress_drag();
        }
    }

    fn finish_current_progress_drag(&mut self) {
        if self.is_ready() {
            self.finish_progress_drag_at(self.preview_position);
        } else {
            self.cancel_progress_drag();
        }
    }

    fn finish_progress_drag_at(&mut self, target: Option<f64>) {
        self.dragging_progress = false;
        self.preview_position = None;

        if let Some(target) = target {
            self.seek(target);
        }
    }

    fn cancel_progress_drag(&mut self) {
        self.dragging_progress = false;
        self.preview_position = None;
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
            self.cancel_progress_drag();
            self.progress_area = None;
        }
    }

    fn status(&self) -> &'static str {
        if self.buffering {
            "Buffering"
        } else if self.playing {
            "Playing"
        } else {
            "Paused"
        }
    }

    fn is_ready(&self) -> bool {
        matches!(self.load_state, LoadState::Ready)
    }

    fn display_lines(&self) -> Vec<Line<'static>> {
        if self.dragging_progress
            && let (Some(cache), Some(position)) = (&self.preview_cache, self.preview_position)
            && let Some(frame) = cache.nearest_frame(position)
        {
            return frame.lines.clone();
        }

        self.emulator.lines(&self.header.terminal)
    }

    fn loading_line(&self) -> Line<'static> {
        let label = match self.load_state {
            LoadState::StreamingEvents => "Loading cast...",
            LoadState::BuildingFrames => "Building preview cache...",
            LoadState::Ready => "",
        };

        Line::from(vec![
            Span::styled(label, Style::default().fg(Color::Yellow)),
            Span::raw(format!(
                "  {}  {}  {:.2}x",
                format_time(self.position),
                self.status(),
                self.speed
            )),
        ])
    }

    fn apply_pending_events(&mut self) {
        while self.next_event < self.events.len() {
            let Some(event) = self.events.get(self.next_event) else {
                break;
            };

            if event.time > self.position {
                break;
            }

            apply_cast_event(&mut self.emulator, event);
            self.next_event += 1;
        }
    }

    fn replay_until_position(&mut self) {
        self.emulator =
            AlacrittyEmulator::new(self.header.width, self.header.height, &self.header.terminal);
        self.next_event = 0;
        self.apply_pending_events();
    }
}

pub fn run(path: PathBuf, header: CastHeader, paused: bool, fullscreen: bool) -> Result<()> {
    enable_raw_mode().context("failed to enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)
        .context("failed to enter alternate screen")?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = RatatuiTerminal::new(backend).context("failed to create terminal")?;
    let result = run_app(&mut terminal, App::new(path, header, paused, fullscreen));
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
    ensure_supported_width(terminal)?;
    terminal
        .draw(|frame| draw(frame, &mut app))
        .context("failed to draw frame")?;

    loop {
        app.process_worker_messages()?;

        if handle_pending_events(&mut app)? {
            return Ok(());
        }

        app.process_worker_messages()?;
        app.tick();

        ensure_supported_width(terminal)?;
        terminal
            .draw(|frame| draw(frame, &mut app))
            .context("failed to draw frame")?;
    }
}

fn ensure_supported_width(terminal: &RatatuiTerminal<CrosstermBackend<Stdout>>) -> Result<()> {
    let size = terminal.size().context("failed to read terminal size")?;
    if usize::from(size.width) > PREVIEW_FRAME_COUNT {
        bail!("terminal windows wider than {PREVIEW_FRAME_COUNT} columns are not supported");
    }

    Ok(())
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

    Ok(false)
}

fn handle_event(app: &mut App, event: Event) -> Result<bool> {
    match event {
        Event::Key(key) => handle_key_event(app, key),
        Event::Mouse(mouse) => handle_mouse_event(app, mouse),
        _ => Ok(false),
    }
}

fn handle_key_event(app: &mut App, key: KeyEvent) -> Result<bool> {
    if key.kind != KeyEventKind::Press {
        return Ok(false);
    }

    if app.dragging_progress {
        app.finish_current_progress_drag();
    }

    match std::mem::replace(&mut app.mode, InputMode::Normal) {
        InputMode::Normal => handle_normal_key(app, key),
        InputMode::Jump(input) => handle_jump_key(app, key, input),
    }
}

fn handle_mouse_event(app: &mut App, mouse: MouseEvent) -> Result<bool> {
    if !app.is_ready() {
        app.cancel_progress_drag();
        return Ok(false);
    }

    let Some(progress_area) = app.progress_area else {
        app.cancel_progress_drag();
        return Ok(false);
    };

    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left)
            if rect_contains(progress_area, mouse.column, mouse.row) =>
        {
            app.dragging_progress = true;
            update_mouse_progress_preview(app, progress_area, mouse.column);
        }
        MouseEventKind::Down(_) if app.dragging_progress => app.finish_current_progress_drag(),
        MouseEventKind::Down(_) => app.cancel_progress_drag(),
        MouseEventKind::Drag(MouseButton::Left) if app.dragging_progress => {
            update_mouse_progress_preview(app, progress_area, mouse.column);
        }
        MouseEventKind::Up(MouseButton::Left) => {
            if app.dragging_progress {
                finish_mouse_progress_drag(app, progress_area, mouse.column);
            } else {
                app.cancel_progress_drag();
            }
        }
        _ if app.dragging_progress => app.finish_current_progress_drag(),
        _ => {}
    }

    Ok(false)
}

fn update_mouse_progress_preview(app: &mut App, progress_area: Rect, column: u16) {
    let percent = mouse_column_to_progress(progress_area, column);
    app.update_progress_preview(percent);
}

fn finish_mouse_progress_drag(app: &mut App, progress_area: Rect, column: u16) {
    let percent = mouse_column_to_progress(progress_area, column);
    app.finish_progress_drag(percent);
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
        KeyCode::End => app.seek_to_end(),
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
            if let Some(duration) = app.duration
                && let Ok(target) = parse_jump_target(&input, duration)
            {
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

fn apply_cast_event(emulator: &mut AlacrittyEmulator, event: &CastEvent) {
    match event.kind {
        EventKind::Output => emulator.process(event.data.as_bytes()),
        EventKind::Resize => {
            if let Some((width, height)) = parse_resize_event(&event.data) {
                emulator.resize(width, height);
            }
        }
        EventKind::Other => {}
    }
}

fn start_event_loader(path: PathBuf, header: CastHeader, tx: Sender<WorkerMessage>) {
    let _ = thread::spawn(move || {
        if let Err(error) = load_events(path, header, &tx) {
            let _ = tx.send(WorkerMessage::Failed(error.to_string()));
        }
    });
}

fn load_events(path: PathBuf, header: CastHeader, tx: &Sender<WorkerMessage>) -> Result<()> {
    let file = File::open(&path)
        .with_context(|| format!("failed to open cast file {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut header_line = String::new();
    let bytes_read = reader
        .read_line(&mut header_line)
        .with_context(|| format!("failed to read cast file {}", path.display()))?;

    if bytes_read == 0 {
        bail!("empty cast file");
    }

    let mut parser = CastEventParser::new(&header);
    let mut batch = Vec::with_capacity(EVENT_BATCH_SIZE);
    let mut last_flush = Instant::now();

    for line in reader.lines() {
        let line = line.with_context(|| format!("failed to read cast file {}", path.display()))?;
        if let Some(event) = parser.parse_line(&line)? {
            batch.push(event);
        }

        if batch.len() >= EVENT_BATCH_SIZE || last_flush.elapsed() >= EVENT_BATCH_LATENCY {
            if !flush_event_batch(tx, &mut batch) {
                return Ok(());
            }
            last_flush = Instant::now();
        }
    }

    if !flush_event_batch(tx, &mut batch) {
        return Ok(());
    }

    let duration = parser.duration();
    let _ = tx.send(WorkerMessage::EventsComplete { duration });
    Ok(())
}

fn flush_event_batch(tx: &Sender<WorkerMessage>, batch: &mut Vec<CastEvent>) -> bool {
    if batch.is_empty() {
        return true;
    }

    let events = std::mem::take(batch);
    tx.send(WorkerMessage::EventsLoaded(events)).is_ok()
}

fn start_preview_builder(
    events: Arc<Vec<CastEvent>>,
    header: CastHeader,
    duration: f64,
    tx: Sender<WorkerMessage>,
) {
    let _ = thread::spawn(move || {
        let cache = build_preview_cache(events.as_ref(), &header, duration);
        let _ = tx.send(WorkerMessage::PreviewReady(cache));
    });
}

fn build_preview_cache(events: &[CastEvent], header: &CastHeader, duration: f64) -> PreviewCache {
    let mut emulator = AlacrittyEmulator::new(header.width, header.height, &header.terminal);
    let mut next_event = 0;
    let mut frames = Vec::with_capacity(PREVIEW_FRAME_COUNT);

    for frame_index in 0..PREVIEW_FRAME_COUNT {
        let target = preview_frame_time(frame_index, duration, PREVIEW_FRAME_COUNT);
        apply_events_until(events, &mut next_event, target, &mut emulator);
        frames.push(PreviewFrame {
            lines: emulator.lines(&header.terminal),
        });
    }

    PreviewCache { duration, frames }
}

fn apply_events_until(
    events: &[CastEvent],
    next_event: &mut usize,
    position: f64,
    emulator: &mut AlacrittyEmulator,
) {
    while *next_event < events.len() {
        let event = &events[*next_event];
        if event.time > position {
            break;
        }

        apply_cast_event(emulator, event);
        *next_event += 1;
    }
}

fn preview_frame_time(index: usize, duration: f64, frame_count: usize) -> f64 {
    if frame_count <= 1 || duration <= 0.0 {
        return 0.0;
    }

    duration * index as f64 / (frame_count - 1) as f64
}

fn preview_frame_index(position: f64, duration: f64, frame_count: usize) -> Option<usize> {
    if frame_count == 0 {
        return None;
    }

    if frame_count == 1 || duration <= 0.0 {
        return Some(0);
    }

    let ratio = (position / duration).clamp(0.0, 1.0);
    Some((ratio * (frame_count - 1) as f64).round() as usize)
}

fn draw(frame: &mut Frame<'_>, app: &mut App) {
    if app.fullscreen {
        app.progress_area = None;
        frame.render_widget(Paragraph::new(app.display_lines()), frame.area());
        return;
    }

    let _title = app.header.title.as_deref().unwrap_or("asciitape");

    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(frame.area());
    frame.render_widget(Paragraph::new(app.display_lines()), vertical[0]);

    if app.is_ready() {
        let duration = app.duration.unwrap_or(0.0);
        let display_position = app.preview_position.unwrap_or(app.position);
        let ratio = if duration > 0.0 {
            (display_position / duration).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let status = if app.dragging_progress {
            "Preview"
        } else {
            app.status()
        };
        let label = format!(
            "{} / {}  {}  {:.2}x",
            format_time(display_position),
            format_time(duration),
            status,
            app.speed
        );
        let progress = Gauge::default()
            .gauge_style(
                Style::default()
                    .fg(Color::Cyan)
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
            .label(label)
            .ratio(ratio);
        app.progress_area = Some(vertical[1]);
        frame.render_widget(progress, vertical[1]);
    } else {
        app.progress_area = Some(vertical[1]);
        frame.render_widget(Paragraph::new(app.loading_line()), vertical[1]);
    }

    let footer = match &app.mode {
        InputMode::Normal => controls_line(),
        InputMode::Jump(input) => Line::from(vec![
            Span::styled("Jump to ", Style::default().fg(Color::Yellow)),
            Span::raw(input),
            Span::styled("_", Style::default().fg(Color::Yellow)),
            Span::raw("  Enter=go  Esc=cancel  e.g. 12.5, 01:20, 75%"),
        ]),
    };
    let footer_layout = match app.header.terminal.term_type.as_deref() {
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

    if let Some(term_type) = app.header.terminal.term_type.as_deref() {
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

    fn assert_close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-9,
            "expected {expected}, got {actual}"
        );
    }

    fn test_app(duration: f64) -> App {
        let path = std::env::temp_dir().join(format!(
            "asciitape-tui-test-{}-{}.cast",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, "{\"version\":2,\"width\":80,\"height\":24}\n").unwrap();
        let header = crate::cast::load_cast_header(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let emulator = AlacrittyEmulator::new(header.width, header.height, &header.terminal);
        let (worker_tx, worker_rx) = mpsc::channel();

        App {
            header,
            events: EventBuffer::Loaded(Arc::new(Vec::new())),
            emulator,
            next_event: 0,
            position: 0.0,
            duration: Some(duration),
            loaded_until: duration,
            speed: 1.0,
            playing: false,
            buffering: false,
            fullscreen: false,
            dragging_progress: false,
            preview_position: None,
            preview_cache: None,
            progress_area: Some(Rect::new(0, 0, 10, 1)),
            last_tick: Instant::now(),
            mode: InputMode::Normal,
            load_state: LoadState::Ready,
            worker_rx,
            worker_tx,
        }
    }

    #[test]
    fn preview_frame_index_uses_nearest_time() {
        assert_eq!(preview_frame_index(0.0, 100.0, 5), Some(0));
        assert_eq!(preview_frame_index(12.0, 100.0, 5), Some(0));
        assert_eq!(preview_frame_index(13.0, 100.0, 5), Some(1));
        assert_eq!(preview_frame_index(87.0, 100.0, 5), Some(3));
        assert_eq!(preview_frame_index(88.0, 100.0, 5), Some(4));
        assert_eq!(preview_frame_index(100.0, 100.0, 5), Some(4));
        assert_eq!(preview_frame_index(50.0, 0.0, 5), Some(0));
        assert_eq!(preview_frame_index(50.0, 100.0, 0), None);
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
    fn finish_current_progress_drag_commits_preview_position() {
        let mut app = test_app(10.0);
        app.position = 5.0;
        app.dragging_progress = true;
        app.preview_position = Some(0.0);

        app.finish_current_progress_drag();

        assert!(!app.dragging_progress);
        assert_eq!(app.preview_position, None);
        assert_close(app.position, 0.0);
    }

    #[test]
    fn key_press_commits_stale_progress_drag_before_toggling_playback() {
        let mut app = test_app(10.0);
        app.position = 10.0;
        app.dragging_progress = true;
        app.preview_position = Some(10.0);

        handle_key_event(
            &mut app,
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::empty()),
        )
        .unwrap();

        assert!(!app.dragging_progress);
        assert_eq!(app.preview_position, None);
        assert!(app.playing);
        assert_close(app.position, 0.0);
    }

    #[test]
    fn mouse_move_commits_stale_progress_drag() {
        let mut app = test_app(10.0);
        app.position = 5.0;
        app.dragging_progress = true;
        app.preview_position = Some(10.0);

        handle_mouse_event(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Moved,
                column: 9,
                row: 0,
                modifiers: KeyModifiers::empty(),
            },
        )
        .unwrap();

        assert!(!app.dragging_progress);
        assert_eq!(app.preview_position, None);
        assert_close(app.position, 10.0);
    }
}
