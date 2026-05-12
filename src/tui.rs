use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, BufRead, BufReader, Stdout},
    path::PathBuf,
    sync::{
        Arc,
        mpsc::{self, Receiver, Sender, TryRecvError},
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
const SEEK_REPLAY_INTERRUPT_BATCH_SIZE: usize = 1024;
const SEEK_LOADING_FRAME_DURATION: Duration = Duration::from_millis(100);
const SEEK_LOADING_FRAMES: [char; 4] = ['|', '/', '-', '\\'];

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

struct PlaybackState {
    emulator: AlacrittyEmulator,
    next_event: usize,
    position: f64,
}

impl PlaybackState {
    fn new(header: &CastHeader) -> Self {
        Self {
            emulator: AlacrittyEmulator::new(header.width, header.height, &header.terminal),
            next_event: 0,
            position: 0.0,
        }
    }
}

struct PendingSeek {
    generation: u64,
    target: f64,
    started_at: Instant,
    display_snapshot: Vec<Line<'static>>,
}

struct SeekRequest {
    generation: u64,
    target: f64,
    active: Option<PlaybackState>,
}

#[derive(Clone, Copy)]
struct SeekTarget {
    generation: u64,
    target: f64,
}

enum SeekCommand {
    Seek(SeekRequest),
    Recycle(PlaybackState),
}

struct SeekResult {
    generation: u64,
    state: PlaybackState,
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
    SeekReady(Box<SeekResult>),
    Failed(String),
}

struct App {
    header: CastHeader,
    events: EventBuffer,
    playback: Option<PlaybackState>,
    display_cache: Vec<Line<'static>>,
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
    pending_seek: Option<PendingSeek>,
    seek_generation: u64,
    seek_tx: Option<Sender<SeekCommand>>,
    progress_area: Option<Rect>,
    last_tick: Instant,
    mode: InputMode,
    load_state: LoadState,
    worker_rx: Receiver<WorkerMessage>,
    worker_tx: Sender<WorkerMessage>,
}

impl App {
    fn new(path: PathBuf, header: CastHeader, paused: bool, fullscreen: bool) -> Self {
        let playback = PlaybackState::new(&header);
        let display_cache = playback.emulator.lines(&header.terminal);
        let (worker_tx, worker_rx) = mpsc::channel();
        start_event_loader(path, header.clone(), worker_tx.clone());

        Self {
            header,
            events: EventBuffer::Streaming(Vec::new()),
            playback: Some(playback),
            display_cache,
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
            pending_seek: None,
            seek_generation: 0,
            seek_tx: None,
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
                self.seek_tx = Some(start_seek_engine(
                    Arc::clone(&events),
                    self.header.clone(),
                    duration,
                    self.worker_tx.clone(),
                ));
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
            WorkerMessage::SeekReady(result) => self.handle_seek_ready(*result),
            WorkerMessage::Failed(error) => bail!("{error}"),
        }

        Ok(())
    }

    fn tick(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_tick).as_secs_f64();
        self.last_tick = now;
        self.buffering = false;

        if self.is_seeking() {
            return;
        }

        if !self.playing {
            return;
        }

        if self.playback.is_none() {
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

        if let Some(playback) = self.playback.as_mut() {
            playback.position = self.position;
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

    fn recycle_playback(&self, state: PlaybackState) {
        if let Some(seek_tx) = &self.seek_tx {
            let _ = seek_tx.send(SeekCommand::Recycle(state));
        }
    }

    fn set_pending_seek(
        &mut self,
        generation: u64,
        target: f64,
        display_snapshot: Vec<Line<'static>>,
    ) {
        self.pending_seek = Some(PendingSeek {
            generation,
            target,
            started_at: Instant::now(),
            display_snapshot,
        });
    }

    fn request_seek(
        &mut self,
        target: f64,
        active: Option<PlaybackState>,
        display_snapshot: Vec<Line<'static>>,
    ) -> (bool, Option<PlaybackState>) {
        let Some(seek_tx) = self.seek_tx.clone() else {
            return (false, active);
        };

        self.seek_generation = self.seek_generation.wrapping_add(1);
        let generation = self.seek_generation;
        self.set_pending_seek(generation, target, display_snapshot);

        match seek_tx.send(SeekCommand::Seek(SeekRequest {
            generation,
            target,
            active,
        })) {
            Ok(()) => (true, None),
            Err(error) => {
                self.pending_seek = None;
                if let SeekCommand::Seek(request) = error.0 {
                    (false, request.active)
                } else {
                    (false, None)
                }
            }
        }
    }

    fn seek(&mut self, position: f64) {
        let display_snapshot = self.current_display_snapshot();
        self.seek_with_snapshot(position, display_snapshot);
    }

    fn seek_with_snapshot(&mut self, position: f64, display_snapshot: Vec<Line<'static>>) {
        let Some(duration) = self.duration else {
            return;
        };
        let position = position.clamp(0.0, duration);
        let active = self.playback.take();

        self.position = position;
        self.buffering = false;
        self.last_tick = Instant::now();

        let (sent, active) = self.request_seek(position, active, display_snapshot);
        if sent {
            return;
        }
        self.playback = active;

        self.replay_until_position();
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
        let display_snapshot = target
            .and_then(|target| self.preview_snapshot(target))
            .unwrap_or_else(|| self.current_display_snapshot());
        self.dragging_progress = false;
        self.preview_position = None;

        if let Some(target) = target {
            self.seek_with_snapshot(target, display_snapshot);
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
        if self.is_seeking() {
            "Seeking"
        } else if self.buffering {
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

    fn is_seeking(&self) -> bool {
        self.pending_seek.is_some()
    }

    fn preview_snapshot(&self, position: f64) -> Option<Vec<Line<'static>>> {
        self.preview_cache
            .as_ref()?
            .nearest_frame(position)
            .map(|frame| frame.lines.clone())
    }

    fn current_display_snapshot(&self) -> Vec<Line<'static>> {
        if self.dragging_progress
            && let (Some(cache), Some(position)) = (&self.preview_cache, self.preview_position)
            && let Some(frame) = cache.nearest_frame(position)
        {
            return frame.lines.clone();
        }

        if let Some(pending) = &self.pending_seek {
            return pending.display_snapshot.clone();
        }

        if let Some(playback) = &self.playback {
            playback.emulator.lines(&self.header.terminal)
        } else {
            self.display_cache.clone()
        }
    }

    fn display_lines(&mut self) -> Vec<Line<'static>> {
        if self.dragging_progress
            && let (Some(cache), Some(position)) = (&self.preview_cache, self.preview_position)
            && let Some(frame) = cache.nearest_frame(position)
        {
            return frame.lines.clone();
        }

        if let Some(pending) = &self.pending_seek {
            return pending.display_snapshot.clone();
        }

        if let Some(playback) = &self.playback {
            let lines = playback.emulator.lines(&self.header.terminal);
            self.display_cache = lines.clone();
            lines
        } else {
            self.display_cache.clone()
        }
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

    fn seek_loading_text(&self) -> Option<String> {
        let pending = self.pending_seek.as_ref()?;
        let frame_duration = SEEK_LOADING_FRAME_DURATION.as_millis();
        let frame_index = (pending
            .started_at
            .elapsed()
            .as_millis()
            .checked_div(frame_duration)
            .unwrap_or(0)
            % SEEK_LOADING_FRAMES.len() as u128) as usize;

        Some(format!(
            "{} seeking {}",
            SEEK_LOADING_FRAMES[frame_index],
            format_time(pending.target)
        ))
    }

    fn handle_seek_ready(&mut self, result: SeekResult) {
        if self
            .pending_seek
            .as_ref()
            .is_none_or(|seek| seek.generation != result.generation)
        {
            self.recycle_playback(result.state);
            return;
        }

        self.position = result.state.position;
        self.playback = Some(result.state);
        self.pending_seek = None;
        self.last_tick = Instant::now();
    }

    fn apply_pending_events(&mut self) {
        let Some(playback) = self.playback.as_mut() else {
            return;
        };

        while playback.next_event < self.events.len() {
            let Some(event) = self.events.get(playback.next_event) else {
                break;
            };

            if event.time > playback.position {
                break;
            }

            apply_cast_event(&mut playback.emulator, event);
            playback.next_event += 1;
        }
    }

    fn replay_until_position(&mut self) {
        self.pending_seek = None;
        let mut playback = PlaybackState::new(&self.header);
        playback.position = self.position;
        self.playback = Some(playback);
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

fn start_seek_engine(
    events: Arc<Vec<CastEvent>>,
    header: CastHeader,
    duration: f64,
    tx: Sender<WorkerMessage>,
) -> Sender<SeekCommand> {
    let (seek_tx, seek_rx) = mpsc::channel();
    let _ = thread::spawn(move || SeekEngine::new(events, header, duration, seek_rx, tx).run());
    seek_tx
}

struct SeekEngine {
    events: Arc<Vec<CastEvent>>,
    header: CastHeader,
    duration: f64,
    rx: Receiver<SeekCommand>,
    tx: Sender<WorkerMessage>,
    pool: BTreeMap<usize, PlaybackState>,
}

impl SeekEngine {
    fn new(
        events: Arc<Vec<CastEvent>>,
        header: CastHeader,
        duration: f64,
        rx: Receiver<SeekCommand>,
        tx: Sender<WorkerMessage>,
    ) -> Self {
        Self {
            events,
            header,
            duration,
            rx,
            tx,
            pool: BTreeMap::new(),
        }
    }

    fn run(mut self) {
        while let Ok(command) = self.rx.recv() {
            match command {
                SeekCommand::Seek(request) => self.run_seek(request),
                SeekCommand::Recycle(state) => self.insert_state(state),
            }
        }
    }

    fn run_seek(&mut self, request: SeekRequest) {
        let mut target = self.prepare_seek_request(request);

        loop {
            match self.drain_commands() {
                Ok(Some(latest)) => target = latest,
                Ok(None) => {}
                Err(()) => return,
            }

            match self.replay_to_target(target) {
                SeekWorkerResult::Ready(result) => {
                    if self.tx.send(WorkerMessage::SeekReady(result)).is_err() {
                        return;
                    }
                    break;
                }
                SeekWorkerResult::Interrupted(latest) => target = latest,
                SeekWorkerResult::Closed => return,
            }
        }
    }

    fn prepare_seek_request(&mut self, mut request: SeekRequest) -> SeekTarget {
        if let Some(active) = request.active.take() {
            self.insert_state(active);
        }

        SeekTarget {
            generation: request.generation,
            target: request.target,
        }
    }

    fn drain_commands(&mut self) -> Result<Option<SeekTarget>, ()> {
        let mut latest = None;

        loop {
            match self.rx.try_recv() {
                Ok(SeekCommand::Seek(request)) => {
                    latest = Some(self.prepare_seek_request(request));
                }
                Ok(SeekCommand::Recycle(state)) => self.insert_state(state),
                Err(TryRecvError::Empty) => return Ok(latest),
                Err(TryRecvError::Disconnected) => return Err(()),
            }
        }
    }

    fn replay_to_target(&mut self, target: SeekTarget) -> SeekWorkerResult {
        let target_next_event = target_next_event(self.events.as_ref(), target.target);
        let mut state = self.take_state_for_target(target_next_event);

        while state.next_event < target_next_event {
            for _ in 0..SEEK_REPLAY_INTERRUPT_BATCH_SIZE {
                if state.next_event >= target_next_event {
                    break;
                }

                apply_cast_event(&mut state.emulator, &self.events[state.next_event]);
                state.next_event += 1;
            }

            match self.drain_commands() {
                Ok(Some(latest)) => {
                    self.insert_state(state);
                    return SeekWorkerResult::Interrupted(latest);
                }
                Ok(None) => {}
                Err(()) => return SeekWorkerResult::Closed,
            }
        }

        state.position = target.target;
        SeekWorkerResult::Ready(Box::new(SeekResult {
            generation: target.generation,
            state,
        }))
    }

    fn take_state_for_target(&mut self, target_next_event: usize) -> PlaybackState {
        let key = self
            .pool
            .range(..=target_next_event)
            .next_back()
            .map(|(&key, _)| key);

        key.and_then(|key| self.pool.remove(&key))
            .unwrap_or_else(|| PlaybackState::new(&self.header))
    }

    fn insert_state(&mut self, state: PlaybackState) {
        if self.is_complete(&state) {
            return;
        }

        self.pool.entry(state.next_event).or_insert(state);
    }

    fn is_complete(&self, state: &PlaybackState) -> bool {
        state.next_event >= self.events.len() && state.position >= self.duration
    }
}

enum SeekWorkerResult {
    Ready(Box<SeekResult>),
    Interrupted(SeekTarget),
    Closed,
}

fn target_next_event(events: &[CastEvent], target: f64) -> usize {
    events.partition_point(|event| event.time <= target)
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
    let seek_loading_text = app.seek_loading_text();
    let seek_loading_width = seek_loading_text
        .as_ref()
        .map_or(0, |text| text.len().saturating_add(2) as u16);
    let term_width = app
        .header
        .terminal
        .term_type
        .as_ref()
        .map_or(0, |term_type| term_type.len().saturating_add(2) as u16);
    let footer_layout = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(seek_loading_width),
            Constraint::Length(term_width),
        ])
        .split(vertical[2]);
    frame.render_widget(Paragraph::new(footer), footer_layout[0]);

    if let Some(text) = seek_loading_text {
        let text = Line::from(vec![Span::styled(text, Style::default().fg(Color::Yellow))]);
        frame.render_widget(
            Paragraph::new(text).alignment(Alignment::Right),
            footer_layout[1],
        );
    }

    if let Some(term_type) = app.header.terminal.term_type.as_deref() {
        let term_type = Line::from(vec![Span::styled(
            term_type,
            Style::default().fg(Color::DarkGray),
        )]);
        frame.render_widget(
            Paragraph::new(term_type).alignment(Alignment::Right),
            footer_layout[2],
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
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEST_FILE_ID: AtomicU64 = AtomicU64::new(0);

    fn assert_close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-9,
            "expected {expected}, got {actual}"
        );
    }

    fn test_header() -> CastHeader {
        let file_id = NEXT_TEST_FILE_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "asciitape-tui-test-{}-{}-{}.cast",
            std::process::id(),
            file_id,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, "{\"version\":2,\"width\":80,\"height\":24}\n").unwrap();
        let header = crate::cast::load_cast_header(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        header
    }

    fn test_app(duration: f64) -> App {
        let header = test_header();
        let playback = PlaybackState::new(&header);
        let display_cache = playback.emulator.lines(&header.terminal);
        let (worker_tx, worker_rx) = mpsc::channel();
        let events = Arc::new(Vec::new());
        let seek_tx = start_seek_engine(
            Arc::clone(&events),
            header.clone(),
            duration,
            worker_tx.clone(),
        );

        App {
            header,
            events: EventBuffer::Loaded(events),
            playback: Some(playback),
            display_cache,
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
            pending_seek: None,
            seek_generation: 0,
            seek_tx: Some(seek_tx),
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
    fn seek_engine_drain_commands_keeps_only_newest_target() {
        let (seek_tx, seek_rx) = mpsc::channel();
        let (worker_tx, _worker_rx) = mpsc::channel();
        let mut engine = SeekEngine::new(
            Arc::new(Vec::new()),
            test_header(),
            10.0,
            seek_rx,
            worker_tx,
        );

        seek_tx
            .send(SeekCommand::Seek(SeekRequest {
                generation: 1,
                target: 1.0,
                active: None,
            }))
            .unwrap();
        seek_tx
            .send(SeekCommand::Seek(SeekRequest {
                generation: 2,
                target: 2.0,
                active: None,
            }))
            .unwrap();

        let latest = engine.drain_commands().unwrap().unwrap();

        assert_eq!(latest.generation, 2);
        assert_close(latest.target, 2.0);
        assert!(engine.drain_commands().unwrap().is_none());
    }

    #[test]
    fn seek_engine_reuses_nearest_earlier_state_by_event_index() {
        let (_seek_tx, seek_rx) = mpsc::channel();
        let (worker_tx, _worker_rx) = mpsc::channel();
        let header = test_header();
        let events = Arc::new(vec![
            CastEvent {
                time: 1.0,
                kind: EventKind::Output,
                data: "a".to_string(),
            },
            CastEvent {
                time: 2.0,
                kind: EventKind::Output,
                data: "b".to_string(),
            },
            CastEvent {
                time: 3.0,
                kind: EventKind::Output,
                data: "c".to_string(),
            },
        ]);
        let mut engine = SeekEngine::new(events, header.clone(), 10.0, seek_rx, worker_tx);
        let mut early = PlaybackState::new(&header);
        early.next_event = 1;
        early.position = 1.0;
        let mut nearest = PlaybackState::new(&header);
        nearest.next_event = 2;
        nearest.position = 2.0;

        engine.insert_state(early);
        engine.insert_state(nearest);

        let state = engine.take_state_for_target(2);

        assert_eq!(state.next_event, 2);
        assert!(!engine.pool.contains_key(&2));
        assert!(engine.pool.contains_key(&1));
    }

    #[test]
    fn seek_engine_interrupts_replay_and_recycles_partial_state() {
        let (seek_tx, seek_rx) = mpsc::channel();
        let (worker_tx, _worker_rx) = mpsc::channel();
        let header = test_header();
        let events = Arc::new(
            (0..SEEK_REPLAY_INTERRUPT_BATCH_SIZE + 10)
                .map(|index| CastEvent {
                    time: index as f64,
                    kind: EventKind::Other,
                    data: String::new(),
                })
                .collect::<Vec<_>>(),
        );
        let mut engine = SeekEngine::new(events, header, 10_000.0, seek_rx, worker_tx);

        seek_tx
            .send(SeekCommand::Seek(SeekRequest {
                generation: 2,
                target: 1.0,
                active: None,
            }))
            .unwrap();

        let result = engine.replay_to_target(SeekTarget {
            generation: 1,
            target: 10_000.0,
        });

        match result {
            SeekWorkerResult::Interrupted(target) => {
                assert_eq!(target.generation, 2);
                assert_close(target.target, 1.0);
            }
            SeekWorkerResult::Ready(_) | SeekWorkerResult::Closed => {
                panic!("expected replay interruption")
            }
        }
        assert!(engine.pool.contains_key(&SEEK_REPLAY_INTERRUPT_BATCH_SIZE));
    }

    #[test]
    fn seek_engine_discards_states_that_reached_end() {
        let (_seek_tx, seek_rx) = mpsc::channel();
        let (worker_tx, _worker_rx) = mpsc::channel();
        let header = test_header();
        let mut engine = SeekEngine::new(
            Arc::new(Vec::new()),
            header.clone(),
            10.0,
            seek_rx,
            worker_tx,
        );
        let mut state = PlaybackState::new(&header);
        state.position = 10.0;

        engine.insert_state(state);

        assert!(engine.pool.is_empty());
    }

    #[test]
    fn mouse_seek_uses_target_preview_as_pending_snapshot() {
        let mut app = test_app(10.0);
        app.dragging_progress = true;
        app.preview_position = Some(0.0);
        app.preview_cache = Some(PreviewCache {
            duration: 10.0,
            frames: vec![
                PreviewFrame {
                    lines: vec![Line::from("old preview")],
                },
                PreviewFrame {
                    lines: vec![Line::from("target preview")],
                },
            ],
        });

        app.finish_progress_drag_at(Some(10.0));

        assert!(!app.dragging_progress);
        assert_eq!(app.preview_position, None);
        assert_eq!(
            app.pending_seek.as_ref().unwrap().display_snapshot,
            vec![Line::from("target preview")]
        );
        assert_eq!(app.display_lines(), vec![Line::from("target preview")]);
    }

    #[test]
    fn stale_seek_result_preserves_newer_pending_snapshot() {
        let mut app = test_app(10.0);
        app.pending_seek = Some(PendingSeek {
            generation: 2,
            target: 2.0,
            started_at: Instant::now(),
            display_snapshot: vec![Line::from("newer snapshot")],
        });

        app.handle_seek_ready(SeekResult {
            generation: 1,
            state: PlaybackState::new(&app.header),
        });

        let pending = app.pending_seek.as_ref().unwrap();
        assert_eq!(pending.generation, 2);
        assert_close(pending.target, 2.0);
        assert_eq!(pending.display_snapshot, vec![Line::from("newer snapshot")]);
        assert_eq!(app.display_lines(), vec![Line::from("newer snapshot")]);
    }

    #[test]
    fn seek_marks_loading_and_new_seek_replaces_target() {
        let mut app = test_app(10.0);

        app.seek(8.0);
        let first_generation = app.pending_seek.as_ref().unwrap().generation;
        app.seek(2.0);

        let pending = app.pending_seek.as_ref().unwrap();
        assert!(pending.generation > first_generation);
        assert_close(pending.target, 2.0);
        assert_close(app.position, 2.0);
        assert_eq!(app.status(), "Seeking");
        assert!(app.seek_loading_text().unwrap().contains("seeking"));
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
