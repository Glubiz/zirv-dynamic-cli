use std::io::{IsTerminal, stdout};
use std::time::{Duration, Instant};

use crossterm::event::{Event, EventStream, KeyCode, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use tokio::sync::mpsc;
use tokio::time::interval;

use crate::script_runner::script::Script;
use crate::script_runner::{self, UiEvent};

/// A single log line with optional error highlighting.
#[derive(Clone)]
struct LogLine {
    content: String,
    is_error: bool,
}

/// Application state for the TUI.
struct App {
    title: String,
    description: Option<String>,
    logs: Vec<LogLine>,
    scroll: u16,
    view_height: u16,
    auto_scroll: bool,
    active_command: Option<String>,
    start: Instant,
    exit_status: Option<i32>,
    should_quit: bool,
}

impl App {
    fn new(title: String, description: Option<String>) -> Self {
        Self {
            title,
            description,
            logs: Vec::new(),
            scroll: 0,
            view_height: 0,
            auto_scroll: true,
            active_command: None,
            start: Instant::now(),
            exit_status: None,
            should_quit: false,
        }
    }

    fn push_log(&mut self, line: String, is_error: bool) {
        self.logs.push(LogLine {
            content: line,
            is_error,
        });
    }

    fn scroll_up(&mut self) {
        if self.scroll > 0 {
            self.scroll -= 1;
            self.auto_scroll = false;
        }
    }

    fn scroll_down(&mut self) {
        if (self.scroll as usize) < self.logs.len().saturating_sub(1) {
            self.scroll += 1;
        } else {
            self.auto_scroll = true;
        }
    }

    fn scroll_page_up(&mut self) {
        let step = self.view_height.max(1);
        self.scroll = self.scroll.saturating_sub(step);
        self.auto_scroll = false;
    }

    fn scroll_page_down(&mut self) {
        let step = self.view_height.max(1);
        let max_scroll = self.logs.len().saturating_sub(self.view_height as usize);
        let new_scroll = (self.scroll as usize + step as usize).min(max_scroll);
        self.scroll = new_scroll as u16;
    }
}

/// Run the Ratatui interface.
pub async fn run(script: &Script, params: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if !stdout().is_terminal() {
        // Fallback to plain output
        return script_runner::execute(script, params, None)
            .await
            .map_err(|e| e.into());
    }

    enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(out);
    let mut terminal = Terminal::new(backend)?;

    let (tx, mut rx) = mpsc::channel::<UiEvent>(100);
    let script_clone = script.clone();
    let params_vec = params.to_vec();
    tokio::spawn(async move {
        let _ = script_runner::execute(&script_clone, &params_vec, Some(tx)).await;
    });

    let mut app = App::new(script.name.clone(), script.description.clone());
    let mut events = EventStream::new();
    let mut tick = interval(Duration::from_millis(100));

    loop {
        terminal.draw(|f| draw_ui(f, &mut app))?;

        tokio::select! {
            _ = tick.tick() => {},
            Some(Ok(ev)) = events.next() => {
                if let Event::Key(key) = ev {
                    match key.code {
                        KeyCode::Char('q') => app.should_quit = true,
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => app.should_quit = true,
                        KeyCode::Up => app.scroll_up(),
                        KeyCode::Down => app.scroll_down(),
                        KeyCode::PageUp => app.scroll_page_up(),
                        KeyCode::PageDown => app.scroll_page_down(),
                        KeyCode::End => { app.auto_scroll = true; },
                        _ => {}
                    }
                }
            }
            Some(event) = rx.recv() => {
                match event {
                    UiEvent::Log { line, is_error } => {
                        app.push_log(line, is_error);
                    }
                    UiEvent::CommandStart { command } => {
                        app.active_command = Some(command);
                    }
                    UiEvent::CommandEnd { status } => {
                        app.exit_status = Some(status);
                        app.active_command = None;
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                app.should_quit = true;
            }
        }

        if app.should_quit {
            break;
        }
    }

    disable_raw_mode()?;
    let mut out = stdout();
    execute!(out, LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

fn draw_ui(f: &mut ratatui::Frame, app: &mut App) {
    let size = f.size();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(size);

    // Header
    let mut header_lines = vec![Line::from(app.title.clone())];
    if let Some(desc) = &app.description {
        header_lines.push(Line::from(desc.clone()));
    }
    let header = Paragraph::new(header_lines).block(Block::default().borders(Borders::ALL));
    f.render_widget(header, chunks[0]);

    // Logs panel
    app.view_height = chunks[1].height.saturating_sub(2);
    if app.auto_scroll {
        let max_scroll = app.logs.len().saturating_sub(app.view_height as usize);
        app.scroll = max_scroll as u16;
    }
    let lines: Vec<Line> = app
        .logs
        .iter()
        .map(|l| {
            let style = if l.is_error {
                Style::default().fg(Color::Red)
            } else {
                Style::default()
            };
            Line::from(Span::styled(l.content.clone(), style))
        })
        .collect();
    let log_widget = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((app.scroll, 0))
        .block(Block::default().title("Logs").borders(Borders::ALL));
    f.render_widget(log_widget, chunks[1]);

    // Status bar
    let elapsed = app.start.elapsed();
    let status = format!(
        "Cmd: {} | Elapsed: {}s | Exit: {}",
        app.active_command.as_deref().unwrap_or("idle"),
        elapsed.as_secs(),
        app.exit_status
            .map(|c| c.to_string())
            .unwrap_or_else(|| "running".into())
    );
    let status_bar = Paragraph::new(status).block(Block::default().borders(Borders::ALL));
    f.render_widget(status_bar, chunks[2]);
}